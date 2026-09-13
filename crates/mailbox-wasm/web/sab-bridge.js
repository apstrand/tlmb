// Shared layout with mailbox_core::wasm_stream.
const STATE = 0;
const LENGTH = 1;
const IDLE = 0;
const READ_REQUEST = 1;
const READ_READY = 2;
const WRITE_REQUEST = 3;
const WRITE_DONE = 4;
const CLOSED = -1;
const FAILED = -2;

export function runWorkerImapProbe({ relayUrl, host, port, onStatus }) {
  if (!crossOriginIsolated || typeof SharedArrayBuffer === "undefined") {
    return Promise.reject(
      new Error("SharedArrayBuffer requires COOP/COEP response headers"),
    );
  }

  const controlBuffer = new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * 2);
  const dataBuffer = new SharedArrayBuffer(64 * 1024);
  const control = new Int32Array(controlBuffer);
  const data = new Uint8Array(dataBuffer);
  const socket = new WebSocket(relayUrl);
  const worker = new Worker("./worker.js", { type: "module" });
  const incoming = [];
  let incomingOffset = 0;
  let settled = false;

  socket.binaryType = "arraybuffer";

  const wake = (state, length = 0) => {
    Atomics.store(control, LENGTH, length);
    Atomics.store(control, STATE, state);
    Atomics.notify(control, STATE);
  };

  const service = () => {
    const state = Atomics.load(control, STATE);
    if (state === WRITE_REQUEST) {
      const length = Atomics.load(control, LENGTH);
      if (length < 0 || length > data.length) {
        wake(FAILED);
        return;
      }
      try {
        socket.send(data.slice(0, length));
        wake(WRITE_DONE);
      } catch {
        wake(FAILED);
      }
      return;
    }
    if (state !== READ_REQUEST || incoming.length === 0) return;

    const requested = Atomics.load(control, LENGTH);
    const current = incoming[0];
    const length = Math.min(requested, current.length - incomingOffset);
    data.set(current.subarray(incomingOffset, incomingOffset + length), 0);
    incomingOffset += length;
    if (incomingOffset === current.length) {
      incoming.shift();
      incomingOffset = 0;
    }
    wake(READ_READY, length);
  };

  return new Promise((resolve, reject) => {
    const interval = setInterval(service, 1);
    const finish = (error, result) => {
      if (settled) return;
      settled = true;
      clearInterval(interval);
      socket.close();
      worker.terminate();
      if (error) reject(error);
      else resolve(result);
    };

    socket.addEventListener("open", () => {
      onStatus?.("relay open; blocking worker started…");
      worker.postMessage({ controlBuffer, dataBuffer, host, port });
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
      if (!settled) wake(CLOSED);
    });
    worker.addEventListener("message", (event) => {
      if (event.data.ok) finish(null, event.data.result);
      else finish(new Error(event.data.error));
    });
    worker.addEventListener("error", (event) => {
      wake(FAILED);
      finish(new Error(event.message || "worker failed"));
    });
  });
}
