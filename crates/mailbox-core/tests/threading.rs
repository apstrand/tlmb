//! L0: JWZ threading (§5.6, §14.2).
//!
//! Threading is where a mailbox stops being a list and starts being a
//! conversation, and it is the one derived value the convergence oracle
//! compares across devices — so these cases are about *determinism* as much as
//! correctness. Every assertion here would also hold on a phone.

use mailbox_core::threading::{normalize_subject, thread, ThreadInput, ThreadingOptions};

/// A compact way to declare a message: id, Message-ID, In-Reply-To, refs,
/// subject, day offset.
struct M {
    id: &'static str,
    msgid: Option<&'static str>,
    irt: Option<&'static str>,
    refs: Vec<String>,
    subject: &'static str,
    day: i64,
}

fn m(id: &'static str, msgid: &'static str, subject: &'static str, day: i64) -> M {
    M {
        id,
        msgid: Some(msgid),
        irt: None,
        refs: vec![],
        subject,
        day,
    }
}

impl M {
    fn reply_to(mut self, parent: &'static str) -> M {
        self.irt = Some(parent);
        self
    }
    fn refs(mut self, refs: &[&str]) -> M {
        self.refs = refs.iter().map(|s| s.to_string()).collect();
        self
    }
    fn no_msgid(mut self) -> M {
        self.msgid = None;
        self
    }
}

fn run(messages: &[M], opts: &ThreadingOptions) -> Vec<(String, String, Option<String>, u32)> {
    let inputs: Vec<ThreadInput<'_>> = messages
        .iter()
        .map(|x| ThreadInput {
            id: x.id,
            message_id: x.msgid,
            in_reply_to: x.irt,
            references: &x.refs,
            subject: x.subject,
            date: x.day * 86_400,
        })
        .collect();
    thread(&inputs, opts)
        .assignments
        .into_iter()
        .map(|a| (a.id, a.thread_id, a.parent_id, a.depth))
        .collect()
}

fn no_subject_grouping() -> ThreadingOptions {
    ThreadingOptions {
        subject_grouping: false,
        ..Default::default()
    }
}

/// Every input message appears in the output exactly once. This is the
/// invariant that a scoped rebuild, a subject merge, or a pruning bug breaks
/// first, and losing a message from every thread means losing it from the UI.
fn assert_total(messages: &[M], out: &[(String, String, Option<String>, u32)]) {
    assert_eq!(
        out.len(),
        messages.len(),
        "threading dropped or duplicated messages"
    );
    for msg in messages {
        assert_eq!(
            out.iter().filter(|(id, ..)| id == msg.id).count(),
            1,
            "`{}` should appear exactly once",
            msg.id
        );
    }
}

#[test]
fn single_message_is_its_own_thread() {
    let msgs = vec![m("a", "<1@x>", "Hello", 0)];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[0].1, "a");
    assert_eq!(out[0].2, None);
    assert_eq!(out[0].3, 0);
}

#[test]
fn reply_joins_its_parent() {
    let msgs = vec![
        m("a", "<1@x>", "Hello", 0),
        m("b", "<2@x>", "Re: Hello", 1).reply_to("<1@x>"),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[1].1, "a", "the thread is named for its root");
    assert_eq!(out[1].2.as_deref(), Some("a"));
    assert_eq!(out[1].3, 1);
}

/// Order of arrival must not matter: a mailbox synced in a different order on
/// another device has to produce the same tree.
#[test]
fn input_order_does_not_matter() {
    let forward = vec![
        m("a", "<1@x>", "Hello", 0),
        m("b", "<2@x>", "Re: Hello", 1).reply_to("<1@x>"),
        m("c", "<3@x>", "Re: Hello", 2).reply_to("<2@x>"),
    ];
    let reversed = vec![
        m("c", "<3@x>", "Re: Hello", 2).reply_to("<2@x>"),
        m("b", "<2@x>", "Re: Hello", 1).reply_to("<1@x>"),
        m("a", "<1@x>", "Hello", 0),
    ];
    assert_eq!(
        run(&forward, &no_subject_grouping()),
        run(&reversed, &no_subject_grouping())
    );
}

/// The whole point of `References`: a deep reply threads onto the root even
/// when the intermediate messages are missing.
#[test]
fn references_chain_survives_missing_middles() {
    let msgs = vec![
        m("a", "<1@x>", "Hello", 0),
        m("d", "<4@x>", "Re: Hello", 3).refs(&["<1@x>", "<2@x>", "<3@x>"]),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[1].1, "a");
    // The empty containers for <2@x> and <3@x> are pruned, so the depth
    // reflects the messages we have rather than the ones we do not.
    assert_eq!(
        out[1].3, 1,
        "pruned placeholders must not inflate the depth"
    );
}

/// A reply to a message nobody downloaded is still a thread. Before the fix
/// this test guards, the promoted child was dropped from the output entirely.
#[test]
fn orphan_reply_becomes_its_own_root() {
    let msgs = vec![m("a", "<1@x>", "Re: something", 0).reply_to("<never@x>")];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[0].1, "a");
    assert_eq!(out[0].2, None);
    assert_eq!(out[0].3, 0);
}

/// Two replies to the same missing ancestor belong together: that is the one
/// case JWZ keeps an empty root for.
#[test]
fn siblings_of_a_missing_ancestor_group() {
    let msgs = vec![
        m("a", "<1@x>", "Re: topic", 0).reply_to("<missing@x>"),
        m("b", "<2@x>", "Re: topic", 1).reply_to("<missing@x>"),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[0].1, out[1].1, "both replies should share a thread");
}

/// A message whose `References` names itself, and a pair that name each other.
/// Both appear in the wild and both are cycles.
#[test]
fn cyclic_references_do_not_hang() {
    let msgs = vec![
        m("a", "<1@x>", "Loop", 0).refs(&["<1@x>", "<2@x>"]),
        m("b", "<2@x>", "Loop", 1).refs(&["<1@x>", "<2@x>"]),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
}

/// Two distinct messages sharing a `Message-ID` — mailing lists that resend,
/// and clients with broken generators. Both must survive.
#[test]
fn duplicate_message_ids_both_survive() {
    let msgs = vec![
        m("a", "<dup@x>", "First", 0),
        m("b", "<dup@x>", "Second", 1),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
}

#[test]
fn missing_message_id_still_threads_as_a_root() {
    let msgs = vec![m("a", "<1@x>", "Hi", 0).no_msgid()];
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert_eq!(out[0].1, "a");
}

#[test]
fn subject_grouping_joins_unlinked_messages_inside_the_window() {
    let msgs = vec![
        m("a", "<1@x>", "Quarterly report", 0),
        m("b", "<2@x>", "Re: Quarterly report", 3),
    ];
    let out = run(&msgs, &ThreadingOptions::default());
    assert_total(&msgs, &out);
    assert_eq!(out[0].1, out[1].1, "same subject, three days apart");
}

/// The narrowing that makes subject grouping tolerable (§5.6): without a
/// window, every "Re: lunch?" a mailbox has ever seen becomes one thread.
#[test]
fn subject_grouping_respects_the_window() {
    let msgs = vec![
        m("a", "<1@x>", "Lunch?", 0),
        m("b", "<2@x>", "Re: Lunch?", 30),
    ];
    let out = run(&msgs, &ThreadingOptions::default());
    assert_total(&msgs, &out);
    assert_ne!(
        out[0].1, out[1].1,
        "thirty days apart is a different conversation"
    );
}

#[test]
fn subject_grouping_can_be_disabled() {
    let msgs = vec![
        m("a", "<1@x>", "Quarterly report", 0),
        m("b", "<2@x>", "Re: Quarterly report", 1),
    ];
    let out = run(&msgs, &no_subject_grouping());
    assert_ne!(out[0].1, out[1].1);
}

/// An empty subject must never become a grouping key, or every subjectless
/// message in the mailbox joins one thread.
#[test]
fn empty_subjects_never_group() {
    let msgs = vec![m("a", "<1@x>", "", 0), m("b", "<2@x>", "", 1)];
    let out = run(&msgs, &ThreadingOptions::default());
    assert_ne!(out[0].1, out[1].1);
}

#[test]
fn subject_normalization() {
    for (input, expected) in [
        ("Re: Hello", "hello"),
        ("RE: RE: Hello", "hello"),
        ("Fwd: Re: Hello", "hello"),
        ("Re[2]: Hello", "hello"),
        ("Re(3): Hello", "hello"),
        ("AW: Hallo", "hallo"),
        ("SV: Hej", "hej"),
        ("  Hello   world  ", "hello world"),
        ("Hello", "hello"),
        // Not a prefix: no colon, so it is part of the subject.
        ("Retrospective", "retrospective"),
        // A colon that is not a reply prefix stays put.
        ("Release: 1.0", "release: 1.0"),
        ("", ""),
    ] {
        assert_eq!(normalize_subject(input), expected, "normalizing {input:?}");
    }
}

/// A thread's messages come out in tree order, and threads come out oldest
/// first. Golden files depend on both.
#[test]
fn output_order_is_stable() {
    let msgs = vec![
        m("z-newer-root", "<10@x>", "Second thread", 10),
        m("a-older-root", "<1@x>", "First thread", 0),
        m("b-reply", "<2@x>", "Re: First thread", 1).reply_to("<1@x>"),
    ];
    let out = run(&msgs, &no_subject_grouping());
    let ids: Vec<&str> = out.iter().map(|(id, ..)| id.as_str()).collect();
    assert_eq!(ids, vec!["a-older-root", "b-reply", "z-newer-root"]);
}

/// A hundred replies to one root, shuffled deterministically. Guards the
/// recursion in `prune`, `walk`, and `emit` against a deep or wide tree.
#[test]
fn wide_and_deep_threads() {
    let mut msgs = Vec::new();
    let ids: Vec<String> = (0..100).map(|i| format!("m{i:03}")).collect();
    let msgids: Vec<String> = (0..100).map(|i| format!("<{i}@x>")).collect();
    for i in 0..100usize {
        msgs.push(M {
            id: Box::leak(ids[i].clone().into_boxed_str()),
            msgid: Some(Box::leak(msgids[i].clone().into_boxed_str())),
            // A chain, not a fan: each message replies to the previous one.
            irt: (i > 0).then(|| Box::leak(msgids[i - 1].clone().into_boxed_str()) as &str),
            refs: vec![],
            subject: "Deep",
            day: i as i64,
        });
    }
    let out = run(&msgs, &no_subject_grouping());
    assert_total(&msgs, &out);
    assert!(out.iter().all(|(_, thread_id, ..)| thread_id == "m000"));
    assert_eq!(
        out.last().unwrap().3,
        99,
        "a hundred-deep chain keeps its depth"
    );
}
