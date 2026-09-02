//! mailarchive: tiny incremental, IDLE-driven IMAP -> Maildir sync (like mbsync, pull-only).
//!
//! Every IMAP folder becomes a Maildir (`cur/`, `new/`, `tmp/`) below the configured root,
//! nested according to the server's hierarchy delimiter. Messages are stored verbatim as
//! RFC 5322 text. Per folder a `.uidvalidity` file remembers UIDVALIDITY and the highest
//! UID seen, so every run only fetches what is new; flags and deletions are mirrored too.

use imap_proto::NameAttribute;
use imap::types::Flag;
use imap::{extensions::idle::WaitOutcome, Connection, ConnectionMode, Session};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, process::Command, thread};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Sess = Session<Connection>;

#[derive(Deserialize)]
struct Config {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    user: String,
    pass: Option<String>,
    pass_cmd: Option<String>,
    maildir: String,
    /// false = plain TCP without TLS (local/test servers only)
    #[serde(default = "default_tls")]
    tls: bool,
    folders: Option<Vec<String>>,
    #[serde(default = "default_idle")]
    idle_secs: u64,
}
fn default_port() -> u16 { 993 }
fn default_idle() -> u64 { 1500 }
fn default_tls() -> bool { true }

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let cfg: Config = fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|s| toml::from_str(&s).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| fatal(&format!("{path}: {e}")));
    let pass = match (&cfg.pass, &cfg.pass_cmd) {
        (Some(p), _) => p.clone(),
        (None, Some(cmd)) => {
            let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap_or_else(|e| fatal(&e.to_string()));
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }
        _ => fatal("config needs `pass` or `pass_cmd`"),
    };
    let root = expand_home(&cfg.maildir);
    loop {
        if let Err(e) = run(&cfg, &pass, &root) {
            eprintln!("error: {e}; reconnecting in 30s");
            thread::sleep(Duration::from_secs(30));
        }
    }
}

fn fatal(msg: &str) -> ! {
    eprintln!("mailarchive: {msg}");
    std::process::exit(1)
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

/// One connection lifetime: full sync, then loop IDLE on INBOX -> sync.
fn run(cfg: &Config, pass: &str, root: &Path) -> Result<()> {
    let mode = if cfg.tls { ConnectionMode::AutoTls } else { ConnectionMode::Plaintext };
    let client = imap::ClientBuilder::new(&cfg.host, cfg.port).mode(mode).connect()?;
    let mut s = client.login(&cfg.user, pass).map_err(|(e, _)| e)?;
    eprintln!("connected to {}", cfg.host);

    // (imap name, local path) for every selectable folder, optionally filtered by config.
    let folders: Vec<(String, PathBuf)> = s
        .list(None, Some("*"))?
        .iter()
        .filter(|n| !n.attributes().contains(&NameAttribute::NoSelect))
        .filter(|n| cfg.folders.as_ref().is_none_or(|f| f.iter().any(|x| x == n.name())))
        .map(|n| {
            let delim = n.delimiter().unwrap_or("/");
            (n.name().to_string(), n.name().split(delim).fold(root.to_path_buf(), |p, c| p.join(c)))
        })
        .collect();

    let mut idle_inbox = true;
    loop {
        for (name, dir) in &folders {
            // after an INBOX event only INBOX is synced; on timeout everything is
            if idle_inbox || name == "INBOX" {
                sync_folder(&mut s, name, dir)?;
            }
        }
        s.examine("INBOX")?;
        let mut h = s.idle();
        h.timeout(Duration::from_secs(cfg.idle_secs)).keepalive(false);
        idle_inbox = h.wait_while(imap::extensions::idle::stop_on_any)? == WaitOutcome::TimedOut;
    }
}

/// Bring one local Maildir in line with the remote folder.
fn sync_folder(s: &mut Sess, name: &str, dir: &Path) -> Result<()> {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    let mb = s.examine(name)?;
    let uv = mb.uid_validity.unwrap_or(0);

    // state: "<uidvalidity>\n<last uid>"
    let state_file = dir.join(".uidvalidity");
    let state = fs::read_to_string(&state_file).unwrap_or_default();
    let mut st = state.lines().map(|l| l.trim().parse::<u32>().unwrap_or(0));
    let (old_uv, mut last_uid) = (st.next().unwrap_or(0), st.next().unwrap_or(0));

    let mut local = scan_local(dir)?;
    if old_uv != uv && !local.is_empty() {
        eprintln!("{name}: UIDVALIDITY changed ({old_uv} -> {uv}), resetting folder");
        for p in local.values() {
            fs::remove_file(p)?;
        }
        local.clear();
        last_uid = 0;
    }

    // remote uid -> maildir flags
    let mut remote: HashMap<u32, String> = HashMap::new();
    if mb.exists > 0 {
        for f in s.uid_fetch("1:*", "(UID FLAGS)")?.iter() {
            if let Some(uid) = f.uid {
                remote.insert(uid, maildir_flags(f.flags()));
            }
        }
    }

    // deletions and flag changes
    let mut removed = 0;
    for (uid, path) in &local {
        match remote.get(uid) {
            None => {
                fs::remove_file(path)?;
                removed += 1;
            }
            Some(flags) => {
                let want = dir.join(if flags.contains('S') { "cur" } else { "new" }).join(file_name(uv, *uid, flags));
                if *path != want {
                    fs::rename(path, want)?;
                }
            }
        }
    }

    // new messages, fetched in small batches, written via tmp/ then renamed
    let mut new: Vec<u32> = remote.keys().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable();
    let mut added = 0;
    for chunk in new.chunks(25) {
        let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        for f in s.uid_fetch(set, "(UID FLAGS BODY.PEEK[])")?.iter() {
            let (Some(uid), Some(body)) = (f.uid, f.body()) else { continue };
            let flags = maildir_flags(f.flags());
            let fname = file_name(uv, uid, &flags);
            let tmp = dir.join("tmp").join(&fname);
            fs::write(&tmp, body)?;
            fs::rename(&tmp, dir.join(if flags.contains('S') { "cur" } else { "new" }).join(&fname))?;
            last_uid = last_uid.max(uid);
            added += 1;
        }
    }
    if let Some(m) = remote.keys().max() {
        last_uid = last_uid.max(*m);
    }
    fs::write(&state_file, format!("{uv}\n{last_uid}\n"))?;
    if added + removed > 0 || old_uv != uv {
        eprintln!("{name}: +{added} -{removed} (total {})", remote.len());
    }
    Ok(())
}

/// Maildir file name; the `,U=<uid>` part is what mbsync uses too.
fn file_name(uv: u32, uid: u32, flags: &str) -> String {
    format!("{uv}.{uid}.mailarchive,U={uid}:2,{flags}")
}

/// Map IMAP system flags to Maildir info letters (sorted, as the spec requires).
fn maildir_flags(flags: &[Flag]) -> String {
    let mut v: Vec<char> = flags
        .iter()
        .filter_map(|f| match f {
            Flag::Draft => Some('D'),
            Flag::Flagged => Some('F'),
            Flag::Answered => Some('R'),
            Flag::Seen => Some('S'),
            Flag::Deleted => Some('T'),
            _ => None,
        })
        .collect();
    v.sort_unstable();
    v.into_iter().collect()
}

/// uid -> path for all messages in cur/ and new/.
fn scan_local(dir: &Path) -> Result<HashMap<u32, PathBuf>> {
    let mut m = HashMap::new();
    for sub in ["cur", "new"] {
        for e in fs::read_dir(dir.join(sub))? {
            let p = e?.path();
            let uid = p
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split(",U=").nth(1))
                .and_then(|r| r.split(':').next())
                .and_then(|u| u.parse().ok());
            if let Some(uid) = uid {
                m.insert(uid, p);
            }
        }
    }
    Ok(m)
}
