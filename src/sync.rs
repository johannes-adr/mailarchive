//! One account's connection lifetime: full sync, then IDLE on INBOX, and the per-folder
//! sync that keeps the local Maildir in line with the server.

use crate::config::Account;
use crate::maildir;
use crate::Result;
use imap::{extensions::idle::WaitOutcome, Connection, ConnectionMode, Session};
use imap_proto::NameAttribute;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Sess = Session<Connection>;

/// Connect, sync every folder, then loop: IDLE on INBOX -> sync. Returns only on error,
/// which the caller turns into a reconnect.
pub fn run(acc: &Account, pass: &str, root: &Path, idle_secs: u64) -> Result<()> {
    let mode = if acc.tls { ConnectionMode::AutoTls } else { ConnectionMode::Plaintext };
    let client = imap::ClientBuilder::new(&acc.host, acc.port).mode(mode).connect()?;
    let mut s = client.login(&acc.user, pass).map_err(|(e, _)| e)?;
    eprintln!("{}: connected to {}", acc.name, acc.host);

    // (imap name, local path) for every selectable folder, optionally filtered by config
    let folders: Vec<(String, PathBuf)> = s
        .list(None, Some("*"))?
        .iter()
        .filter(|n| !n.attributes().contains(&NameAttribute::NoSelect))
        .filter(|n| acc.folders.is_empty() || acc.folders.iter().any(|x| x == n.name()))
        .map(|n| (n.name().to_string(), maildir::folder_path(root, n.name(), n.delimiter().unwrap_or("/"))))
        .collect();

    let mut idle_inbox = true;
    loop {
        for (name, dir) in &folders {
            // after an INBOX event only INBOX is synced; on timeout everything is
            if idle_inbox || name == "INBOX" {
                sync_folder(&mut s, acc, name, dir)?;
            }
        }
        s.examine("INBOX")?;
        let mut h = s.idle();
        h.timeout(Duration::from_secs(idle_secs)).keepalive(false);
        idle_inbox = h.wait_while(imap::extensions::idle::stop_on_any)? == WaitOutcome::TimedOut;
    }
}

/// Bring one local Maildir in line with the remote folder.
fn sync_folder(s: &mut Sess, acc: &Account, name: &str, dir: &Path) -> Result<()> {
    maildir::create(dir)?;
    let mb = s.examine(name)?;
    let uv = mb.uid_validity.unwrap_or(0);

    // state: "<uidvalidity>\n<last uid>"
    let state_file = dir.join(".uidvalidity");
    let state = std::fs::read_to_string(&state_file).unwrap_or_default();
    let mut st = state.lines().map(|l| l.trim().parse::<u32>().unwrap_or(0));
    let (old_uv, mut last_uid) = (st.next().unwrap_or(0), st.next().unwrap_or(0));

    let local = maildir::scan(dir)?;
    // A changed UIDVALIDITY invalidates every local UID, but that is never a reason to
    // delete mail: the server may have been migrated or the folder recreated. Refuse to
    // touch the folder and let the operator decide (move the directory aside, then the
    // next run refetches it into a fresh one).
    if old_uv != uv && !local.is_empty() {
        eprintln!(
            "{}/{name}: UIDVALIDITY changed ({old_uv} -> {uv}) - folder left untouched. \
             Move {} aside (or fix .uidvalidity) to resume syncing it.",
            acc.name,
            dir.display()
        );
        return Ok(());
    }

    // remote uid -> maildir flags
    let mut remote: HashMap<u32, String> = HashMap::new();
    if mb.exists > 0 {
        for f in s.uid_fetch("1:*", "(UID FLAGS)")?.iter() {
            if let Some(uid) = f.uid {
                remote.insert(uid, maildir::flags_of(f.flags()));
            }
        }
    }

    // vanished messages and flag changes
    let (mut removed, mut orphans) = (0, 0);
    for (uid, path) in &local {
        match remote.get(uid) {
            None if acc.expunge => {
                std::fs::remove_file(path)?;
                removed += 1;
            }
            // gone upstream but kept here - that is the point of an archive
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

    // new messages, fetched in small batches, written via tmp/ then renamed
    let mut new: Vec<u32> = remote.keys().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable();
    let mut added = 0;
    for chunk in new.chunks(25) {
        let set = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        for f in s.uid_fetch(set, "(UID FLAGS BODY.PEEK[])")?.iter() {
            let (Some(uid), Some(body)) = (f.uid, f.body()) else { continue };
            let flags = maildir::flags_of(f.flags());
            let fname = maildir::file_name(uv, uid, &flags);
            let dst = dir.join(maildir::subdir(&flags)).join(&fname);
            maildir::write_durable(&dir.join("tmp").join(&fname), &dst, body)?;
            last_uid = last_uid.max(uid);
            added += 1;
        }
    }
    if let Some(m) = remote.keys().max() {
        last_uid = last_uid.max(*m);
    }
    // only now that every message body is on disk may the state advance
    let tmp = dir.join(".uidvalidity.tmp");
    maildir::write_durable(&tmp, &state_file, format!("{uv}\n{last_uid}\n").as_bytes())?;
    if added + removed > 0 || old_uv != uv {
        let kept = if orphans > 0 { format!(" ~{orphans} kept") } else { String::new() };
        eprintln!("{}/{name}: +{added} -{removed}{kept} (total {})", acc.name, remote.len());
    }
    Ok(())
}
