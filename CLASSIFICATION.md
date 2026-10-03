# Classifying mail: interesting, bulk, and suspect

Date: 2026-10-03
Status: design note. Nothing here is built.

## The ask

Automatic classification of incoming mail — not only spam, but "interesting"
against "not interesting" — and then a view of unread mail scoped to one class,
so that a whole class can be dealt with in one keystroke: mark read or archive
the uninteresting, junk the suspected spam.

## What the mailbox actually looks like

Measured against the real store on this machine (account `peter`, 52,071
messages) rather than assumed, because the shape of the answer depends on it:

| | |
|---|---|
| Inbox | 18,290 messages from 2,518 distinct senders |
| Top 20 senders | 6,619 messages — **36% of the inbox** |
| Of those 20 | 19 are machine senders (`transaction@`, `order-update@`, `no-reply…`, `notification+…@facebookmail.com`); one is a person |
| Replied to | 3,565 messages answered, across 2,453 threads |
| `contacts.sent_count > 0` | **0 rows** — the column exists and nothing populates it |
| Trash | 12,694 messages, 3,026 of them unread: deleted without being opened |
| Inbox senders whose mail is ≥80% binned (≥5 seen) | 84 senders, accounting for 330 messages still in the inbox |
| Bodies held locally | 2,263 of 52,071 (4%); `peter@nena.se`: 914 of 23,355 |

Three conclusions, and the whole design follows from them.

**Sender identity alone covers a third of the inbox.** Whatever else is true, a
classifier that cannot beat "who sent this" is not worth having, and one that
only does that much already pays for itself.

**The best signal for "interesting" is behavioural and already present.** 3,565
answered messages are 3,565 labels the user has given us for free, and
`sent_count` — have I ever written to this person — is the single most
discriminating feature available. It is schema'd and empty.

**Headers are available at ingest; bodies mostly are not.** Sync fetches
`BODY.PEEK[HEADER]` for every message (`imap/mod.rs`), and `mime::parse` hands
back every header not broken out into a column — but none of it is retained, and
only 4% of messages have a body in the CAS. So anything derived from headers
must be extracted *during ingest*, when they are in hand. It cannot be
backfilled across the whole store without re-fetching from the server.

## The design: three axes, not one score

A single "interestingness" number would merge three judgements that rest on
different evidence and fail in different ways. Keep them apart, compute each the
cheapest way that works, and expose each as a term in the §6.2 grammar — the
same move the filter buttons already make: a filter is a query term wearing a
button, so there is one answer to "why am I seeing these messages" and it is
visible.

### `is:bulk` — machine-sent. Deterministic, no training.

`List-Unsubscribe`, `List-Id`, `Precedence: bulk|list`, `Auto-Submitted`, a
no-reply localpart, an ESP's `DKIM d=`. These do not say "unwanted"; they say
"a mailing system sent this to many people", which is a fact about the message
and not a guess about the reader.

Extracted at ingest into a `signals` row of a few bits per message. No corpus,
no training, no false positives on personal mail, and it is the axis that covers
the 36%.

### `is:suspect` — spam. Here a classifier is the right tool.

Spam is adversarial and content-driven, which is the one place in this problem
where naive Bayes earns its keep. Start with what the server already decided —
the `$Junk` keyword, `X-Spam-*`, a failed `Authentication-Results` — and add a
local Bayes over subject and preview tokens after. The corpus is already here:
the Junk folder and the 3,026 unread-trashed messages as negatives, answered
threads as positives.

Bogofilter scale: one table of token counts, no new dependency, trains in
seconds on a mailbox this size.

### `is:known` and a priority score — personal relevance. Per correspondent.

A `senders` table — or `contacts`, extended: messages seen, opened, answered,
archived unread, trashed unread, flagged, last interaction. Priority is a
function of those counters.

**Not** token Bayes. The label the user gives is per-correspondent ("I bin
these"), so content Bayes would learn the vocabulary of whatever they are
working on this year and misfire on anything new. And it could not produce the
sentence the interface needs: *you binned 48 of the last 50 of these, unread*.
Explainability is not decoration here — it is what makes a key that acts on
thirty messages at once safe to press.

While in this code, populate `contacts.sent_count` from the Sent folder's
recipients. It is free, it is the best feature on this axis, and it is currently
zero for all 13,467 contacts.

## What not to do

- **No remote model.** §2: no telemetry, no network the user did not initiate.
  Mail is the most sensitive corpus a person has; it does not leave the device
  to be scored.
- **No local LLM or embedding model, at least not first.** It breaks "one SQLite
  file, runs unchanged on mobile" (§16) and the §10 budgets, and it cannot
  explain itself. If it ever earns its place, it fits behind the same `signals`
  interface as a fourth scorer rather than replacing the three.
- **Never move mail automatically.** Classification proposes; a person presses
  one key. A misfiling nobody asked for is indistinguishable from data loss, and
  an undo stack only helps someone who knows something happened.
- **Store the score, not the verdict.** A cached integer on `messages` keeps a
  listing an index scan at a million rows (§10); recompute it with
  `mailbox classify --rebuild`. Then changing the formula is a command, not a
  migration.

## The views are saved queries

Nothing new in the state model. A view is a query, exactly as the folder buttons
are:

- Triage: `in:inbox is:unread is:bulk`
- Suspected spam: `in:inbox is:suspect`
- What is left, which is the point of the exercise:
  `in:inbox is:unread NOT is:bulk NOT is:suspect`

The TUI gets all three by typing them; the GUI gets buttons that write them into
the search box, like the filters it already has.

## The missing primitive: act on the listing

There is no bulk action anywhere today. The GUI has no multi-select, and
`flag`, `mv`, `rm`, `archive` and `spam` take ids only (`TargetArgs`).

Rather than checkbox selection, **make the view the selection**: one key that
applies an action to every message the current query matches, with a count to
confirm before and the existing undo behind it. "One key to junk them all" then
means literally that, and it composes with every query rather than only with the
classifier's own views.

The same reach belongs in the contract, not just the window: `-q/--query` on the
write subcommands, and `query` beside `ids` in the `/api/flag` and `/api/move`
routes, so the CLI and an agent can do what the button does (§6.4).

## Schema sketch

```sql
-- v8: ingest-time header facts. One row per message, written once.
CREATE TABLE signals (
    message_rowid INTEGER PRIMARY KEY REFERENCES messages(rowid) ON DELETE CASCADE,
    -- Bitset: list-unsubscribe, list-id, precedence-bulk, auto-submitted,
    -- no-reply localpart, authentication failure, server-side junk verdict.
    bits          INTEGER NOT NULL DEFAULT 0,
    -- The list this came from, where it named itself, so a whole list can be
    -- acted on as a unit.
    list_id       TEXT
);

-- Per-correspondent behaviour: the labels the user has already given.
CREATE TABLE senders (
    addr            TEXT PRIMARY KEY,
    seen            INTEGER NOT NULL DEFAULT 0,
    opened          INTEGER NOT NULL DEFAULT 0,
    answered        INTEGER NOT NULL DEFAULT 0,
    archived_unread INTEGER NOT NULL DEFAULT 0,
    trashed_unread  INTEGER NOT NULL DEFAULT 0,
    flagged         INTEGER NOT NULL DEFAULT 0,
    last_action     INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;

-- And on messages: the cached score, so ordering stays an index scan.
ALTER TABLE messages ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
```

`senders` is maintained by the code that already records a write — the `ops`
queue and the `changes` log both pass through `store::write`, which is the one
place that knows a human did something.

## Staging

1. **`signals` at ingest, `is:bulk`, the two views, act-on-the-listing.** No
   classifier and no training; handles the 36%, and has no false-positive risk
   because every bit in it is a header the sender set themselves. Backfill
   scoped to the inbox only — a full backfill means re-fetching headers for
   52,000 messages, which is not worth it for mail already filed.
2. **`senders` counters, `is:known`, priority ordering, and
   `contacts.sent_count` populated from Sent.**
3. **The Bayes spam scorer**, with `mailbox classify --train` and
   `mailbox classify --explain <id>` so the whole thing is inspectable from the
   command line, as §6.7 asks of anything that makes a decision.

Stage 1 is where nearly all the benefit is.

## Verification

- L0: a fixture per header shape — list mail, a transactional receipt, a
  personal reply, a forged `List-Unsubscribe` on a personal message — asserting
  the bits and nothing else.
- L1: ingest a fixture mailbox, assert `is:bulk` selects exactly the mail it
  should, then move one message and assert the sender counters moved with it.
- Oracle, on a real mailbox: `classify --explain` over the inbox, eyeballed
  against what the reader actually did with that sender's last twenty messages.
  Not a CI test; the thing to run before trusting the score.
- §10: the listing budget holds only if `priority` is a column and not a
  per-row computation. Measure it on the million-message corpus before the
  score reaches the default sort.

## Open questions

- Does `is:bulk` mean "not interesting"? It does not, quite: a flight
  confirmation is bulk and matters. A second bit for *transactional* (the mail
  names an order, a booking, a payment) may be worth having before a key
  archives a whole class.
- Should the score follow the user across devices? It is derived state, so the
  cheap answer is no — each device recomputes from mail it has. §5.4's state
  folder is for assertions a device cannot derive, and this is not one.
- Does a `tag:` carry the verdict for interoperability (`tag:bulk` visible to
  other clients once M8 syncs tags), or does it stay a local column? Local
  first; a verdict is cheaper to recompute than to reconcile.
