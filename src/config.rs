//! The JSON config file (comments allowed), how an account's password is obtained, and hooks.

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
    /// One-off repair of message dates written by older versions; see `crate::backfill`.
    #[serde(default)]
    pub backfill_dates: bool,
    #[serde(default)]
    pub hooks: Hooks,
}

/// Shell commands run on events. `%%mail_path%%` in a command is replaced by the path of the
/// message, already shell-quoted, so it needs no quotes of its own.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Runs after a message has been stored on disk (during the first sync for every message).
    pub mail_received: Option<String>,
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
    /// Name of an env var holding the password (a `.env` in the working directory is read too).
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
        Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    /// JSON with `//` and `/* */` comments allowed.
    fn parse(text: &str) -> Result<Config> {
        let cfg: Config = serde_json::from_reader(json_comments::StripComments::new(text.as_bytes()))?;
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
            // two accounts on the implicit env fallback must not read the same variable
            if a.pass.is_none() && a.pass_env.is_none() && a.pass_cmd.is_none() {
                let var = a.default_pass_env();
                if self.accounts[..i].iter().any(|b| {
                    b.pass.is_none() && b.pass_env.is_none() && b.pass_cmd.is_none() && b.default_pass_env() == var
                }) {
                    return Err(format!("accounts {:?} and another both fall back to {var}; set pass_env", a.name).into());
                }
            }
        }
        Ok(())
    }

    /// Maildir root with a leading `~/` expanded.
    pub fn root(&self) -> Result<PathBuf> {
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
            let p = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']).to_string();
            if p.is_empty() {
                return Err(format!("{}: pass_cmd printed nothing", self.name).into());
            }
            return Ok(p);
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

impl Hooks {
    /// Run `mail_received` for the message at `path`. A failing hook is logged, never fatal:
    /// the message is already safely on disk.
    pub fn on_mail_received(&self, path: &Path) {
        let Some(cmd) = &self.mail_received else { return };
        let quoted = format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"));
        let cmd = cmd.replace("%%mail_path%%", &quoted);
        match Command::new("sh").arg("-c").arg(&cmd).status() {
            Ok(st) if st.success() => {}
            Ok(st) => eprintln!("hook mail_received: {cmd}: {st}"),
            Err(e) => eprintln!("hook mail_received: {cmd}: {e}"),
        }
    }
}

/// `~/x` -> `$HOME/x`; an unset HOME is an error rather than a silent relative path.
pub fn expand_home(p: &str) -> Result<PathBuf> {
    match p.strip_prefix("~/") {
        Some(rest) => {
            let home = env::var("HOME").map_err(|_| format!("{p}: HOME is not set"))?;
            Ok(PathBuf::from(home).join(rest))
        }
        None => Ok(PathBuf::from(p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn parse(s: &str) -> Result<Config> {
        Config::parse(s)
    }

    #[test]
    fn defaults_fill_in_and_names_must_be_directory_safe() {
        let c = parse(r#"{"maildir":"/m","accounts":[{"name":"web.de","host":"h","user":"u","pass":"p"}]}"#).unwrap();
        assert_eq!(c.idle_secs, 1500);
        assert!(!c.backfill_dates);
        assert!(c.hooks.mail_received.is_none());
        let a = &c.accounts[0];
        assert_eq!((a.port, a.tls, a.expunge, a.sync_flags), (993, true, false, true));

        for bad in ["", "a/b", ".hidden"] {
            let j = format!(r#"{{"maildir":"/m","accounts":[{{"name":"{bad}","host":"h","user":"u"}}]}}"#);
            assert!(parse(&j).is_err(), "{bad:?} must be rejected as a directory name");
        }
        assert!(parse(r#"{"maildir":"/m","accounts":[
            {"name":"a","host":"h","user":"u"},{"name":"a","host":"h","user":"u"}]}"#).is_err());
        assert!(parse(r#"{"maildir":"/m","accounts":[]}"#).is_err());
        // "web.de" and "web-de" would both read MAILARCHIVE_PASS_WEB_DE
        assert!(parse(r#"{"maildir":"/m","accounts":[
            {"name":"web.de","host":"h","user":"u"},{"name":"web-de","host":"h","user":"u"}]}"#).is_err());
        assert!(parse(r#"{"maildir":"/m","accounts":[
            {"name":"web.de","host":"h","user":"u","pass":"p"},{"name":"web-de","host":"h","user":"u"}]}"#).is_ok());
        // a typo must not be silently ignored
        assert!(parse(r#"{"maildir":"/m","idlesecs":5,"accounts":[{"name":"a","host":"h","user":"u"}]}"#).is_err());
    }

    #[test]
    fn comments_are_allowed_and_hooks_run_with_the_quoted_path() {
        let c = parse(r#"{
            // where mail goes
            "maildir": "/m", /* idle_secs left at default */
            "accounts": [{"name": "a", "host": "h", "user": "u", "pass": "p"}],
            "hooks": {"mail_received": "printf %s %%mail_path%% > $OUT"}
        }"#).unwrap();
        assert_eq!(c.hooks.mail_received.as_deref(), Some("printf %s %%mail_path%% > $OUT"));

        let out = env::temp_dir().join(format!("mailarchive-hook-{}", std::process::id()));
        env::set_var("OUT", &out);
        // spaces and a quote in the path survive the shell
        c.hooks.on_mail_received(Path::new("/m/a/it's/cur/1 2"));
        assert_eq!(fs::read_to_string(&out).unwrap(), "/m/a/it's/cur/1 2");
        fs::remove_file(out).unwrap();
        // unknown hook names are typos
        assert!(parse(r#"{"maildir":"/m","accounts":[{"name":"a","host":"h","user":"u"}],"hooks":{"mail_recieved":"x"}}"#).is_err());
    }

    #[test]
    fn password_sources_have_a_precedence() {
        let mut a: Account = serde_json::from_str(r#"{"name":"web.de","host":"h","user":"u"}"#).unwrap();
        assert_eq!(a.default_pass_env(), "MAILARCHIVE_PASS_WEB_DE");
        env::remove_var("MAILARCHIVE_PASS_WEB_DE");
        assert!(a.password().is_err());

        a.pass = Some("from-file".into());
        assert_eq!(a.password().unwrap(), "from-file");

        a.pass_cmd = Some("printf from-cmd".into());
        assert_eq!(a.password().unwrap(), "from-cmd");

        a.pass_cmd = Some("exit 1".into());
        assert!(a.password().is_err(), "a failing pass_cmd must not fall through to `pass`");

        a.pass_cmd = Some("true".into());
        assert!(a.password().is_err(), "a pass_cmd printing nothing is an error, not an empty password");
    }

    #[test]
    fn home_must_be_set_for_tilde() {
        env::set_var("HOME", "/h");
        assert_eq!(expand_home("~/Mail").unwrap(), PathBuf::from("/h/Mail"));
        assert_eq!(expand_home("/abs").unwrap(), PathBuf::from("/abs"));
        env::remove_var("HOME");
        assert!(expand_home("~/Mail").is_err());
        env::set_var("HOME", "/h");
    }
}
