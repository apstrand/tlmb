//! Local JWZ threading (§5.6).
//!
//! Server-side `THREAD` is not used even where advertised, because
//! implementations disagree about the same mailbox and a conversation must
//! group identically on a laptop and a phone. So this is the only threader,
//! it runs on every platform, and it is deterministic: the same input set
//! produces the same tree and the same ids, in the same order, every time.
//!
//! The algorithm is Jamie Zawinski's, with one deliberate narrowing — step 5's
//! subject-based grouping of otherwise unlinked messages is restricted to
//! messages within [`ThreadingOptions::subject_window`] of each other, because
//! unrestricted subject grouping merges every "Re: lunch?" a mailbox has ever
//! seen into one thread.

use std::collections::HashMap;

/// What the threader needs from one message. Everything is borrowed: threading
/// runs over the whole mailbox and must not copy it.
#[derive(Debug, Clone)]
pub struct ThreadInput<'a> {
    /// The stable id of §5.2.
    pub id: &'a str,
    /// `Message-ID`, already passed through [`crate::id::normalize_message_id`].
    pub message_id: Option<&'a str>,
    /// `In-Reply-To`, normalized. Used only when `references` does not already
    /// name it.
    pub in_reply_to: Option<&'a str>,
    /// `References`, normalized, in header order (oldest ancestor first).
    pub references: &'a [String],
    pub subject: &'a str,
    /// Unix seconds; `Date`, falling back to `INTERNALDATE`.
    pub date: i64,
}

#[derive(Debug, Clone)]
pub struct ThreadingOptions {
    /// `threading.subject` (§5.6). Off for users whose mail defeats the
    /// heuristic — mailing lists that rewrite subjects, ticket systems that
    /// reuse them.
    pub subject_grouping: bool,
    /// How far apart two same-subject roots may be and still be joined.
    pub subject_window: i64,
}

impl Default for ThreadingOptions {
    fn default() -> Self {
        ThreadingOptions {
            subject_grouping: true,
            subject_window: 14 * 86_400,
        }
    }
}

/// Where one message landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    /// The id of the thread's root message (§5.6).
    pub thread_id: String,
    /// The message this one replies to *within the local set*. `None` for a
    /// root, and also for a reply whose parent we have not downloaded.
    pub parent_id: Option<String>,
    /// Distance from the thread root, for rendering `show --thread`.
    pub depth: u32,
}

/// The result: one assignment per input message, ordered by thread and then by
/// tree position. The order is part of the contract — golden files depend on it.
#[derive(Debug, Clone, Default)]
pub struct Threading {
    pub assignments: Vec<Assignment>,
}

impl Threading {
    pub fn thread_of(&self, id: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.thread_id.as_str())
    }
}

/// A JWZ container. Arena-allocated: the algorithm is all reparenting, and
/// index links keep that borrow-free.
#[derive(Debug, Default)]
struct Container {
    /// Index into the input slice, when a real message occupies this node.
    msg: Option<usize>,
    parent: Option<usize>,
    children: Vec<usize>,
    /// Set when the node is spliced out during pruning.
    dead: bool,
}

struct Arena {
    nodes: Vec<Container>,
}

impl Arena {
    fn new() -> Arena {
        Arena { nodes: Vec::new() }
    }

    fn alloc(&mut self) -> usize {
        self.nodes.push(Container::default());
        self.nodes.len() - 1
    }

    /// Detaches `child` from whatever parent it has.
    fn unlink(&mut self, child: usize) {
        if let Some(p) = self.nodes[child].parent.take() {
            self.nodes[p].children.retain(|c| *c != child);
        }
    }

    /// Reparents `child` under `parent`, refusing anything that would make a
    /// cycle. Broken `References` headers — a message listing itself, two
    /// messages each claiming the other as ancestor — are common enough that
    /// this check is load-bearing rather than defensive.
    fn link(&mut self, parent: usize, child: usize) {
        if parent == child || self.is_ancestor(child, parent) {
            return;
        }
        self.unlink(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    fn is_ancestor(&self, maybe_ancestor: usize, node: usize) -> bool {
        let mut cur = Some(node);
        let mut guard = 0;
        while let Some(n) = cur {
            if n == maybe_ancestor {
                return true;
            }
            // A cycle already in the arena would hang the walk; the link()
            // guard prevents one, and this bounds the damage if it ever fails.
            guard += 1;
            if guard > self.nodes.len() {
                return true;
            }
            cur = self.nodes[n].parent;
        }
        false
    }
}

/// Threads a set of messages. `msgs` may be in any order; the output is not
/// affected by it.
pub fn thread(msgs: &[ThreadInput<'_>], opts: &ThreadingOptions) -> Threading {
    let mut arena = Arena::new();
    // Message-ID -> container. Only real, normalized ids get an entry; a
    // message without one cannot be anybody's parent.
    let mut by_msgid: HashMap<&str, usize> = HashMap::new();

    // Step 1. One pass, in a fixed order so that duplicate-Message-ID
    // tie-breaking does not depend on how the store handed us the rows.
    let mut order: Vec<usize> = (0..msgs.len()).collect();
    order.sort_by(|a, b| (msgs[*a].date, msgs[*a].id).cmp(&(msgs[*b].date, msgs[*b].id)));

    for &i in &order {
        let m = &msgs[i];

        // 1A: find or make this message's container.
        let this = match m.message_id {
            Some(mid) => match by_msgid.get(mid).copied() {
                Some(c) if arena.nodes[c].msg.is_none() => {
                    arena.nodes[c].msg = Some(i);
                    c
                }
                // A duplicate Message-ID. Both messages are real, so the second
                // gets its own container and stays out of the table — it can be
                // a child, but nothing will thread *onto* it.
                Some(_) => {
                    let c = arena.alloc();
                    arena.nodes[c].msg = Some(i);
                    c
                }
                None => {
                    let c = arena.alloc();
                    arena.nodes[c].msg = Some(i);
                    by_msgid.insert(mid, c);
                    c
                }
            },
            None => {
                let c = arena.alloc();
                arena.nodes[c].msg = Some(i);
                c
            }
        };

        // 1B: chain the References together, oldest first, without disturbing
        // links that a better-informed message already established.
        let refs = reference_chain(m);
        let mut prev: Option<usize> = None;
        for r in &refs {
            let c = match by_msgid.get(r.as_str()).copied() {
                Some(c) => c,
                None => {
                    let c = arena.alloc();
                    // Leaked into the table deliberately: this empty container
                    // is how a reply finds its siblings before their common
                    // ancestor has been downloaded.
                    by_msgid.insert(intern(r), c);
                    c
                }
            };
            if let Some(p) = prev {
                if arena.nodes[c].parent.is_none() {
                    arena.link(p, c);
                }
            }
            prev = Some(c);
        }

        // 1C: the last reference is this message's parent, and it wins over
        // whatever the container had — this message's own header is better
        // evidence than an inference drawn from someone else's.
        if let Some(p) = prev {
            arena.link(p, this);
        }
    }

    // Step 2/3: the root set is everything still parentless.
    let mut roots: Vec<usize> = (0..arena.nodes.len())
        .filter(|&i| arena.nodes[i].parent.is_none())
        .collect();

    // Step 4: prune the empty containers the References invented.
    for &r in &roots {
        prune(&mut arena, r, true);
    }
    // Recompute rather than filter. Pruning *promotes* nodes: an empty root
    // holding one child — a reply to a message we never downloaded — is spliced
    // out and its child becomes a root that was not in the pre-prune set.
    // Filtering the old list would silently drop that message from every thread.
    roots = (0..arena.nodes.len())
        .filter(|&i| {
            !arena.nodes[i].dead && arena.nodes[i].parent.is_none() && has_content(&arena, i)
        })
        .collect();

    // Step 5: join same-subject roots, narrowly.
    if opts.subject_grouping {
        roots = group_by_subject(&mut arena, msgs, roots, opts.subject_window);
    }

    // Step 6: emit, ordered.
    roots.sort_by_key(|&r| {
        let (d, id) = earliest(&arena, msgs, r);
        (d, id.to_string())
    });

    let mut out = Vec::with_capacity(msgs.len());
    for r in roots {
        let thread_id = thread_id_of(&arena, msgs, r).to_string();
        emit(&arena, msgs, r, &thread_id, None, 0, &mut out);
    }
    Threading { assignments: out }
}

/// `References`, with `In-Reply-To` appended when the header did not already
/// name it. RFC 5322 says `References` should end with the parent; plenty of
/// clients disagree, and this is the repair for that.
fn reference_chain(m: &ThreadInput<'_>) -> Vec<String> {
    let mut refs: Vec<String> = Vec::with_capacity(m.references.len() + 1);
    for r in m.references {
        // Self-references and repeats are both common in forwarded threads.
        if Some(r.as_str()) == m.message_id || refs.iter().any(|x| x == r) {
            continue;
        }
        refs.push(r.clone());
    }
    if let Some(irt) = m.in_reply_to {
        if Some(irt) != m.message_id && !refs.iter().any(|x| x == irt) {
            refs.push(irt.to_string());
        }
    }
    refs
}

/// The arena's Message-ID table borrows from the input for real ids, but
/// invented containers key on a `References` entry that outlives the loop only
/// as a leak. Threading runs once per sync pass over a bounded set, and the
/// alternative — an owned-key map — costs an allocation per lookup on the hot
/// path.
fn intern(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn has_content(arena: &Arena, node: usize) -> bool {
    arena.nodes[node].msg.is_some() || !arena.nodes[node].children.is_empty()
}

/// JWZ step 4. Depth-first, children before parent, so a chain of empties
/// collapses in one pass.
fn prune(arena: &mut Arena, node: usize, is_root: bool) {
    let children = arena.nodes[node].children.clone();
    for c in children {
        prune(arena, c, false);
    }

    if arena.nodes[node].msg.is_some() {
        return;
    }
    let kids = arena.nodes[node].children.clone();

    if kids.is_empty() {
        // An empty container nobody replied to: a `References` entry for a
        // message we do not have and never will thread anything onto.
        arena.unlink(node);
        arena.nodes[node].dead = true;
        return;
    }

    // An empty container with exactly one child is pure indirection. JWZ keeps
    // multi-child empties at the root — they are a real "these are all replies
    // to something we don't have" grouping — but a single child would be
    // promoted to a root of one, which is the same thing with an extra level.
    if kids.len() == 1 || !is_root {
        let parent = arena.nodes[node].parent;
        for c in kids {
            match parent {
                Some(p) => arena.link(p, c),
                None => {
                    arena.unlink(c);
                }
            }
        }
        arena.unlink(node);
        arena.nodes[node].dead = true;
    }
}

/// Step 5, narrowed: two roots join only when their normalized subjects match
/// *and* they are within `window` seconds of each other.
fn group_by_subject(
    arena: &mut Arena,
    msgs: &[ThreadInput<'_>],
    roots: Vec<usize>,
    window: i64,
) -> Vec<usize> {
    // Anchor per subject: the earliest root carrying it, plus the latest date
    // seen in the group so far, so a long-running thread keeps absorbing.
    let mut anchors: HashMap<String, (usize, i64)> = HashMap::new();
    let mut ordered = roots.clone();
    ordered.sort_by_key(|&r| {
        let (d, id) = earliest(arena, msgs, r);
        (d, id.to_string())
    });

    let mut absorbed: Vec<usize> = Vec::new();
    for root in ordered {
        let subject = root_subject(arena, msgs, root).map(normalize_subject);
        let Some(key) = subject.filter(|s| !s.is_empty()) else {
            continue;
        };
        let (first, last) = span(arena, msgs, root);

        match anchors.get_mut(&key) {
            Some((anchor, anchor_last)) if first - *anchor_last <= window => {
                // Attach under the anchor's own container when it holds a real
                // message; the anchor is by construction the older thread, so
                // this reads as "later mail on the same subject continues it".
                let target = *anchor;
                arena.link(target, root);
                absorbed.push(root);
                if last > *anchor_last {
                    *anchor_last = last;
                }
            }
            Some(_) => {
                // Same subject, but far enough apart to be a different
                // conversation. Leave it alone and let it anchor from now on.
                anchors.insert(key, (root, last));
            }
            None => {
                anchors.insert(key, (root, last));
            }
        }
    }

    roots
        .into_iter()
        .filter(|r| !absorbed.contains(r))
        .collect()
}

/// Strips reply and forward prefixes, collapses whitespace, folds case.
///
/// Kept to the prefixes that are genuinely universal plus the handful of
/// non-English ones common in practice; anything cleverer starts merging
/// unrelated mail, which is the failure mode this heuristic is already on
/// probation for.
pub fn normalize_subject(s: &str) -> String {
    const PREFIXES: &[&str] = &[
        "re", "fwd", "fw", "aw", "sv", "vs", "antw", "rif", "res", "odp",
    ];
    let mut cur = s.trim();
    loop {
        let lower = cur.to_ascii_lowercase();
        let mut stripped = false;
        for p in PREFIXES {
            // `Re:` and the `Re[2]:` / `Re(2):` counted forms.
            let rest = lower.strip_prefix(p).and_then(|r| {
                let r = r.trim_start();
                let r = match r.strip_prefix('[').or_else(|| r.strip_prefix('(')) {
                    Some(inner) => {
                        let end = inner.find([']', ')'])?;
                        if !inner[..end].chars().all(|c| c.is_ascii_digit()) {
                            return None;
                        }
                        &inner[end + 1..]
                    }
                    None => r,
                };
                r.trim_start().strip_prefix(':')
            });
            if let Some(rest) = rest {
                let consumed = cur.len() - rest.len();
                cur = cur[consumed..].trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    let mut out = String::with_capacity(cur.len());
    let mut space = false;
    for c in cur.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The subject a root thread is known by: its own message's, or — for an empty
/// root container — its earliest child's.
fn root_subject<'a>(arena: &Arena, msgs: &[ThreadInput<'a>], root: usize) -> Option<&'a str> {
    if let Some(i) = arena.nodes[root].msg {
        return Some(msgs[i].subject);
    }
    let mut best: Option<(i64, &str, &str)> = None;
    walk(arena, root, &mut |n| {
        if let Some(i) = arena.nodes[n].msg {
            let cand = (msgs[i].date, msgs[i].id, msgs[i].subject);
            if best.is_none_or(|b| (cand.0, cand.1) < (b.0, b.1)) {
                best = Some(cand);
            }
        }
    });
    best.map(|b| b.2)
}

/// Earliest `(date, id)` in the subtree — the sort key for threads and the
/// fallback thread id.
fn earliest<'a>(arena: &Arena, msgs: &[ThreadInput<'a>], root: usize) -> (i64, &'a str) {
    let mut best: Option<(i64, &str)> = None;
    walk(arena, root, &mut |n| {
        if let Some(i) = arena.nodes[n].msg {
            let cand = (msgs[i].date, msgs[i].id);
            if best.is_none_or(|b| cand < b) {
                best = Some(cand);
            }
        }
    });
    best.unwrap_or((0, ""))
}

fn span(arena: &Arena, msgs: &[ThreadInput<'_>], root: usize) -> (i64, i64) {
    let mut lo = i64::MAX;
    let mut hi = i64::MIN;
    walk(arena, root, &mut |n| {
        if let Some(i) = arena.nodes[n].msg {
            lo = lo.min(msgs[i].date);
            hi = hi.max(msgs[i].date);
        }
    });
    if lo == i64::MAX {
        (0, 0)
    } else {
        (lo, hi)
    }
}

/// The thread's id (§5.6): the root message's, or — when the root we have is a
/// placeholder for a message nobody downloaded — the earliest real message in
/// it.
///
/// The consequence is honest and worth stating: if the true root arrives later,
/// the thread's id changes. There is no id we could mint for a message we have
/// never seen that would also be the same on another device.
fn thread_id_of<'a>(arena: &Arena, msgs: &[ThreadInput<'a>], root: usize) -> &'a str {
    match arena.nodes[root].msg {
        Some(i) => msgs[i].id,
        None => earliest(arena, msgs, root).1,
    }
}

fn walk(arena: &Arena, node: usize, f: &mut impl FnMut(usize)) {
    f(node);
    for &c in &arena.nodes[node].children {
        walk(arena, c, f);
    }
}

fn emit(
    arena: &Arena,
    msgs: &[ThreadInput<'_>],
    node: usize,
    thread_id: &str,
    parent_id: Option<&str>,
    depth: u32,
    out: &mut Vec<Assignment>,
) {
    // An empty container contributes no row but does not consume a level
    // either: its children are the thread's real top-level messages.
    let (own_id, child_depth) = match arena.nodes[node].msg {
        Some(i) => {
            out.push(Assignment {
                id: msgs[i].id.to_string(),
                thread_id: thread_id.to_string(),
                parent_id: parent_id.map(str::to_string),
                depth,
            });
            (Some(msgs[i].id), depth + 1)
        }
        None => (parent_id, depth),
    };

    let mut children = arena.nodes[node].children.clone();
    children.sort_by_key(|&c| {
        let (d, id) = earliest(arena, msgs, c);
        (d, id.to_string())
    });
    for c in children {
        emit(arena, msgs, c, thread_id, own_id, child_depth, out);
    }
}
