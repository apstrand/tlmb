import {
    clearCheckpoint,
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

function accountId(config) {
    return Object.keys(config?.accounts ?? {})[0];
}

async function context() {
    const settings = await loadSettings();
    const config = settings.config ?? {};
    const id = accountId(config);
    return {
        config,
        id,
        credential: id ? await loadCredential(id) : undefined,
        checkpoint: id ? await loadCheckpoint(id) : undefined,
    };
}

async function workerCall(request, network) {
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
            socket = new WebSocket(relay);
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
    const response = await workerCall({
        operation: "exec",
        method,
        args: JSON.stringify(args),
        config: JSON.stringify(current.config),
        credential: current.credential,
        checkpoint: current.checkpoint,
    }, network);
    if (response.snapshot?.length && current.id) {
        await saveCheckpoint(response.snapshot, current.id);
    }
    return response.result;
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

export async function api(path, options = {}) {
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
    const response = await workerCall({
        operation: "test",
        args: JSON.stringify(spec),
    }, true);
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
    if (old.id && old.id !== account.id) await clearCheckpoint(old.id);
    await saveSettings({ config: account.config });
    await saveCredential(account.id, spec.password);
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
