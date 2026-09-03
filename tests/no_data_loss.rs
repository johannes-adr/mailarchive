//! Drives the real binary against scripted IMAP servers to pin the archive guarantees.
//! The server speaks just enough IMAP: LOGIN, LIST, EXAMINE, UID FETCH, IDLE. Every
//! scenario is a list of folders, each with a per-round view (UIDVALIDITY + messages);
//! a round ends when the client reaches IDLE.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Folder {
    name: &'static str,
    /// LIST attributes, e.g. `\NonExistent`
    attrs: &'static str,
    /// listed from this round on (folders created server-side later)
    since_round: usize,
    /// `Some("NO ...")` makes every EXAMINE of this folder fail
    examine_reply: Option<&'static str>,
    /// in round 0, a body fetch that names one of these uids gets a tagged NO
    body_no_uids: Vec<u32>,
    /// (uidvalidity, (uid, body)...) per round; the last one repeats
    rounds: Vec<(u32, Vec<(u32, &'static str)>)>,
}

fn folder(name: &'static str, rounds: Vec<(u32, Vec<(u32, &'static str)>)>) -> Folder {
    Folder { name, attrs: "", since_round: 0, examine_reply: None, body_no_uids: vec![], rounds }
}

struct Server {
    login_reply: &'static str,
    folders: Vec<Folder>,
}

#[derive(Default)]
struct Log {
    cmds: Mutex<Vec<String>>,
    conns: Mutex<usize>,
}

fn serve(stream: TcpStream, srv: Arc<Server>, done: mpsc::Sender<usize>, log: Arc<Log>, round: Arc<Mutex<usize>>) {
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    // the client may be killed mid-write; that must not panic the server thread
    let out = |w: &mut TcpStream, s: &str| {
        let _ = w.write_all(s.as_bytes());
        let _ = w.write_all(b"\r\n");
    };
    out(&mut w, "* OK [CAPABILITY IMAP4rev1 IDLE] ready");
    let mut selected: Option<Folder> = None;
    let mut line = String::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        let cmd = line.trim_end().to_string();
        line.clear();
        log.cmds.lock().unwrap().push(cmd.clone());
        let tag = cmd.split_whitespace().next().unwrap_or("*").to_string();
        let upper = cmd.to_uppercase();
        let rd = *round.lock().unwrap();
        if upper.contains("CAPABILITY") {
            out(&mut w, "* CAPABILITY IMAP4rev1 IDLE");
            out(&mut w, &format!("{tag} OK CAPABILITY done"));
        } else if upper.contains("LOGIN") {
            out(&mut w, &format!("{tag} {}", srv.login_reply));
        } else if upper.contains("LIST") {
            for f in srv.folders.iter().filter(|f| f.since_round <= rd) {
                out(&mut w, &format!("* LIST ({}) \"/\" \"{}\"", f.attrs, f.name));
            }
            out(&mut w, &format!("{tag} OK LIST done"));
        } else if upper.contains("EXAMINE") {
            let name = cmd.split_whitespace().nth(2).unwrap_or("").trim_matches('"');
            match srv.folders.iter().find(|f| f.name == name && f.since_round <= rd) {
                Some(f) if f.examine_reply.is_some() => out(&mut w, &format!("{tag} {}", f.examine_reply.unwrap())),
                Some(f) => {
                    let (uv, msgs) = &f.rounds[rd.min(f.rounds.len() - 1)];
                    out(&mut w, &format!("* {} EXISTS", msgs.len()));
                    out(&mut w, "* 0 RECENT");
                    out(&mut w, &format!("* OK [UIDVALIDITY {uv}] uidvalidity"));
                    out(&mut w, "* OK [UIDNEXT 9999] uidnext");
                    out(&mut w, &format!("{tag} OK [READ-ONLY] EXAMINE done"));
                    selected = Some(f.clone());
                }
                None => out(&mut w, &format!("{tag} NO [NONEXISTENT] no such mailbox")),
            }
        } else if upper.contains("UID FETCH") {
            let Some(f) = selected.clone() else { continue };
            let (_, msgs) = &f.rounds[rd.min(f.rounds.len() - 1)];
            let with_body = upper.contains("BODY");
            let set = cmd.split_whitespace().nth(3).unwrap_or("").to_string();
            let wants = |uid: &u32| set == "1:*" || set.split(',').any(|x| x == uid.to_string());
            if with_body && rd == 0 && f.body_no_uids.iter().any(wants) {
                out(&mut w, &format!("{tag} NO [SERVERBUG] fetch failed"));
                continue;
            }
            for (i, (uid, body)) in msgs.iter().enumerate().filter(|(_, (u, _))| wants(u)) {
                let seq = i + 1;
                if with_body {
                    let _ = w.write_all(
                        format!("* {seq} FETCH (UID {uid} FLAGS (\\Seen) BODY[] {{{}}}\r\n", body.len()).as_bytes(),
                    );
                    let _ = w.write_all(body.as_bytes());
                    out(&mut w, ")");
                } else {
                    out(&mut w, &format!("* {seq} FETCH (UID {uid} FLAGS (\\Seen))"));
                }
            }
            out(&mut w, &format!("{tag} OK FETCH done"));
        } else if upper.contains("IDLE") {
            out(&mut w, "+ idling");
            let _ = done.send(rd);
            let mut d = String::new();
            let _ = r.read_line(&mut d); // DONE
            out(&mut w, &format!("{tag} OK IDLE terminated"));
            *round.lock().unwrap() += 1;
        } else if upper.contains("LOGOUT") {
            out(&mut w, "* BYE");
            out(&mut w, &format!("{tag} OK LOGOUT done"));
            return;
        } else {
            out(&mut w, &format!("{tag} OK done"));
        }
    }
}

/// Bind a scripted server on a free port. The receiver yields a round number whenever that
/// round's sync is finished (the client has reached IDLE); reconnects are served too.
fn spawn_server(srv: Server) -> (u16, mpsc::Receiver<usize>, Arc<Log>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    let (srv, log, round) = (Arc::new(srv), Arc::new(Log::default()), Arc::new(Mutex::new(0usize)));
    let log2 = log.clone();
    std::thread::spawn(move || loop {
        let (sock, _) = listener.accept().unwrap();
        *log2.conns.lock().unwrap() += 1;
        let (s, t, l, r) = (srv.clone(), tx.clone(), log2.clone(), round.clone());
        std::thread::spawn(move || serve(sock, s, t, l, r));
    });
    (port, rx, log)
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A fresh Maildir root with a config for one account (`extra` = more JSON fields) and the
/// binary running against it; stderr goes to `<root>/stderr.log`.
struct Run {
    root: PathBuf,
    errlog: PathBuf,
    _kill: Kill,
}

fn prepare_root(tag: &str, port: u16) -> PathBuf {
    let root = std::env::temp_dir().join(format!("mailarchive-test-{tag}-{port}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn start_in(root: PathBuf, accounts: &str) -> Run {
    let cfg_path = root.join("config.json");
    std::fs::write(
        &cfg_path,
        format!(r#"{{"maildir": {root:?}, "idle_secs": 1, "accounts": [{accounts}]}}"#, root = root.to_str().unwrap()),
    )
    .unwrap();
    let errlog = root.join("stderr.log");
    let child = Command::new(env!("CARGO_BIN_EXE_mailarchive"))
        .args(["--input-config", cfg_path.to_str().unwrap()])
        .stderr(Stdio::from(std::fs::File::create(&errlog).unwrap()))
        .spawn()
        .unwrap();
    Run { root, errlog, _kill: Kill(child) }
}

fn start(tag: &str, port: u16, extra: &str) -> Run {
    let root = prepare_root(tag, port);
    start_in(root, &account("acc", port, extra))
}

fn account(name: &str, port: u16, extra: &str) -> String {
    format!(r#"{{"name": {name:?}, "host": "127.0.0.1", "port": {port}, "user": "u", "pass": "p", "tls": false {extra}}}"#)
}

impl Run {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.errlog).unwrap_or_default()
    }
    fn dir(&self, acc: &str, folder: &str) -> PathBuf {
        self.root.join(acc).join(folder)
    }
}

fn wait_rounds(rx: &mpsc::Receiver<usize>, n: usize) -> Vec<usize> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = vec![];
    while seen.len() < n && Instant::now() < deadline {
        if let Ok(r) = rx.recv_timeout(Duration::from_secs(10)) {
            seen.push(r);
        }
    }
    seen
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

fn one_inbox(rounds: Vec<(u32, Vec<(u32, &'static str)>)>) -> Server {
    Server { login_reply: "OK LOGIN done", folders: vec![folder("INBOX", rounds)] }
}

/// Round 0 fetches two messages, round 1 loses uid 2 upstream, round 2 changes UIDVALIDITY.
/// A second account runs alongside to pin the per-account directory layout.
#[test]
fn keeps_mail_that_vanishes_upstream_and_survives_uidvalidity_change() {
    let (port, rx, _) = spawn_server(one_inbox(vec![
        (100, vec![(1, "Subject: one\r\n\r\nbody one"), (2, "Subject: two\r\n\r\nbody two")]),
        (100, vec![(1, "Subject: one\r\n\r\nbody one")]),
        (200, vec![(7, "Subject: new\r\n\r\nafter migration")]),
    ]));
    let (port2, rx2, _) = spawn_server(one_inbox(vec![(5, vec![(3, "Subject: elsewhere\r\n\r\nsecond account")])]));
    let root = prepare_root("layout", port);
    let run = start_in(root, &format!("{}, {}", account("web.de", port, ""), account("other", port2, "")));
    let inbox = run.dir("web.de", "INBOX");

    for r in wait_rounds(&rx, 3) {
        let n = names(&inbox);
        match r {
            0 => assert_eq!(n.len(), 2, "both messages fetched"),
            1 => {
                assert_eq!(n.len(), 2, "uid 2 vanished upstream but must stay on disk: {n:?}");
                assert!(n.iter().any(|f| f.contains(",U=2")));
            }
            _ => {
                assert_eq!(n.len(), 2, "UIDVALIDITY change must not delete anything: {n:?}");
                assert!(!n.iter().any(|f| f.contains(",U=7")), "and must not refetch into the old folder");
                assert_eq!(std::fs::read_to_string(inbox.join(".uidvalidity")).unwrap(), "100\n", "state kept");
            }
        }
    }
    assert!(run.stderr().contains("UIDVALIDITY changed (100 -> 200)"), "{}", run.stderr());
    for f in names(&inbox) {
        let p = ["cur", "new"].iter().map(|s| inbox.join(s).join(&f)).find(|p| p.exists()).unwrap();
        assert!(!std::fs::read(&p).unwrap().is_empty(), "{f} is empty");
    }

    rx2.recv_timeout(Duration::from_secs(10)).expect("second account never synced");
    let other = names(&run.dir("other", "INBOX"));
    assert_eq!(other.len(), 1, "second account has its own mail: {other:?}");
    assert!(other[0].contains(",U=3"));
}

/// A first sync that fails half-way (the second batch of 26 gets NO) resumes on the next
/// pass instead of leaving a folder that looks foreign forever.
#[test]
fn interrupted_first_sync_resumes() {
    static BODIES: [&str; 26] = ["b"; 26];
    let msgs: Vec<(u32, &'static str)> = (1..=26).map(|u| (u, BODIES[(u - 1) as usize])).collect();
    let mut srv = one_inbox(vec![(100, msgs.clone()), (100, msgs)]);
    srv.folders[0].body_no_uids = vec![26];
    let (port, rx, log) = spawn_server(srv);
    let run = start("resume", port, "");
    assert_eq!(wait_rounds(&rx, 2), vec![0, 1]);
    let err = run.stderr();
    assert!(err.contains("skipped: No Response: [SERVERBUG]"), "batch failure is this folder's problem only: {err}");
    assert!(!err.contains("left untouched"), "no wedge after an interrupted first sync: {err}");
    assert_eq!(names(&run.dir("acc", "INBOX")).len(), 26, "round 1 fetched the missing uid 26");
    assert_eq!(*log.conns.lock().unwrap(), 1, "no reconnect needed");
}

/// A directory holding mail but no state file is of unknown origin (an mbsync Maildir's
/// `,U=` numbers are not IMAP UIDs): refuse it, do not adopt or delete it.
#[test]
fn foreign_maildir_is_left_alone() {
    let (port, rx, _) = spawn_server(one_inbox(vec![(100, vec![(1, "x"), (2, "y")])]));
    let root = prepare_root("foreign", port);
    let inbox = root.join("acc").join("INBOX");
    std::fs::create_dir_all(inbox.join("cur")).unwrap();
    std::fs::write(inbox.join("cur").join("1693000000.1_1.host,U=1:2,S"), "mbsync file").unwrap();
    let run = start_in(root, &account("acc", port, ""));
    assert_eq!(wait_rounds(&rx, 1), vec![0]);
    assert!(run.stderr().contains("no .uidvalidity but 1 messages present"), "{}", run.stderr());
    assert_eq!(names(&inbox), vec!["1693000000.1_1.host,U=1:2,S"]);
    assert!(!inbox.join(".uidvalidity").exists());
}

/// A folder that refuses EXAMINE is skipped; the ones after it still sync, on the same
/// connection, and a folder created server-side later is picked up on the next full pass.
#[test]
fn bad_folder_is_skipped_and_new_folders_are_found() {
    let ok = |n| folder(n, vec![(1, vec![(1, "m")])]);
    let mut ghost = folder("Ghost", vec![]);
    ghost.attrs = "\\NonExistent";
    ghost.examine_reply = Some("NO [NOPERM] no read access");
    let mut later = ok("Later");
    later.since_round = 1;
    let mut dotdot = ok("..");
    dotdot.rounds = vec![(1, vec![(1, "escape")])];
    let (port, rx, log) = spawn_server(Server {
        login_reply: "OK LOGIN done",
        folders: vec![ok("Aaa"), ghost, dotdot, ok("Zzz"), later, ok("INBOX")],
    });
    let run = start("skip", port, "");
    assert_eq!(wait_rounds(&rx, 2), vec![0, 1]);
    let err = run.stderr();
    for f in ["Aaa", "Zzz", "INBOX", "Later"] {
        assert_eq!(names(&run.dir("acc", f)).len(), 1, "{f}: {err}");
    }
    assert!(err.contains("acc/Ghost: skipped: No Response: [NOPERM]"), "{err}");
    assert!(err.contains("folder \"..\" skipped"), "{err}");
    assert!(!run.root.join("cur").exists() && !run.root.join("acc").join("cur").exists(), "nothing written to a root");
    assert!(!err.contains("reconnecting"), "{err}");
    assert_eq!(*log.conns.lock().unwrap(), 1);
}

/// `* 0 EXISTS` with `expunge: true` keeps everything: "gone" is only trusted once the
/// server actually enumerated something.
#[test]
fn zero_exists_does_not_empty_the_archive_under_expunge() {
    let (port, rx, log) = spawn_server(one_inbox(vec![(100, vec![(1, "one"), (2, "two")]), (100, vec![])]));
    let run = start("zero", port, r#", "expunge": true"#);
    assert_eq!(wait_rounds(&rx, 2), vec![0, 1]);
    assert_eq!(names(&run.dir("acc", "INBOX")).len(), 2, "{}", run.stderr());
    let fetches = log.cmds.lock().unwrap().iter().filter(|c| c.contains("UID FETCH")).count();
    assert_eq!(fetches, 2, "no FETCH on an empty mailbox, and no delete without one");
}

/// With `expunge: true` and a populated server view, a vanished message IS deleted - the
/// opt-in mirror mode still works.
#[test]
fn expunge_mirrors_a_real_deletion() {
    let (port, rx, _) = spawn_server(one_inbox(vec![(100, vec![(1, "one"), (2, "two")]), (100, vec![(1, "one")])]));
    let run = start("expunge", port, r#", "expunge": true"#);
    assert_eq!(wait_rounds(&rx, 2), vec![0, 1]);
    let n = names(&run.dir("acc", "INBOX"));
    assert_eq!(n.len(), 1, "{n:?}");
    assert!(n[0].contains(",U=1"));
}

/// A rejected LOGIN is reported and retried later; the growing delay itself is a unit test
/// in main.rs (waiting 90 s here would be silly).
#[test]
fn login_failure_is_reported_not_hammered() {
    let mut srv = one_inbox(vec![(1, vec![])]);
    srv.login_reply = "NO [AUTHENTICATIONFAILED] bad password";
    let (port, _rx, log) = spawn_server(srv);
    let run = start("backoff", port, "");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !run.stderr().contains("reconnecting") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(run.stderr().contains("AUTHENTICATIONFAILED"), "{}", run.stderr());
    assert!(run.stderr().contains("reconnecting in 30s"), "{}", run.stderr());
    assert_eq!(*log.conns.lock().unwrap(), 1, "no second LOGIN within the delay");
}
