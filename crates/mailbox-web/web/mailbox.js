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

const token = document.getElementById("config").dataset.token;

const el = (id) => document.getElementById(id);
const listEl = el("list");
const readerEl = el("reader");

const state = {
    query: "",
    messages: [],
    selected: null,
    /// The listing request whose answer is still wanted. Type three characters
    /// quickly and the answers can arrive in any order; painting a stale one is
    /// how a search box appears to ignore what was typed.
    listSeq: 0,
    openSeq: 0,
    inFlight: 0,
    /// The pending "this has been read" timer, cancelled when the cursor moves.
    readTimer: 0,
};

// ---- talking to the server ----

async function api(path, options = {}) {
    const headers = { "X-Mailbox-Token": token };
    if (options.body) headers["Content-Type"] = "application/json";

    state.inFlight += 1;
    el("spinner").hidden = false;
    try {
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
        const params = new URLSearchParams({ q: state.query, limit: "200" });
        const listing = await api(`/api/ls?${params}`);
        if (seq !== state.listSeq) return; // A newer question has been asked.
        el("error").textContent = "";

        const previous = state.selected?.id;
        state.messages = listing.messages ?? [];
        drawList();

        const same = state.messages.findIndex((m) => m.id === previous);
        // Keep the cursor on the same message where one exists, so a refresh
        // does not move the selection out from under someone mid-read.
        if (state.messages.length) select(same >= 0 ? same : 0, { pane: false });
        else clearReader();
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

        row.addEventListener("click", () => select(index));
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

async function select(index, { pane = true } = {}) {
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
        const shown = await api(`/api/show?id=${encodeURIComponent(message.id)}&format=html`);
        if (seq !== state.openSeq) return;
        drawReader(shown.message);
        startReadDwell(message, seq);
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

    state.readTimer = setTimeout(async () => {
        // Still the same message. Moving on cancels the timer, but a reply
        // that arrived late could have moved the cursor since.
        if (seq !== state.openSeq) return;
        try {
            await post("/api/flag", { ids: [message.id], add: ["seen"] });
            // The row, not the whole listing: reloading here would move things
            // under someone who is in the middle of reading.
            message.flags = [...message.flags, "seen"];
            for (const row of listEl.children) {
                if (state.messages[Number(row.dataset.index)]?.id === message.id) {
                    row.classList.remove("unread");
                }
            }
        } catch (e) {
            fail(e);
        }
    }, READ_DWELL_MS);
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
    change(() => post("/api/flag", { ids: [id], ...spec }), "changed");

const moveTo = (id, folder) =>
    change(() => post("/api/move", { ids: [id], folder }), `moved to ${folder}`);

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
        const nav = el("folders");
        // The first button *is* the inbox, so listing the inbox folder beside
        // it would be the same place named twice.
        const rest = folders.filter((f) => f.special_use !== "inbox");
        nav.replaceChildren(
            folderButton("Inbox", "", inboxUnread(folders)),
            ...rest.map((f) =>
                folderButton(f.name, `in:${f.path.includes(" ") ? `"${f.path}"` : f.path}`, f.unread)),
        );
    } catch (e) {
        fail(e);
    }
}

function inboxUnread(folders) {
    return folders
        .filter((f) => f.special_use === "inbox")
        .reduce((n, f) => n + f.unread, 0);
}

function folderButton(label, query, unread = 0) {
    const button = document.createElement("button");
    button.className = "folder" + (state.query === query ? " is-current" : "");
    button.dataset.query = query;
    button.append(span("name", label));
    if (unread) button.append(span("count", String(unread)));
    button.addEventListener("click", () => {
        el("search").value = query;
        run(query);
        showPane("list");
    });
    return button;
}

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

async function replyTo(mode) {
    if (!state.selected) return;
    try {
        const params = new URLSearchParams({ id: state.selected.id, mode });
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

const keys = {
    j: () => move(1),
    ArrowDown: () => move(1),
    k: () => move(-1),
    ArrowUp: () => move(-1),
    u: () => withSelected((m) => flag(m.id, m.flags.includes("seen")
        ? { remove: ["seen"] } : { add: ["seen"] })),
    f: () => withSelected((m) => flag(m.id, m.flags.includes("flagged")
        ? { remove: ["flagged"] } : { add: ["flagged"] })),
    s: () => withSelected((m) => moveTo(m.id, "archive")),
    d: () => withSelected((m) => moveTo(m.id, "trash")),
    z: () => withSelected((m) => snooze(m.id, "tomorrow")),
    "/": () => el("search").focus(),
    m: () => compose(null),
    r: () => replyTo("sender"),
    R: () => replyTo("all"),
    // `f` is flag in the terminal, so forwarding takes the next letter along
    // rather than stealing it.
    F: () => replyTo("forward"),
    // Sync has a button; `g` is the convenience, and `r` belongs to reply
    // the way it does in the terminal.
    g: () => syncNow(),
};

function move(delta) {
    const at = state.messages.findIndex((m) => m.id === state.selected?.id);
    const next = Math.min(Math.max((at < 0 ? 0 : at) + delta, 0), state.messages.length - 1);
    select(next, { pane: false });
}

function withSelected(run) {
    if (state.selected) run(state.selected);
}

document.addEventListener("keydown", (event) => {
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

// ---- panes, on a screen too narrow for three ----

function showPane(name) {
    document.body.dataset.pane = name;
    el("back").hidden = name === "list";
}

el("back").addEventListener("click", () => showPane("list"));

// ---- starting up ----

function run(query) {
    state.query = query;
    for (const button of el("folders").children) {
        button.classList.toggle("is-current", button.dataset.query === query);
    }
    reload();
}

el("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(el("search").value.trim());
    el("search").blur();
});

function syncNow() {
    return change(async () => {
        const result = await post("/api/sync", {});
        const added = (result.synced ?? []).reduce((n, r) => n + r.messages_added, 0);
        say(`synced — ${added} new message${added === 1 ? "" : "s"}`);
    }, "synced");
}

el("sync").addEventListener("click", () => syncNow());

el("host").textContent =
    window.__MAILBOX_SHELL__ === "gui" ? "local store" : location.host;

loadFolders();
loadIdentities();
reload();
