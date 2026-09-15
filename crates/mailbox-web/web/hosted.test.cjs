const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");

function client(relay, account, options = {}) {
    const urls = [];
    const requests = [];
    let settings = { config: { accounts: options.accounts ?? { test: account } } };
    const credentials = { ...options.credentials };
    const checkpoints = { ...options.checkpoints };
    class Worker {
        listeners = {};
        addEventListener(name, handler) { this.listeners[name] = handler; }
        postMessage(request) {
            requests.push(request);
            const spec = JSON.parse(request.args);
            const id = spec.id || "test";
            const accounts = request.config ? JSON.parse(request.config).accounts : {};
            if (request.method === "sync" && options.fail?.includes(Object.keys(accounts)[0])) {
                queueMicrotask(() => this.listeners.message({ data: { ok: false, error: "sync failed" } }));
                return;
            }
            const result = request.operation === "prepare"
                ? { id, config: { accounts: { [id]: spec } } }
                : request.method === "sync"
                    ? { synced: Object.keys(accounts).map((account) => ({ account, messages_added: 1 })) }
                    : { ok: true };
            const snapshots = Object.fromEntries(Object.keys(accounts).map((id) => [id, new Uint8Array([requests.length])]));
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
        fetch: async () => ({ ok: true, json: async () => ({ relay }) }),
        loadSettings: async () => settings,
        saveSettings: async (value) => { settings = value; },
        loadCredential: async (id) => credentials[id],
        saveCredential: async (id, password) => { credentials[id] = password; },
        loadCheckpoint: async (id) => checkpoints[id],
        saveCheckpoint: async (snapshot, id) => { checkpoints[id] = snapshot; },
    });
    vm.runInContext(source, context);
    return { api: context.api, urls, requests, credentials, checkpoints, settings: () => settings };
}

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
    await Promise.all([c.api("/api/ls"), c.api("/api/status")]);
    assert.equal(c.requests[1].checkpoints.test[0], 1);
});
