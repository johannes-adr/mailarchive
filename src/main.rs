//! CLI entry point: read the JSON config, then run one thread per account.

use clap::Parser;
use mailarchive::config::{expand_home, Config};
use mailarchive::sync;
use std::path::PathBuf;
use std::time::Duration;
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
    // before the config is read, so `pass_env` can resolve against it
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.input_config).unwrap_or_else(|e| fatal(&e.to_string()));
    let root = cli.maildir.as_deref().map(expand_home).unwrap_or_else(|| cfg.root());

    let accounts: Vec<_> = cfg
        .accounts
        .iter()
        .filter(|a| cli.account.is_empty() || cli.account.iter().any(|n| n == &a.name))
        .collect();
    if accounts.is_empty() {
        fatal(&format!("no account matches {:?}", cli.account));
    }
    // fail before connecting anything if a password source is broken
    let passwords: Vec<String> = accounts
        .iter()
        .map(|a| a.password().unwrap_or_else(|e| fatal(&e.to_string())))
        .collect();

    // one connection, one IDLE loop, one thread per account
    thread::scope(|scope| {
        for (acc, pass) in accounts.iter().zip(&passwords) {
            let dir = root.join(&acc.name);
            scope.spawn(move || loop {
                if let Err(e) = sync::run(acc, pass, &dir, cfg.idle_secs) {
                    eprintln!("{}: error: {e}; reconnecting in 30s", acc.name);
                    thread::sleep(Duration::from_secs(30));
                }
            });
        }
    });
}

fn fatal(msg: &str) -> ! {
    eprintln!("mailarchive: {msg}");
    std::process::exit(1)
}
