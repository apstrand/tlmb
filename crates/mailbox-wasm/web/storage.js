const DB_NAME = "mailbox-hosted";
const DB_VERSION = 2;
const SETTINGS_STORE = "settings";
const CREDENTIALS_STORE = "credentials";

function indexedDb() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.addEventListener("upgradeneeded", () => {
      if (!request.result.objectStoreNames.contains(SETTINGS_STORE)) {
        request.result.createObjectStore(SETTINGS_STORE);
      }
      if (!request.result.objectStoreNames.contains(CREDENTIALS_STORE)) {
        request.result.createObjectStore(CREDENTIALS_STORE);
      }
    });
    request.addEventListener("success", () => resolve(request.result));
    request.addEventListener("error", () => reject(request.error));
  });
}

export async function loadSettings() {
  const db = await indexedDb();
  return new Promise((resolve, reject) => {
    const request = db
      .transaction(SETTINGS_STORE, "readonly")
      .objectStore(SETTINGS_STORE)
      .get("hosted");
    request.addEventListener("success", () => resolve(request.result || {}));
    request.addEventListener("error", () => reject(request.error));
  }).finally(() => db.close());
}

export async function saveSettings(settings) {
  const db = await indexedDb();
  return new Promise((resolve, reject) => {
    const transaction = db.transaction(SETTINGS_STORE, "readwrite");
    transaction.objectStore(SETTINGS_STORE).put(settings, "hosted");
    transaction.addEventListener("complete", resolve);
    transaction.addEventListener("error", () => reject(transaction.error));
  }).finally(() => db.close());
}

export async function loadCredential(account) {
  const db = await indexedDb();
  return new Promise((resolve, reject) => {
    const request = db
      .transaction(CREDENTIALS_STORE, "readonly")
      .objectStore(CREDENTIALS_STORE)
      .get(account);
    request.addEventListener("success", () => resolve(request.result));
    request.addEventListener("error", () => reject(request.error));
  }).finally(() => db.close());
}

export async function saveCredential(account, credential) {
  const db = await indexedDb();
  return new Promise((resolve, reject) => {
    const transaction = db.transaction(CREDENTIALS_STORE, "readwrite");
    transaction.objectStore(CREDENTIALS_STORE).put(credential, account);
    transaction.addEventListener("complete", resolve);
    transaction.addEventListener("error", () => reject(transaction.error));
  }).finally(() => db.close());
}

export async function persistedSettingsProbe() {
  const expectedSettings = {
    relay: "ws://127.0.0.1:9001/",
    account: "browser",
  };
  const expectedCredential = "hosted-credential-probe";
  const previousSettings = await loadSettings();
  const previousCredential = await loadCredential("browser");
  const restored =
    previousSettings.account === expectedSettings.account &&
    previousCredential === expectedCredential;
  await saveSettings(expectedSettings);
  await saveCredential("browser", expectedCredential);
  const ok =
    (await loadSettings()).account === expectedSettings.account &&
    (await loadCredential("browser")) === expectedCredential;
  // Never include the credential value in DOM-visible diagnostics.
  return { ok, restored, account: expectedSettings.account };
}

async function accountDirectory(account) {
  const root = await navigator.storage.getDirectory();
  const mailbox = await root.getDirectoryHandle("mailbox", { create: true });
  return mailbox.getDirectoryHandle(account, { create: true });
}

export async function loadCheckpoint(account = "browser") {
  const directory = await accountDirectory(account);
  try {
    const handle = await directory.getFileHandle("store.bin");
    return new Uint8Array(await (await handle.getFile()).arrayBuffer());
  } catch (error) {
    if (error.name !== "NotFoundError") throw error;
    // Upgrade a raw SQLite image written by the initial Stage 2 prototype.
    try {
      const legacy = await directory.getFileHandle("meta.db");
      return new Uint8Array(await (await legacy.getFile()).arrayBuffer());
    } catch (legacyError) {
      if (legacyError.name === "NotFoundError") return undefined;
      throw legacyError;
    }
  }
}

export async function saveCheckpoint(bytes, account = "browser") {
  const directory = await accountDirectory(account);
  const handle = await directory.getFileHandle("store.bin", { create: true });
  // createWritable commits its temporary backing file when close resolves, so
  // readers see the previous complete SQLite image or the new complete image.
  const writable = await handle.createWritable();
  await writable.write(bytes);
  await writable.close();
}

export async function persistedDatabaseProbe(BrowserStore) {
  const checkpoint = await loadCheckpoint();
  const restored = checkpoint !== undefined;
  const store = new BrowserStore(checkpoint);
  const before = store.folderCount();
  const after = store.ensureProbeFolder();
  const bytesBefore = JSON.parse(store.storageProbe());
  await saveCheckpoint(store.snapshot());
  store.free();

  // Reopen from exactly what OPFS now holds, proving deserialization as well
  // as serialization in a single run. A page reload should report restored.
  const reopened = new BrowserStore(await loadCheckpoint());
  const reopenedCount = reopened.folderCount();
  const bytesReopened = JSON.parse(reopened.storageProbe());
  reopened.free();
  return {
    ok: reopenedCount === after && bytesBefore.ok && bytesReopened.restored,
    restored,
    before,
    after,
    reopenedCount,
    byteStorageRestored: bytesBefore.restored,
  };
}
