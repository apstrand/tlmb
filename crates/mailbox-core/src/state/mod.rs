//! Cross-device state that IMAP cannot carry (§5.4) — M8.
//!
//! > Tags, saved searches, per-account UI preferences, snooze times, and the
//! > address book need to follow the user without a third-party server. They
//! > are stored as append-only, encrypted records in a dedicated IMAP folder
//! > (default `Mailbox/.state`, hidden from the UI).
//!
//! IMAP carries flags, folders, and messages. It has nowhere to put a tag, a
//! saved search, or the time you asked a message to come back — so those have
//! lived on one device each, and a second device silently disagreed. This is
//! the substrate that fixes it, without adding a server: the user's own
//! mailbox is the transport, and a folder of small encrypted messages is the
//! log.
//!
//! ## Why a log and not a document
//!
//! Two devices that both edit a state *document* have to agree on who wrote
//! last, and IMAP gives them no way to find out — there is no compare-and-swap
//! on an `APPEND`, and two devices that were both offline will both append.
//! So nothing here overwrites anything. A device only ever **appends an
//! assertion**: this message has this tag, this search is called this, this
//! snooze is cleared. The current state is a fold over every assertion anyone
//! has made, and that fold is a pure function of the *set* of records.
//!
//! That is what makes it converge. Records arrive in different orders on
//! different devices, and a set does not care; two devices holding the same
//! records compute the same [`View`] whatever order they saw them in, with no
//! coordination and no clock agreement. §14.3's convergence oracle is the test
//! of exactly this, and [`tests::any_order_gives_the_same_view`] is its L0
//! form.
//!
//! ## What the fold does when two devices disagree
//!
//! Every entry names a [`Key`] it competes on — a tag on a message, a saved
//! search by name — and the winner is the latest assertion for that key under
//! [`Record::order`]. Wall-clock time decides, which is the honest answer for
//! a person editing their own mailbox on two machines and the wrong one under
//! clock skew; the tiebreak past it is arbitrary but *deterministic*, which is
//! the property that actually matters. Nothing silently merges: a lost write
//! is one assertion losing to a later one, and both are still in the log.
//!
//! Revocation is the exception, and deliberately so — see [`View::devices`].
//!
//! ## What is not here
//!
//! The address book's *counts* (§5.1: "derived from mail traffic") are not
//! state. Every device sees the same traffic and derives the same ranking, so
//! syncing it would be shipping a cache. What the log carries is the part
//! traffic cannot supply: a name you gave an address yourself.

pub mod wire;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One assertion a device made about the shared state.
///
/// Every variant is "this is so", never "change this by": an increment would
/// have to be applied exactly once, and a log that a device may see twice —
/// which is every log delivered over IMAP — cannot promise that. An assertion
/// applied twice is the same assertion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    /// A tag on a message, present or removed. The message is the §5.2 stable
    /// id, which is the same on every device — that is what lets a tag applied
    /// on a phone find the same message on a laptop after a UIDVALIDITY reset.
    Tag {
        message: String,
        tag: String,
        present: bool,
    },
    /// A saved search. `None` deletes it.
    SavedSearch { name: String, query: Option<String> },
    /// When a message should come back. `None` un-snoozes it.
    ///
    /// §7 shipped snooze local-only, and every surface says which machine
    /// holds it. This is what removes that caveat.
    Snooze { message: String, until: Option<i64> },
    /// One §12 preference. `None` restores the default.
    Preference { key: String, value: Option<String> },
    /// A name for an address that mail traffic did not supply. `None` forgets
    /// it and goes back to what traffic says.
    Contact { addr: String, name: Option<String> },
    /// A device announcing itself, for `mailbox devices` (§8.3).
    Device {
        id: String,
        name: String,
        platform: String,
    },
    /// A device revoked (§8.3). See [`View::devices`] for why this one does
    /// not play by the last-writer rule.
    RevokeDevice { id: String },
}

/// What an entry competes on. Two entries with the same key are two answers to
/// one question, and the fold keeps the later.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Tag(String, String),
    SavedSearch(String),
    Snooze(String),
    Preference(String),
    Contact(String),
    /// `Device` and `RevokeDevice` share a key on purpose: they are two things
    /// to say about one device.
    Device(String),
}

impl Entry {
    pub fn key(&self) -> Key {
        match self {
            Entry::Tag { message, tag, .. } => Key::Tag(message.clone(), tag.clone()),
            Entry::SavedSearch { name, .. } => Key::SavedSearch(name.clone()),
            Entry::Snooze { message, .. } => Key::Snooze(message.clone()),
            Entry::Preference { key, .. } => Key::Preference(key.clone()),
            Entry::Contact { addr, .. } => Key::Contact(addr.clone()),
            Entry::Device { id, .. } => Key::Device(id.clone()),
            Entry::RevokeDevice { id } => Key::Device(id.clone()),
        }
    }
}

/// One record in the log: an assertion, and enough about its origin to order
/// it against the others.
///
/// All of this is sealed before it leaves the device ([`wire`]). None of it is
/// in a header a server operator can read — not even the device id, which
/// would otherwise tell them how many machines the user owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Random, 16 bytes hex. The record's identity, and so the deduplication:
    /// a folder fetched twice yields the same ids and the log does not grow.
    pub id: String,
    /// Which device wrote it.
    pub device: String,
    /// That device's own counter. Meaningful only against records from the
    /// same device, where it is the true order regardless of what its clock
    /// said.
    pub seq: u64,
    /// Unix seconds, from the writing device's clock.
    pub at: i64,
    pub entry: Entry,
}

impl Record {
    /// The total order the fold resolves conflicts by.
    ///
    /// `at` first, because a person editing their own state expects the last
    /// thing they did to win. Then `seq`, which repairs a same-second pair
    /// from one device. Then `device` and `id`, which decide nothing
    /// meaningful and exist only so that two devices holding the same records
    /// never disagree about the answer.
    pub fn order(&self) -> (i64, u64, &str, &str) {
        (self.at, self.seq, &self.device, &self.id)
    }
}

/// The append-only log: a set of records, keyed by id so a record seen twice
/// is stored once.
#[derive(Debug, Clone, Default)]
pub struct Log {
    records: BTreeMap<String, Record>,
}

impl Log {
    pub fn new() -> Log {
        Log::default()
    }

    /// Adds a record. Returns whether it was new — which is what tells a
    /// caller a fetched folder actually carried something.
    pub fn insert(&mut self, record: Record) -> bool {
        self.records.insert(record.id.clone(), record).is_none()
    }

    /// Takes in everything from `other`. Order-independent, like every other
    /// operation here: this is set union.
    pub fn merge(&mut self, other: &Log) -> usize {
        let mut added = 0;
        for record in other.records.values() {
            if self.insert(record.clone()) {
                added += 1;
            }
        }
        added
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.records.contains_key(id)
    }

    /// Folds the log into the state it describes.
    pub fn view(&self) -> View {
        let mut winners: BTreeMap<Key, &Record> = BTreeMap::new();
        let mut revoked: Vec<String> = Vec::new();

        for record in self.records.values() {
            // Revocation is monotone: once anyone has said a device is
            // revoked, it stays revoked no matter what is asserted later. A
            // last-writer rule here would let a device that still holds the
            // old key un-revoke itself by appending a fresh announcement with
            // a later timestamp — and the whole point of §8.3 is that it
            // cannot. (The enforcement is the key rotation revocation
            // performs; this is the bookkeeping agreeing with it.)
            if let Entry::RevokeDevice { id } = &record.entry {
                if !revoked.contains(id) {
                    revoked.push(id.clone());
                }
            }
            let key = record.entry.key();
            match winners.get(&key) {
                Some(current) if current.order() >= record.order() => {}
                _ => {
                    winners.insert(key, record);
                }
            }
        }

        let mut view = View::default();
        for (key, record) in winners {
            match (&key, &record.entry) {
                (_, Entry::Tag { tag, present, .. }) => {
                    if let Key::Tag(message, _) = &key {
                        if *present {
                            view.tags
                                .entry(message.clone())
                                .or_default()
                                .push(tag.clone());
                        }
                    }
                }
                (
                    _,
                    Entry::SavedSearch {
                        name,
                        query: Some(q),
                    },
                ) => {
                    view.saved_searches.insert(name.clone(), q.clone());
                }
                (
                    _,
                    Entry::Snooze {
                        message,
                        until: Some(u),
                    },
                ) => {
                    view.snoozed.insert(message.clone(), *u);
                }
                (
                    _,
                    Entry::Preference {
                        key: k,
                        value: Some(v),
                    },
                ) => {
                    view.preferences.insert(k.clone(), v.clone());
                }
                (
                    _,
                    Entry::Contact {
                        addr,
                        name: Some(n),
                    },
                ) => {
                    view.contacts.insert(addr.clone(), n.clone());
                }
                (_, Entry::Device { id, name, platform }) => {
                    view.devices.push(DeviceRecord {
                        id: id.clone(),
                        name: name.clone(),
                        platform: platform.clone(),
                        last_seen: record.at,
                        revoked: false,
                    });
                }
                // A device whose only record is its revocation: it is named
                // rather than dropped, so `devices` can show that it was
                // revoked without needing the announcement to still exist
                // after a compaction.
                (Key::Device(id), Entry::RevokeDevice { .. }) => {
                    view.devices.push(DeviceRecord {
                        id: id.clone(),
                        name: String::new(),
                        platform: String::new(),
                        last_seen: record.at,
                        revoked: true,
                    });
                }
                _ => {}
            }
        }

        for device in &mut view.devices {
            if revoked.contains(&device.id) {
                device.revoked = true;
            }
        }
        for tags in view.tags.values_mut() {
            tags.sort();
        }
        view.devices.sort_by(|a, b| a.id.cmp(&b.id));
        view
    }
}

/// A device as `mailbox devices` shows it (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// Unix seconds of the latest record this device is known by.
    pub last_seen: i64,
    pub revoked: bool,
}

/// The state the log describes: the answer, with the log's history folded out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    /// Message id → its tags, sorted.
    pub tags: BTreeMap<String, Vec<String>>,
    pub saved_searches: BTreeMap<String, String>,
    /// Message id → unix seconds it comes back.
    pub snoozed: BTreeMap<String, i64>,
    pub preferences: BTreeMap<String, String>,
    /// Address → the name a person gave it, not the one traffic supplied.
    pub contacts: BTreeMap<String, String>,
    pub devices: Vec<DeviceRecord>,
}

impl View {
    pub fn tags_for(&self, message: &str) -> &[String] {
        self.tags.get(message).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Writes records as one device, keeping that device's `seq` monotonic.
///
/// The counter is what makes two edits in the same second from one machine
/// resolve in the order they were made, which a one-second clock cannot say.
pub struct Writer {
    device: String,
    seq: u64,
}

impl Writer {
    /// `seq` starts above the highest this device has already written, which
    /// the caller reads out of its own log.
    pub fn new(device: impl Into<String>, next_seq: u64) -> Writer {
        Writer {
            device: device.into(),
            seq: next_seq,
        }
    }

    /// The next `seq` this writer will use, to be persisted so a restart does
    /// not reuse one.
    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    pub fn write(&mut self, entry: Entry, clock: &crate::Clock, rng: &crate::Rng) -> Record {
        let record = Record {
            id: rng.hex(16),
            device: self.device.clone(),
            seq: self.seq,
            at: clock.now(),
            entry,
        };
        self.seq += 1;
        record
    }
}

/// The highest `seq` `device` has in this log, or `None` if it has written
/// nothing. `Writer::new` takes one more than this.
pub fn highest_seq(log: &Log, device: &str) -> Option<u64> {
    log.records()
        .filter(|r| r.device == device)
        .map(|r| r.seq)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, device: &str, seq: u64, at: i64, entry: Entry) -> Record {
        Record {
            id: id.into(),
            device: device.into(),
            seq,
            at,
            entry,
        }
    }

    fn tag(
        id: &str,
        device: &str,
        seq: u64,
        at: i64,
        message: &str,
        tag: &str,
        present: bool,
    ) -> Record {
        record(
            id,
            device,
            seq,
            at,
            Entry::Tag {
                message: message.into(),
                tag: tag.into(),
                present,
            },
        )
    }

    fn log_of(records: &[Record]) -> Log {
        let mut log = Log::new();
        for r in records {
            log.insert(r.clone());
        }
        log
    }

    #[test]
    fn an_assertion_becomes_state() {
        let log = log_of(&[tag("a", "laptop", 0, 100, "m1", "work", true)]);
        assert_eq!(log.view().tags_for("m1"), ["work"]);
    }

    #[test]
    fn a_later_assertion_wins() {
        let log = log_of(&[
            tag("a", "laptop", 0, 100, "m1", "work", true),
            tag("b", "phone", 0, 200, "m1", "work", false),
        ]);
        assert!(log.view().tags_for("m1").is_empty());
    }

    /// The removal arriving *first* must not win. This is the case a naive
    /// "apply in arrival order" implementation gets wrong, and the reason the
    /// fold reads the whole set rather than replaying it.
    #[test]
    fn an_earlier_removal_does_not_beat_a_later_add() {
        let log = log_of(&[
            tag("b", "phone", 0, 100, "m1", "work", false),
            tag("a", "laptop", 0, 200, "m1", "work", true),
        ]);
        assert_eq!(log.view().tags_for("m1"), ["work"]);
    }

    /// §14.3's convergence oracle, in its L0 form: the same records in any
    /// order are the same state. Every permutation, not a sample — the set is
    /// small enough to be exhaustive, and "converges for the orders we tried"
    /// is not the property.
    #[test]
    fn any_order_gives_the_same_view() {
        let records = vec![
            tag("a", "laptop", 0, 100, "m1", "work", true),
            tag("b", "phone", 0, 150, "m1", "work", false),
            tag("c", "laptop", 1, 200, "m1", "urgent", true),
            record(
                "d",
                "phone",
                1,
                120,
                Entry::SavedSearch {
                    name: "unread".into(),
                    query: Some("is:unread".into()),
                },
            ),
            record(
                "e",
                "laptop",
                2,
                180,
                Entry::Snooze {
                    message: "m2".into(),
                    until: Some(9_000),
                },
            ),
        ];

        let expected = log_of(&records).view();
        let mut order: Vec<usize> = (0..records.len()).collect();
        let mut seen = 0;
        permutations(&mut order, 0, &mut |perm| {
            let shuffled: Vec<Record> = perm.iter().map(|i| records[*i].clone()).collect();
            assert_eq!(
                log_of(&shuffled).view(),
                expected,
                "order {perm:?} disagreed"
            );
            seen += 1;
        });
        assert_eq!(seen, 120, "every permutation of five records");
    }

    /// Two devices that were both offline, each with records the other never
    /// saw, agree the moment they exchange them — in either direction.
    #[test]
    fn two_divergent_logs_agree_after_merging_either_way() {
        let mine = log_of(&[
            tag("a", "laptop", 0, 100, "m1", "work", true),
            tag("c", "laptop", 1, 300, "m2", "later", true),
        ]);
        let theirs = log_of(&[
            tag("b", "phone", 0, 200, "m1", "work", false),
            tag("d", "phone", 1, 250, "m3", "receipts", true),
        ]);

        let mut a = mine.clone();
        assert_eq!(a.merge(&theirs), 2);
        let mut b = theirs.clone();
        assert_eq!(b.merge(&mine), 2);
        assert_eq!(a.view(), b.view());

        // And merging what it already has changes nothing.
        assert_eq!(a.merge(&theirs), 0);
        assert_eq!(a.view(), b.view());
    }

    #[test]
    fn a_record_seen_twice_is_stored_once() {
        let r = tag("a", "laptop", 0, 100, "m1", "work", true);
        let mut log = Log::new();
        assert!(log.insert(r.clone()));
        assert!(!log.insert(r));
        assert_eq!(log.len(), 1);
    }

    /// One device, two edits in the same second: the clock cannot separate
    /// them and `seq` must.
    #[test]
    fn a_devices_own_counter_separates_edits_within_a_second() {
        let log = log_of(&[
            tag("a", "laptop", 7, 100, "m1", "work", true),
            tag("b", "laptop", 8, 100, "m1", "work", false),
        ]);
        assert!(log.view().tags_for("m1").is_empty());
    }

    #[test]
    fn deleting_a_saved_search_removes_it() {
        let log = log_of(&[
            record(
                "a",
                "laptop",
                0,
                100,
                Entry::SavedSearch {
                    name: "unread".into(),
                    query: Some("is:unread".into()),
                },
            ),
            record(
                "b",
                "laptop",
                1,
                200,
                Entry::SavedSearch {
                    name: "unread".into(),
                    query: None,
                },
            ),
        ]);
        assert!(log.view().saved_searches.is_empty());
    }

    /// §8.3: revocation is not a last-writer-wins field. A revoked device that
    /// still holds the old key must not be able to re-announce itself back
    /// into the list.
    #[test]
    fn a_revoked_device_cannot_re_announce_itself() {
        let log = log_of(&[
            record(
                "a",
                "phone",
                0,
                100,
                Entry::Device {
                    id: "phone".into(),
                    name: "Phone".into(),
                    platform: "ios".into(),
                },
            ),
            record(
                "b",
                "laptop",
                0,
                200,
                Entry::RevokeDevice { id: "phone".into() },
            ),
            // Later than the revocation, and from the revoked device itself.
            record(
                "c",
                "phone",
                1,
                300,
                Entry::Device {
                    id: "phone".into(),
                    name: "Phone".into(),
                    platform: "ios".into(),
                },
            ),
        ]);
        let view = log.view();
        let phone = view
            .devices
            .iter()
            .find(|d| d.id == "phone")
            .expect("the device is listed");
        assert!(phone.revoked, "revocation must be permanent");
    }

    #[test]
    fn a_writer_keeps_its_counter_moving() {
        let clock = crate::Clock::fixed(1000);
        let rng = crate::Rng::seeded(b"state");
        let mut writer = Writer::new("laptop", 5);
        let first = writer.write(
            Entry::Preference {
                key: "theme".into(),
                value: Some("dark".into()),
            },
            &clock,
            &rng,
        );
        let second = writer.write(
            Entry::Preference {
                key: "theme".into(),
                value: Some("light".into()),
            },
            &clock,
            &rng,
        );
        assert_eq!(first.seq, 5);
        assert_eq!(second.seq, 6);
        assert_ne!(first.id, second.id);
        assert_eq!(writer.next_seq(), 7);

        // Same second, so `seq` is what decides — and it does.
        let log = log_of(&[first, second]);
        assert_eq!(
            log.view().preferences.get("theme").map(String::as_str),
            Some("light")
        );
    }

    #[test]
    fn highest_seq_finds_where_to_resume() {
        let log = log_of(&[
            tag("a", "laptop", 3, 100, "m1", "x", true),
            tag("b", "laptop", 9, 110, "m1", "y", true),
            tag("c", "phone", 40, 120, "m1", "z", true),
        ]);
        assert_eq!(highest_seq(&log, "laptop"), Some(9));
        assert_eq!(highest_seq(&log, "phone"), Some(40));
        assert_eq!(highest_seq(&log, "tablet"), None);
    }

    /// Calls `body` with every permutation of `items`.
    fn permutations(items: &mut Vec<usize>, k: usize, body: &mut impl FnMut(&[usize])) {
        if k == items.len() {
            body(items);
            return;
        }
        for i in k..items.len() {
            items.swap(k, i);
            permutations(items, k + 1, body);
            items.swap(k, i);
        }
    }
}
