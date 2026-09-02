//! The generated performance corpus (§14.1, §14.5).
//!
//! > Generated corpora (the 1M-message perf set, §14.5) are produced from a
//! > seeded generator and cached as a CI artifact rather than committed.
//!
//! A million messages is 300 MB of mail nobody wants in a repository, so it is
//! generated instead — deterministically, from a seed, so two runs on two
//! machines measure the same mailbox.
//!
//! This writes to the store directly rather than going through
//! [`crate::store::ingest`]. That is a deliberate deviation and worth being
//! honest about: the ingest path parses MIME per message, which at a million
//! messages costs minutes and measures the parser rather than the store. What
//! the §10 budgets are about is query performance over a large mailbox, so the
//! generator produces the *rows* a sync would have produced and skips the
//! journey.

use rusqlite::params;

use crate::error::Result;
use crate::store::Store;

/// What to generate.
#[derive(Debug, Clone)]
pub struct CorpusSpec {
    pub messages: u64,
    /// Everything varies with this and nothing else.
    pub seed: u64,
    /// How many folders to spread the mail across, INBOX included.
    pub folders: u32,
    /// Fraction of messages left unread, 0..=100.
    pub unread_percent: u32,
    /// The newest message is `now`; the oldest is this many days earlier.
    pub span_days: i64,
}

impl Default for CorpusSpec {
    fn default() -> Self {
        CorpusSpec {
            messages: 1_000_000,
            seed: 1,
            folders: 8,
            unread_percent: 12,
            span_days: 1095,
        }
    }
}

/// What was generated, for the caller to report.
#[derive(Debug, Clone, Default)]
pub struct CorpusReport {
    pub messages: u64,
    pub threads: u64,
    pub folders: u32,
    pub elapsed_ms: u64,
}

/// A tiny deterministic PRNG.
///
/// SplitMix64: two lines, no dependency, and the same sequence on every
/// platform — which is the whole requirement. Nothing here is a security
/// decision, so [`crate::Rng`]'s hashing would only cost time.
struct Seq(u64);

impl Seq {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const FIRST_NAMES: &[&str] = &[
    "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi", "ivan", "judy", "mallory",
    "niaj", "olivia", "peggy", "rupert", "sybil", "trent", "victor", "walter", "zoe",
];
const DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "mail.example.net",
    "corp.example.com",
    "lists.example.org",
];
const SUBJECT_WORDS: &[&str] = &[
    "invoice",
    "report",
    "meeting",
    "review",
    "release",
    "budget",
    "proposal",
    "schedule",
    "incident",
    "summary",
    "notes",
    "draft",
    "renewal",
    "contract",
    "shipment",
    "reminder",
    "quarterly",
    "onboarding",
    "migration",
    "postmortem",
];
/// Planted terms with known selectivity, so a full-text measurement means
/// something.
///
/// A generated corpus whose every word appears in most messages does not
/// measure search — it measures sorting. These make the benchmark's questions
/// answerable: `NEEDLE` is what an ordinary search looks like, `FREQUENT` is
/// the worst case worth reporting.
pub const NEEDLE_TERM: &str = "zqneedle";
/// Roughly one message in a hundred carries [`NEEDLE_TERM`].
pub const NEEDLE_IN: u64 = 100;
pub const FREQUENT_TERM: &str = "zqfrequent";
/// Roughly one message in four carries [`FREQUENT_TERM`].
pub const FREQUENT_IN: u64 = 4;

/// How large the generated vocabulary is.
///
/// Real mail has a long tail: a few words in almost everything, most words in
/// almost nothing. Drawing from a pool of thirty gave every term a 74%
/// selectivity, which is not a search anyone performs.
const VOCABULARY: u64 = 20_000;

const BODY_WORDS: &[&str] = &[
    "the",
    "please",
    "find",
    "attached",
    "regarding",
    "yesterday",
    "deployment",
    "customer",
    "database",
    "latency",
    "rollback",
    "approved",
    "pending",
    "invoice",
    "payment",
    "quarter",
    "meeting",
    "agenda",
    "action",
    "items",
    "thanks",
    "regards",
    "following",
    "discussion",
    "proposal",
    "estimate",
    "timeline",
    "blocked",
    "resolved",
    "escalated",
];
const FOLDER_NAMES: &[&str] = &[
    "Archive", "Sent", "Drafts", "Trash", "Junk", "Projects", "Receipts", "Lists", "Travel",
];

impl Store {
    /// Fills the store with a generated corpus.
    ///
    /// Existing content is left alone, so a corpus can be grown; a benchmark
    /// wanting a clean one should start from an empty store.
    pub fn generate_corpus(&mut self, spec: &CorpusSpec) -> Result<CorpusReport> {
        let started = std::time::Instant::now();
        let now = self.clock.now();
        let account = self.account.clone();

        // Folders first: a placement needs one, and the ranking decides which
        // folder a message is reported as living in (§5.3).
        let mut folder_ids = Vec::new();
        folder_ids.push(self.upsert_folder(&crate::store::FolderRecord {
            path: "INBOX".into(),
            name: "INBOX".into(),
            delimiter: Some("/".into()),
            special_use: Some("inbox".into()),
            selectable: true,
            subscribed: true,
        })?);
        for name in FOLDER_NAMES
            .iter()
            .take(spec.folders.saturating_sub(1) as usize)
        {
            folder_ids.push(self.upsert_folder(&crate::store::FolderRecord {
                path: (*name).to_string(),
                name: (*name).to_string(),
                delimiter: Some("/".into()),
                special_use: crate::imap::guess_special_use(name),
                selectable: true,
                subscribed: true,
            })?);
        }

        let mut rng = Seq(spec.seed ^ 0x5DEE_CE66_D1EE_7B1F);
        let mut threads = 0u64;
        let mut written = 0u64;

        // One transaction per chunk. A single transaction over a million rows
        // grows the WAL without bound; committing periodically keeps memory and
        // disk flat, and a generator is allowed to be interrupted.
        const CHUNK: u64 = 20_000;
        while written < spec.messages {
            let batch = CHUNK.min(spec.messages - written);
            let start_index = written;
            let mut rng_chunk = Seq(rng.next());
            let (msgs, thread_count) = self.transaction("generate_corpus", |tx| {
                write_chunk(
                    tx,
                    &account,
                    spec,
                    &folder_ids,
                    start_index,
                    batch,
                    now,
                    &mut rng_chunk,
                )
            })?;
            written += msgs;
            threads += thread_count;
        }

        self.refresh_folder_counts()?;
        Ok(CorpusReport {
            messages: written,
            threads,
            folders: folder_ids.len() as u32,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn write_chunk(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    spec: &CorpusSpec,
    folder_ids: &[i64],
    start_index: u64,
    count: u64,
    now: crate::clock::Timestamp,
    rng: &mut Seq,
) -> Result<(u64, u64)> {
    let mut insert_message = tx.prepare(
        "INSERT OR IGNORE INTO messages
            (id, thread_id, parent_id, thread_depth, message_id, in_reply_to, refs,
             date, subject, subject_norm, addrs, flags, size, has_attachment, preview,
             primary_folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )?;
    let mut insert_address = tx.prepare(
        "INSERT INTO addresses (message_rowid, field, name, addr, ord) VALUES (?1, ?2, ?3, ?4, 0)",
    )?;
    let mut insert_placement = tx.prepare(
        "INSERT INTO placements (folder_id, uid, message_rowid, flags, internaldate)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut insert_fts = tx.prepare("INSERT INTO messages_fts (rowid, body) VALUES (?1, ?2)")?;

    let span = spec.span_days.max(1) * 86_400;
    let mut threads = 0u64;
    let mut written = 0u64;

    while written < count {
        // A thread of one to six messages, so the corpus has the shape a real
        // mailbox has rather than a million unrelated singletons.
        let thread_len = (1 + rng.below(6)).min(count - written);
        threads += 1;

        let root_index = start_index + written;
        let sender = format!(
            "{}{}@{}",
            rng.pick(FIRST_NAMES),
            rng.below(500),
            rng.pick(DOMAINS)
        );
        let subject = format!(
            "{} {} {}",
            rng.pick(SUBJECT_WORDS),
            rng.pick(SUBJECT_WORDS),
            rng.below(10_000)
        );
        let subject_norm = crate::threading::normalize_subject(&subject);
        // Oldest first within a thread, and the thread itself lands anywhere in
        // the span.
        let thread_start = now - (rng.below(span as u64) as i64);
        let folder = folder_ids[rng.below(folder_ids.len() as u64) as usize];

        let mut root_id = String::new();
        for position in 0..thread_len {
            let index = root_index + position;
            let message_id = format!("gen-{}-{}@example.invalid", spec.seed, index);
            let id = crate::id::message_id(
                account,
                Some(&message_id),
                &crate::id::Fallback {
                    date: thread_start,
                    from: &sender,
                    subject: &subject,
                    size: 0,
                },
            );
            if position == 0 {
                root_id = id.clone();
            }

            let date = thread_start + (position as i64) * 3_600;
            let unread = rng.below(100) < u64::from(spec.unread_percent);
            let flagged = rng.below(100) < 4;
            let has_attachment = rng.below(100) < 15;

            let mut flag_list: Vec<String> = Vec::new();
            if !unread {
                flag_list.push(crate::flags::SEEN.into());
            }
            if flagged {
                flag_list.push(crate::flags::FLAGGED.into());
            }
            let flags = crate::flags::pack(&flag_list);

            let recipient = format!("peter@{}", rng.pick(DOMAINS));
            let display = format!("{} {}", rng.pick(FIRST_NAMES), rng.pick(FIRST_NAMES));
            let addrs = serde_json::json!({
                "from": [{ "name": display, "address": sender }],
                "to": [{ "address": recipient }],
                "cc": [],
                "bcc": [],
                "reply_to": [],
            })
            .to_string();

            // A Zipf-ish body: a handful of very common words, then a long
            // tail drawn from a large vocabulary. `below(below(n))` skews hard
            // towards low indices, which is the shape word frequencies have.
            let mut words: Vec<String> = Vec::with_capacity(44);
            for _ in 0..8 {
                words.push((*rng.pick(BODY_WORDS)).to_string());
            }
            for _ in 0..32 {
                let bound = 1 + rng.below(VOCABULARY);
                words.push(format!("w{}", rng.below(bound)));
            }
            if index.is_multiple_of(NEEDLE_IN) {
                words.push(NEEDLE_TERM.to_string());
            }
            if index.is_multiple_of(FREQUENT_IN) {
                words.push(FREQUENT_TERM.to_string());
            }
            let body = words.join(" ");
            let preview: String = body.chars().take(120).collect();
            let display_subject = if position == 0 {
                subject.clone()
            } else {
                format!("Re: {subject}")
            };

            insert_message.execute(params![
                id,
                root_id,
                if position == 0 {
                    None
                } else {
                    Some(root_id.clone())
                },
                if position == 0 { 0i64 } else { 1 },
                message_id,
                Option::<String>::None,
                "[]",
                date,
                display_subject,
                subject_norm,
                addrs,
                flags,
                (2_000 + rng.below(60_000)) as i64,
                has_attachment as i64,
                preview,
                folder,
            ])?;
            let rowid = tx.last_insert_rowid();

            insert_address.execute(params![rowid, "from", display, sender])?;
            insert_address.execute(params![rowid, "to", "", recipient])?;
            insert_placement.execute(params![folder, index as i64 + 1, rowid, flags, date])?;
            // The text FTS would hold for a message whose body has been
            // fetched: subject, participants, and the body itself (§5.1).
            insert_fts.execute(params![
                rowid,
                format!("{display_subject}\n{display} {sender}\n{recipient}\n{body}")
            ])?;

            written += 1;
        }
    }
    Ok((written, threads))
}
