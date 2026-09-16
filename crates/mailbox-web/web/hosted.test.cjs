const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");

function client(relay, account, options = {}) {
    const urls = [];
    const fetches = [];
    const requests = [];
    let settings = { config: { accounts: options.accounts ?? { test: account } }, remoteFolders: options.remoteFolders };
    const credentials = { ...options.credentials };
    const checkpoints = { ...options.checkpoints };
    class Worker {
        listeners = {};
        addEventListener(name, handler) { this.listeners[name] = handler; }
        postMessage(request) {
            requests.push(request);
            if (request.method === "ensure_body" && options.blockBody) {
                options.onBlocked();
                return;
            }
            const spec = JSON.parse(request.args);
            const id = spec.id || spec.address?.split("@")[0] || "test";
            const accounts = request.config ? JSON.parse(request.config).accounts : {};
            if (request.method === "sync" && options.fail?.includes(Object.keys(accounts)[0])) {
                queueMicrotask(() => this.listeners.message({ data: { ok: false, error: "sync failed" } }));
                return;
            }
            let result = request.operation === "prepare"
                ? { id, config: { accounts: { [id]: spec } } }
                : request.method === "sync"
                    ? { synced: Object.keys(accounts).map((account) => ({ account, messages_added: 1 })) }
                    : request.method === "accounts"
                        ? { accounts: Object.keys(accounts).map((id) => ({ id })) }
                        : request.method === "folders"
                            ? { folders: options.folders ?? [] }
                            : request.method === "remote_folders"
                                ? { folders: (options.folders ?? []).filter(folder => Object.hasOwn(accounts, folder.account)) }
                                : request.method === "move" ? { changed: 1 }
                            : { ok: true };
            if (request.method === "sync" && options.syncReply) result = options.syncReply(spec);
            if (options.reply) result = options.reply(request, result);
            const snapshots = request.persist ? Object.fromEntries(Object.keys(accounts).map((id) => [id, new Uint8Array([requests.length])])) : {};
            queueMicrotask(() => this.listeners.message({ data: { ok: true, result, snapshots } }));
        }
        terminate() {}
    }
    class WebSocket {
        constructor(url) { urls.push(url); }
        addEventListener(name, handler) {
            if (name === "open") queueMicrotask(handler);
        }
        close() {}
    }
    const source = fs.readFileSync(`${__dirname}/hosted.js`, "utf8")
        .replace(/^import[\s\S]*?from "\.\/storage.js";/, "")
        .replaceAll("export ", "");
    const context = vm.createContext({
        URL, Worker, WebSocket, SharedArrayBuffer, Atomics, Int32Array, Uint8Array,
        setInterval, clearInterval, queueMicrotask, crossOriginIsolated: true,
        location: { href: "https://mail.example/" },
        fetch: async (input) => {
            const url = new URL(String(input), "https://mail.example/");
            fetches.push(url.href);
            if (url.pathname === "/_discover") {
                const result = options.discovery ?? {
                    known: true, imap_host: "imap.discovered.example", imap_port: 993,
                    imap_security: "implicit", smtp_host: "smtp.discovered.example",
                    smtp_port: 587, smtp_security: "starttls", imap_source: "srv",
                    smtp_source: "srv", authoritative: true,
                };
                return { ok: !result.error, status: result.error ? 400 : 200,
                    json: async () => result };
            }
            return { ok: true, status: 200, json: async () => ({ relay }) };
        },
        loadSettings: async () => settings,
        saveSettings: async (value) => { settings = value; },
        loadCredential: async (id) => credentials[id],
        saveCredential: async (id, password) => { credentials[id] = password; },
        loadCheckpoint: async (id) => checkpoints[id],
        saveCheckpoint: async (snapshot, id) => { checkpoints[id] = snapshot; },
    });
    vm.runInContext(source, context);
    return { api: context.api, syncProgressively: context.syncProgressively,
        hostedImageUrl: context.hostedImageUrl,
        urls, fetches, requests, credentials, checkpoints, settings: () => settings };
}

test("hosted remote images use the HTTP endpoint on the configured relay", async () => {
    const c = client("wss://relay.example/imap", accounts?.home);
    await c.api("/api/status");
    assert.equal(
        c.hostedImageUrl("https://claude.ai/images/email/social_x.png"),
        "https://relay.example/_image?url=https%3A%2F%2Fclaude.ai%2Fimages%2Femail%2Fsocial_x.png",
    );
});

test("hosted server discovery uses the relay HTTP endpoint", async () => {
    const c = client("wss://relay.example/account", accounts?.home);
    const result = await c.api("/api/discover?address=me%40custom.example");
    assert.equal(result.imap_host, "imap.discovered.example");
    assert.equal(
        c.fetches.find(url => url.includes("/_discover")),
        "https://relay.example/_discover?address=me%40custom.example",
    );
});

test("one site routes account tests and sync to each account destination", async () => {
    const c = client("wss://relay.example/", { imap_host: "imap.one.example", imap_port: 993 });
    for (const spec of [
        { imap_host: "imap.one.example", imap_port: 993 },
        { imap_host: "imap.two.example", imap_security: "starttls" },
    ]) {
        await c.api("/api/accounts/test", { method: "POST", body: JSON.stringify(spec) });
    }
    await c.api("/api/sync", { method: "POST", body: "{}" });
    assert.deepEqual(c.urls, [
        "wss://relay.example/?host=imap.one.example&port=993",
        "wss://relay.example/?host=imap.two.example&port=143",
        "wss://relay.example/?host=imap.one.example&port=993",
    ]);
});

test("named relay URLs retain their fixed route", async () => {
    const c = client("wss://relay.example/one", { imap_host: "imap.one.example" });
    await c.api("/api/sync", { method: "POST", body: "{}" });
    assert.deepEqual(c.urls, ["wss://relay.example/one"]);
});

const accounts = {
    home: { address: "me@one.example", imap_host: "imap.one.example", imap_port: 993 },
    work: { address: "me@two.example", imap_host: "imap.two.example", imap_port: 993 },
};

test("multiple accounts restore together and sync with separate credentials and checkpoints", async () => {
    const c = client("wss://relay.example/", undefined, {
        accounts, credentials: { home: "home-password", work: "work-password" },
        checkpoints: { home: new Uint8Array([10]), work: new Uint8Array([20]) },
    });
    await c.api("/api/ls");
    assert.deepEqual(Object.keys(JSON.parse(c.requests[0].config).accounts), ["home", "work"]);
    const result = await c.api("/api/sync", { method: "POST", body: "{}" });
    assert.deepEqual(Array.from(result.synced, (item) => item.account), ["home", "work"]);
    for (const [index, id] of ["home", "work"].entries()) {
        const request = c.requests[index + 1];
        assert.deepEqual(Object.keys(JSON.parse(request.config).accounts), [id]);
        assert.deepEqual(Object.keys(request.credentials), [id]);
        assert.equal(request.credentials[id], `${id}-password`);
        assert.deepEqual(Object.keys(request.checkpoints), [id]);
    }
    assert.notDeepEqual(c.checkpoints.home, c.checkpoints.work);
    assert.equal(new URL(c.urls[0]).searchParams.get("host"), "imap.one.example");
    assert.equal(new URL(c.urls[1]).searchParams.get("host"), "imap.two.example");
});

test("adding an account retains existing mail and rejects duplicate IDs", async () => {
    const image = new Uint8Array([10]);
    const c = client("wss://relay.example/", undefined, {
        accounts: { home: accounts.home }, credentials: { home: "old-password" },
        checkpoints: { home: image },
    });
    const options = { method: "POST", body: JSON.stringify({ id: "work", ...accounts.work, password: "new-password" }) };
    await c.api("/api/accounts", options);
    assert.deepEqual(Object.keys(c.settings().config.accounts), ["home", "work"]);
    assert.equal(c.credentials.home, "old-password");
    assert.equal(c.credentials.work, "new-password");
    assert.equal(c.checkpoints.home, image);
    await assert.rejects(c.api("/api/accounts", options), /already exists/);
});

test("a conflicting generated account id falls back to the email address", async () => {
    const c = client("wss://relay.example/", undefined, {
        accounts: { peter: { address: "peter@one.example", imap_host: "imap.one.example" } },
    });
    await c.api("/api/accounts", { method: "POST", body: JSON.stringify({
        address: "peter@two.example", password: "new-password",
        imap_host: "imap.two.example", smtp_host: "smtp.two.example",
    }) });
    assert.deepEqual(Object.keys(c.settings().config.accounts), ["peter", "peter@two.example"]);
    assert.equal(c.credentials["peter@two.example"], "new-password");
});

test("one failed account does not stop sync of the others", async () => {
    const c = client("wss://relay.example/", undefined, { accounts, fail: ["home"] });
    const result = await c.api("/api/sync", { method: "POST", body: "{}" });
    assert.equal(result.synced[0].account, "work");
    assert.equal(result.errors[0].account, "home");
    assert.equal(c.urls.length, 2);
    const failed = client("wss://relay.example/", undefined, { accounts, fail: ["home", "work"] });
    await assert.rejects(failed.api("/api/sync", { method: "POST", body: "{}" }), /home: sync failed/);
});

test("overlapping calls load the checkpoint committed by the preceding call", async () => {
    const c = client("wss://relay.example/", accounts.home);
    await Promise.all([c.api("/api/flag", { method: "POST", body: '{"ids":["mail"],"add":["seen"]}' }), c.api("/api/status")]);
    assert.equal(c.requests[1].checkpoints.test[0], 1);
});

test("lazy sync loads one inbox batch per account and counts other folders without backfilling them", async () => {
    const counts = new Map();
    const c = client("wss://relay.example/", undefined, {
        accounts,
        folders: [
            { account: "home", path: "INBOX", selectable: true },
            { account: "work", path: "INBOX", selectable: true },
            { account: "home", path: "Archive", selectable: true },
            { account: "home", path: "Container", selectable: false },
        ],
        syncReply: (spec) => {
            const key = `${spec.account}/${spec.folders[0]}`;
            const count = (counts.get(key) ?? 0) + 1;
            counts.set(key, count);
            return { synced: [{ account: spec.account, messages_added: 100,
                bodies_fetched: 5, folders_failed: 0 }],
                coverage: { backfilling: count === 1 ? [{ folder: spec.folders[0] }] : [] } };
        },
    });
    const visible = [];
    await c.syncProgressively({}, async ({ account, folder }) => {
        visible.push(`${account}/${folder}`);
        await c.api("/api/ls");
    });
    assert.deepEqual([...new Set(visible)], ["home/INBOX", "work/INBOX"]);
    assert.deepEqual(Object.fromEntries(counts), { "home/INBOX": 1, "work/INBOX": 1 });
    const syncs = c.requests.filter(request => request.method === "sync");
    assert.equal(JSON.parse(syncs[0].args).max_headers, 100);
    assert.equal(syncs.length, 2);
    assert.equal(JSON.parse(syncs[0].args).max_prefetch, 0);
    assert.equal(JSON.parse(syncs[1].args).max_prefetch, 0);
    const first = c.requests.findIndex(request => request.method === "sync");
    assert.equal(c.requests[first + 1].method, "ls");
    assert.equal(c.requests[first + 1].checkpoints.home[0], first + 1);
    assert.equal(c.requests.filter(request => request.method === "remote_folders").length, 2);
});

test("progressive sync can stop after a committed batch and resume on another run", async () => {
    const c = client("wss://relay.example/", undefined, { accounts });
    const signal = { aborted: false };
    const stopped = await c.syncProgressively({}, async () => { signal.aborted = true; }, signal);
    assert.equal(stopped.stopped, true);
    assert.equal(c.requests.filter(request => request.method === "sync").length, 1);
    assert(c.checkpoints.home);
    await c.syncProgressively({ account: "work" });
    assert.equal(new URL(c.urls[1]).searchParams.get("host"), "imap.two.example");
});

test("lazy sync does not retry failed folders or continue backfilling partial folders", async () => {
    const c = client("wss://relay.example/", undefined, { accounts, fail: ["home"] });
    const result = await c.syncProgressively();
    assert.equal(result.errors[0].account, "home");
    assert.equal(c.urls.length, 3);
    const stalled = client("wss://relay.example/", accounts.home, {
        syncReply: () => ({ synced: [{ account: "test", messages_added: 0, bodies_fetched: 0 }],
            coverage: { backfilling: [{ folder: "INBOX" }] } }),
    });
    const stopped = await stalled.syncProgressively();
    assert.equal(stopped.errors.length, 0);
    assert.equal(stalled.urls.length, 2);
});

test("opening another folder fetches only one requested header batch", async () => {
    const c = client("wss://relay.example/", undefined, { accounts });
    await c.syncProgressively({ account: "work", folders: ["Archive"] });
    const syncs = c.requests.filter(request => request.method === "sync");
    assert.equal(syncs.length, 1);
    assert.deepEqual(JSON.parse(syncs[0].args).folders, ["Archive"]);
    assert.equal(JSON.parse(syncs[0].args).account, "work");
    assert.equal(c.requests.some(request => request.method === "remote_folders"), false);
});

test("folder listing shows server counts separately from downloaded headers", async () => {
    const c = client("wss://relay.example/", undefined, { accounts,
        folders: [{ account: "home", path: "INBOX", total: 100, unread: 10 }],
        remoteFolders: { home: [{ account: "home", path: "INBOX", total: 5000, unread: 200 }] },
    });
    const result = await c.api("/api/folders");
    assert.equal(result.folders[0].total, 5000);
    assert.equal(result.folders[0].downloaded, 100);
    assert.equal(result.folders[0].unread, 200);
    assert.equal(c.requests[0].persist, false);
});

test("a delete interrupts a slow body read and still commits its checkpoint", async () => {
    let blocked;
    const waiting = new Promise(resolve => { blocked = resolve; });
    const c = client("wss://relay.example/", undefined, { accounts, blockBody: true, onBlocked: blocked,
        reply: (request, result) => request.method === "show"
            ? { message: { id: "mail", account: "work" } } : result,
    });
    const show = c.api("/api/show?id=mail&format=html");
    await waiting;
    const deleted = c.api("/api/move", { method: "POST", body: '{"ids":["mail"],"folder":"trash"}' });
    await Promise.all([show, deleted]);
    assert(c.checkpoints.work);
    assert.equal(c.requests.find(request => request.method === "show").persist, false);
    assert.equal(c.requests.find(request => request.method === "move").persist, true);
});

test("deleting updates cached server folder counts until the next refresh", async () => {
    const c = client("wss://relay.example/", undefined, { accounts, remoteFolders: { work: [
        { path: "INBOX", total: 5000, unread: 100 },
        { path: "Deleted Items", special_use: "trash", total: 50, unread: 0 },
    ] } });
    await c.api("/api/move", { method: "POST", body: JSON.stringify({
        ids: ["mail"], folder: "trash", account: "work", source_folder: "INBOX", unread: true,
    }) });
    assert.equal(c.settings().remoteFolders.work[0].total, 4999);
    assert.equal(c.settings().remoteFolders.work[1].total, 51);
});

test("opening a header-only message fetches its body through the owning account", async () => {
    let fetched = false;
    const c = client("wss://relay.example/", undefined, { accounts,
        reply: (request, result) => {
            if (request.method === "show") return { message: { id: "message-id", account: "work",
                ...(fetched ? { body: { content: "Loaded body" } } : {}) } };
            if (request.method === "ensure_body") fetched = true;
            return result;
        },
    });
    const shown = await c.api("/api/show?id=message-id&format=html");
    assert.equal(shown.message.body.content, "Loaded body");
    const request = c.requests.find(request => request.method === "ensure_body");
    assert.deepEqual(Object.keys(JSON.parse(request.config).accounts), ["work"]);
    assert.equal(new URL(c.urls[0]).searchParams.get("host"), "imap.two.example");
});
