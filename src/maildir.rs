//! Everything that touches the local Maildir: naming, durable writes, and reading back
//! which UIDs are already stored.

use crate::Result;
use imap::types::Flag;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, path::Component};

/// Maildir file name; the `,U=<uid>` part is what mbsync uses too.
pub fn file_name(uv: u32, uid: u32, flags: &str) -> String {
    format!("{uv}.{uid}.mailarchive,U={uid}:2,{flags}")
}

/// Map IMAP system flags to Maildir info letters (sorted, as the spec requires).
pub fn flags_of(flags: &[Flag]) -> String {
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

/// `cur` for read mail, `new` for unread - the Maildir convention.
pub fn subdir(flags: &str) -> &'static str {
    if flags.contains('S') {
        "cur"
    } else {
        "new"
    }
}

pub fn create(dir: &Path) -> Result<()> {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    Ok(())
}

/// uid -> path for all messages in cur/ and new/.
pub fn scan(dir: &Path) -> Result<HashMap<u32, PathBuf>> {
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

/// Write `data` to `tmp` and move it to `dst` so that the name only ever becomes visible
/// once the content is on stable storage: fsync the file before the rename, fsync the
/// containing directory after it. Without this a power cut leaves correctly named but empty
/// files, and since the UID is part of the name they would never be fetched again.
///
/// `mtime` (a unix timestamp) is applied before the fsync, so the published file already
/// carries the message's own date: IMAP servers report a Maildir message's date from the
/// file's mtime, so leaving it at the time of the fetch would show every archived message
/// as having arrived when it was downloaded.
pub fn write_durable(tmp: &Path, dst: &Path, data: &[u8], mtime: Option<i64>) -> Result<()> {
    let mut f = fs::File::create(tmp)?;
    f.write_all(data)?;
    if let Some(secs) = mtime {
        f.set_times(times(secs))?;
    }
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, dst)?;
    fsync_dir(parent(dst))
}

/// Set the mtime of a message already on disk to the unix timestamp `secs`.
pub fn set_mtime(path: &Path, secs: i64) -> Result<()> {
    // write access: `set_times` needs it on some platforms, opening read-only would work
    // only on unix
    fs::File::options().write(true).open(path)?.set_times(times(secs))?;
    Ok(())
}

/// Access and modification time from a unix timestamp; timestamps before 1970 (a mail with
/// a bogus date) work too.
fn times(secs: i64) -> fs::FileTimes {
    let t = match secs {
        0.. => UNIX_EPOCH + Duration::from_secs(secs as u64),
        _ => UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()),
    };
    fs::FileTimes::new().set_accessed(t).set_modified(t)
}

/// mtime of a message as a unix timestamp, `None` if it cannot be read.
pub fn mtime(path: &Path) -> Option<i64> {
    let t = fs::metadata(path).ok()?.modified().ok()?;
    Some(match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// Rename within the Maildir (a flag change), durably.
pub fn rename_durable(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to)?;
    fsync_dir(parent(to))
}

/// fsync a directory so a rename into it survives a crash.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn parent(p: &Path) -> &Path {
    p.parent().filter(|d| d.components().next().is_some()).unwrap_or(Path::new("."))
}

/// Turn an IMAP folder name into a path below `root`, one directory per hierarchy level.
/// A flat namespace (no delimiter) nests on `/`, since that cannot be a directory name
/// anyway. Both name and delimiter come from the server and must never steer writes out
/// of the Maildir or onto another folder's directory, so a name with an empty, `.`, `..`,
/// absolute, or slash-containing component is rejected (`None`) instead of silently
/// folded onto a neighbour.
pub fn folder_path(root: &Path, name: &str, delim: Option<&str>) -> Option<PathBuf> {
    let delim = delim.filter(|d| !d.is_empty()).unwrap_or("/");
    let mut p = root.to_path_buf();
    for c in name.split(delim) {
        let mut comps = Path::new(c).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(seg)), None) if seg != "." && seg != ".." => p.push(seg),
            _ => return None,
        }
    }
    (p != root).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_flags() {
        assert_eq!(file_name(100, 7, "S"), "100.7.mailarchive,U=7:2,S");
        assert_eq!(flags_of(&[Flag::Seen, Flag::Answered]), "RS", "info letters are sorted");
        assert_eq!(flags_of(&[Flag::Recent]), "", "non-maildir flags are dropped");
        assert_eq!((subdir("S"), subdir("")), ("cur", "new"));
    }

    #[test]
    fn folder_paths_stay_below_the_root_or_are_rejected() {
        let root = Path::new("/m/acct");
        let ok = |n, d| folder_path(root, n, d).unwrap();
        assert_eq!(ok("Archive/2024", Some("/")), Path::new("/m/acct/Archive/2024"));
        assert_eq!(ok("Archive.2024", Some(".")), Path::new("/m/acct/Archive/2024"));
        assert_eq!(ok("a/b", None), Path::new("/m/acct/a/b"), "flat namespace nests on '/'");
        assert_eq!(ok("a/b", Some("")), Path::new("/m/acct/a/b"), "empty delimiter = flat");
        // anything that would leave the root, land on the root itself, or need a '/' inside
        // one directory name is refused, never folded onto a neighbour
        for (n, d) in [("../../etc", Some("/")), ("/abs/x", Some("/")), ("..", Some("/")), (".", Some("/")),
                       ("INBOX/..", Some("/")), ("", Some("/")), ("a//b", Some("/")), ("a/b", Some("."))] {
            assert!(folder_path(root, n, d).is_none(), "{n:?} with {d:?}");
        }
    }

    #[test]
    fn write_durable_publishes_the_name_only_with_content() {
        let dir = std::env::temp_dir().join(format!("mailarchive-md-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create(&dir).unwrap();
        let dst = dir.join("cur").join(file_name(1, 42, "S"));
        write_durable(&dir.join("tmp").join("x"), &dst, b"body", Some(1_600_000_000)).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"body");
        assert_eq!(mtime(&dst), Some(1_600_000_000), "the message keeps its own date, not the fetch time");
        set_mtime(&dst, -86_400).unwrap();
        assert_eq!(mtime(&dst), Some(-86_400), "dates before 1970 survive too");
        assert_eq!(scan(&dir).unwrap().keys().copied().collect::<Vec<_>>(), vec![42]);
        assert!(!dir.join("tmp").join("x").exists(), "tmp file is moved, not copied");
        fs::remove_dir_all(&dir).unwrap();
    }
}
