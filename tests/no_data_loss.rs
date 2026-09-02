//! Drives the real binary against a scripted IMAP server to pin the archive guarantees:
//! a message that disappears upstream stays on disk, and a UIDVALIDITY change deletes
//! nothing. The server speaks just enough IMAP for one folder.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One server-side view of INBOX, served for one sync round.
struct Round {
    uidvalidity: u32,
    msgs: Vec<(u32, &'static str)>, // (uid, body)
}

fn serve(stream: TcpStream, rounds: Vec<Round>, done: mpsc::Sender<usize>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    let out = |w: &mut TcpStream, s: &str| {
        w.write_all(s.as_bytes()).unwrap();
        w.write_all(b"\r\n").unwrap();
    };
    out(&mut w, "* OK [CAPABILITY IMAP4rev1 IDLE] ready");

    let mut round = 0usize;
    let mut line = String::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        let cmd = line.trim_end().to_string();
        line.clear();
        let tag = cmd.split_whitespace().next().unwrap_or("*").to_string();
        let upper = cmd.to_uppercase();
        let cur = &rounds[round.min(rounds.len() - 1)];

        if upper.contains("CAPABILITY") {
            out(&mut w, "* CAPABILITY IMAP4rev1 IDLE");
            out(&mut w, &format!("{tag} OK CAPABILITY done"));
        } else if upper.contains("LOGIN") {
            out(&mut w, &format!("{tag} OK LOGIN done"));
        } else if upper.contains("LIST") {
            out(&mut w, "* LIST () \"/\" \"INBOX\"");
            out(&mut w, &format!("{tag} OK LIST done"));
        } else if upper.contains("EXAMINE") {
            out(&mut w, &format!("* {} EXISTS", cur.msgs.len()));
            out(&mut w, "* 0 RECENT");
            out(&mut w, &format!("* OK [UIDVALIDITY {}] uidvalidity", cur.uidvalidity));
            out(&mut w, "* OK [UIDNEXT 9999] uidnext");
            out(&mut w, &format!("{tag} OK [READ-ONLY] EXAMINE done"));
        } else if upper.contains("UID FETCH") {
            let with_body = upper.contains("BODY");
            for (i, (uid, body)) in cur.msgs.iter().enumerate() {
                let seq = i + 1;
                if with_body {
                    // only the explicitly requested uids
                    if !cmd.split_whitespace().nth(3).unwrap_or("").split(',').any(|u| u == uid.to_string()) {
                        continue;
                    }
                    w.write_all(
                        format!("* {seq} FETCH (UID {uid} FLAGS (\\Seen) BODY[] {{{}}}\r\n", body.len())
                            .as_bytes(),
                    )
                    .unwrap();
                    w.write_all(body.as_bytes()).unwrap();
                    out(&mut w, ")");
                } else {
                    out(&mut w, &format!("* {seq} FETCH (UID {uid} FLAGS (\\Seen))"));
                }
            }
            out(&mut w, &format!("{tag} OK FETCH done"));
        } else if upper.contains("IDLE") {
            out(&mut w, "+ idling");
            let _ = done.send(round);
            let mut d = String::new();
            let _ = r.read_line(&mut d); // DONE
            out(&mut w, &format!("{tag} OK IDLE terminated"));
            round += 1;
        } else if upper.contains("LOGOUT") {
            out(&mut w, "* BYE");
            out(&mut w, &format!("{tag} OK LOGOUT done"));
            return;
        } else {
            out(&mut w, &format!("{tag} OK done"));
        }
    }
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = ["cur", "new"]
        .iter()
        .flat_map(|s| std::fs::read_dir(dir.join(s)).into_iter().flatten())
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Round 0 fetches two messages, round 1 loses uid 2 upstream, round 2 changes UIDVALIDITY.
#[test]
fn keeps_mail_that_vanishes_upstream_and_survives_uidvalidity_change() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        serve(
            sock,
            vec![
                Round { uidvalidity: 100, msgs: vec![(1, "Subject: one\r\n\r\nbody one"), (2, "Subject: two\r\n\r\nbody two")] },
                Round { uidvalidity: 100, msgs: vec![(1, "Subject: one\r\n\r\nbody one")] },
                Round { uidvalidity: 200, msgs: vec![(7, "Subject: new\r\n\r\nafter migration")] },
            ],
            tx,
        );
    });

    let root = std::env::temp_dir().join(format!("mailarchive-test-{port}"));
    let _ = std::fs::remove_dir_all(&root);
    let inbox = root.join("INBOX");

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_mailarchive"));
    let child = Command::new(bin)
        .args(["--host", "127.0.0.1", "--port", &port.to_string(), "--user", "u",
               "--maildir", root.to_str().unwrap(), "--tls", "false", "--idle-secs", "1"])
        .env("MAILARCHIVE_PASS", "p")
        .spawn()
        .unwrap();
    let _kill = Kill(child);

    // wait for each round to reach IDLE, i.e. its sync is finished
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    while seen.len() < 3 && Instant::now() < deadline {
        if let Ok(r) = rx.recv_timeout(Duration::from_secs(10)) {
            seen.push(r);
            match r {
                0 => assert_eq!(names(&inbox).len(), 2, "both messages fetched"),
                1 => {
                    let n = names(&inbox);
                    assert_eq!(n.len(), 2, "uid 2 vanished upstream but must stay on disk: {n:?}");
                    assert!(n.iter().any(|f| f.contains(",U=2")), "the archived copy is the one that vanished");
                }
                _ => {
                    let n = names(&inbox);
                    assert_eq!(n.len(), 2, "UIDVALIDITY change must not delete anything: {n:?}");
                    assert!(!n.iter().any(|f| f.contains(",U=7")), "and must not refetch into the old folder");
                    let state = std::fs::read_to_string(inbox.join(".uidvalidity")).unwrap();
                    assert!(state.starts_with("100\n"), "state kept for manual resolution: {state:?}");
                }
            }
        }
    }
    assert_eq!(seen.len(), 3, "server did not reach all three rounds");

    // every stored file has its content, not just its name
    for f in names(&inbox) {
        let p = ["cur", "new"].iter().map(|s| inbox.join(s).join(&f)).find(|p| p.exists()).unwrap();
        assert!(!std::fs::read(&p).unwrap().is_empty(), "{f} is empty");
    }
}
