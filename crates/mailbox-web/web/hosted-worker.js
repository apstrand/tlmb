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

        const mailbox = HostedMailbox.openAccounts(
            request.config,
            JSON.stringify(request.credentials ?? {}),
            request.checkpoints ?? {},
        );
        try {
            const reply = JSON.parse(mailbox.exec(request.method, request.args));
            const { schema: _schema, command: _command, ...result } = reply;
            if (request.method === "sync") {
                result.coverage = JSON.parse(mailbox.exec("status", "{}"))
                    .accounts[0]?.coverage;
            }
            // Read requests use message IDs, so their transient listing history
            // need not rewrite every account's database and body archive.
            const snapshots = request.persist ? mailbox.snapshots() : {};
            self.postMessage(
                { ok: true, result, snapshots },
                Object.values(snapshots).map((snapshot) => snapshot.buffer),
            );
        } finally {
            mailbox.free();
        }
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
