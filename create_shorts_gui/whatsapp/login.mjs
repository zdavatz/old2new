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

import makeWASocket, {
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  fetchLatestBaileysVersion,
} from "@whiskeysockets/baileys";
import qrcode from "qrcode-terminal";
import pino from "pino";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";
import { rmSync, existsSync } from "fs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const AUTH_DIR = resolve(__dirname, "auth");
const logger = pino({ level: "silent" });

console.log(`Auth dir: ${AUTH_DIR}`);

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

  sock.ev.on("creds.update", saveCreds);
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

  console.log("LINKED");
}

main()
  .then(() => process.exit(0))
  .catch((err) => {
    console.error("Error:", err.message);
    process.exit(1);
  });
