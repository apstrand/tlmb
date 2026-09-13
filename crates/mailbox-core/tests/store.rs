//! L0: the store — ingestion, search, cursors, and the dump the convergence
//! oracle compares (§5.1, §6.7, §14.3).
//!
//! No server is involved: fixtures seed the store directly (§14.1), which is
//! what makes these run in microseconds and in parallel.

use std::collections::BTreeSet;

use mailbox_core::clock::Clock;
use mailbox_core::fixture::Fixture;
use mailbox_core::query::parse;
use mailbox_core::store::ingest::Incoming;
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::store::{FolderRecord, Store};
use mailbox_core::Tracer;

const FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[folder]]
path = "Archive"
special_use = "archive"

[[message]]
name = "root"
folder = "INBOX"
from = "Alice <alice@example.com>"
to = ["peter@example.com"]
subject = "Invoice 42"
date = "2026-08-01T10:00:00Z"
message_id = "<inv42@example.com>"
body = "Please pay the invoice."
[[message.attachment]]
filename = "invoice.pdf"
mime_type = "application/pdf"
text = "%PDF-1.4"

[[message]]
name = "reply"
folder = "INBOX"
from = "Peter <peter@example.com>"
to = ["alice@example.com"]
subject = "Re: Invoice 42"
date = "2026-08-01T12:00:00Z"
message_id = "<reply@example.com>"
in_reply_to = "<inv42@example.com>"
flags = ["\\Seen"]
body = "Paid, thanks."

[[message]]
name = "digest"
folder = "Archive"
from = "News <news@example.org>"
subject = "Weekly digest"
date = "2026-07-20T08:00:00Z"
flags = ["\\Seen", "\\Flagged"]
body = "Nothing to see."

[[message]]
name = "big"
folder = "Archive"
from = "Backup <backup@example.org>"
subject = "Large report"
date = "2026-07-01T08:00:00Z"
body = "See attached."
[[message.attachment]]
filename = "report.CSV"
mime_type = "text/csv"
generate_bytes = 40000
"#;

struct Fx {
    _dir: tempdir::TempDir,
    store: Store,
    ids: std::collections::BTreeMap<String, String>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempdir::TempDir::new();
        let mut store = Store::open(dir.path(), "test", Tracer::disabled(), Clock::fixed(0))
            .expect("open store");
        let fixture = Fixture::parse(FIXTURE).expect("parse fixture");
        let ids = fixture.seed_store(&mut store).expect("seed store");
        Fx {
            _dir: dir,
            store,
            ids,
        }
    }

    fn id(&self, name: &str) -> &str {
        self.ids
            .get(name)
            .unwrap_or_else(|| panic!("no fixture message named {name}"))
    }

    /// Runs a query and returns the matching fixture names, in result order.
    fn names(&self, query: &str) -> Vec<String> {
        self.names_with(
            query,
            ListOptions {
                sort: Sort::DateDesc,
                ..Default::default()
            },
        )
    }

    fn names_with(&self, query: &str, opts: ListOptions) -> Vec<String> {
        let node =
            parse(query, &Clock::fixed(1_788_264_000)).unwrap_or_else(|e| panic!("`{query}`: {e}"));
        let listing = self.store.search(&node, &opts).expect("search");
        listing
            .messages
            .iter()
            .map(|m| {
                self.ids
                    .iter()
                    .find(|(_, id)| **id == m.id)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_else(|| m.id.clone())
            })
            .collect()
    }
}

/// A temporary directory that cleans up after itself. Hand-rolled because the
/// alternative is a dev-dependency for eleven lines.
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed).expect("entropy");
            let name = format!("mailbox-test-{}", u64::from_le_bytes(seed));
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn fixture_seeds_every_message() {
    let fx = Fx::new();
    assert_eq!(fx.store.message_count().unwrap(), 4);
    assert_eq!(fx.store.folder_count().unwrap(), 2);
    // Two seen, two not.
    assert_eq!(fx.store.unread_count().unwrap(), 2);
}

#[test]
fn serialized_in_memory_store_round_trips() {
    let mut store = Store::open_in_memory("browser", Tracer::disabled(), Clock::fixed(0)).unwrap();
    store
        .upsert_folder(&FolderRecord {
            path: "INBOX".into(),
            name: "Inbox".into(),
            selectable: true,
            special_use: Some("inbox".into()),
            ..Default::default()
        })
        .unwrap();

    let database = store.serialize().unwrap();
    assert!(database.starts_with(b"SQLite format 3\0"));

    let restored = Store::open_serialized(
        Some(&database),
        "browser",
        Tracer::disabled(),
        Clock::fixed(0),
    )
    .unwrap();
    let folders = restored.folders().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].path, "INBOX");
    assert_eq!(folders[0].special_use.as_deref(), Some("inbox"));
}

#[test]
fn folder_counts_are_derived_not_incremented() {
    let fx = Fx::new();
    let folders = fx.store.folders().unwrap();
    let inbox = folders.iter().find(|f| f.path == "INBOX").unwrap();
    assert_eq!(inbox.total, 2);
    assert_eq!(inbox.unread, 1);
    assert_eq!(inbox.special_use.as_deref(), Some("inbox"));
}

#[test]
fn structured_search() {
    let fx = Fx::new();
    assert_eq!(fx.names("from:alice"), vec!["root"]);
    assert_eq!(fx.names("is:unread"), vec!["root", "big"]);
    assert_eq!(fx.names("is:read"), vec!["reply", "digest"]);
    assert_eq!(fx.names("in:inbox"), vec!["reply", "root"]);
    assert_eq!(fx.names("in:archive"), vec!["digest", "big"]);
    assert_eq!(fx.names("is:flagged"), vec!["digest"]);
    assert_eq!(fx.names("has:attachment"), vec!["root", "big"]);
    assert_eq!(fx.names("subject:invoice"), vec!["reply", "root"]);
}

#[test]
fn full_text_search_covers_headers_and_body() {
    let fx = Fx::new();
    assert_eq!(fx.names("\"Nothing to see\""), vec!["digest"]);
    // A bare word finds a sender, because `from:` is structured but `alice`
    // should still find her mail (§6.2).
    assert!(fx.names("alice").contains(&"root".to_string()));
}

#[test]
fn boolean_combinations() {
    let fx = Fx::new();
    assert_eq!(fx.names("in:inbox is:unread"), vec!["root"]);
    assert_eq!(fx.names("in:inbox OR in:archive").len(), 4);
    assert_eq!(fx.names("in:archive NOT is:flagged"), vec!["big"]);
    assert_eq!(fx.names("-is:seen"), fx.names("is:unread"));
}

/// `NOT` must exclude rows for which the inner predicate is unknown, not leave
/// them out of both sides.
#[test]
fn negation_is_total() {
    let fx = Fx::new();
    let all = fx.names("").len();
    let matching = fx.names("has:attachment").len();
    let not_matching = fx.names("NOT has:attachment").len();
    assert_eq!(matching + not_matching, all);
}

#[test]
fn date_and_size_terms() {
    let fx = Fx::new();
    assert_eq!(fx.names("before:2026-07-25"), vec!["digest", "big"]);
    assert_eq!(fx.names("before:2026-07-10"), vec!["big"]);
    assert_eq!(fx.names("after:2026-08-01"), vec!["reply", "root"]);
    // The 40 KB generated attachment, base64-encoded, is comfortably over 32 KB.
    assert_eq!(fx.names("larger:32k"), vec!["big"]);
    assert!(!fx.names("smaller:2k").contains(&"big".to_string()));
}

#[test]
fn filename_glob_is_case_insensitive() {
    let fx = Fx::new();
    // The fixture spells it `report.CSV`.
    assert_eq!(fx.names("filename:*.csv"), vec!["big"]);
    assert_eq!(fx.names("filename:*.CSV"), vec!["big"]);
    assert_eq!(fx.names("filename:invoice.pdf"), vec!["root"]);
}

#[test]
fn to_me_resolves_identities() {
    let fx = Fx::new();
    let opts = ListOptions {
        identities: vec!["peter@example.com".into()],
        sort: Sort::DateDesc,
        ..Default::default()
    };
    assert_eq!(fx.names_with("to:me", opts), vec!["root"]);
}

#[test]
fn account_term_is_decided_at_compile_time() {
    let fx = Fx::new();
    assert_eq!(fx.names("account:test").len(), 4);
    assert_eq!(fx.names("account:other").len(), 0);
}

#[test]
fn unread_first_is_the_default_order() {
    let fx = Fx::new();
    let names = fx.names_with(
        "",
        ListOptions {
            sort: Sort::UnreadFirst,
            ..Default::default()
        },
    );
    // Unread first (newest of those first), then read (newest first).
    assert_eq!(names, vec!["root", "big", "reply", "digest"]);
}

#[test]
fn date_order_both_directions() {
    let fx = Fx::new();
    let desc = fx.names_with(
        "",
        ListOptions {
            sort: Sort::DateDesc,
            ..Default::default()
        },
    );
    let mut asc = fx.names_with(
        "",
        ListOptions {
            sort: Sort::DateAsc,
            ..Default::default()
        },
    );
    asc.reverse();
    assert_eq!(desc, asc);
}

/// Cursors, not offsets (§6.4): paging through the whole set must visit every
/// row exactly once, in every sort order.
#[test]
fn cursor_pagination_is_total_and_ordered() {
    let fx = Fx::new();
    for sort in [Sort::DateDesc, Sort::DateAsc, Sort::UnreadFirst] {
        let expected = fx.names_with(
            "",
            ListOptions {
                sort,
                limit: 100,
                ..Default::default()
            },
        );

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let opts = ListOptions {
                sort,
                limit: 1,
                cursor: cursor.clone(),
                ..Default::default()
            };
            let node = parse("", &Clock::fixed(0)).unwrap();
            let page = fx.store.search(&node, &opts).unwrap();
            for m in &page.messages {
                seen.push(
                    fx.ids
                        .iter()
                        .find(|(_, id)| **id == m.id)
                        .map(|(n, _)| n.clone())
                        .unwrap_or_default(),
                );
            }
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(
            seen, expected,
            "paging with {sort:?} lost or reordered rows"
        );
    }
}

#[test]
fn a_forged_cursor_is_a_usage_error() {
    let fx = Fx::new();
    let opts = ListOptions {
        cursor: Some("not-a-cursor".into()),
        ..Default::default()
    };
    let node = parse("", &Clock::fixed(0)).unwrap();
    assert!(fx.store.search(&node, &opts).is_err());
}

#[test]
fn id_prefixes_resolve() {
    let fx = Fx::new();
    let full = fx.id("root").to_string();
    assert_eq!(fx.store.resolve_id(&full[..7]).unwrap(), full);
    assert_eq!(fx.store.resolve_id(&full).unwrap(), full);

    // A hex prefix that matches nothing is `not_found`; something that is not
    // an id at all is a usage error. §6.6 gives those different exit codes, and
    // an agent distinguishes "wrong id" from "wrong argument" by that alone.
    assert_eq!(
        fx.store.resolve_id("ffffff0").unwrap_err().code(),
        mailbox_api::ErrorCode::NotFound
    );
    assert_eq!(
        fx.store.resolve_id("not-hex").unwrap_err().code(),
        mailbox_api::ErrorCode::Usage
    );
}

/// An ambiguous prefix must never resolve to "the first match".
///
/// The caller most hurt by a silent pick is an agent acting on the result, and
/// it is exactly the one that cannot notice. Seventeen messages guarantee by
/// pigeonhole that two ids share a first hex digit, so this is deterministic
/// rather than probabilistic.
#[test]
fn an_ambiguous_prefix_is_an_error() {
    let dir = tempdir::TempDir::new();
    let mut store = Store::open(dir.path(), "test", Tracer::disabled(), Clock::fixed(0)).unwrap();
    let folder = store
        .upsert_folder(&FolderRecord {
            path: "INBOX".into(),
            name: "INBOX".into(),
            delimiter: Some("/".into()),
            special_use: Some("inbox".into()),
            selectable: true,
            subscribed: true,
        })
        .unwrap();

    let mut ids = Vec::new();
    for i in 0..17u32 {
        let raw = format!("From: a@b.c\r\nSubject: m{i}\r\nMessage-ID: <{i}@x>\r\n\r\nbody\r\n")
            .into_bytes();
        ids.push(
            store
                .apply_message(&Incoming {
                    folder_id: folder,
                    uid: i + 1,
                    flags: vec![],
                    internaldate: 1_785_578_400 + i64::from(i),
                    size: raw.len() as u64,
                    modseq: None,
                    headers: raw.clone(),
                    raw: Some(raw),
                })
                .unwrap()
                .id,
        );
    }

    let mut by_first: std::collections::BTreeMap<char, usize> = Default::default();
    for id in &ids {
        *by_first.entry(id.chars().next().unwrap()).or_default() += 1;
    }
    let (shared, count) = by_first
        .iter()
        .find(|(_, n)| **n > 1)
        .expect("seventeen ids over sixteen hex digits must collide");

    let err = store.resolve_id(&shared.to_string()).unwrap_err();
    assert_eq!(err.code(), mailbox_api::ErrorCode::Ambiguous);
    assert!(
        err.to_string().contains(&count.to_string()),
        "the error should say how many: {err}"
    );
}

#[test]
fn threads_are_assembled_from_the_store() {
    let fx = Fx::new();
    let summary = fx.store.summary(fx.id("reply")).unwrap();
    let thread = fx.store.thread(&summary.thread_id).unwrap();
    assert_eq!(thread.message_count, 2);
    assert_eq!(thread.unread_count, 1);
    assert_eq!(
        thread.subject, "Invoice 42",
        "a thread is named for its root"
    );
    assert!(thread
        .participants
        .iter()
        .any(|p| p.address == "alice@example.com"));
}

#[test]
fn bodies_round_trip_through_the_blob_store() {
    let fx = Fx::new();
    let raw = fx.store.raw_message(fx.id("root")).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("Please pay the invoice."));

    let detail = fx
        .store
        .message(fx.id("root"), &Default::default())
        .unwrap();
    assert!(detail
        .body
        .unwrap()
        .content
        .contains("Please pay the invoice."));
    assert_eq!(
        detail
            .attachments
            .iter()
            .filter(|a| a.is_attachment)
            .count(),
        1
    );
}

/// The dedup property of §5.1: the same message in two folders is one row and
/// one blob.
#[test]
fn a_message_in_two_folders_is_stored_once() {
    let fx = Fx::new();
    let mut store = fx.store;

    let archive_id = store.folder_id("Archive").unwrap().unwrap();
    let raw = store.raw_message(fx.ids.get("root").unwrap()).unwrap();
    let before = store.message_count().unwrap();

    store
        .apply_message(&Incoming {
            folder_id: archive_id,
            uid: 9001,
            flags: vec!["\\Seen".into()],
            internaldate: 1_785_578_400,
            size: raw.len() as u64,
            modseq: None,
            headers: raw.clone(),
            raw: Some(raw),
        })
        .unwrap();

    assert_eq!(
        store.message_count().unwrap(),
        before,
        "the same message must not duplicate"
    );
    // And it is now findable in both folders.
    let node = parse("in:archive", &Clock::fixed(0)).unwrap();
    let hits = store
        .search(
            &node,
            &ListOptions {
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(hits
        .messages
        .iter()
        .any(|m| m.id == *fx.ids.get("root").unwrap()));
}

/// The rank ordering of §5.3: a message in the inbox and a catch-all is
/// reported as being in the inbox.
#[test]
fn the_primary_placement_prefers_the_inbox_over_a_catch_all() {
    let dir = tempdir::TempDir::new();
    let mut store = Store::open(dir.path(), "test", Tracer::disabled(), Clock::fixed(0)).unwrap();

    let all_mail = store
        .upsert_folder(&FolderRecord {
            path: "[Gmail]/All Mail".into(),
            name: "All Mail".into(),
            delimiter: Some("/".into()),
            special_use: Some("all".into()),
            selectable: true,
            subscribed: true,
        })
        .unwrap();
    let inbox = store
        .upsert_folder(&FolderRecord {
            path: "INBOX".into(),
            name: "INBOX".into(),
            delimiter: Some("/".into()),
            special_use: Some("inbox".into()),
            selectable: true,
            subscribed: true,
        })
        .unwrap();

    let raw = b"From: a@b.c\r\nSubject: Hi\r\nMessage-ID: <x@y>\r\n\r\nbody\r\n".to_vec();
    let mut incoming = Incoming {
        folder_id: all_mail,
        uid: 1,
        flags: vec![],
        internaldate: 1_785_578_400,
        size: raw.len() as u64,
        modseq: None,
        headers: raw.clone(),
        raw: Some(raw.clone()),
    };
    // All Mail first, so the wrong answer would stick if ranking were "first
    // one wins".
    store.apply_message(&incoming).unwrap();
    incoming.folder_id = inbox;
    incoming.uid = 1;
    let applied = store.apply_message(&incoming).unwrap();

    let summary = store.summary(&applied.id).unwrap();
    assert_eq!(summary.folder, "INBOX");
}

#[test]
fn removing_the_last_placement_removes_the_message() {
    let fx = Fx::new();
    let mut store = fx.store;
    let inbox = store.folder_id("INBOX").unwrap().unwrap();
    let before = store.message_count().unwrap();

    let uids = store.placement_uids(inbox).unwrap();
    let uid = *uids.iter().next().unwrap();
    assert!(store.remove_placement(inbox, uid).unwrap());
    assert_eq!(store.message_count().unwrap(), before - 1);
}

#[test]
fn retain_uids_expunges_what_the_server_no_longer_lists() {
    let fx = Fx::new();
    let mut store = fx.store;
    let inbox = store.folder_id("INBOX").unwrap().unwrap();
    let removed = store.retain_uids(inbox, &BTreeSet::from([1u32])).unwrap();
    assert_eq!(removed, 1);
    assert_eq!(store.placement_uids(inbox).unwrap(), BTreeSet::from([1]));
}

#[test]
fn clearing_a_folder_is_the_uidvalidity_reset_path() {
    let fx = Fx::new();
    let mut store = fx.store;
    let inbox = store.folder_id("INBOX").unwrap().unwrap();
    store.clear_folder_placements(inbox).unwrap();
    assert!(store.placement_uids(inbox).unwrap().is_empty());
    // The Archive messages survive; only the folder's own placements went.
    assert_eq!(store.message_count().unwrap(), 2);
}

/// The property the convergence oracle depends on: a rebuild scoped to the
/// messages a sync pass touched produces the same threads as a full rebuild
/// (§14.3).
#[test]
fn scoped_and_full_rebuilds_agree() {
    let fx = Fx::new();
    let mut store = fx.store;
    let node = parse("", &Clock::fixed(0)).unwrap();

    let scoped = store.store_dump(&node).unwrap();
    store.rebuild_threads(None).unwrap();
    let full = store.store_dump(&node).unwrap();

    assert_eq!(scoped["threads"], full["threads"]);
    assert_eq!(scoped["messages"], full["messages"]);
}

/// The dump is a golden-file oracle, so it must be byte-stable across runs of
/// the same store.
#[test]
fn store_dump_is_stable() {
    let fx = Fx::new();
    let node = parse("", &Clock::fixed(0)).unwrap();
    let a = serde_json::to_string(&fx.store.store_dump(&node).unwrap()).unwrap();
    let b = serde_json::to_string(&fx.store.store_dump(&node).unwrap()).unwrap();
    assert_eq!(a, b);
}

/// Two stores seeded from the same manifest must produce identical dumps —
/// this is the convergence oracle in its simplest form (§14.3).
#[test]
fn identical_input_produces_identical_stores() {
    let a = Fx::new();
    let b = Fx::new();
    let node = parse("", &Clock::fixed(0)).unwrap();
    assert_eq!(
        a.store.store_dump(&node).unwrap(),
        b.store.store_dump(&node).unwrap()
    );
}

#[test]
fn contacts_are_derived_from_traffic() {
    let fx = Fx::new();
    let hits = fx.store.contacts("alice", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].address, "alice@example.com");
    assert_eq!(hits[0].name.as_deref(), Some("Alice"));
}

#[test]
fn read_only_opens_refuse_writes() {
    let dir = tempdir::TempDir::new();
    {
        let mut store =
            Store::open(dir.path(), "test", Tracer::disabled(), Clock::fixed(0)).unwrap();
        Fixture::parse(FIXTURE)
            .unwrap()
            .seed_store(&mut store)
            .unwrap();
    }
    let mut store =
        Store::open_read_only(dir.path(), "test", Tracer::disabled(), Clock::fixed(0)).unwrap();
    assert_eq!(store.message_count().unwrap(), 4);
    assert!(
        store.rebuild_threads(None).is_err(),
        "a read-only store must refuse to write"
    );
}

// ---- §6.4: the change feed ----

/// Seeding the store wrote arrivals, and a cursor walks them in order.
#[test]
fn the_feed_is_ordered_and_cursored() {
    let fx = Fx::new();
    let all = fx.store.changes_since(0, 100).expect("read the feed");
    assert!(!all.changes.is_empty(), "seeding should have said so");
    assert!(
        all.changes.windows(2).all(|w| w[0].seq < w[1].seq),
        "the feed is a log, and a log is in order"
    );
    assert_eq!(all.cursor, all.changes.last().unwrap().seq);
    assert!(!all.truncated);

    // A cursor mid-log returns the tail, and nothing before it.
    let mid = all.changes[0].seq;
    let rest = fx.store.changes_since(mid, 100).expect("resume");
    assert_eq!(rest.changes.len(), all.changes.len() - 1);
    assert!(rest.changes.iter().all(|c| c.seq > mid));
}

/// A watcher away longer than the log is told so, rather than resumed silently
/// short. Being handed a partial mailbox with no way to notice is the failure
/// this field exists to prevent.
#[test]
fn a_cursor_the_feed_has_outlived_reports_truncated() {
    let mut fx = Fx::new();
    let all = fx.store.changes_since(0, 100).expect("read the feed");
    let first = all.changes[0].seq;

    // Simulate the trim: drop everything up to and including the event this
    // cursor is sitting on, plus the one after it.
    fx.store
        .transaction("trim-for-test", |tx| {
            tx.execute("DELETE FROM changes WHERE seq <= ?1", [first + 1])?;
            Ok(())
        })
        .expect("trim");

    let page = fx.store.changes_since(first, 100).expect("resume");
    assert!(
        page.truncated,
        "the event after this cursor is gone, and the caller must hear about it"
    );

    // And a cursor that is still covered is not reported as truncated.
    let live = page.changes.first().map(|c| c.seq).expect("something left");
    assert!(!fx.store.changes_since(live, 100).expect("resume").truncated);
}

/// Reading does not consume: two watchers at the same cursor see the same
/// events, which is what makes the feed a log rather than a queue.
#[test]
fn reading_the_feed_leaves_it_alone() {
    let fx = Fx::new();
    let once = fx.store.changes_since(0, 100).expect("read");
    let twice = fx.store.changes_since(0, 100).expect("read again");
    assert_eq!(once.changes.len(), twice.changes.len());
    assert_eq!(once.cursor, twice.cursor);
}

/// A local write is marked local; one the server told us about is not. An
/// agent that flags what it reads would otherwise wake itself up.
#[test]
fn the_feed_separates_this_devices_writes_from_the_servers() {
    let mut fx = Fx::new();
    let id = fx.id("root").to_string();
    let before = fx.store.latest_change().expect("cursor");

    fx.store
        .set_flags(&id, &["seen".to_string()], &[])
        .expect("flag");

    let page = fx.store.changes_since(before, 100).expect("read");
    let event = page.changes.first().expect("an event for the write");
    assert_eq!(event.event, "flag");
    assert!(event.local, "this device set that flag");

    // Whereas the arrivals that seeded the store came from the server.
    let arrivals = fx.store.changes_since(0, 100).expect("read");
    assert!(arrivals
        .changes
        .iter()
        .filter(|c| c.event == "new")
        .all(|c| !c.local));
}

/// The property the TUI's "did anything change" check rests on: a write made
/// by *another* connection is visible to a reader that asks again.
///
/// Obvious for one connection, and not obvious across two — which is the case
/// that matters, because the web interface and the TUI are separate processes
/// and each holds its own handle to the same file.
#[test]
fn a_second_connection_sees_the_first_ones_changes() {
    let dir = tempdir::TempDir::new();
    let mut writer = Store::open(dir.path(), "test", Tracer::disabled(), Clock::fixed(0))
        .expect("open for writing");
    let fixture = Fixture::parse(FIXTURE).expect("parse");
    let ids = fixture.seed_store(&mut writer).expect("seed");

    let reader = Store::open_read_only(dir.path(), "test", Tracer::disabled(), Clock::fixed(0))
        .expect("open for reading");
    let before = reader.latest_change().expect("read the feed");

    let id = ids.get("root").expect("the fixture message").clone();
    writer
        .set_flags(&id, &["flagged".to_string()], &[])
        .expect("flag it");

    let after = reader.latest_change().expect("read the feed again");
    assert!(
        after > before,
        "a reader that asked again should see the write: {before} -> {after}"
    );

    let page = reader.changes_since(before, 10).expect("read");
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].event, "flag");
    assert_eq!(page.changes[0].message.as_deref(), Some(id.as_str()));
}
