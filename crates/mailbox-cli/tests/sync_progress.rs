//! What a sync pass says about itself while it runs (§7).
//!
//! A pass over several accounts and their folders is the longest thing the
//! interfaces do, and for most of it there is nothing new to draw. A spinner
//! alone cannot be told from a hang, so the pass reports the boundaries it
//! already has and the interfaces say where it has got to.
//!
//! The property under test is that the report arrives *during* the pass and
//! names real folders — a callback that only fired at the end would satisfy
//! every type in the chain and none of the point.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use mailbox_core::imap::sync::{Progress, SyncOptions};
use mailbox_core::imap::Credential;
use mailbox_core::session::{Session, SessionOptions};
use mailbox_core::{Clock, Tracer};
use mailbox_l3::{ImapServer, Message};

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn a_pass_reports_the_account_and_folder_it_is_working_on() {
    let (_server, config, state) = world();
    mailbox_core::credentials::install("test", Credential::Password("testpass".into()));
    let mut session = Session::open(
        &SessionOptions {
            config_path: Some(config),
            state_dir: Some(state),
            account: Some("test".into()),
            ..Default::default()
        },
        Tracer::disabled(),
        Clock::fixed(0),
    )
    .expect("open the session");

    let mut seen: Vec<Progress> = Vec::new();
    let result = session
        .sync_watched(&SyncOptions::default(), &mut |at| seen.push(at))
        .expect("the pass ran");
    assert!(result.synced[0].messages_added > 0, "nothing was synced");

    // The account is named before the connection is made: on a first pass
    // that is the slowest step and the one with least to show for itself.
    assert_eq!(
        seen.first()
            .map(|at| (at.account.as_str(), at.folder.is_none())),
        Some(("test", true)),
        "the pass did not name its account before connecting: {seen:?}"
    );

    // And every folder it examined, by the name the server gave it.
    let folders: Vec<&str> = seen.iter().filter_map(|at| at.folder.as_deref()).collect();
    assert!(
        folders.iter().any(|f| f.eq_ignore_ascii_case("INBOX")),
        "the inbox was never reported: {folders:?}"
    );
    assert!(
        seen.iter().all(|at| at.account == "test"),
        "a stage named the wrong account: {seen:?}"
    );

    // A report per folder examined, plus the one for the account itself.
    assert_eq!(
        folders.len(),
        result.synced[0].folders_examined as usize,
        "reported {} folders, examined {}: {folders:?}",
        folders.len(),
        result.synced[0].folders_examined
    );
}

/// The plain `sync` is the same pass with nobody listening, and stays that way.
#[test]
fn a_pass_nobody_is_watching_still_runs() {
    let (_server, config, state) = world();
    mailbox_core::credentials::install("test", Credential::Password("testpass".into()));
    let mut session = Session::open(
        &SessionOptions {
            config_path: Some(config),
            state_dir: Some(state),
            account: Some("test".into()),
            ..Default::default()
        },
        Tracer::disabled(),
        Clock::fixed(0),
    )
    .expect("open the session");

    let result = session.sync(&SyncOptions::default()).expect("the pass ran");
    assert!(result.synced[0].messages_added > 0);
}

fn world() -> (ImapServer, PathBuf, PathBuf) {
    let messages: Vec<Message> = (1..=5)
        .map(|uid| {
            Message::simple(
                uid,
                &format!("Message {uid}"),
                &format!("m{uid}@example.com"),
            )
        })
        .collect();
    let server = ImapServer::start(messages).expect("start the L3 server");
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("mailbox-syncprog-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[accounts.test]\naddress = \"peter@example.com\"\n\
             imap_host = \"127.0.0.1\"\nimap_port = {}\n\
             imap_security = \"none\"\nrequire_tls = false\n\
             \n[notify]\nenabled = false\n",
            server.port()
        ),
    )
    .expect("write config");

    (server, config, dir.join("state"))
}
