//! One account's connection lifetime: full sync, then IDLE on INBOX, and the per-folder
//! sync that keeps the local Maildir in line with the server.

use crate::config::{Account, Config};
use crate::maildir;
use crate::Result;
use imap::{extensions::idle::WaitOutcome, Connection, Session};
use imap_proto::NameAttribute;
use std::collections::{HashMap, HashSet};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

type Sess = Session<Connection>;

/// Socket timeouts outside IDLE. A half-open connection (laptop sleep, NAT reset) would
/// otherwise block the account thread forever with no reconnect.
const IO_TIMEOUT: Duration = Duration::from_secs(120);

/// Connect, sync every folder, then loop: IDLE on INBOX -> sync. Returns only on error,
/// which the caller turns into a reconnect.
pub fn run(cfg: &Config, acc: &Account, pass: &str, root: &Path) -> Result<()> {
    let (client, socket) = connect(acc)?;
    let mut s = client.login(&acc.user, pass).map_err(|(e, _)| e)?;
    eprintln!("{}: connected to {}", acc.name, acc.host);

    // The first pass and every IDLE timeout sync all folders; an INBOX event syncs only INBOX.
    let mut sync_all = true;
    loop {
        // re-LIST on every full pass so folders created server-side show up
        for (name, dir) in folders(&mut s, acc, root)? {
            if sync_all || name == "INBOX" {
                sync_folder(&mut s, cfg, acc, &name, &dir).or_else(|e| skip_if_folder_error(acc, &name, e))?;
            }
        }
        s.examine("INBOX")?;
        let mut h = s.idle();
        h.timeout(Duration::from_secs(cfg.idle_secs)).keepalive(false);
        sync_all = h.wait_while(imap::extensions::idle::stop_on_any)? == WaitOutcome::TimedOut;
        // IDLE clears the read timeout when it returns; put ours back
        socket.set_read_timeout(Some(IO_TIMEOUT))?;
    }
}

/// A tagged NO/BAD (no permission, vanished folder) is this folder's problem only; the
/// session is still in sync, so log and carry on. Anything else ends the connection.
fn skip_if_folder_error(acc: &Account, name: &str, e: Box<dyn std::error::Error>) -> Result<()> {
    match e.downcast_ref::<imap::Error>() {
        Some(imap::Error::No(_) | imap::Error::Bad(_)) => {
            eprintln!("{}/{name}: skipped: {e}", acc.name);
            Ok(())
        }
        _ => Err(e),
    }
}

/// Open the TCP connection ourselves so that we keep a handle on the socket for timeouts.
/// `tls: true` is implicit TLS (port 993); STARTTLS is not supported.
fn connect(acc: &Account) -> Result<(imap::Client<Connection>, TcpStream)> {
    let addr = (acc.host.as_str(), acc.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| format!("{}: {} does not resolve", acc.name, acc.host))?;
    let tcp = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(IO_TIMEOUT))?;
    let socket = tcp.try_clone()?;
    let conn: Connection = if acc.tls {
        Box::new(native_tls::TlsConnector::new()?.connect(&acc.host, tcp)?)
    } else {
        Box::new(tcp)
    };
    let mut client = imap::Client::new(conn);
    client.read_greeting()?;
    Ok((client, socket))
}

/// (imap name, local path) for every selectable folder, optionally filtered by config.
/// A folder whose name cannot be mapped to a directory below `root`, or whose directory
/// another folder already uses, is skipped with a warning rather than merged.
fn folders(s: &mut Sess, acc: &Account, root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut taken = HashSet::new();
    let mut out = Vec::new();
    for n in s.list(None, Some("*"))?.iter() {
        let selectable = !n.attributes().contains(&NameAttribute::NoSelect);
        let wanted = acc.folders.is_empty() || acc.folders.iter().any(|x| x == n.name());
        if !(selectable && wanted) {
            continue;
        }
        match maildir::folder_path(root, n.name(), n.delimiter()) {
            Some(dir) if taken.insert(dir.clone()) => out.push((n.name().to_string(), dir)),
            Some(dir) => eprintln!(
                "{}: folder {:?} skipped: {} is already used by another folder",
                acc.name,
                n.name(),
                dir.display()
            ),
            None => eprintln!("{}: folder {:?} skipped: not a usable directory name", acc.name, n.name()),
        }
    }
    Ok(out)
}

/// Bring one local Maildir in line with the remote folder.
fn sync_folder(s: &mut Sess, cfg: &Config, acc: &Account, name: &str, dir: &Path) -> Result<()> {
    maildir::create(dir)?;
    let mb = s.examine(name)?;
    let uv = mb.uid_validity.ok_or("server reported no UIDVALIDITY")?;

    let local = maildir::scan(dir)?;
    let old_uv = read_uidvalidity(dir);
    let new_epoch = old_uv != Some(uv);
    if new_epoch && !local.is_empty() {
        // A changed UIDVALIDITY invalidates every local UID, but that is never a reason to
        // delete mail: the server may have been migrated or the folder recreated. Refuse to
        // touch the folder and let the operator decide (move the directory aside, then the
        // next run refetches it into a fresh one). The same applies to a directory that
        // holds mail but no state file: its UIDs are of unknown origin.
        let why = match old_uv {
            Some(o) => format!("UIDVALIDITY changed ({o} -> {uv})"),
            None => format!("no .uidvalidity but {} messages present", local.len()),
        };
        eprintln!("{}/{name}: {why} - folder left untouched. Move {} aside to resume syncing it.", acc.name, dir.display());
        return Ok(());
    }
    if new_epoch {
        // first contact with this epoch: record it now, so an interrupted first sync
        // resumes next time instead of looking like a foreign directory
        write_uidvalidity(dir, uv)?;
    }

    let remote = remote_flags(s, mb.exists)?;
    let (removed, orphans) = reconcile_existing(acc, dir, uv, &local, &remote)?;
    let (added, missing) = fetch_new(s, cfg, dir, uv, &local, &remote)?;

    if missing > 0 {
        eprintln!("{}/{name}: {missing} message(s) returned without a body, will retry", acc.name);
    }
    if added + removed > 0 || new_epoch {
        let kept = if orphans > 0 { format!(" ~{orphans} kept") } else { String::new() };
        eprintln!("{}/{name}: +{added} -{removed}{kept} (total {})", acc.name, remote.len());
    }
    Ok(())
}

/// The `.uidvalidity` state file: "<uidvalidity>\n" - which epoch the ,U= numbers in this
/// directory belong to. `None` if absent or unreadable.
fn read_uidvalidity(dir: &Path) -> Option<u32> {
    let state = std::fs::read_to_string(dir.join(".uidvalidity")).unwrap_or_default();
    state.lines().next().and_then(|l| l.trim().parse().ok())
}

fn write_uidvalidity(dir: &Path, uv: u32) -> Result<()> {
    maildir::write_durable(&dir.join(".uidvalidity.tmp"), &dir.join(".uidvalidity"), format!("{uv}\n").as_bytes())
}

/// remote uid -> maildir flags for every message in the selected folder.
fn remote_flags(s: &mut Sess, exists: u32) -> Result<HashMap<u32, String>> {
    let mut remote = HashMap::new();
    if exists > 0 {
        for f in s.uid_fetch("1:*", "(UID FLAGS)")?.iter() {
            if let Some(uid) = f.uid {
                remote.insert(uid, maildir::flags_of(f.flags()));
            }
        }
    }
    Ok(remote)
}

/// Handle messages already on disk: delete the ones that vanished upstream (only under
/// `expunge`), rename on flag changes (only under `sync_flags`). Returns (removed, orphans),
/// where an orphan is a message gone upstream but kept here - that is the point of an archive.
fn reconcile_existing(
    acc: &Account,
    dir: &Path,
    uv: u32,
    local: &HashMap<u32, PathBuf>,
    remote: &HashMap<u32, String>,
) -> Result<(usize, usize)> {
    let (mut removed, mut orphans) = (0, 0);
    for (uid, path) in local {
        match remote.get(uid) {
            // only trust "gone" when the server actually enumerated something; a bare
            // `0 EXISTS` must not empty the archive
            None if acc.expunge && !remote.is_empty() => {
                std::fs::remove_file(path)?;
                removed += 1;
            }
            None => orphans += 1,
            Some(flags) if acc.sync_flags => {
                let want = dir.join(maildir::subdir(flags)).join(maildir::file_name(uv, *uid, flags));
                if *path != want {
                    maildir::rename_durable(path, &want)?;
                }
            }
            Some(_) => {}
        }
    }
    Ok((removed, orphans))
}

/// Fetch messages not yet on disk, in small batches, each written via tmp/ then renamed and
/// handed to the `mail_received` hook. Returns (added, missing), where missing counts uids
/// the server listed but returned without a body; they stay absent locally and are retried
/// on the next pass.
fn fetch_new(
    s: &mut Sess,
    cfg: &Config,
    dir: &Path,
    uv: u32,
    local: &HashMap<u32, PathBuf>,
    remote: &HashMap<u32, String>,
) -> Result<(usize, usize)> {
    let mut new: Vec<u32> = remote.keys().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable();
    let (mut added, mut missing) = (0, 0);
    for chunk in new.chunks(25) {
        let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let mut got = 0;
        for f in s.uid_fetch(set, "(UID FLAGS BODY.PEEK[])")?.iter() {
            let (Some(uid), Some(body)) = (f.uid, f.body()) else { continue };
            let flags = maildir::flags_of(f.flags());
            let dst = dir.join(maildir::subdir(&flags)).join(maildir::file_name(uv, uid, &flags));
            // tmp name without flags, so a retry after a flag change reuses the same file
            maildir::write_durable(&dir.join("tmp").join(format!("{uv}.{uid}")), &dst, body)?;
            cfg.hooks.on_mail_received(&dst);
            got += 1;
        }
        added += got;
        missing += chunk.len().saturating_sub(got);
    }
    Ok((added, missing))
}
