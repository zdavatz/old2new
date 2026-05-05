#!/usr/bin/env node
// One-shot WhatsApp linking via Baileys — prints the QR code to stdout
// (qrcode-terminal ASCII), waits for the user to scan, then exits when
// the connection opens. Call from create_shorts_gui's "Link WhatsApp"
// button. The auth/ dir is written next to this script.

import makeWASocket, {
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  fetchLatestBaileysVersion,
  DisconnectReason,
} from "@whiskeysockets/baileys";
import qrcode from "qrcode-terminal";
import pino from "pino";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const AUTH_DIR = resolve(__dirname, "auth");
const logger = pino({ level: "silent" });

console.log(`Auth dir: ${AUTH_DIR}`);

let done = false;

async function connect() {
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
    browser: ["create_shorts", "GUI", "1.0"],
    syncFullHistory: false,
    markOnlineOnConnect: false,
  });

  sock.ev.on("creds.update", saveCreds);

  return new Promise((resolvePromise, reject) => {
    // 5-min timeout — gives time to scan
    const timeout = setTimeout(() => {
      sock.end();
      reject(new Error("Link timeout (5 min)"));
    }, 300000);

    sock.ev.on("connection.update", async (update) => {
      const { connection, lastDisconnect, qr } = update;

      if (qr) {
        console.log("\nQR-CODE-BEGIN");
        qrcode.generate(qr, { small: true });
        console.log("QR-CODE-END");
        console.log("Scan with WhatsApp → Settings → Linked Devices → Link a Device");
      }

      if (connection === "open") {
        clearTimeout(timeout);
        console.log("LINKED");
        done = true;
        // Give saveCreds() a moment to flush.
        setTimeout(() => process.exit(0), 1500);
      }

      if (connection === "close") {
        clearTimeout(timeout);
        if (done) {
          resolvePromise();
          return;
        }
        const statusCode = lastDisconnect?.error?.output?.statusCode;
        if (statusCode === DisconnectReason.loggedOut) {
          reject(new Error("Logged out before scan"));
        } else {
          reject(new Error(`Connection closed (status ${statusCode ?? "?"})`));
        }
      }
    });
  });
}

connect()
  .then(() => process.exit(0))
  .catch((err) => {
    console.error("Error:", err.message);
    process.exit(1);
  });
