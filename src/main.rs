//! mailarchive: tiny incremental, IDLE-driven IMAP -> Maildir sync (like mbsync, pull-only).
//!
//! Every IMAP folder becomes a Maildir (`cur/`, `new/`, `tmp/`) below the configured root,
//! nested according to the server's hierarchy delimiter. Messages are stored verbatim as
//! RFC 5322 text. Per folder a `.uidvalidity` file remembers UIDVALIDITY and the highest
//! UID seen, so every run only fetches what is new.
//!
//! It is an archive, not a mirror: a message that disappears server-side stays on disk
//! unless `--expunge` is set, a changed UIDVALIDITY makes the folder stop rather than
//! delete anything, and every message is fsynced before its name becomes visible.
//!
//! Configuration comes from CLI flags, each with an env fallback; a `.env` in the working
//! directory is loaded first. The password is the one setting with no flag - it would be
//! world-readable in `ps` - so it lives in `MAILARCHIVE_PASS` or behind `--pass-cmd`.

use clap::Parser;
use imap_proto::NameAttribute;
use imap::types::Flag;
use imap::{extensions::idle::WaitOutcome, Connection, ConnectionMode, Session};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, process::Command, thread};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Sess = Session<Connection>;

/// Incremental, IDLE-driven IMAP -> Maildir archive.
#[derive(Parser)]
#[command(version, about, after_help = "\
The password has no flag on purpose (it would show up in `ps`): set MAILARCHIVE_PASS,
or point --pass-cmd at a command that prints it. Every flag can also be given as its
env var, and a `.env` in the working directory is read before the flags are parsed.")]
struct Config {
    /// IMAP server host
    #[arg(long, env = "MAILARCHIVE_HOST")]
    host: String,

    /// IMAP server port
    #[arg(long, env = "MAILARCHIVE_PORT", default_value_t = 993)]
    port: u16,

    /// Login user
    #[arg(long, env = "MAILARCHIVE_USER")]
    user: String,

    /// Command printing the password on stdout (alternative to MAILARCHIVE_PASS)
    #[arg(long, env = "MAILARCHIVE_PASS_CMD")]
    pass_cmd: Option<String>,

    /// Local Maildir root ("~" is expanded)
    #[arg(long, env = "MAILARCHIVE_MAILDIR")]
    maildir: String,

    /// Use TLS; `--tls false` is plain TCP for local/test servers only
    #[arg(long, env = "MAILARCHIVE_TLS", default_value_t = true,
          num_args = 0..=1, default_missing_value = "true")]
    tls: bool,

    /// Only sync these folders (comma separated; default: all)
    #[arg(long, env = "MAILARCHIVE_FOLDERS", value_delimiter = ',')]
    folders: Vec<String>,

    /// Seconds to wait in IDLE on INBOX before a full re-sync of all folders
    #[arg(long, env = "MAILARCHIVE_IDLE_SECS", default_value_t = 1500)]
    idle_secs: u64,

    /// Mirror server-side deletions by deleting the local copy. Off by default: this is an
    /// archive, so what vanishes upstream (expunge, retention policy, a stray phone tap)
    /// is kept here.
    #[arg(long, env = "MAILARCHIVE_EXPUNGE", default_value_t = false,
          num_args = 0..=1, default_missing_value = "true")]
    expunge: bool,

    /// Mirror flag changes (\Seen, \Flagged, ...) by renaming the local file
    #[arg(long, env = "MAILARCHIVE_SYNC_FLAGS", default_value_t = true,
          num_args = 0..=1, default_missing_value = "true")]
    sync_flags: bool,
}

fn main() {
    // before the flags are parsed, so `.env` feeds clap's env fallbacks
    let _ = dotenvy::dotenv();
    let cfg = Config::parse();
    let pass = match (env::var("MAILARCHIVE_PASS").ok(), &cfg.pass_cmd) {
        (Some(p), _) if !p.is_empty() => p,
        (_, Some(cmd)) => {
            let out = Command::new("sh").arg("-c").arg(cmd).output().unwrap_or_else(|e| fatal(&e.to_string()));
            if !out.status.success() {
                fatal(&format!("--pass-cmd failed: {}", String::from_utf8_lossy(&out.stderr).trim_end()));
            }
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }
        _ => fatal("no password: set MAILARCHIVE_PASS or pass --pass-cmd"),
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
        .filter(|n| cfg.folders.is_empty() || cfg.folders.iter().any(|x| x == n.name()))
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
                sync_folder(&mut s, cfg, name, dir)?;
            }
        }
        s.examine("INBOX")?;
        let mut h = s.idle();
        h.timeout(Duration::from_secs(cfg.idle_secs)).keepalive(false);
        idle_inbox = h.wait_while(imap::extensions::idle::stop_on_any)? == WaitOutcome::TimedOut;
    }
}

/// Bring one local Maildir in line with the remote folder.
fn sync_folder(s: &mut Sess, cfg: &Config, name: &str, dir: &Path) -> Result<()> {
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

    let local = scan_local(dir)?;
    // A changed UIDVALIDITY invalidates every local UID, but that is never a reason to
    // delete mail: the server may have been migrated or the folder recreated. Refuse to
    // touch the folder and let the operator decide (move the directory aside, then let the
    // next run refetch it into a fresh one).
    if old_uv != uv && !local.is_empty() {
        eprintln!(
            "{name}: UIDVALIDITY changed ({old_uv} -> {uv}) - folder left untouched. \
             Move {} aside (or fix .uidvalidity) to resume syncing it.",
            dir.display()
        );
        return Ok(());
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

    // vanished messages and flag changes
    let (mut removed, mut orphans) = (0, 0);
    for (uid, path) in &local {
        match remote.get(uid) {
            None if cfg.expunge => {
                fs::remove_file(path)?;
                removed += 1;
            }
            // gone upstream but kept here - that is the point of an archive
            None => orphans += 1,
            Some(flags) if cfg.sync_flags => {
                let want = dir.join(if flags.contains('S') { "cur" } else { "new" }).join(file_name(uv, *uid, flags));
                if *path != want {
                    fs::rename(path, &want)?;
                    fsync_dir(want.parent().unwrap_or(dir))?;
                }
            }
            Some(_) => {}
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
            let dst = dir.join(if flags.contains('S') { "cur" } else { "new" }).join(&fname);
            write_durable(&tmp, &dst, body)?;
            last_uid = last_uid.max(uid);
            added += 1;
        }
    }
    if let Some(m) = remote.keys().max() {
        last_uid = last_uid.max(*m);
    }
    // only now that every message body is on disk may the state advance
    let state_tmp = dir.join(".uidvalidity.tmp");
    write_durable(&state_tmp, &state_file, format!("{uv}\n{last_uid}\n").as_bytes())?;
    if added + removed > 0 || old_uv != uv {
        let kept = if orphans > 0 { format!(" ~{orphans} kept") } else { String::new() };
        eprintln!("{name}: +{added} -{removed}{kept} (total {})", remote.len());
    }
    Ok(())
}

/// Write `data` to `tmp` and move it to `dst` so that the name only ever becomes visible
/// once the content is on stable storage: fsync the file before the rename, fsync the
/// containing directory after it. Without this a power cut leaves correctly named but empty
/// files, and since the UID is part of the name they would never be fetched again.
fn write_durable(tmp: &Path, dst: &Path, data: &[u8]) -> Result<()> {
    let mut f = fs::File::create(tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, dst)?;
    fsync_dir(dst.parent().unwrap_or(Path::new(".")))
}

/// fsync a directory so a rename into it survives a crash.
fn fsync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)?.sync_all()?;
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
