//! CLI entry point: read the JSON config, then run one thread per account.

use clap::Parser;
use mailarchive::config::{expand_home, Config};
use mailarchive::sync;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::thread;

/// Incremental, IDLE-driven IMAP -> Maildir archive for any number of accounts.
#[derive(Parser)]
#[command(version, about, after_help = "\
Each account in the config becomes <maildir>/<name>/, with one directory per IMAP folder
below it. Passwords come from `pass_cmd`, `pass_env`, `pass`, or MAILARCHIVE_PASS_<NAME>;
a `.env` in the working directory is loaded before the config is read.")]
struct Cli {
    /// JSON config file listing the accounts
    #[arg(short = 'c', long, env = "MAILARCHIVE_CONFIG", default_value = "config.json")]
    input_config: PathBuf,

    /// Override the Maildir root from the config
    #[arg(long, env = "MAILARCHIVE_MAILDIR")]
    maildir: Option<String>,

    /// Only run these accounts (comma separated; default: all)
    #[arg(long, value_delimiter = ',')]
    account: Vec<String>,
}

fn main() {
    // before the config is read, so `pass_env` can resolve against it; only the working
    // directory's .env, never a parent's
    let _ = dotenvy::from_path(".env");
    let cli = Cli::parse();
    let cfg = Config::load(&cli.input_config).unwrap_or_else(|e| fatal(&e.to_string()));
    let root = match &cli.maildir {
        Some(m) => expand_home(m),
        None => cfg.root(),
    }
    .unwrap_or_else(|e| fatal(&e.to_string()));

    for n in &cli.account {
        if !cfg.accounts.iter().any(|a| &a.name == n) {
            fatal(&format!("no account named {n:?} in {}", cli.input_config.display()));
        }
    }

    let accounts: Vec<_> = cfg
        .accounts
        .iter()
        .filter(|a| cli.account.is_empty() || cli.account.iter().any(|n| n == &a.name))
        .collect();
    // fail before connecting anything if a password source is broken
    let passwords: Vec<String> = accounts
        .iter()
        .map(|a| a.password().unwrap_or_else(|e| fatal(&e.to_string())))
        .collect();

    // one connection, one IDLE loop, one thread per account
    thread::scope(|scope| {
        for (acc, pass) in accounts.iter().zip(&passwords) {
            let dir = root.join(&acc.name);
            scope.spawn(move || {
                let mut prev = None;
                loop {
                    let started = Instant::now();
                    if let Err(e) = sync::run(acc, pass, &dir, cfg.idle_secs) {
                        let delay = next_delay(prev, started.elapsed());
                        eprintln!("{}: error: {e}; reconnecting in {}s", acc.name, delay.as_secs());
                        thread::sleep(delay);
                        prev = Some(delay);
                    }
                }
            });
        }
    });
}

const MIN_DELAY: Duration = Duration::from_secs(30);
const MAX_DELAY: Duration = Duration::from_secs(15 * 60);

/// Reconnect delay after a failed connection. `prev` is the delay used last time (None on
/// the first failure). A connection that lived a while was working, so start over at the
/// minimum; one that died at once (bad password, refused) doubles the previous delay, so a
/// wrong credential does not hammer the provider into a lockout.
fn next_delay(prev: Option<Duration>, lived: Duration) -> Duration {
    match prev {
        Some(p) if lived <= Duration::from_secs(120) => (p * 2).min(MAX_DELAY),
        _ => MIN_DELAY,
    }
}

fn fatal(msg: &str) -> ! {
    eprintln!("mailarchive: {msg}");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backs_off_only_for_immediate_failures() {
        let s = Duration::from_secs;
        let mut prev = None;
        let mut seen = vec![];
        for _ in 0..8 {
            let d = next_delay(prev, s(1));
            seen.push(d.as_secs());
            prev = Some(d);
        }
        assert_eq!(seen, [30, 60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(next_delay(Some(s(900)), s(3600)), MIN_DELAY, "a long-lived connection resets the backoff");
    }
}
