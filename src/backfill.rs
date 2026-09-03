//! One-off repair for archives written before the mtime of a message was set from the
//! server's INTERNALDATE: those files carry the time they were downloaded, which is what an
//! IMAP server (Dovecot and friends) reports as the message's date, so every mail looks like
//! it arrived during the first sync.
//!
//! Enabled with `"backfill_dates": true` in the config and meant to be removed again once it
//! has run: every folder it has repaired gets a `.dates_backfilled` marker, so a config left
//! on this setting costs nothing beyond that first pass. Deleting the markers makes it run
//! again. This whole module can be dropped once no archive needs it any more.

use crate::maildir;
use crate::sync::Sess;
use crate::Result;
use std::path::Path;

const MARKER: &str = ".dates_backfilled";

/// Set every local message's mtime to the INTERNALDATE the server reports for its UID. The
/// folder must already be selected. Messages that are only here (gone upstream) keep the
/// mtime they have - the server has no date for them any more.
pub fn run(s: &mut Sess, acc: &str, name: &str, dir: &Path) -> Result<()> {
    let marker = dir.join(MARKER);
    if marker.exists() {
        return Ok(());
    }
    let local = maildir::scan(dir)?;
    let mut fixed = 0;
    if !local.is_empty() {
        for f in s.uid_fetch("1:*", "(UID INTERNALDATE)")?.iter() {
            let (Some(uid), Some(date)) = (f.uid, f.internal_date()) else { continue };
            let Some(path) = local.get(&uid) else { continue };
            let secs = date.timestamp();
            if maildir::mtime(path) != Some(secs) {
                maildir::set_mtime(path, secs)?;
                fixed += 1;
            }
        }
    }
    // only now: an interrupted pass must run again rather than leave half the folder wrong
    maildir::write_durable(&dir.join("tmp").join(MARKER), &marker, b"", None)?;
    if fixed > 0 {
        eprintln!("{acc}/{name}: {fixed} message date(s) restored from the server");
    }
    Ok(())
}
