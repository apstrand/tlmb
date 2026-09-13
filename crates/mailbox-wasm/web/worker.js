import init, { worker_imap_probe } from "./pkg/mailbox_wasm.js";

self.addEventListener("message", async (event) => {
  const { controlBuffer, dataBuffer, host, port } = event.data;
  try {
    await init();
    const result = worker_imap_probe(
      new Int32Array(controlBuffer),
      new Uint8Array(dataBuffer),
      host,
      port,
    );
    self.postMessage({ ok: true, result: JSON.parse(result) });
  } catch (error) {
    self.postMessage({ ok: false, error: String(error?.stack || error) });
  }
});
