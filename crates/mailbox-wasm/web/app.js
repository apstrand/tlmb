import init, { l0_probe } from "./pkg/mailbox_wasm.js";
import { runWorkerImapProbe } from "./sab-bridge.js";

const l0 = document.querySelector("#l0");
const tls = document.querySelector("#tls");

try {
  await init();
  l0.textContent = JSON.stringify(JSON.parse(l0_probe()), null, 2);
  l0.dataset.ok = "true";
} catch (error) {
  l0.textContent = String(error?.stack || error);
  l0.dataset.ok = "false";
}

function runTls() {
  tls.textContent = "opening relay…";
  tls.dataset.ok = "false";

  const host = document.querySelector("#host").value.trim();
  const port = Number(document.querySelector("#port").value);
  return runWorkerImapProbe({
    relayUrl: document.querySelector("#relay").value.trim(),
    host,
    port,
    onStatus: (status) => (tls.textContent = status),
  })
    .then((result) => {
      tls.textContent = JSON.stringify(result, null, 2);
      tls.dataset.ok = "true";
      return true;
    })
    .catch((error) => {
      tls.textContent = String(error?.stack || error);
      tls.dataset.ok = "false";
      return false;
    });
}

document.querySelector("#tls-form").addEventListener("submit", (event) => {
  event.preventDefault();
  runTls();
});

if (new URL(location.href).searchParams.get("run-tls") === "1") {
  await runTls();
}
