import init, {
    HostedMailbox,
    installBrowserStream,
    prepareAccount,
    testAccount,
} from "./pkg/mailbox_wasm.js";

let initialized;

self.addEventListener("message", async (event) => {
    const request = event.data;
    try {
        initialized ??= init();
        await initialized;
        if (request.controlBuffer) {
            installBrowserStream(
                new Int32Array(request.controlBuffer),
                new Uint8Array(request.dataBuffer),
            );
        }

        if (request.operation === "prepare") {
            self.postMessage({ ok: true, result: JSON.parse(prepareAccount(request.args)) });
            return;
        }
        if (request.operation === "test") {
            self.postMessage({ ok: true, result: JSON.parse(testAccount(request.args)) });
            return;
        }

        const mailbox = new HostedMailbox(
            request.config,
            request.credential,
            request.checkpoint,
        );
        const reply = JSON.parse(mailbox.exec(request.method, request.args));
        const { schema: _schema, command: _command, ...result } = reply;
        const snapshot = mailbox.snapshot().slice();
        mailbox.free();
        self.postMessage(
            { ok: true, result, snapshot },
            snapshot.length ? [snapshot.buffer] : [],
        );
    } catch (error) {
        self.postMessage({ ok: false, error: readableError(error) });
    }
});

function readableError(error) {
    const text = String(error?.message || error?.stack || error);
    try {
        const parsed = JSON.parse(text);
        return parsed?.error?.message || text;
    } catch {
        return text;
    }
}
