//! mailarchive: tiny incremental, IDLE-driven IMAP -> Maildir archive (like mbsync, pull-only).
//!
//! One JSON config lists any number of IMAP accounts; each gets its own directory below the
//! Maildir root (`<maildir>/<account name>/INBOX/{cur,new,tmp}`) and its own connection,
//! synced and IDLEd in its own thread.
//!
//! It is an archive, not a mirror: a message that disappears server-side stays on disk
//! unless the account sets `"expunge": true`, a changed UIDVALIDITY makes the folder stop
//! rather than delete anything, and every message is fsynced before its name becomes visible.

pub mod backfill;
pub mod config;
pub mod maildir;
pub mod sync;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
