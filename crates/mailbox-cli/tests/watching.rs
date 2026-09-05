//! The background sync loop (§5.3), against a server that can be told when to
//! speak.
//!
//! What makes this testable in milliseconds rather than minutes is that every
//! wait is an argument: the idle window, the poll interval, and the number of
//! passes all come from the caller, and the L3 server decides when to announce
//! new mail. Nothing here sleeps hoping something happened.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use mailbox_l3::{ImapFault, ImapServer, Message};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
    server: ImapServer,
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl World {
    fn new(messages: Vec<Message>) -> World {
        let server = ImapServer::start(messages).expect("start the L3 IMAP server");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-watch-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = {}\n\
                 imap_security = \"none\"\n\
                 require_tls = false\n\
                 \n[notify]\n\
                 enabled = false\n",
                server.port()
            ),
        )
        .expect("write config");

        World {
            state: dir.join("state"),
            config,
            dir,
            server,
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.command(args).output().map(to_run).expect("run")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "watch")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1");
        cmd
    }

    /// Runs `sync --watch` for a bounded number of passes.
    fn watch(&self, passes: usize, extra: &[&str]) -> Run {
        let passes = passes.to_string();
        let mut args = vec![
            "--json",
            "--timeout",
            "5000",
            "sync",
            "--watch",
            "--max-passes",
            &passes,
        ];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn subjects(&self) -> Vec<String> {
        let run = self.run(&["debug", "store-dump"]);
        assert_eq!(run.status, 0, "store unreadable:\n{}", run.stderr);
        let dump: serde_json::Value = serde_json::from_str(&run.stdout).expect("dump");
        let mut subjects: Vec<String> = dump["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["subject"].as_str().unwrap_or_default().to_string())
            .collect();
        subjects.sort();
        subjects
    }
}

fn to_run(out: std::process::Output) -> Run {
    Run {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The documents `--json` streams, one per pass.
fn passes(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l} ({e})")))
        .collect()
}

// ---- the loop ----

/// The point of the whole feature: mail that arrives while nobody asked for it
/// still arrives.
#[test]
fn mail_that_arrives_during_the_wait_is_synced_without_being_asked() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);

    // The server announces a second message a moment into the IDLE.
    world.server.set_messages(vec![
        Message::simple(1, "First", "one@example.com"),
        Message::simple(2, "Second", "two@example.com"),
    ]);
    world
        .server
        .wake_idle_after(Some(Duration::from_millis(80)));

    let run = world.watch(2, &["--idle-window", "2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    assert_eq!(
        world.subjects(),
        vec!["First".to_string(), "Second".to_string()],
        "the second pass did not happen, or found nothing"
    );

    let documents = passes(&run.stdout);
    assert_eq!(documents.len(), 2, "{:#?}", documents);
    assert_eq!(documents[0]["payload"]["wake"], "started");
    // The second pass happened because the *server* spoke, not because a timer
    // went off. That distinction is the whole difference between IDLE and a
    // poll.
    assert_eq!(
        documents[1]["payload"]["wake"], "server",
        "{:#?}",
        documents
    );
}

/// A server with nothing to say still ends the wait, because the client's own
/// window ends it. RFC 2177 requires re-issuing at least every 29 minutes and
/// the network is less patient than that.
#[test]
fn a_silent_server_does_not_stop_the_loop() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.set_fault(ImapFault::SilentIdle);

    let started = Instant::now();
    let run = world.watch(2, &["--idle-window", "2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let documents = passes(&run.stdout);
    assert_eq!(documents.len(), 2, "{:#?}", documents);
    assert_eq!(
        documents[1]["payload"]["wake"], "elapsed",
        "the wait should have ended on our window: {documents:#?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the loop waited far longer than its window"
    );
}

/// A connection reaped mid-wait is the commonest thing that happens to an idle
/// socket, and it must cost a reconnect rather than the loop.
#[test]
fn a_connection_dropped_during_the_wait_is_survived() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.set_fault(ImapFault::DropDuringIdle);

    let run = world.watch(2, &["--idle-window", "2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let documents = passes(&run.stdout);
    assert_eq!(documents.len(), 2, "{:#?}", documents);
    // The second pass still ran and still succeeded.
    assert!(
        documents[1]["payload"]["result"].is_object(),
        "{documents:#?}"
    );
    assert_eq!(world.subjects(), vec!["First".to_string()]);
}

/// §4.1: exactly one process per account may be the sync engine. A loop that
/// holds a connection for hours makes this matter in a way `sync --once` did
/// not.
#[test]
fn a_second_sync_engine_is_refused_while_one_is_running() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.set_fault(ImapFault::SilentIdle);

    // One long-running watcher, started and left holding the lock.
    let mut first = world
        .command(&["--json", "sync", "--watch", "--idle-window", "2"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start the first watcher");

    // Reading its first pass is also the wait for it to have taken the lock.
    {
        use std::io::{BufRead, BufReader};
        let mut line = String::new();
        BufReader::new(first.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("the watcher should announce its first pass");
        assert!(line.contains("sync"), "{line}");
    }

    let second = world.watch(1, &[]);
    let _ = first.kill();
    let _ = first.wait();

    assert_ne!(second.status, 0, "a second sync engine was allowed");
    assert!(
        second.stderr.contains("already syncing"),
        "the refusal should say why: {}",
        second.stderr
    );
}

/// And the lock is released when the owner goes away, however it goes away.
#[test]
fn the_lock_is_released_when_the_owner_dies() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.set_fault(ImapFault::SilentIdle);

    let mut first = world
        .command(&["--json", "sync", "--watch", "--idle-window", "2"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start the first watcher");
    {
        use std::io::{BufRead, BufReader};
        let mut line = String::new();
        BufReader::new(first.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("first pass");
    }

    // Killed, not asked to stop: a pid file would still be sitting there.
    let _ = first.kill();
    let _ = first.wait();

    let second = world.watch(1, &[]);
    assert_eq!(
        second.status, 0,
        "the lock outlived its owner:\n{}",
        second.stderr
    );
}

/// A server that does not offer `IDLE` is not a server this cannot watch.
#[test]
fn a_server_without_idle_is_polled_instead() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.advertise_idle(false);

    // The second pass happens at all, on a timer rather than on a word from
    // the server.
    let started = Instant::now();
    let run = world.watch(2, &["--poll", "1", "--idle-window", "2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let documents = passes(&run.stdout);
    assert_eq!(documents.len(), 2, "{documents:#?}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the poll interval was not honoured"
    );
}

/// `--watch` is a stream, so its output is one document per pass rather than
/// one at the end that never comes.
#[test]
fn the_json_output_is_a_document_per_pass() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world
        .server
        .wake_idle_after(Some(Duration::from_millis(50)));

    let run = world.watch(3, &["--idle-window", "2"]);
    let documents = passes(&run.stdout);

    assert_eq!(documents.len(), 3);
    for document in &documents {
        assert_eq!(document["schema"], "mailbox/v1", "{document:#}");
        assert_eq!(document["command"], "sync", "{document:#}");
        assert!(
            document["payload"]["result"]["synced"].is_array(),
            "{document:#}"
        );
    }
}

/// `--watch` and `--until` are opposite instructions: one runs forever, the
/// other stops at a named event.
#[test]
fn watch_refuses_the_flags_that_contradict_it() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    let run = world.run(&["sync", "--watch", "--until", "sync.begin", "--trace"]);
    assert_ne!(run.status, 0);
    assert!(
        run.stderr.contains("cannot be used with") || run.stderr.contains("conflict"),
        "{}",
        run.stderr
    );
}

/// The connection is usable after a wait. A client that leaves the session in
/// the idling state reads its next command's response as `IDLE`'s completion,
/// and the two stop agreeing about what answers what — which costs nothing
/// while every wait gets a fresh connection, and everything the moment one is
/// reused.
#[test]
fn the_session_still_works_after_idling() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world
        .server
        .wake_idle_after(Some(Duration::from_millis(50)));

    // Three passes: each wait is followed by a full sync on a connection that
    // has to be in a sane state for the pass to succeed.
    let run = world.watch(3, &["--idle-window", "2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let documents = passes(&run.stdout);
    assert_eq!(documents.len(), 3, "{documents:#?}");
    for document in &documents {
        assert!(
            document["payload"]["result"].is_object(),
            "a pass after an idle failed: {document:#}"
        );
    }
}

/// The lock covers a single pass too, not just the loop. §4.1 says one sync
/// engine per account, and a `sync --once` racing a watcher interleaves into
/// the same incoherent store.
#[test]
fn a_single_pass_is_refused_while_a_watcher_holds_the_account() {
    let world = World::new(vec![Message::simple(1, "First", "one@example.com")]);
    world.server.set_fault(ImapFault::SilentIdle);

    let mut watcher = world
        .command(&["--json", "sync", "--watch", "--idle-window", "20"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("start the watcher");
    {
        use std::io::{BufRead, BufReader};
        let mut line = String::new();
        BufReader::new(watcher.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("first pass");
    }

    let once = world.run(&["sync", "--once"]);
    let _ = watcher.kill();
    let _ = watcher.wait();

    assert_ne!(once.status, 0, "a second sync engine was allowed");
    // And it says the thing that matters: nothing the user did is lost.
    assert!(
        once.stderr.contains("queued") && once.stderr.contains("already syncing"),
        "{}",
        once.stderr
    );
}
