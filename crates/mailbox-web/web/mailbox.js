// The graphical interface (§7): a client of the §6.4 contract.
//
// Plain modules, no build step and no framework — the same reasoning as the
// hand-written HTTP it talks to. `mailbox serve` loads this in a browser;
// `mailbox gui` loads it in a Tauri webview. Same page, same keys, same
// routes: every call here is one the CLI prints with `--json`, and there is
// deliberately no route that runs a command.
//
// The token goes out as a header on every request and never into a URL, so it
// stays out of history and out of any `Referer`. The cookie the server set is
// `HttpOnly` and cannot be read from here, which is the point of it.
//
// The desktop shell sets `window.__MAILBOX_SHELL__` before this module runs,
// so the status line can say "local store" rather than an ephemeral port.

const pageConfig = document.getElementById("config");
const token = pageConfig.dataset.token;
const hostedBackend = pageConfig.dataset.backend === "hosted"
    ? await import("./hosted.js")
    : null;
if (hostedBackend) await hostedBackend.ready;

const el = (id) => document.getElementById(id);
const listEl = el("list");
const readerEl = el("reader");

const state = {
    query: "",
    messages: [],
    listLimit: 100,
    activeFolder: { path: "INBOX" },
    partial: false,
    unloadedFolders: false,
    folderRecords: [],
    selected: null,
    /// The listing request whose answer is still wanted. Type three characters
    /// quickly and the answers can arrive in any order; painting a stale one is
    /// how a search box appears to ignore what was typed.
    listSeq: 0,
    openSeq: 0,
    inFlight: 0,
    /// The pending "this has been read" timer, cancelled when the cursor moves.
    readTimer: 0,
    /// Which of the three panes the arrows are acting on.
    focusPane: "list",
    /// The message being dragged, if one is.
    dragging: null,
};

// ---- talking to the server ----

async function api(path, options = {}) {
    const headers = { "X-Mailbox-Token": token };
    if (options.body) headers["Content-Type"] = "application/json";

    state.inFlight += 1;
    el("spinner").hidden = false;
    try {
        if (hostedBackend) return await hostedBackend.api(path, options);
        const response = await fetch(path, { ...options, headers });
        const text = await response.text();
        const value = text ? JSON.parse(text) : {};
        if (!response.ok) {
            // §6.5's error shape, which every failure here answers with.
            throw new Error(value?.error?.message ?? `request failed (${response.status})`);
        }
        return value;
    } finally {
        state.inFlight -= 1;
        el("spinner").hidden = state.inFlight === 0;
    }
}

const post = (path, body) =>
    api(path, { method: "POST", body: JSON.stringify(body) });

function fail(error) {
    el("error").textContent = String(error.message ?? error);
    el("status").textContent = "";
}

function say(message) {
    el("status").textContent = message;
    el("error").textContent = "";
}

// ---- the listing ----

async function reload() {
    const seq = ++state.listSeq;
    try {
        const params = new URLSearchParams({ q: state.query, limit: String(state.listLimit) });
        const listing = await api(`/api/ls?${params}`);
        if (seq !== state.listSeq) return; // A newer question has been asked.
        el("error").textContent = "";

        const previous = state.selected?.id;
        // Where the selection was, not just what it was on. A message that is
        // gone — deleted, archived, filed — took its id with it, and landing
        // back at the top of the list is how someone loses their place in a
        // mailbox they were working down.
        const previousAt = state.messages.findIndex((m) => m.id === previous);
        state.messages = (listing.messages ?? []).filter((message) => !pendingMoves.has(message.id));
        state.partial = Boolean(listing.partial);
        el("list-coverage").hidden = !(state.partial || state.unloadedFolders);
        drawList();

        const same = state.messages.findIndex((m) => m.id === previous);
        // Keep the cursor on the same message where one exists, so a refresh
        // does not move the selection out from under someone mid-read; and
        // where it does not, keep the *place*, which is the next message down
        // — or the new last one, when the list just lost its end.
        if (state.messages.length) {
            const at = same >= 0
                ? same
                : Math.min(Math.max(previousAt, 0), state.messages.length - 1);
            select(at, { pane: false, cachedOnly: true });
        } else clearReader();
    } catch (e) {
        if (seq === state.listSeq) fail(e);
    }
}

function drawList() {
    // §7: multi-account is invisible until you want it — one list, a colour
    // per account. A single-account mailbox has nothing to colour.
    const accounts = new Set(state.messages.map((m) => m.account).filter(Boolean));
    const multi = accounts.size > 1;

    listEl.replaceChildren(...state.messages.map((message, index) => {
        const row = document.createElement("li");
        row.className = "message";
        row.setAttribute("role", "option");
        row.dataset.index = String(index);
        row.id = `message-${index}`;
        if (!message.flags.includes("seen")) row.classList.add("unread");
        if (multi) {
            row.classList.add("multi-account");
            row.style.setProperty("--account-hue", String(hue(message.account)));
        }

        const marks =
            (message.flags.includes("flagged") ? "★" : "") +
            (message.has_attachment ? " \u{1F4CE}" : "");

        row.append(
            span("from", sender(message)),
            span("date", shortDate(message.date)),
            span("subject", message.subject || "(no subject)"),
            span("preview", message.preview ?? ""),
        );
        if (marks.trim()) row.querySelector(".date").prepend(span("marks", marks + " "));

        // Dragging a message to a folder files it — the mouse's version of
        // `mv`. The id travels as plain text so a drop can be acted on
        // without trusting anything held in this page's variables between
        // the pick-up and the let-go.
        row.draggable = true;
        row.addEventListener("dragstart", (event) => {
            event.dataTransfer.setData("text/plain", message.id);
            event.dataTransfer.effectAllowed = "move";
            row.classList.add("is-dragging");
            // Dragging a message that is not the one under the cursor should
            // move that message, not the selected one.
            state.dragging = message.id;
        });
        row.addEventListener("dragend", () => {
            row.classList.remove("is-dragging");
            state.dragging = null;
        });
        row.addEventListener("click", () => {
            // Clicking the message already under the cursor is a second,
            // deliberate look at it. The dwell exists to keep a cursor passing
            // through from marking a mailbox read; a click is not passing
            // through, so it does not have to wait.
            if (state.selected?.id === message.id) markRead(message);
            else select(index);
        });
        return row;
    }));

    el("list-empty").hidden = state.messages.length > 0;
}

/** Stable hue from an account id, so the same account is the same colour. */
function hue(account) {
    let h = 0;
    for (let i = 0; i < account.length; i++) {
        h = (h * 33 + account.charCodeAt(i)) >>> 0;
    }
    return h % 360;
}

function span(className, text) {
    const node = document.createElement("span");
    node.className = className;
    node.textContent = text;
    return node;
}

function sender(message) {
    const first = message.from?.[0];
    if (!first) return "(unknown sender)";
    return first.name?.trim() || first.address;
}

function addresses(list) {
    return (list ?? []).map((a) => a.name?.trim() || a.address).join(", ");
}

/** `2026-09-01T12:00:00Z` in the reader's own locale, at a length that fits. */
function shortDate(rfc3339) {
    const when = new Date(rfc3339);
    if (Number.isNaN(when.getTime())) return rfc3339;
    const today = new Date();
    const sameDay = when.toDateString() === today.toDateString();
    return sameDay
        ? when.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
        : when.toLocaleDateString([], { month: "short", day: "numeric" });
}

// ---- the reading pane ----

async function select(index, { pane = true, cachedOnly = false } = {}) {
    if (index < 0 || index >= state.messages.length) return;
    const message = state.messages[index];
    state.selected = message;

    for (const row of listEl.children) {
        const on = row.dataset.index === String(index);
        row.setAttribute("aria-selected", on ? "true" : "false");
        if (on) row.scrollIntoView({ block: "nearest" });
    }
    listEl.setAttribute("aria-activedescendant", `message-${index}`);
    if (pane) showPane("reader");

    const seq = ++state.openSeq;
    try {
        // The sanitized HTML of §9. A document rather than flattened text is
        // the whole reason this frontend builds DOM.
        const path = `/api/show?id=${encodeURIComponent(message.id)}&format=html`;
        let shown = await api(path + (hostedBackend ? "&cached=1" : ""));
        if (seq !== state.openSeq) return;
        drawReader(shown.message);
        if (hostedBackend && !cachedOnly && !shown.message?.body) {
            shown = await api(path);
            if (seq !== state.openSeq) return;
            drawReader(shown.message);
        }
        if (!hostedBackend || !cachedOnly || shown.message?.body) startReadDwell(message, seq);
    } catch (e) {
        if (seq === state.openSeq) fail(e);
    }
}

/// §7: a message read for long enough is read.
///
/// A pause, because the reading pane fills in as the cursor moves: without one,
/// holding `j` down would mark a whole mailbox read on the way past.
const READ_DWELL_MS = 1500;

function startReadDwell(message, seq) {
    clearTimeout(state.readTimer);
    if (message.flags.includes("seen")) return;

    state.readTimer = setTimeout(() => {
        // Still the same message. Moving on cancels the timer, but a reply
        // that arrived late could have moved the cursor since.
        if (seq !== state.openSeq) return;
        markRead(message);
    }, READ_DWELL_MS);
}

/** Marks one message seen and repaints only what that changed. */
async function markRead(message) {
    if (message.flags.includes("seen")) return;
    clearTimeout(state.readTimer);
    try {
        await post("/api/flag", { ids: [message.id], add: ["seen"],
            account: message.account, folder: message.folder });
        // The row, not the whole listing: reloading here would move things
        // under someone who is in the middle of reading.
        message.flags = [...message.flags, "seen"];
        for (const row of listEl.children) {
            if (state.messages[Number(row.dataset.index)]?.id === message.id) {
                row.classList.remove("unread");
            }
        }
        // The folder counts are unread counts, so one of them just went
        // down. The nav, unlike the listing, can be redrawn from under
        // someone without moving what they are reading.
        await loadFolders();
    } catch (e) {
        fail(e);
    }
}

function clearReader() {
    state.selected = null;
    readerEl.replaceChildren(Object.assign(document.createElement("p"), {
        className: "empty",
        textContent: "No message selected",
    }));
}

function drawReader(message) {
    if (!message) return clearReader();

    const head = document.createElement("div");
    head.className = "head";

    const subject = document.createElement("h1");
    subject.textContent = message.subject || "(no subject)";

    const who = document.createElement("div");
    who.className = "who";
    const from = document.createElement("strong");
    from.textContent = addresses(message.from);
    who.append(from);
    if (message.to?.length) who.append(` to ${addresses(message.to)}`);
    who.append(` · ${new Date(message.date).toLocaleString()}`);

    head.append(subject, who, actions(message));

    const parts = [head];

    const remote = hasRemoteContent(message.body?.content ?? "");
    if (remote && !imagesAllowedFor(message)) {
        parts.push(remoteNotice(message));
    }

    parts.push(bodyFrame(message));

    const attachments = (message.attachments ?? []).filter((a) => a.is_attachment);
    if (attachments.length) {
        const strip = document.createElement("div");
        strip.className = "attachments";
        for (const a of attachments) {
            strip.append(span("attachment", `\u{1F4CE} ${a.filename ?? a.part} (${size(a.size)})`));
        }
        parts.push(strip);
    }

    readerEl.replaceChildren(...parts);
}

function actions(message) {
    const row = document.createElement("div");
    row.className = "actions";

    const add = (label, run) => {
        const button = document.createElement("button");
        button.textContent = label;
        button.addEventListener("click", run);
        row.append(button);
        return button;
    };

    const seen = message.flags.includes("seen");
    add(seen ? "Mark unread" : "Mark read", () =>
        flag(message.id, seen ? { remove: ["seen"] } : { add: ["seen"] }));
    add(message.flags.includes("flagged") ? "Unflag" : "Flag", () =>
        flag(message.id, message.flags.includes("flagged")
            ? { remove: ["flagged"] }
            : { add: ["flagged"] }));
    add("Reply", () => replyTo("sender"));
    add("Reply all", () => replyTo("all"));
    add("Forward", () => replyTo("forward"));
    add("Archive", () => moveTo(message.id, "archive"));
    add("Delete", () => moveTo(message.id, "trash"));
    add("Spam", () => moveTo(message.id, "junk"));
    // §7's snooze, in its local-only form: the times anyone actually picks.
    for (const [label, until] of [["2h", "2h"], ["Tomorrow", "tomorrow"], ["Next week", "+7d"]]) {
        add(label, () => snooze(message.id, until));
    }
    return row;
}

/** The body, in a frame that can do nothing but display it. */
function bodyFrame(message) {
    const frame = document.createElement("iframe");
    frame.className = "body";
    frame.setAttribute("title", "Message body");
    // No scripts, no forms, no same-origin: whatever the sanitizer missed
    // cannot reach the page around it, the token in it, or anywhere else (§9).
    // Popups are allowed only so a link the reader clicks can open — the
    // sanitizer gives every anchor `target="_blank"`, and without this the
    // frame swallows the click. Nothing in the frame can open one by itself,
    // because nothing in the frame can run.
    frame.setAttribute("sandbox", "allow-popups allow-popups-to-escape-sandbox");
    frame.setAttribute("referrerpolicy", "no-referrer");
    // A real URL rather than `srcdoc`. A `srcdoc` frame inherits this page's
    // content policy, so the page's `img-src` would decide whether a message's
    // images may load — and the reader saying "load images" would be quietly
    // overruled. Given its own URL the frame gets exactly the policy the server
    // sends with it.
    frame.src = bodyUrl(message, imagesAllowedFor(message));
    return frame;
}

function bodyUrl(message, images) {
    if (hostedBackend) return hostedBackend.hostedBodyUrl(message, images);
    const params = new URLSearchParams({ id: message.id });
    if (images) params.set("images", "1");
    return `/body?${params}`;
}

function hasRemoteContent(html) {
    return html.includes("data-blocked-src=");
}

/** §9: remembered per sender, not per message. */
function senderKey(message) {
    return `images:${message.from?.[0]?.address ?? "unknown"}`;
}

function imagesAllowedFor(message) {
    try {
        return localStorage.getItem(senderKey(message)) === "yes";
    } catch {
        // A browser that refuses storage gets the safe answer, every time.
        return false;
    }
}

function remoteNotice(message) {
    const notice = document.createElement("div");
    notice.className = "notice";
    notice.append(span(
        "text",
        "Images in this message are on someone else\u2019s server. " +
        "Loading them tells them you read it.",
    ));

    const show = () => {
        notice.remove();
        const frame = readerEl.querySelector("iframe.body");
        if (frame) frame.src = bodyUrl(message, true);
    };

    const once = document.createElement("button");
    once.textContent = "Load images";
    once.addEventListener("click", show);

    const always = document.createElement("button");
    always.textContent = "Always for this sender";
    always.addEventListener("click", () => {
        try { localStorage.setItem(senderKey(message), "yes"); } catch { /* fine */ }
        show();
    });

    notice.append(once, always);
    return notice;
}

function size(bytes) {
    const units = ["B", "KB", "MB", "GB"];
    let n = bytes, i = 0;
    while (n >= 1024 && i < units.length - 1) { n /= 1024; i += 1; }
    return `${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

// ---- writes ----

async function change(run, what) {
    try {
        await run();
        say(what);
        await reload();
    } catch (e) {
        fail(e);
    }
}

const flag = (id, spec) =>
    change(async () => {
        // The account travels with the write for the same reason it does on a
        // move: the hosted backend keeps one store per account, and the folder
        // counts it caches are per account too.
        const account = state.messages.find((message) => message.id === id)?.account;
        await post("/api/flag", { ids: [id], ...spec, account });
        // `seen` is what the folder counts count.
        if ([...spec.add ?? [], ...spec.remove ?? []].includes("seen")) await loadFolders();
    }, "changed");

const pendingMoves = new Set();
/**
 * Moves this window can put back, newest last — the terminal's undo (§7).
 *
 * Held in the page and nowhere else, like the terminal holds it in its
 * session: it is a courtesy for the thing you just did by mistake, not a
 * history of the mailbox. Reloading loses it, which is the same promise the
 * terminal makes by forgetting on quit.
 */
const undoStack = [];

/** Deep enough for a wrong move a few messages ago, shallow enough to end. */
const UNDO_DEPTH = 20;

/** Puts back the last message that was filed. */
async function undoLast() {
    const last = undoStack.pop();
    if (!last) {
        say("nothing to undo");
        return;
    }
    try {
        await post("/api/move", { ids: [last.id], folder: last.folder,
            account: last.account, source_folder: last.movedTo });
        say(`restored to ${last.folder}`);
        await Promise.all([reload(), loadFolders()]);
    } catch (error) {
        // It goes back on the stack: the move did not happen, so the thing
        // that needs undoing still does.
        undoStack.push(last);
        fail(error);
    }
}

async function moveTo(id, folder) {
    const index = state.messages.findIndex((message) => message.id === id);
    const message = state.messages[index];
    // Where it was, recorded before it goes. The folder's own path, not the
    // special use it was filed under, because that is where it has to land.
    if (message) {
        undoStack.push({ id, account: message.account, folder: message.folder, movedTo: folder });
        if (undoStack.length > UNDO_DEPTH) undoStack.shift();
    }
    const request = post("/api/move", { ids: [id], folder,
        account: message?.account, source_folder: message?.folder,
        unread: message ? !message.flags.includes("seen") : false });
    const alreadyTrashed = message && state.folderRecords.some((record) =>
        record.account === message.account && record.path === message.folder && record.special_use === "trash");
    if (hostedBackend && message && folder === "trash" && !alreadyTrashed) {
        pendingMoves.add(id);
        clearTimeout(state.readTimer);
        state.openSeq += 1;
        state.messages.splice(index, 1);
        drawList();
        clearReader();
        if (state.messages.length) select(Math.min(index, state.messages.length - 1), { pane: false, cachedOnly: true });
    }
    try {
        await request;
        pendingMoves.delete(id);
        say(`moved to ${folder}`);
        await Promise.all([reload(), loadFolders()]);
    } catch (error) {
        pendingMoves.delete(id);
        const at = undoStack.findIndex((entry) => entry.id === id);
        if (at >= 0) undoStack.splice(at, 1);
        await reload();
        fail(error);
    }
}

const snooze = (id, until) =>
    change(async () => {
        const done = await post("/api/snooze", { ids: [id], until });
        // §7: a snooze is local until M8, so the interface names the machine
        // holding the timer rather than implying it is synced.
        say(`snoozed until ${done.until} — on ${done.device}`);
    }, "snoozed");

// ---- folders ----

async function loadFolders() {
    try {
        const { folders } = await api("/api/folders");
        state.folderRecords = folders;
        renderFolders();
        state.unloadedFolders = folders.some((folder) => folder.total > (folder.downloaded ?? folder.total));
        el("list-coverage").hidden = !(state.partial || state.unloadedFolders);
        el("load-more").hidden = !hostedBackend || !state.activeFolder || !folders.some((folder) =>
            (!state.activeFolder.account || folder.account === state.activeFolder.account) &&
            folder.path.toUpperCase() === state.activeFolder.path.toUpperCase() &&
            folder.total > (folder.downloaded ?? folder.total));
    } catch (e) {
        fail(e);
    }
}

let folderLayout = "unified";
/** Whether "Other folders" is open. Sits beside the per-folder state. */
const OTHER_FOLDERS_KEY = "other";

/**
 * Which folder groups are folded open, remembered between visits.
 *
 * Kept in this browser and nowhere else: it is a convenience of the window
 * someone is sitting at, not a property of the mailbox, and it should not
 * follow them to another machine or need the store to be written to. Every
 * access is guarded — a private window can refuse all three of these.
 */
const FOLDER_STATE_KEY = "mailbox.folders.open";

const folderOpenState = new Map(readFolderOpenState());

function readFolderOpenState() {
    try {
        return Object.entries(JSON.parse(localStorage.getItem(FOLDER_STATE_KEY) ?? "{}"));
    } catch {
        return [];
    }
}

function saveFolderOpenState() {
    try {
        localStorage.setItem(FOLDER_STATE_KEY, JSON.stringify(Object.fromEntries(folderOpenState)));
    } catch {
        // No storage, or a browser that refuses it. The nav still works; it
        // just starts folded the way it always did.
    }
}
const useNames = { inbox: "Inbox", sent: "Sent", drafts: "Drafts", trash: "Trash", junk: "Junk", archive: "Archive", all: "All mail", flagged: "Flagged", important: "Important" };

function rememberFolderOpenState() {
    for (const details of el("folder-view").querySelectorAll("details[data-folder-key]")) {
        folderOpenState.set(`${folderLayout}:${details.dataset.folderKey}`, details.open);
    }
    saveFolderOpenState();
}

// Folding a group is worth remembering the moment it happens, not only when
// something else redraws the nav — the next redraw may be the page reopening.
el("folder-view").addEventListener("toggle", rememberFolderOpenState, true);

function selectFolderQuery(query) {
    el("search").value = query;
    state.query = query;
    state.activeFolder = undefined;
    state.listLimit = 100;
    run(query);
    showPane("list");
}

function folderDetails(label, records, key, aggregateQuery) {
    const details = document.createElement("details");
    details.dataset.folderKey = key;
    const remembered = folderOpenState.get(`${folderLayout}:${key}`);
    details.open = remembered ?? folderLayout === "tree";
    const summary = document.createElement("summary");
    const total = records.reduce((n, f) => n + f.total, 0);
    const unread = records.reduce((n, f) => n + f.unread, 0);
    const labelNode = span("name", label);
    // Space selects the group rather than folding it (the disclosure's own
    // default). Folding is what the triangle and Enter are for; Space on a
    // folder should show its mail, which is what Space does on every row
    // below it.
    if (aggregateQuery) {
        summary.tabIndex = 0;
        summary.addEventListener("keydown", (event) => {
            if (event.key !== " ") return;
            event.preventDefault();
            selectFolderQuery(aggregateQuery);
        });
    }
    // §7: the count is the number that asks to be acted on. A folder with
    // nothing unread shows nothing — a grey 4134 beside Trash is a number
    // nobody has ever needed at a glance. It waits in the hover, with the
    // rest of what is worth knowing about the folder.
    summary.title = folderStats(records);
    if (aggregateQuery) {
        labelNode.classList.add("summary-link");
        labelNode.title = "Show this folder across all accounts";
        labelNode.addEventListener("click", (event) => {
            event.preventDefault(); event.stopPropagation(); selectFolderQuery(aggregateQuery);
        });
    }
    summary.append(labelNode, span("count", unread ? String(unread) : ""));
    if (unread > 0) summary.classList.add("has-unread");
    details.append(summary, ...records
        .slice().sort((a, b) => a.account.localeCompare(b.account) || a.path.localeCompare(b.path))
        // A leaf under "Inbox" does not need to say "Inbox" again — the row
        // above it does. What it adds is which account's inbox it is, and in
        // the tree, where the account is the heading, which folder.
        .map((f) => folderButton(
            folderLayout === "tree" ? f.name : f.account,
            `account:${f.account} in:${f.path.includes(" ") ? `"${f.path}"` : f.path}`,
            f.total, f.unread, { account: f.account, path: f.path })));
    return details;
}

/**
 * What a folder is worth saying on hover: what it is called on the server,
 * whose it is, and how much is in it.
 *
 * The server's own path, because a folder shown under its special use is
 * shown under a name the server may never have used — someone looking for
 * "Archive" on their provider needs to know it is called "[Gmail]/All Mail".
 */
function folderStats(records) {
    const lines = records
        .slice()
        .sort((a, b) => a.account.localeCompare(b.account) || a.path.localeCompare(b.path))
        .map((f) => {
            const who = addressFor(f.account);
            const counts = `${f.unread} unread, ${f.total} message${f.total === 1 ? "" : "s"}`;
            return `${who} — ${f.path}: ${counts}`;
        });
    return lines.join("\n");
}

/**
 * Makes a folder row a place a message can be dropped.
 *
 * The drop target is the folder's own path rather than its special use, for
 * the same reason undo records the path: it is where the message actually
 * has to land. A message can only be dropped into a folder of its own
 * account — a move across accounts is a copy and a delete, which is not what
 * dragging looks like it does, so it is refused rather than half-done.
 */
function acceptDrops(node, path, account) {
    node.addEventListener("dragover", (event) => {
        if (!state.dragging) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "move";
        node.classList.add("is-drop-target");
    });
    node.addEventListener("dragleave", () => node.classList.remove("is-drop-target"));
    node.addEventListener("drop", async (event) => {
        event.preventDefault();
        node.classList.remove("is-drop-target");
        const id = event.dataTransfer.getData("text/plain") || state.dragging;
        if (!id) return;
        const message = state.messages.find((m) => m.id === id);
        if (account && message && message.account !== account) {
            fail(new Error(`${message.account} and ${account} are different accounts`));
            return;
        }
        if (message && message.folder === path) return;
        await moveTo(id, path);
    });
}

/** The address behind an account's short name, for the hovers. */
function addressFor(account) {
    return accountRecords.find((a) => a.id === account)?.address ?? account;
}

function renderFolders() {
    rememberFolderOpenState();
    const view = el("folder-view");
    const records = state.folderRecords.filter((f) => f.selectable !== false);
    if (folderLayout === "tree") {
        const byAccount = new Map();
        for (const f of records) (byAccount.get(f.account) ?? byAccount.set(f.account, []).get(f.account)).push(f);
        view.replaceChildren(...[...byAccount].sort().map(([account, fs]) =>
            folderDetails(account, fs, `account:${account}`, `account:${account}`)));
        return;
    }
    const uses = new Map(), names = new Map();
    for (const f of records) {
        if (f.special_use) (uses.get(f.special_use) ?? uses.set(f.special_use, []).get(f.special_use)).push(f);
        else (names.get(f.name) ?? names.set(f.name, []).get(f.name)).push(f);
    }
    // The inbox is where the mail someone is waiting for arrives, so it goes
    // first however it sorts; the trash is where mail goes to be forgotten,
    // so it goes down with the rest rather than sitting beside the inbox.
    const DEMOTED = new Set(["trash"]);
    const used = [...uses]
        .filter(([use]) => !DEMOTED.has(use))
        .sort((a, b) =>
            Number(b[0] === "inbox") - Number(a[0] === "inbox")
            || (useNames[a[0]] ?? a[0]).localeCompare(useNames[b[0]] ?? b[0]));
    const demoted = [...uses]
        .filter(([use]) => DEMOTED.has(use))
        .map(([use, fs]) => [useNames[use] ?? use, fs, `use:${use}`, `in:${use}`]);
    const ordinary = [...names]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([name, fs]) => [name, fs, `name:${name}`, `in:${name.includes(" ") ? `"${name}"` : name}`]);
    const nodes = used.map(([use, fs]) => folderDetails(useNames[use] ?? use, fs, `use:${use}`, `in:${use}`));
    // Sorted together, so the trash takes its place by name among them.
    const others = [...demoted, ...ordinary].sort((a, b) => a[0].localeCompare(b[0]));
    if (others.length) {
        // A heading someone can put away. The folders under it are the long
        // tail — a provider's own machinery, old labels — and on an account
        // with forty of them they push the inbox off the screen.
        const other = document.createElement("details");
        other.className = "other-folders";
        other.dataset.folderKey = OTHER_FOLDERS_KEY;
        other.open = folderOpenState.get(`${folderLayout}:${OTHER_FOLDERS_KEY}`) ?? false;
        const heading = document.createElement("summary");
        heading.textContent = "Other folders";
        const unread = others.reduce((n, [, fs]) => n + fs.reduce((m, f) => m + f.unread, 0), 0);
        heading.append(span("count", unread ? String(unread) : ""));
        if (unread > 0) heading.classList.add("has-unread");
        other.append(heading, ...others.map(([label, fs, key, query]) =>
            folderDetails(label, fs, key, query)));
        nodes.push(other);
    }
    view.replaceChildren(...nodes);
}

el("folders-unified").addEventListener("click", () => {
    folderLayout = "unified";
    el("folders-unified").classList.add("is-selected"); el("folders-tree").classList.remove("is-selected");
    el("folders-unified").setAttribute("aria-selected", "true"); el("folders-tree").setAttribute("aria-selected", "false");
    renderFolders();
});
el("folders-tree").addEventListener("click", () => {
    folderLayout = "tree";
    el("folders-tree").classList.add("is-selected"); el("folders-unified").classList.remove("is-selected");
    el("folders-tree").setAttribute("aria-selected", "true"); el("folders-unified").setAttribute("aria-selected", "false");
    renderFolders();
});

function folderButton(label, query, total = 0, unread = 0, scope) {
    const button = document.createElement("button");
    button.className = "folder" + (state.query === query ? " is-current" : "");
    button.dataset.query = query;
    button.append(span("name", label));
    // §7: the count is the number that asks to be acted on — unread, and
    // nothing at all when there is none. The size of the folder is the rarer
    // question, so it waits in the hover beside it.
    button.append(span("count", unread > 0 ? String(unread) : ""));
    if (unread > 0) button.classList.add("has-unread");
    // The address rather than the short name, and the folder's own path on
    // the server — both are things the row is too narrow to show and someone
    // occasionally needs exactly.
    button.title = scope
        ? `${addressFor(scope.account)} — ${scope.path}: ${unread} unread, ${total} message${total === 1 ? "" : "s"}`
        : `${unread} unread, ${total} message${total === 1 ? "" : "s"}`;
    if (scope) acceptDrops(button, scope.path, scope.account);
    button.addEventListener("click", () => {
        el("search").value = query;
        state.activeFolder = scope;
        state.listLimit = 100;
        // The folder was a means to the messages; the arrows follow them.
        state.focusPane = "list";
        run(query);
        showPane("list");
        if (hostedBackend && scope) syncNow({ account: scope.account, folders: [scope.path] });
    });
    return button;
}

// ---- accounts ----

let accountRecords = [];
async function loadAccounts() {
    try {
        accountRecords = (await api("/api/accounts")).accounts ?? [];
        const list = el("account-list");
        list.replaceChildren(...accountRecords.map((account) => {
            const row = document.createElement("div");
            row.className = "account-row";
            row.append(span("name", account.id), span("address", account.address), span("server", account.imap_host));
            const edit = document.createElement("button");
            edit.className = "text"; edit.textContent = "Edit"; edit.disabled = !setupAllowed;
            edit.addEventListener("click", () => openAccountEditor(account));
            row.append(edit);
            return row;
        }));
        const add = el("account-add");
        add.disabled = !setupAllowed;
        add.title = setupAllowed ? "" : "This server does not take a password; add the account from the terminal.";
    } catch (e) { fail(e); }
}

/** Opens the setup form on an empty slate, for an account that is not the first. */
function beginAddAccount() {
    el("setup-form").reset();
    security = { imap: null, smtp: null };
    providerKnown = false;
    setupError("");
    el("setup-status").textContent = "";
    // Cancel is what gets back to the mailbox; the first-launch form has
    // nothing to go back to and so does not show it.
    el("setup-cancel").hidden = false;
    showSetup();
}

function openAccountEditor(account) {
    el("account-edit-id").value = account.id;
    el("account-edit-title").textContent = `Edit ${account.id}`;
    for (const [id, value] of [["address", account.address], ["imap-host", account.imap_host], ["imap-port", account.imap_port], ["imap-user", account.login_user], ["smtp-host", account.smtp_host], ["smtp-port", account.smtp_port], ["smtp-user", account.smtp_user]]) {
        el(`account-edit-${id}`).value = value ?? "";
    }
    el("account-edit-password").value = "";
    el("account-edit-note").textContent = account.has_credential ? "A password is stored. Leave blank to keep it." : "No password is stored.";
    el("account-edit-form").hidden = false;
}

el("accounts").addEventListener("click", async () => {
    el("account-edit-form").hidden = true;
    await loadAccounts();
    el("accounts-window").showModal();
});
el("accounts-close").addEventListener("click", () => el("accounts-window").close());
el("account-add").addEventListener("click", () => {
    el("accounts-window").close();
    beginAddAccount();
});
el("account-edit-cancel").addEventListener("click", () => { el("account-edit-form").hidden = true; });
el("account-edit-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const value = (id) => el(id).value.trim();
    const spec = {
        id: value("account-edit-id"), address: value("account-edit-address"),
        imap_host: value("account-edit-imap-host"), imap_port: Number(value("account-edit-imap-port")),
        imap_user: value("account-edit-imap-user"), smtp_host: value("account-edit-smtp-host"),
        smtp_port: Number(value("account-edit-smtp-port")), smtp_user: value("account-edit-smtp-user"),
        password: value("account-edit-password") || undefined,
    };
    try {
        await api("/api/accounts/edit", { method: "POST", body: JSON.stringify(spec) });
        el("account-edit-note").textContent = "Saved.";
        await loadAccounts(); await loadFolders();
    } catch (e) { el("account-edit-note").textContent = e.message; }
});

// ---- writing (§7) ----

const composeWindow = el("compose-window");
let identities = [];

async function loadIdentities() {
    try {
        identities = (await api("/api/identities")).identities ?? [];
    } catch (e) {
        fail(e);
    }
    const select = el("compose-from");
    select.replaceChildren(...identities.map((identity) => {
        const option = document.createElement("option");
        option.value = identity.address;
        option.textContent = identity.display;
        return option;
    }));
}

/// Opens the window on a draft. `null` is a blank one.
function compose(draft) {
    const values = draft ?? { to: [], cc: [], bcc: [], subject: "", body: "" };
    el("compose-to").value = (values.to ?? []).join(", ");
    el("compose-cc").value = (values.cc ?? []).join(", ");
    el("compose-bcc").value = (values.bcc ?? []).join(", ");
    el("compose-subject").value = values.subject ?? "";
    el("compose-body").value = values.body ?? "";
    el("compose-note").textContent = "";

    // Threading is the core's to decide (§5.6), so it rides along untouched.
    composeWindow.dataset.inReplyTo = values.in_reply_to ?? "";
    composeWindow.dataset.references = JSON.stringify(values.references ?? []);

    // §7: the From selector is one keystroke away, and defaults to the
    // identity the thread was addressed to.
    if (values.from) {
        const wanted = values.from.match(/<([^>]+)>/)?.[1] ?? values.from;
        const match = identities.find((i) => i.address === wanted.toLowerCase());
        if (match) el("compose-from").value = match.address;
    }

    const hasCc = (values.cc ?? []).length > 0;
    el("cc-field").hidden = !hasCc;
    el("bcc-field").hidden = !hasCc;

    composeWindow.showModal();
    // A reply starts in the body; a new message starts in the To field,
    // because that is the part that is empty.
    const body = el("compose-body");
    if (values.to?.length) {
        body.focus();
        // At the top, above the quotation. Focusing a textarea leaves the view
        // wherever the browser put it, which for a long quoted reply is the
        // bottom — so you start writing underneath someone else's message.
        body.setSelectionRange(0, 0);
        body.scrollTop = 0;
    } else {
        el("compose-to").focus();
    }
}

async function replyTo(mode, message = state.selected) {
    if (!message) return;
    try {
        const params = new URLSearchParams({ id: message.id, mode });
        compose(await api(`/api/reply?${params}`));
    } catch (e) {
        fail(e);
    }
}

async function sendDraft() {
    const button = el("compose-send");
    button.disabled = true;
    el("compose-note").textContent = "sending…";

    const split = (value) =>
        value.split(",").map((s) => s.trim()).filter(Boolean);

    try {
        const result = await post("/api/send", {
            from: el("compose-from").value || undefined,
            to: split(el("compose-to").value),
            cc: split(el("compose-cc").value),
            bcc: split(el("compose-bcc").value),
            subject: el("compose-subject").value,
            body: el("compose-body").value,
            in_reply_to: composeWindow.dataset.inReplyTo || undefined,
            references: JSON.parse(composeWindow.dataset.references || "[]"),
        });
        composeWindow.close();
        // §5.5: queued, not sent. Saying "sent" when the outbox still holds it
        // is a lie the moment submission fails.
        say(`queued for ${result.recipients.join(", ")}`);
    } catch (e) {
        el("compose-note").textContent = "";
        fail(e);
    } finally {
        button.disabled = false;
    }
}

el("compose").addEventListener("click", () => compose(null));
el("compose-send").addEventListener("click", sendDraft);
el("compose-discard").addEventListener("click", () => composeWindow.close());
el("compose-cc-toggle").addEventListener("click", () => {
    const hidden = el("cc-field").hidden;
    el("cc-field").hidden = !hidden;
    el("bcc-field").hidden = !hidden;
    if (hidden) el("compose-cc").focus();
});

// ---- what the keyboard does (§7, the terminal's muscle memory) ----
//
// The same letters as the terminal, because §7 asks for one set of muscle
// memory rather than two. They are `mailbox-tui`'s `Keymap::vim` defaults, and
// where the two used to differ the terminal wins: `s` marks read rather than
// archiving, `a` archives, `f` forwards, `F` flags, and sync is `@` — which
// frees `g` to be the top of the list, as it is everywhere else `g` appears.

const keys = {
    // The letters act on the message list wherever you are, as they do in the
    // terminal. The arrows are spatial: they move between the panes and then
    // within whichever one you are in.
    j: () => move(1),
    k: () => move(-1),
    ArrowDown: () => step(1),
    ArrowUp: () => step(-1),
    ArrowRight: () => focusPane(PANE_RIGHT[state.focusPane]),
    ArrowLeft: () => focusPane(PANE_LEFT[state.focusPane]),
    g: () => select(0),
    Home: () => select(0),
    G: () => select(state.messages.length - 1),
    End: () => select(state.messages.length - 1),
    PageDown: () => page(1),
    PageUp: () => page(-1),
    s: () => withSelected((m) => flag(m.id, m.flags.includes("seen")
        ? { remove: ["seen"] } : { add: ["seen"] })),
    F: () => withSelected((m) => flag(m.id, m.flags.includes("flagged")
        ? { remove: ["flagged"] } : { add: ["flagged"] })),
    a: () => withSelected((m) => moveTo(m.id, "archive")),
    d: () => withSelected((m) => moveTo(m.id, "trash")),
    S: () => withSelected((m) => moveTo(m.id, "junk")),
    u: () => undoLast(),
    z: () => withSelected((m) => snooze(m.id, "tomorrow")),
    "/": () => el("search").focus(),
    m: () => compose(null),
    r: () => replyTo("sender"),
    R: () => replyTo("all"),
    f: () => replyTo("forward"),
    "@": () => syncNow(),
    "?": () => { if (!el("help-window").open) el("help-window").showModal(); },
};

/**
 * How far a page moves: what is actually on screen, as the terminal does it.
 *
 * Measured from a real row rather than assumed, so it is right at any zoom
 * and with any font. One row of overlap, so the message you were looking at
 * when you pressed the key is still there to orient by.
 */
function pageRows() {
    const row = listEl.firstElementChild;
    const height = row?.getBoundingClientRect().height ?? 0;
    if (!height) return 10;
    return Math.max(1, Math.floor(listEl.clientHeight / height) - 1);
}

el("help").addEventListener("click", () => el("help-window").showModal());
el("help-close").addEventListener("click", () => el("help-window").close());

function move(delta) {
    const at = state.messages.findIndex((m) => m.id === state.selected?.id);
    const next = Math.min(Math.max((at < 0 ? 0 : at) + delta, 0), state.messages.length - 1);
    select(next, { pane: false });
}

function withSelected(run) {
    if (state.selected) run(state.selected);
}

// ---- moving between the panes (§7) ----
//
// Left and right walk across the three panes; up and down then act on
// whichever one you are in — folders, messages, or the message itself. The
// letters keep acting on the list from anywhere, so `j` and `d` do not change
// meaning depending on where the focus happens to be.

const PANE_RIGHT = { folders: "list", list: "reader", reader: "reader" };
const PANE_LEFT = { reader: "list", list: "folders", folders: "folders" };

/** The folder rows the arrows visit: groups and folders, as drawn, skipping
 *  anything inside a group that is folded shut. */
function folderRows() {
    return [...el("folder-view").querySelectorAll("summary, .folder")]
        .filter((node) => node.offsetParent !== null);
}

function focusPane(name) {
    if (!name) return;
    state.focusPane = name;
    // Narrow, the panes are a stack rather than a row, so moving to one has
    // to bring it on screen before anything in it can take focus.
    showPane(name);
    if (name === "folders") {
        const rows = folderRows();
        const at = rows.indexOf(document.activeElement);
        // Try from where the folder cursor was, and keep trying: a row that
        // was drawn a moment ago may have been replaced by a redraw landing
        // between the key and here, and focus falling through to the document
        // leaves the arrows doing nothing at all.
        for (const row of rows.slice(at >= 0 ? at : 0).concat(rows)) {
            row.focus();
            if (document.activeElement === row) return;
        }
        el("folder-view").focus?.();
    } else if (name === "list") {
        listEl.focus();
    } else {
        readerEl.focus();
    }
}

/** One step down or up, in whichever pane has the focus. */
function step(delta) {
    if (state.focusPane === "folders") {
        const rows = folderRows();
        const at = rows.indexOf(document.activeElement);
        const next = rows[Math.min(Math.max((at < 0 ? 0 : at) + delta, 0), rows.length - 1)];
        next?.focus();
        next?.scrollIntoView({ block: "nearest" });
        return;
    }
    if (state.focusPane === "reader") {
        // A line of the message, not a message: in the reader the thing being
        // moved through is the text.
        readerEl.scrollBy({ top: delta * 60 });
        return;
    }
    move(delta);
}

function page(direction) {
    if (state.focusPane === "reader") {
        readerEl.scrollBy({ top: direction * readerEl.clientHeight * 0.9 });
        return;
    }
    if (state.focusPane === "folders") {
        step(direction * 5);
        return;
    }
    move(direction * pageRows());
}

document.addEventListener("keydown", (event) => {
    // Nothing to drive until there is an account. The shortcuts act on a
    // listing and a selection, and neither exists on the setup page.
    if (document.body.dataset.state === "setup") return;

    // The compose window owns every key while it is open. A `d` typed into a
    // message must not delete the one behind it.
    if (composeWindow.open) {
        if (event.key === "Escape") composeWindow.close();
        // Ctrl/Cmd-Enter sends, which is what every mail client has taught
        // people to expect.
        if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
            event.preventDefault();
            sendDraft();
        }
        return;
    }

    // While a field has focus every key is text, or `/` could never be
    // searched for. `instanceof` because a key event can be aimed at the
    // document itself, which has no `matches`.
    const typing =
        event.target instanceof Element &&
        event.target.matches("input, textarea, select");
    if (event.key === "Escape" && !menuEl.hidden) {
        closeMenu();
        return;
    }
    if (event.key === "Escape") {
        if (typing) event.target.blur();
        else if (state.query) { el("search").value = ""; run(""); }
        return;
    }
    if (typing || event.metaKey || event.ctrlKey || event.altKey) return;

    const action = keys[event.key];
    if (action) {
        event.preventDefault();
        action();
    }
});

// ---- the right-click menu (§7) ----
//
// The desktop window's own menu is a browser's — back, forward, reload — for
// a program with no pages to go back to. This replaces it on the things that
// have actions, and names the key beside each one, so the menu teaches the
// keyboard rather than competing with it.
//
// It does not replace the menu on text. Selecting a passage and reaching for
// copy is the one thing the browser's menu is for, so the reader, the inputs
// and the compose window keep theirs.

const menuEl = el("context-menu");

/** What can be done to a message, and the key that does it. */
function messageMenu(message) {
    const seen = message.flags.includes("seen");
    const flagged = message.flags.includes("flagged");
    return [
        ["Open", "Enter", () => select(state.messages.indexOf(message))],
        [seen ? "Mark unread" : "Mark read", "s",
            () => flag(message.id, seen ? { remove: ["seen"] } : { add: ["seen"] })],
        [flagged ? "Unflag" : "Flag", "F",
            () => flag(message.id, flagged ? { remove: ["flagged"] } : { add: ["flagged"] })],
        null,
        ["Reply", "r", () => replyTo("sender", message)],
        ["Reply all", "R", () => replyTo("all", message)],
        ["Forward", "f", () => replyTo("forward", message)],
        null,
        ["Archive", "a", () => moveTo(message.id, "archive")],
        ["Move to trash", "d", () => moveTo(message.id, "trash")],
        ["Mark spam", "S", () => moveTo(message.id, "junk")],
        ["Snooze until tomorrow", "z", () => snooze(message.id, "tomorrow")],
    ];
}

/** What can be done to a folder. */
function folderMenu(button) {
    const scope = button.dataset.query;
    return [
        ["Show messages", "Space", () => button.click()],
        ["Sync this folder", "@", () => {
            button.click();
            syncNow(state.activeFolder
                ? { account: state.activeFolder.account, folders: [state.activeFolder.path] }
                : {});
        }],
        null,
        ["Copy search for this folder", "", () => navigator.clipboard?.writeText(scope)],
    ];
}

function openMenu(items, x, y) {
    menuEl.replaceChildren(...items.map((item) => {
        if (!item) {
            const rule = document.createElement("hr");
            return rule;
        }
        const [label, key, run] = item;
        const entry = document.createElement("button");
        entry.type = "button";
        entry.setAttribute("role", "menuitem");
        entry.append(span("label", label), span("key", key));
        entry.addEventListener("click", () => { closeMenu(); run(); });
        return entry;
    }));
    menuEl.hidden = false;
    // Placed after it is measurable, and nudged back on screen when the
    // pointer was near an edge.
    const box = menuEl.getBoundingClientRect();
    menuEl.style.left = `${Math.min(x, window.innerWidth - box.width - 8)}px`;
    menuEl.style.top = `${Math.min(y, window.innerHeight - box.height - 8)}px`;
    menuEl.querySelector("button")?.focus();
}

function closeMenu() {
    menuEl.hidden = true;
    menuEl.replaceChildren();
}

document.addEventListener("contextmenu", (event) => {
    const row = event.target.closest?.(".message");
    const folder = event.target.closest?.(".folder");
    if (row) {
        const message = state.messages[Number(row.dataset.index)];
        if (!message) return;
        event.preventDefault();
        openMenu(messageMenu(message), event.clientX, event.clientY);
        return;
    }
    if (folder) {
        event.preventDefault();
        openMenu(folderMenu(folder), event.clientX, event.clientY);
        return;
    }
    // Everywhere else the browser's menu is the right one — except in the
    // desktop window, where it offers to navigate a program that has nowhere
    // to navigate to. Text keeps its menu even there, since copy is the whole
    // reason anyone opens it.
    const text = event.target.closest?.("#reader, input, textarea, #compose-window");
    if (window.__MAILBOX_SHELL__ === "gui" && !text) event.preventDefault();
});

for (const name of ["click", "scroll", "blur"]) {
    window.addEventListener(name, () => { if (!menuEl.hidden) closeMenu(); }, true);
}

// ---- panes, on a screen too narrow for three ----

/// Narrow, the three panes are a stack rather than a row, and back walks up
/// it. The folders are the top: without this the list was the top, which left
/// the folder nav with nothing that could reach it and back on the list pane
/// with nowhere to go.
const PANE_ABOVE = { reader: "list", list: "folders" };

function showPane(name) {
    document.body.dataset.pane = name;
    const above = PANE_ABOVE[name];
    const back = el("back");
    back.hidden = !above;
    if (above) back.setAttribute("aria-label", `Back to the ${above}`);
}

el("back").addEventListener("click", () => {
    showPane(PANE_ABOVE[document.body.dataset.pane] ?? "folders");
});

// Clicking in a pane is the other way of choosing it, and the arrows should
// pick up where the mouse left off rather than somewhere else.
for (const [pane, node] of [["folders", el("folders")], ["list", el("list-pane")], ["reader", readerEl]]) {
    node.addEventListener("focusin", () => { state.focusPane = pane; });
    node.addEventListener("mousedown", () => { state.focusPane = pane; });
}

// ---- starting up ----

function run(query) {
    state.query = query;
    for (const button of el("folders").children) {
        button.classList.toggle("is-current", button.dataset.query === query);
    }
    drawFilters();
    reload();
}

/**
 * The filters, which are query terms wearing buttons (§6.2).
 *
 * Deliberately not a second kind of state. Pressing one edits the search box,
 * so there is one answer to "why am I seeing these messages" and it is
 * visible; and a term typed by hand lights its own button. It also means a
 * filter composes with a folder for free — `account:x in:INBOX is:unread` is
 * just a query.
 */
const FILTERS = [
    ["filter-unread", "is:unread"],
    ["filter-flagged", "is:flagged"],
    ["filter-attachment", "has:attachment"],
];

function queryTerms(text) {
    return text.trim().split(/\s+/).filter(Boolean);
}

function drawFilters() {
    const terms = queryTerms(el("search").value).map((t) => t.toLowerCase());
    for (const [id, token] of FILTERS) {
        const on = terms.includes(token);
        el(id).classList.toggle("is-selected", on);
        el(id).setAttribute("aria-pressed", on ? "true" : "false");
    }
}

function toggleFilter(token) {
    const terms = queryTerms(el("search").value);
    const at = terms.findIndex((t) => t.toLowerCase() === token);
    if (at >= 0) terms.splice(at, 1);
    else terms.push(token);
    const next = terms.join(" ");
    el("search").value = next;
    // A filter narrows what is already on screen, so the folder stays the
    // folder: `load-more` and the per-folder sync still know where they are.
    state.listLimit = 100;
    run(next);
}

for (const [id, token] of FILTERS) {
    el(id).addEventListener("click", () => toggleFilter(token));
}

el("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    state.activeFolder = null;
    el("load-more").hidden = true;
    run(el("search").value.trim());
    showPane("list");
    el("search").blur();
});

el("load-more").addEventListener("click", async () => {
    const scope = state.activeFolder;
    if (!scope || syncTask) return;
    state.listLimit += 100;
    el("load-more").disabled = true;
    try { await syncNow({ account: scope.account, folders: [scope.path] }); }
    finally { el("load-more").disabled = false; }
});

/** How often the window syncs by itself, once it is open. */
const BACKGROUND_SYNC_MS = 5 * 60 * 1000;

/** How often to ask the server's sync pass where it has got to. */
const PROGRESS_POLL_MS = 700;

let progressTimer = 0;

/**
 * Says which account and folder the server's pass is on, until it stops.
 *
 * The hosted backend syncs in this page and reports as it goes; a served
 * mailbox syncs inside one long request, so the only way to know is to ask on
 * another connection. A pass over several accounts is slow enough that a
 * spinner on its own is indistinguishable from a hang.
 */
async function pollProgress() {
    try {
        const at = await api("/api/sync/progress");
        if (at.running) {
            say(at.folder ? `syncing ${at.account} — ${at.folder}` : `syncing ${at.account}…`);
        }
    } catch {
        // Whatever went wrong, the sync's own reply will say it better.
    }
    progressTimer = setTimeout(pollProgress, PROGRESS_POLL_MS);
}

function stopPollingProgress() {
    clearTimeout(progressTimer);
    progressTimer = 0;
}

let syncTask;
let syncController;
/**
 * Runs a sync pass.
 *
 * `auto` marks the passes nobody asked for — the one on open and the ones on
 * the timer. Those report a failure into the status line rather than the error
 * line: a mailbox that cannot reach its server is worth saying once, quietly,
 * and not worth colouring red every five minutes on a laptop that is offline.
 */
function syncNow(args = {}, { auto = false } = {}) {
    if (syncTask) {
        return args.account ? syncTask.then(() => syncNow(args, { auto })) : syncTask;
    }
    syncController = new AbortController();
    el("sync").textContent = hostedBackend ? "Stop sync" : "Syncing…";
    el("sync").disabled = !hostedBackend;
    syncTask = (async () => {
        state.inFlight += 1;
        el("spinner").hidden = false;
        if (!hostedBackend) {
            say("syncing…");
            pollProgress();
        }
        try {
            const result = hostedBackend
                ? await hostedBackend.syncProgressively(args, async ({ account, folder, result }) => {
                    const added = result.synced.reduce((n, r) => n + r.messages_added, 0);
                    say(`loading ${account} / ${folder} — ${added} new messages`);
                    await Promise.all([reload(), loadFolders()]);
                }, syncController.signal)
                : await post("/api/sync", {});
            const added = (result.synced ?? []).reduce((n, r) => n + r.messages_added, 0);
            const errors = (result.errors ?? []).map((error) => `${error.account}: ${error.message}`);
            say(`${result.stopped ? "sync paused" : "synced"} — ${added} new message${added === 1 ? "" : "s"}` +
                (errors.length ? `; ${errors.join("; ")}` : ""));
            await Promise.all([reload(), loadFolders()]);
        } catch (error) {
            if (auto) say(`sync failed — ${error.message ?? error}`);
            else fail(error);
        } finally {
            stopPollingProgress();
            state.inFlight -= 1;
            el("spinner").hidden = state.inFlight === 0;
            el("sync").textContent = "Sync";
            el("sync").disabled = false;
        }
    })().finally(() => { syncTask = undefined; });
    return syncTask;
}

/**
 * Keeps the mailbox current while the window is open.
 *
 * Skipped while the page is hidden: a window left open for a week behind other
 * windows should not keep a connection to the mail server busy on its owner's
 * behalf, and it syncs on the way back anyway.
 */
function startBackgroundSync() {
    setInterval(() => {
        if (document.body.dataset.state !== "mail") return;
        if (document.hidden || syncTask) return;
        syncNow({}, { auto: true });
    }, BACKGROUND_SYNC_MS);
    document.addEventListener("visibilitychange", () => {
        if (!document.hidden && document.body.dataset.state === "mail" && !syncTask) {
            syncNow({}, { auto: true });
        }
    });
}

el("sync").addEventListener("click", () => {
    if (hostedBackend && syncTask) syncController.abort();
    else syncNow();
});

el("host").textContent = hostedBackend
    ? "browser store"
    : window.__MAILBOX_SHELL__ === "gui" ? "local store" : location.host;

/**
 * What the store is, on hover: where it is and how much of it there is.
 *
 * Asked for once the mailbox is up rather than drawn from the listing, because
 * the size is a property of the disk and not of the messages on screen.
 */
async function describeStore() {
    try {
        const status = await api("/api/status");
        const messages = (status.accounts ?? []).reduce((n, a) => n + a.messages, 0);
        const lines = [
            hostedBackend
                ? "Kept in this browser's storage, on this device only."
                : status.store_path ?? "On this machine.",
            `${size(status.store_bytes ?? 0)} on disk`,
            `${messages} message${messages === 1 ? "" : "s"} in ${
                (status.accounts ?? []).length} account${
                (status.accounts ?? []).length === 1 ? "" : "s"}`,
        ];
        el("host").title = lines.join("\n");
    } catch {
        // The status call is a nicety; a failure has already been reported
        // by whatever else needed the server.
    }
}

// Which build this is, at the end of the status bar. A screenshot of a bug
// carries the version with it this way, and the commit behind the tooltip is
// what turns "0.1.6" into a build somebody can check out.
el("version").textContent = pageConfig.dataset.version || "";

// The stamp is a button into the about box: the full build detail, the one
// link this client has, and — where the platform can — the update controls.
el("about-version").textContent = pageConfig.dataset.version || "";
el("about-build").textContent = pageConfig.dataset.build || "";
el("version").addEventListener("click", () => {
    if (!el("about-window").open) el("about-window").showModal();
});
el("about-close").addEventListener("click", () => el("about-window").close());

// ---- the first launch (§9) ----
//
// `serve` and `gui` start whether or not an account exists, so the page has
// two states and has to decide which before it asks for any mail. An empty
// mailbox and an unconfigured one look identical in a listing, and telling
// someone "No messages" when what is missing is the account is how a working
// program looks broken.
//
// The setup state is a form, not an instruction to go and type something: a
// browser tab and the desktop window are exactly the two places where there
// may be no terminal to type it in. It is the iOS setup screen's flow, over
// the same core calls.

/** Said in two places, and it is the thing people most want to know. */
const PASSWORD_NOTE = hostedBackend
    ? "The password is checked against the mail server, then stays in this browser — " +
      "it is never sent to the site host."
    : "The password is checked against the mail server before the account is " +
      "saved, and then goes to this machine's keychain — never into the config file.";

/** How often to look for an account added elsewhere, when this page cannot. */
const SETUP_POLL_MS = 2000;

/** Whether this server will take a password — loopback only. See `server.rs`. */
const setupAllowed = pageConfig.dataset.setup === "1";
el("setup-cancel").addEventListener("click", () => {
    el("setup-password").value = "";
    start();
});

let setupTimer = 0;

async function accountsExist() {
    const { accounts } = await api("/api/accounts");
    return (accounts ?? []).length > 0;
}

/** Loads the mailbox, or shows the setup card and waits for an account. */
async function start() {
    let configured;
    try {
        configured = await accountsExist();
    } catch (e) {
        // A failure here is the token or the server, not the config; say so
        // and let the mailbox load, where the same error will be legible.
        fail(e);
        configured = true;
    }

    if (!configured) {
        el("setup-cancel").hidden = true;
        showSetup();
        return;
    }

    clearTimeout(setupTimer);
    document.body.dataset.state = "mail";
    el("setup").hidden = true;
    showPane(document.body.dataset.pane || "list");
    loadFolders();
    loadAccounts();
    loadIdentities();
    reload();

    describeStore();

    // Mail that arrived while the window was shut is the mail someone opened
    // it for, so the pass starts with the window rather than waiting to be
    // asked. It runs behind the listing, which draws from the store and does
    // not wait for the network.
    syncNow({}, { auto: true });
    if (!backgroundSyncStarted) {
        backgroundSyncStarted = true;
        startBackgroundSync();
    }
}

/** `start()` runs again after setup, and the timer is wanted only once. */
let backgroundSyncStarted = false;

function showSetup() {
    document.body.dataset.state = "setup";
    el("setup").hidden = false;
    el("setup-form").hidden = !setupAllowed;
    el("setup-guide").hidden = setupAllowed;
    say("");
    if (setupAllowed) {
        updatePlaceholders();
        el("setup-address").focus();
        return;
    }
    // Nothing to do here but wait for the account to be added elsewhere, so
    // watch for it: coming back to find the mail already there is the whole
    // point of leaving the page open.
    clearTimeout(setupTimer);
    setupTimer = setTimeout(pollForAccount, SETUP_POLL_MS);
}

async function pollForAccount() {
    try {
        if (await accountsExist()) {
            await start();
            return;
        }
    } catch (e) {
        setupError(e);
    }
    setupTimer = setTimeout(pollForAccount, SETUP_POLL_MS);
}

function setupError(error) {
    const box = el("setup-error");
    box.textContent = String(error?.message ?? error ?? "");
    box.hidden = !box.textContent;
}

// ---- where the servers come from ----
//
// Two lookups, as on the phone. The built-in table is instant and offline, so
// it runs as the address is typed and folds the server fields away when it
// recognises the domain. Discovery talks to a nameserver, so it waits behind a
// button — and says where each answer came from, because a `SRV` record and a
// guess at `imap.<domain>` deserve different amounts of trust (§9).

let providerTimer = 0;
let providerKnown = false;

const domainOf = (address) => address.split("@")[1] ?? "";

/** What an empty host field means: discovery's answer, else the usual name. */
function placeholderHost(service) {
    const domain = domainOf(el("setup-address").value.trim());
    return domain ? `${service}.${domain}` : `${service}.example.com`;
}

el("setup-address").addEventListener("input", () => {
    updatePlaceholders();
    clearTimeout(providerTimer);
    providerTimer = setTimeout(lookupProvider, 300);
});

function updatePlaceholders() {
    el("setup-imap-host").placeholder = placeholderHost("imap");
    el("setup-smtp-host").placeholder = placeholderHost("smtp");
    const address = el("setup-address").value.trim();
    for (const id of ["setup-imap-user", "setup-smtp-user"]) {
        el(id).placeholder = address || "Your address";
    }
    el("setup-discover").disabled = !domainOf(address).includes(".");
}

/** The instant, offline table lookup, on every keystroke. */
async function lookupProvider() {
    const address = el("setup-address").value.trim();
    if (!address.includes("@")) {
        providerKnown = false;
        return;
    }

    let hints;
    try {
        hints = await api(`/api/providers?address=${encodeURIComponent(address)}`);
    } catch {
        // The table is a convenience. Failing to reach it is not a reason to
        // stop someone typing their own servers in.
        return;
    }
    if (address !== el("setup-address").value.trim()) return; // Typed on since.

    providerKnown = hints.known === true;
    if (!providerKnown) return;
    apply(hints);
    // Nothing left to fill in, so nothing left to show.
    showServers(false);
}

/** The network lookup, behind the button. */
async function discoverServers() {
    const address = el("setup-address").value.trim();
    const button = el("setup-discover");
    const note = el("setup-discover-note");

    button.disabled = true;
    note.textContent = `Looking up ${domainOf(address)}…`;
    try {
        apply(await api(`/api/discover?address=${encodeURIComponent(address)}`));
    } catch (e) {
        note.textContent = "";
        setupError(e);
    } finally {
        button.disabled = false;
        updatePlaceholders();
    }
}

el("setup-discover").addEventListener("click", () => discoverServers());

/** Where a set of settings came from, in the words the phone uses. */
function provenance(hints) {
    switch (hints.imap_source) {
        case "table": return "Server settings for this provider are built in.";
        case "autoconfig": return "Found in the domain's autoconfiguration.";
        case "well_known": return "Found in the domain's published autoconfiguration.";
        case "ispdb": return "Found in the Mozilla provider database.";
        case "autodiscover": return "Found through the domain's Exchange autodiscovery.";
        case "srv": return "Found in the domain's own DNS records.";
        case "mx": return "Guessed from the domain's mail exchanger — check them.";
        case "guess": return "No settings published; these are the usual names — check them.";
        default: return "";
    }
}

/// Fills in what a lookup is confident about, leaving typed-in values alone.
///
/// The security of each host rides along in a field of its own: it is not
/// something to ask anyone to choose, and dropping it would quietly downgrade
/// a STARTTLS server to the implicit-TLS default.
let security = { imap: null, smtp: null };

function apply(hints) {
    if (hints.imap_host && (!el("setup-imap-host").value || !providerKnown)) {
        el("setup-imap-host").value = hints.imap_host;
    }
    if (hints.imap_port) el("setup-imap-port").value = String(hints.imap_port);
    if (hints.imap_user && !el("setup-imap-user").value) {
        el("setup-imap-user").value = hints.imap_user;
    }
    if (hints.smtp_host && (!el("setup-smtp-host").value || !providerKnown)) {
        el("setup-smtp-host").value = hints.smtp_host;
    }
    if (hints.smtp_port) el("setup-smtp-port").value = String(hints.smtp_port);
    if (hints.smtp_user && !el("setup-smtp-user").value) {
        el("setup-smtp-user").value = hints.smtp_user;
    }
    security = {
        imap: hints.imap_security ?? security.imap,
        smtp: hints.smtp_security ?? security.smtp,
    };

    const said = provenance(hints);
    el("setup-hint").textContent = said ? `${said} ${PASSWORD_NOTE}` : PASSWORD_NOTE;
    el("setup-discover-note").textContent = "";
}

function showServers(shown) {
    el("setup-servers").hidden = !shown;
    el("setup-servers-toggle").textContent = shown ? "Hide servers" : "Servers";
}

el("setup-servers-toggle").addEventListener("click", () => {
    showServers(el("setup-servers").hidden);
});

// One login for both servers is the overwhelmingly common case, so it is the
// default and the SMTP field appears only when it is turned off.
el("setup-same-user").addEventListener("change", () => {
    el("setup-smtp-user-field").hidden = el("setup-same-user").checked;
});

const oauthProviders = {
    google: { auth: "https://accounts.google.com/o/oauth2/v2/auth", token: "https://oauth2.googleapis.com/token", scope: "https://mail.google.com/" },
    microsoft: { auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize", token: "https://login.microsoftonline.com/common/oauth2/v2.0/token", scope: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send" },
    yahoo: { auth: "https://api.login.yahoo.com/oauth2/request_auth", token: "https://api.login.yahoo.com/oauth2/get_token", scope: "mail-w" },
};
function base64url(bytes) { let s = ""; for (const byte of bytes) s += String.fromCharCode(byte); return btoa(s).replaceAll("+", "-").replaceAll("/", "_").replaceAll("=", ""); }
async function startOAuth() {
    const client = el("setup-oauth-client").value.trim();
    const provider = oauthProviders[el("setup-oauth-provider").value];
    if (!client) { el("setup-oauth-note").textContent = "Enter the OAuth client ID first."; return; }
    const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
    const challenge = base64url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))));
    const state = base64url(crypto.getRandomValues(new Uint8Array(16)));
    const redirect = `${location.origin}${location.pathname}`;
    sessionStorage.setItem("mailbox-oauth", JSON.stringify({ verifier, state, provider: el("setup-oauth-provider").value, client, redirect }));
    const url = new URL(provider.auth);
    url.search = new URLSearchParams({ response_type: "code", client_id: client, redirect_uri: redirect, scope: provider.scope, code_challenge: challenge, code_challenge_method: "S256", state, access_type: "offline", prompt: "consent" });
    location.assign(url);
}
async function finishOAuth() {
    const params = new URLSearchParams(location.search), pending = JSON.parse(sessionStorage.getItem("mailbox-oauth") || "null");
    if (!pending || !params.get("code")) return;
    sessionStorage.removeItem("mailbox-oauth"); history.replaceState({}, "", location.pathname);
    if (params.get("state") !== pending.state) throw new Error("OAuth state did not match; sign-in was cancelled.");
    const provider = oauthProviders[pending.provider];
    const response = await fetch(provider.token, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ grant_type: "authorization_code", code: params.get("code"), client_id: pending.client, redirect_uri: pending.redirect, code_verifier: pending.verifier }) });
    const result = await response.json();
    if (!response.ok || !result.access_token) throw new Error(result.error_description || result.error || "OAuth token exchange failed.");
    el("setup-auth").value = "oauth"; el("setup-auth").dispatchEvent(new Event("change"));
    el("setup-oauth-provider").value = pending.provider; el("setup-oauth-client").value = pending.client;
    el("setup-password").value = result.access_token;
    el("setup-oauth-note").textContent = "Signed in. Add the account to finish setup.";
}
el("setup-auth").addEventListener("change", () => {
    const oauth = el("setup-auth").value === "oauth";
    el("setup-oauth-fields").hidden = !oauth;
    el("setup-password").required = !oauth;
    el("setup-password").closest(".setup-field").querySelector("label").textContent = oauth ? "Access token" : "Password";
});
el("setup-oauth-login").addEventListener("click", () => startOAuth().catch(setupError));
finishOAuth().catch(setupError);

// ---- testing, and adding ----

/// The account as the fields currently describe it.
///
/// One reading for both buttons: Test has to send exactly what Add would, or
/// it is testing something other than what it is about to save.
function accountFromForm() {
    const value = (id) => el(id).value.trim();
    const port = (id, fallback) => Number(value(id) || fallback);
    const user = value("setup-imap-user");

    return {
        id: value("setup-id"),
        address: value("setup-address"),
        password: el("setup-password").value,
        // An empty field means the placeholder, so the two are resolved
        // together here rather than sending a host with no port.
        imap_host: value("setup-imap-host") || placeholderHost("imap"),
        imap_port: port("setup-imap-port", 993),
        imap_user: user,
        imap_security: security.imap,
        smtp_host: value("setup-smtp-host") || placeholderHost("smtp"),
        smtp_port: port("setup-smtp-port", 587),
        smtp_user: el("setup-same-user").checked ? user : value("setup-smtp-user"),
        smtp_security: security.smtp,
        oauth_provider: el("setup-auth").value === "oauth" ? el("setup-oauth-provider").value : undefined,
        oauth_client_id: el("setup-auth").value === "oauth" ? value("setup-oauth-client") : undefined,
    };
}

/** Both buttons make a connection, so neither may run twice or at once. */
function setupBusy(busy, button, label) {
    el("setup-add").disabled = busy;
    el("setup-test").disabled = busy;
    el("setup-cancel").disabled = busy;
    button.textContent = busy ? label : button.dataset.label;
}

for (const id of ["setup-add", "setup-test"]) {
    el(id).dataset.label = el(id).textContent;
}

el("setup-test").addEventListener("click", () => testAccount());

/// Connects and logs in, and writes nothing.
///
/// The answer says what the server offers, because "it worked" is the least
/// useful true thing to report: the folder count says the login reached the
/// right mailbox, and the sync path is the answer to "why is my mail slow"
/// before it has been asked (§5.3).
async function testAccount() {
    const button = el("setup-test");
    setupBusy(true, button, "Testing…");
    setupError("");
    el("setup-status").textContent = "";
    try {
        const result = await post("/api/accounts/test", accountFromForm());
        el("setup-status").textContent =
            `Logged in to ${result.server} as ${result.login_user} — ` +
            `${result.folders} folder${result.folders === 1 ? "" : "s"}, ` +
            `syncs by ${result.sync_path}.`;
    } catch (e) {
        setupError(e);
        // Whatever the server objected to, the answer is in these fields.
        showServers(true);
    } finally {
        setupBusy(false, button);
    }
}

el("setup-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    await addAccount();
});

async function addAccount() {
    const button = el("setup-add");

    setupBusy(true, button, "Checking…");
    setupError("");
    el("setup-status").textContent = "";
    say("");
    try {
        const added = await post("/api/accounts", accountFromForm());

        // Out of the DOM as soon as it has been used. It is not needed again,
        // and a password sitting in a field is one a screen or a form restore
        // can hand to whoever is there next.
        el("setup-password").value = "";

        await start();
        say(`added ${added.address} — ${added.credential ?? "no credential"}`);
        // The mailbox is open and empty until something fetches; nobody should
        // have to know to press Sync on their first minute.
        syncNow({ account: added.id });
    } catch (e) {
        setupError(e);
        // Whatever the server objected to, the answer is in these fields.
        showServers(true);
    } finally {
        setupBusy(false, button);
    }
}

// ---- updating the desktop app ----
//
// The packaged Linux app asks the Flatpak portal for updates; the shell holds
// that state and hands it over through Tauri's IPC. In a browser, in a
// non-Flatpak build, and before anything newer exists, this draws nothing:
// an "up to date" line nobody asked about is noise in a status bar.

const shellIpc = window.__MAILBOX_SHELL__ === "gui" ? window.__TAURI__?.core : null;

const UPDATE_COPY = {
    available: ["A new version is available", "Install"],
    ready: ["The new version is installed", "Restart"],
};

function drawUpdate(status) {
    const box = el("update");
    const action = el("update-action");
    const [text, button] = UPDATE_COPY[status.state] ?? [null, null];
    box.classList.toggle("is-failed", status.state === "failed");

    if (status.state === "installing") {
        el("update-text").textContent = `Updating… ${status.percent}%`;
        action.hidden = true;
    } else if (status.state === "failed") {
        el("update-text").textContent = `Update failed: ${status.message}`;
        action.textContent = "Try again";
        action.hidden = false;
    } else if (text) {
        el("update-text").textContent = text;
        action.textContent = button;
        action.hidden = false;
    } else {
        box.hidden = true;
        return;
    }
    action.dataset.state = status.state;
    box.hidden = false;
}

/**
 * The same update state, in the about box.
 *
 * The button does whatever there is to do: install what the portal found,
 * restart into what it installed, or — when there is nothing — ask again and
 * say so. Only the portal actually decides whether a build is newer (see
 * mailbox-ui's update.rs); this never claims to have checked by itself.
 */
function drawAboutUpdate(status) {
    const box = el("about-update");
    const check = el("about-check");
    const note = el("about-update-note");
    if (!shellIpc || status.state === "unsupported") {
        box.hidden = true;
        return;
    }
    box.hidden = false;
    check.dataset.state = status.state;
    check.disabled = status.state === "installing";
    if (status.state === "available") {
        check.textContent = "Install update";
        note.textContent = "A new version is available.";
    } else if (status.state === "ready") {
        check.textContent = "Restart";
        note.textContent = "The new version is installed.";
    } else if (status.state === "installing") {
        check.textContent = "Installing…";
        note.textContent = `${status.percent}%`;
    } else if (status.state === "failed") {
        check.textContent = "Try again";
        note.textContent = `Update failed: ${status.message}`;
    } else {
        check.textContent = "Check for updates";
        note.textContent = "";
    }
}

async function aboutCheck() {
    const state = el("about-check").dataset.state;
    if (state === "available" || state === "ready" || state === "failed") {
        await updateAction();
        return;
    }
    // Nothing pending: ask the portal where it has got to and say so. It
    // watches on its own, so this reports rather than triggers.
    try {
        const status = await shellIpc.invoke("update_status");
        drawAboutUpdate(status);
        if (status.state === "current") {
            el("about-update-note").textContent = "This is the newest version the remote offers.";
        }
    } catch (e) {
        el("about-update-note").textContent = String(e.message ?? e);
    }
}

async function updateAction() {
    const action = el("update-action");
    if (action.dataset.state === "ready") {
        // Restarting closes this window, and an open compose window is the
        // only copy of what is in it.
        if (composeWindow.open) {
            fail(new Error("send or discard the message before restarting"));
            return;
        }
        action.disabled = true;
        try {
            await shellIpc.invoke("update_restart");
        } catch (e) {
            fail(e);
            action.disabled = false;
        }
        return;
    }
    await shellIpc.invoke("update_install").catch(fail);
}

function showUpdate(status) {
    drawUpdate(status);
    drawAboutUpdate(status);
}

if (shellIpc) {
    window.addEventListener("mailbox-update", (event) => showUpdate(event.detail));
    el("update-action").addEventListener("click", () => updateAction());
    el("about-check").addEventListener("click", () => aboutCheck());
    shellIpc.invoke("update_status").then(showUpdate, () => {});
}

start();
