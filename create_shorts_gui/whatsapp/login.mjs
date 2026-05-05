#!/usr/bin/env node
// WhatsApp login via Baileys — port of pegelstand's proven login flow
// (`~/software/pegelstand/whatsapp/login.mjs`), with two additions for
// create_shorts_gui:
//   - QR ASCII is wrapped in QR-CODE-BEGIN/QR-CODE-END markers so the
//     Rust GUI can pick it up and render it in a modal.
//   - On success, we print the literal sentinel `LINKED` so the GUI
//     can transition state without parsing localized strings.
//
// Flow:
//   1. Open socket → either `connection: open` or `connection: close`.
//   2. On `open` → success.
//   3. On `close` with status 515 (restartRequired) — normal post-pair
//      restart — reconnect once and wait for `open` again.
//   4. On `close` with status 401/403 — stale session — wipe `auth/`
//      and retry once with a fresh QR.
//   5. Anything else → fail with the status code.
//
// Persistence: Baileys' EventEmitter doesn't await listeners, so a
// naive `sock.ev.on("creds.update", saveCreds)` fires-and-forgets the
// write. If we exit too quickly, `creds.json` ends up truncated to 0
// bytes and the saved session looks unlinked on next launch. We chain
// saves into a single in-flight promise and `await` it (plus an
// explicit final saveCreds()) before printing LINKED.

import makeWASocket, {
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  fetchLatestBaileysVersion,
} from "@whiskeysockets/baileys";
import qrcode from "qrcode-terminal";
import pino from "pino";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { rmSync, existsSync, statSync } from "fs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const AUTH_DIR = resolve(__dirname, "auth");
const logger = pino({ level: "silent" });

console.log(`Auth dir: ${AUTH_DIR}`);

let lastSavePromise = Promise.resolve();
let savePromiseRef; // tracks the live `saveCreds` callback for explicit flush

async function startSocket() {
  const { state, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
  const { version } = await fetchLatestBaileysVersion();
  console.log(`WA version: ${version.join(".")}`);

  const sock = makeWASocket({
    auth: {
      creds: state.creds,
      keys: makeCacheableSignalKeyStore(state.keys, logger),
    },
    version,
    logger,
    browser: ["create_shorts", "Desktop", "1.0"],
    syncFullHistory: false,
    markOnlineOnConnect: false,
  });

  // Chain saves so we can await the last write before exit.
  sock.ev.on("creds.update", () => {
    lastSavePromise = lastSavePromise.then(() => saveCreds()).catch(() => {});
  });
  savePromiseRef = saveCreds;
  return sock;
}

async function loginOnce() {
  const sock = await startSocket();

  return new Promise((resolve) => {
    let done = false;
    const finish = (result) => {
      if (done) return;
      done = true;
      clearTimeout(timeout);
      resolve(result);
    };
    const timeout = setTimeout(() => {
      try { sock.end(); } catch (_) {}
      finish({ ok: false, msg: "Link timeout (5 min)" });
    }, 300000);

    sock.ev.on("connection.update", (update) => {
      const { connection, qr, lastDisconnect } = update;

      if (qr) {
        console.log("\nQR-CODE-BEGIN");
        qrcode.generate(qr, { small: true });
        console.log("QR-CODE-END");
        console.log("Scan with WhatsApp → Settings → Linked Devices → Link a Device");
      }

      if (connection === "open") {
        finish({ ok: true });
        setTimeout(() => { try { sock.end(); } catch (_) {} }, 500);
      }

      if (connection === "close") {
        const err = lastDisconnect?.error;
        const code = err?.output?.statusCode;
        const msg = err?.message || "unknown";
        finish({ ok: false, code, msg });
      }
    });
  });
}

async function flushCredsToDisk() {
  // Drain whatever creds.update listeners queued up.
  await lastSavePromise;
  // Explicit final write — last in-memory state wins.
  if (savePromiseRef) {
    try { await savePromiseRef(); } catch (_) {}
  }
  // Verify creds.json actually has content. Some builds of Baileys
  // emit creds.update *after* `connection: open`, and Node's fsync
  // path is async — give it up to ~3 s to settle.
  const credsPath = resolve(AUTH_DIR, "creds.json");
  for (let i = 0; i < 30; i++) {
    try {
      const sz = statSync(credsPath).size;
      if (sz > 100) return;
    } catch (_) {}
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function main() {
  let result = await loginOnce();

  if (!result.ok) {
    console.log(`Connection closed (code ${result.code ?? "?"}, ${result.msg})`);

    if (result.code === 401 || result.code === 403) {
      console.log("Stale session — clearing and retrying with fresh QR.");
      if (existsSync(AUTH_DIR)) {
        rmSync(AUTH_DIR, { recursive: true, force: true });
      }
      result = await loginOnce();
      if (!result.ok) throw new Error(`Re-login failed: ${result.msg}`);
    } else if (result.code === 515) {
      console.log("Restart required — reconnecting…");
      result = await loginOnce();
      if (!result.ok) throw new Error(`Reconnect failed: ${result.msg}`);
    } else {
      throw new Error(`Connection closed: ${result.msg}`);
    }
  }

  await flushCredsToDisk();
  console.log("LINKED");
}

main()
  .then(() => process.exit(0))
  .catch((err) => {
    console.error("Error:", err.message);
    process.exit(1);
  });
