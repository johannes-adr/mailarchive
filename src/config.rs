//! The JSON config file and how an account's password is obtained.

use crate::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::{env, fs, process::Command};

/// Top level of the config file: where mail goes, how long to idle, and the accounts.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Maildir root ("~" is expanded); every account gets a directory below it.
    pub maildir: String,
    /// Seconds to wait in IDLE on INBOX before a full re-sync of all folders.
    #[serde(default = "default_idle")]
    pub idle_secs: u64,
    pub accounts: Vec<Account>,
}

/// One IMAP account. `name` is also its directory name below the Maildir root.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Password in the file itself - least preferred, keep the file mode 0600.
    pub pass: Option<String>,
    /// Name of an env var holding the password (a `.env` next to the binary is read too).
    pub pass_env: Option<String>,
    /// Command printing the password on stdout; wins over the other two.
    pub pass_cmd: Option<String>,
    /// false = plain TCP without TLS (local/test servers only).
    #[serde(default = "default_true")]
    pub tls: bool,
    /// Only sync these folders; empty = all.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Delete the local copy when a message disappears on the server. Off by default: this
    /// is an archive, so what vanishes upstream (expunge, retention policy, a stray phone
    /// tap) is kept here.
    #[serde(default)]
    pub expunge: bool,
    /// Mirror flag changes (\Seen, \Flagged, ...) by renaming the local file.
    #[serde(default = "default_true")]
    pub sync_flags: bool,
}

fn default_port() -> u16 { 993 }
fn default_idle() -> u64 { 1500 }
fn default_true() -> bool { true }

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let cfg: Config = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Account names become directory names and must be unique, so they are checked before
    /// a single connection is opened rather than surfacing as a confusing path later.
    fn validate(&self) -> Result<()> {
        if self.accounts.is_empty() {
            return Err("config has no accounts".into());
        }
        for (i, a) in self.accounts.iter().enumerate() {
            if a.name.is_empty() || a.name.contains(['/', '\\']) || a.name.starts_with('.') {
                return Err(format!("account name {:?} is not a usable directory name", a.name).into());
            }
            if self.accounts[..i].iter().any(|b| b.name == a.name) {
                return Err(format!("duplicate account name {:?}", a.name).into());
            }
        }
        Ok(())
    }

    /// Maildir root with a leading `~/` expanded.
    pub fn root(&self) -> PathBuf {
        expand_home(&self.maildir)
    }
}

impl Account {
    /// `pass_cmd` > `pass_env` > `pass` > `MAILARCHIVE_PASS_<NAME>`.
    pub fn password(&self) -> Result<String> {
        if let Some(cmd) = &self.pass_cmd {
            let out = Command::new("sh").arg("-c").arg(cmd).output()?;
            if !out.status.success() {
                return Err(format!(
                    "{}: pass_cmd failed: {}",
                    self.name,
                    String::from_utf8_lossy(&out.stderr).trim_end()
                )
                .into());
            }
            return Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string());
        }
        if let Some(var) = &self.pass_env {
            return env::var(var).map_err(|_| format!("{}: env var {var} is not set", self.name).into());
        }
        if let Some(p) = &self.pass {
            return Ok(p.clone());
        }
        let var = self.default_pass_env();
        env::var(&var).map_err(|_| {
            format!("{}: no password - set pass_cmd, pass_env, pass, or {var}", self.name).into()
        })
    }

    /// Env var consulted when the account names no password source at all.
    pub fn default_pass_env(&self) -> String {
        let n: String = self
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect();
        format!("MAILARCHIVE_PASS_{n}")
    }
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = serde_json::from_str(s)?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn defaults_fill_in_and_names_must_be_directory_safe() {
        let c = parse(r#"{"maildir":"/m","accounts":[{"name":"web.de","host":"h","user":"u","pass":"p"}]}"#).unwrap();
        assert_eq!(c.idle_secs, 1500);
        let a = &c.accounts[0];
        assert_eq!((a.port, a.tls, a.expunge, a.sync_flags), (993, true, false, true));

        for bad in ["", "a/b", ".hidden"] {
            let j = format!(r#"{{"maildir":"/m","accounts":[{{"name":"{bad}","host":"h","user":"u"}}]}}"#);
            assert!(parse(&j).is_err(), "{bad:?} must be rejected as a directory name");
        }
        assert!(parse(r#"{"maildir":"/m","accounts":[
            {"name":"a","host":"h","user":"u"},{"name":"a","host":"h","user":"u"}]}"#).is_err());
        assert!(parse(r#"{"maildir":"/m","accounts":[]}"#).is_err());
        // a typo must not be silently ignored
        assert!(parse(r#"{"maildir":"/m","idlesecs":5,"accounts":[{"name":"a","host":"h","user":"u"}]}"#).is_err());
    }

    #[test]
    fn password_sources_have_a_precedence() {
        let mut a: Account = serde_json::from_str(r#"{"name":"web.de","host":"h","user":"u"}"#).unwrap();
        assert_eq!(a.default_pass_env(), "MAILARCHIVE_PASS_WEB_DE");
        assert!(a.password().is_err());

        a.pass = Some("from-file".into());
        assert_eq!(a.password().unwrap(), "from-file");

        a.pass_cmd = Some("printf from-cmd".into());
        assert_eq!(a.password().unwrap(), "from-cmd");

        a.pass_cmd = Some("exit 1".into());
        assert!(a.password().is_err(), "a failing pass_cmd must not fall through to `pass`");
    }
}
