//! Everything that touches the local Maildir: naming, durable writes, and reading back
//! which UIDs are already stored.

use crate::Result;
use imap::types::Flag;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
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
pub fn write_durable(tmp: &Path, dst: &Path, data: &[u8]) -> Result<()> {
    let mut f = fs::File::create(tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, dst)?;
    fsync_dir(parent(dst))
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
/// Components that would escape `root` (`.`, `..`, absolute parts) are dropped - a folder
/// name comes from the server and must never steer writes out of the Maildir.
pub fn folder_path(root: &Path, name: &str, delim: &str) -> PathBuf {
    name.split(delim)
        .filter(|c| !c.is_empty())
        .fold(root.to_path_buf(), |p, c| match Path::new(c).components().next() {
            Some(Component::Normal(seg)) if Path::new(c).components().count() == 1 => p.join(seg),
            _ => p,
        })
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
    fn folder_paths_stay_below_the_root() {
        let root = Path::new("/m/acct");
        assert_eq!(folder_path(root, "Archive/2024", "/"), Path::new("/m/acct/Archive/2024"));
        assert_eq!(folder_path(root, "Archive.2024", "."), Path::new("/m/acct/Archive/2024"));
        assert_eq!(folder_path(root, "../../etc", "/"), Path::new("/m/acct/etc"));
        assert_eq!(folder_path(root, "/abs/x", "/"), Path::new("/m/acct/abs/x"));
    }

    #[test]
    fn write_durable_publishes_the_name_only_with_content() {
        let dir = std::env::temp_dir().join(format!("mailarchive-md-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        create(&dir).unwrap();
        let dst = dir.join("cur").join(file_name(1, 42, "S"));
        write_durable(&dir.join("tmp").join("x"), &dst, b"body").unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"body");
        assert_eq!(scan(&dir).unwrap().keys().copied().collect::<Vec<_>>(), vec![42]);
        assert!(!dir.join("tmp").join("x").exists(), "tmp file is moved, not copied");
        fs::remove_dir_all(&dir).unwrap();
    }
}
