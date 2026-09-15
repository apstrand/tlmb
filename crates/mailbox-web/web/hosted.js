import {
    loadCheckpoint,
    loadCredential,
    loadSettings,
    saveCheckpoint,
    saveCredential,
    saveSettings,
} from "./storage.js";

const STATE = 0;
const LENGTH = 1;
const READ_REQUEST = 1;
const READ_READY = 2;
const WRITE_REQUEST = 3;
const WRITE_DONE = 4;
const CLOSED = -1;
const FAILED = -2;

const siteConfig = fetch("config.json").then(async (response) => {
    if (!response.ok) throw new Error("hosted config.json is missing");
    return response.json();
});

async function context() {
    const settings = await loadSettings();
    const config = settings.config ?? {};
    const credentials = Object.create(null);
    const checkpoints = Object.create(null);
    await Promise.all(Object.keys(config.accounts ?? {}).map(async (id) => {
        const credential = await loadCredential(id);
        if (credential !== undefined) credentials[id] = credential;
        const checkpoint = await loadCheckpoint(id);
        if (checkpoint !== undefined) checkpoints[id] = checkpoint;
    }));
    return { config, credentials, checkpoints };
}

async function workerCall(request, network, account) {
    const worker = new Worker("hosted-worker.js", { type: "module" });
    let socket;
    let interval;
    let control;
    let data;
    let settled = false;
    const incoming = [];
    let incomingOffset = 0;

    const wake = (state, length = 0) => {
        Atomics.store(control, LENGTH, length);
        Atomics.store(control, STATE, state);
        Atomics.notify(control, STATE);
    };
    const service = () => {
        const state = Atomics.load(control, STATE);
        if (state === WRITE_REQUEST) {
            const length = Atomics.load(control, LENGTH);
            if (length < 0 || length > data.length) return wake(FAILED);
            try {
                socket.send(data.slice(0, length));
                wake(WRITE_DONE);
            } catch {
                wake(FAILED);
            }
        } else if (state === READ_REQUEST && incoming.length) {
            const current = incoming[0];
            const length = Math.min(
                Atomics.load(control, LENGTH),
                current.length - incomingOffset,
            );
            data.set(current.subarray(incomingOffset, incomingOffset + length));
            incomingOffset += length;
            if (incomingOffset === current.length) {
                incoming.shift();
                incomingOffset = 0;
            }
            wake(READ_READY, length);
        }
    };

    return new Promise((resolve, reject) => {
        const finish = (error, value) => {
            if (settled) return;
            settled = true;
            clearInterval(interval);
            socket?.close();
            worker.terminate();
            error ? reject(error) : resolve(value);
        };
        worker.addEventListener("message", (event) => {
            if (event.data.ok) finish(null, event.data);
            else finish(new Error(event.data.error));
        });
        worker.addEventListener("error", (event) => {
            if (control) wake(FAILED);
            finish(new Error(event.message || "hosted worker failed"));
        });

        if (!network) {
            worker.postMessage(request);
            return;
        }
        if (!crossOriginIsolated || typeof SharedArrayBuffer === "undefined") {
            finish(new Error("This host must send COOP and COEP headers for mailbox sync."));
            return;
        }
        siteConfig.then(({ relay }) => {
            if (!relay) {
                finish(new Error(
                    "This site was generated without a relay URL, so it cannot sync.",
                ));
                return;
            }
            const controlBuffer = new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * 2);
            const dataBuffer = new SharedArrayBuffer(64 * 1024);
            control = new Int32Array(controlBuffer);
            data = new Uint8Array(dataBuffer);
            const relayUrl = new URL(relay);
            if (relayUrl.pathname.replace(/\/+$/, "") === "") {
                if (!account?.imap_host) throw new Error("Account has no IMAP host.");
                const security = account.imap_security;
                const port = account.imap_port ?? (
                    ["starttls", "none", "plain"].includes(security) ? 143 : 993
                );
                relayUrl.searchParams.set("host", account.imap_host);
                relayUrl.searchParams.set("port", String(port));
            }
            socket = new WebSocket(relayUrl.href);
            socket.binaryType = "arraybuffer";
            interval = setInterval(service, 1);
            socket.addEventListener("open", () => {
                worker.postMessage({ ...request, controlBuffer, dataBuffer });
            });
            socket.addEventListener("message", (event) => {
                incoming.push(new Uint8Array(event.data));
                service();
            });
            socket.addEventListener("error", () => {
                wake(FAILED);
                finish(new Error("relay connection failed"));
            });
            socket.addEventListener("close", () => {
                if (Atomics.load(control, STATE) === READ_REQUEST) wake(CLOSED);
            });
        }).catch((error) => finish(error));
    });
}

async function core(method, args = {}, { network = false } = {}) {
    const current = await context();
    if (network && method === "sync") return syncAccounts(current, args);
    return exec(current, method, args);
}

async function exec(current, method, args, network = false, account) {
    const response = await workerCall({
        operation: "exec",
        method,
        args: JSON.stringify(args),
        config: JSON.stringify(current.config),
        credentials: current.credentials,
        checkpoints: current.checkpoints,
    }, network, account);
    for (const [id, snapshot] of Object.entries(response.snapshots ?? {})) {
        await saveCheckpoint(snapshot, id);
    }
    return response.result;
}

async function syncAccounts(current, args) {
    const result = { synced: [] };
    let firstError;
    for (const [id, account] of Object.entries(current.config.accounts ?? {})) {
        try {
            const reply = await exec({
                config: { ...current.config, accounts: { [id]: account } },
                credentials: { [id]: current.credentials[id] },
                checkpoints: current.checkpoints[id] ? { [id]: current.checkpoints[id] } : {},
            }, "sync", args, true, account);
            result.synced.push(...(reply.synced ?? []));
            if (reply.submitted) {
                result.submitted ??= {};
                for (const [key, count] of Object.entries(reply.submitted)) {
                    result.submitted[key] = (result.submitted[key] ?? 0) + count;
                }
            }
        } catch (error) {
            (result.errors ??= []).push({ account: id, message: error.message });
            firstError ??= new Error(`${id}: ${error.message}`);
        }
    }
    if (!result.synced.length && firstError) throw firstError;
    return result;
}

function body(options) {
    return options.body ? JSON.parse(options.body) : {};
}

function flagChanges(value) {
    return [
        ...(value.add ?? []).map((item) => `+${item}`),
        ...(value.remove ?? []).map((item) => `-${item}`),
        ...(value.add_tags ?? []).map((item) => `+tag:${item}`),
        ...(value.remove_tags ?? []).map((item) => `-tag:${item}`),
    ];
}

// Serialize calls so a read cannot overwrite a checkpoint changed by sync or
// another mutation while its Worker was running.
let pending = Promise.resolve();
export function api(path, options = {}) {
    const result = pending.then(() => dispatch(path, options));
    pending = result.catch(() => {});
    return result;
}

async function dispatch(path, options) {
    const url = new URL(path, location.href);
    const route = url.pathname.replace(/^\/api\//, "");
    const value = body(options);
    switch (`${options.method ?? "GET"} ${route}`) {
        case "GET accounts": return core("accounts");
        case "GET ls": return core("ls", {
            query: url.searchParams.get("q") || undefined,
            limit: Number(url.searchParams.get("limit") || 50),
            cursor: url.searchParams.get("cursor") || undefined,
        });
        case "GET search": return core("search", {
            query: url.searchParams.get("q") || "",
            limit: Number(url.searchParams.get("limit") || 50),
        });
        case "GET show": return core("show", {
            id: url.searchParams.get("id"),
            format: url.searchParams.get("format") || "md",
            thread: url.searchParams.get("thread") === "1",
        });
        case "GET folders": return core("folders");
        case "GET identities": return core("identities");
        case "GET status": return core("status");
        case "GET outbox": return core("outbox");
        case "GET snoozed": return core("snoozed");
        case "GET reply": {
            const mode = url.searchParams.get("mode") || "sender";
            return core("reply_draft", {
                id: url.searchParams.get("id"),
                all: mode === "all",
                forward: mode === "forward",
            });
        }
        case "GET providers": return core("providers", {
            address: url.searchParams.get("address"),
        });
        case "GET discover": throw new Error(
            "Browser DNS discovery is unavailable; enter the IMAP and SMTP servers.",
        );
        case "POST flag": return core("flag", {
            ids: value.ids,
            changes: flagChanges(value),
        });
        case "POST move": return core("move", value);
        case "POST snooze": return core("snooze", value);
        case "POST send": return core("send", value);
        case "POST sync": return core("sync", value, { network: true });
        case "POST accounts/test": return test(value);
        case "POST accounts": return add(value);
        default: throw new Error(`hosted backend has no ${options.method ?? "GET"} ${route}`);
    }
}

async function test(spec) {
    const prepared = await workerCall({
        operation: "prepare",
        args: JSON.stringify(spec),
    }, false);
    const account = prepared.result.config.accounts[prepared.result.id];
    const response = await workerCall({
        operation: "test",
        args: JSON.stringify(spec),
    }, true, account);
    return response.result;
}

async function add(spec) {
    await test(spec);
    const prepared = await workerCall({
        operation: "prepare",
        args: JSON.stringify(spec),
    }, false);
    const account = prepared.result;
    const old = await context();
    const config = {
        ...old.config,
        accounts: { ...old.config.accounts, ...account.config.accounts },
    };
    if (Object.hasOwn(old.config.accounts ?? {}, account.id)) {
        throw new Error(`Account '${account.id}' already exists; choose a different account ID.`);
    }
    await saveCredential(account.id, spec.password);
    await saveSettings({ config });
    return {
        id: account.id,
        address: spec.address,
        discovered: account.discovered,
        credential: "browser storage",
    };
}

export function hostedBodyUrl(message, images) {
    let html = message.body?.content ?? "";
    if (images) html = html.replaceAll("data-blocked-src=", "src=");
    const policy = images
        ? "default-src 'none'; img-src https: http: data:; style-src 'unsafe-inline'"
        : "default-src 'none'; img-src data:; style-src 'unsafe-inline'";
    const document = `<!doctype html><meta charset="utf-8">` +
        `<meta http-equiv="Content-Security-Policy" content="${policy}">` + html;
    return URL.createObjectURL(new Blob([document], { type: "text/html" }));
}
