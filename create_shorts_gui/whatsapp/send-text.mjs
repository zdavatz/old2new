#!/usr/bin/env node
// Send a plain-text message to a WhatsApp contact or group via Baileys.
// auth/ and node_modules/ are expected next to this script — set up by
// create_shorts_gui's Settings → Setup WhatsApp button.
//
// Override WA_AUTH_DIR via env var to point at an existing auth dir
// (e.g. pegelstand's whatsapp/auth) and skip the QR scan.
//
// Args: <phone-or-jid> <message...>

import makeWASocket, {
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  fetchLatestBaileysVersion,
  DisconnectReason,
} from "@whiskeysockets/baileys";
import qrcode from "qrcode-terminal";
import pino from "pino";
import { existsSync, rmSync } from "fs";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const AUTH_DIR = resolve(process.env.WA_AUTH_DIR || resolve(__dirname, "auth"));
if (!existsSync(AUTH_DIR)) {
  // Baileys creates this on first run — but if the user explicitly
  // pointed WA_AUTH_DIR at a non-existent path, that's an error.
  if (process.env.WA_AUTH_DIR) {
    console.error(`WA_AUTH_DIR does not exist: ${AUTH_DIR}`);
    process.exit(2);
  }
}

const logger = pino({ level: "silent" });

const [,, jidArg, ...messageParts] = process.argv;
const message = messageParts.join(" ");

if (!jidArg || !message) {
  console.error("Usage: node send-text.mjs <phone-or-jid> <message...>");
  process.exit(1);
}

let jid;
if (jidArg.includes("@")) jid = jidArg;
else if (/^\d+$/.test(jidArg)) jid = `${jidArg}@s.whatsapp.net`;
else { console.error(`Bad JID: ${jidArg}`); process.exit(1); }

console.log(`Auth: ${AUTH_DIR}`);
console.log(`Target: ${jid}`);

let retries = 0;
const MAX_RETRIES = 3;
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
    const timeout = setTimeout(() => {
      sock.end();
      reject(new Error("Connection timeout (90s)"));
    }, 90000);

    sock.ev.on("connection.update", async (update) => {
      const { connection, lastDisconnect, qr } = update;

      if (qr) {
        console.error("Not linked. Run 'Link WhatsApp' in Settings first.");
        clearTimeout(timeout);
        sock.end();
        reject(new Error("Not linked — run Link WhatsApp first."));
        return;
      }

      if (connection === "open") {
        clearTimeout(timeout);
        try {
          console.log(`Connected. Sending text to ${jid}...`);
          const sendResult = await Promise.race([
            sock.sendMessage(jid, { text: message }),
            new Promise((_, rej) => setTimeout(() => rej(new Error("sendMessage timeout (30s)")), 30000)),
          ]);
          console.log("Sent!", sendResult?.key?.id ? `(id: ${sendResult.key.id})` : "");
          done = true;
          // Force exit — sock.end() triggers close handler hangs.
          // Wait briefly so saveCreds() async write lands on disk.
          setTimeout(() => process.exit(0), 1500);
        } catch (err) {
          console.error("Send error:", err.message);
          sock.end();
          reject(err);
        }
      }

      if (connection === "close") {
        clearTimeout(timeout);
        if (done) {
          resolvePromise();
          return;
        }
        const statusCode = lastDisconnect?.error?.output?.statusCode;
        if (statusCode === DisconnectReason.loggedOut) {
          console.error("Session expired. Re-link via Settings.");
          rmSync(AUTH_DIR, { recursive: true, force: true });
          reject(new Error("Session expired"));
        } else if (retries < MAX_RETRIES) {
          retries++;
          connect().then(resolvePromise).catch(reject);
        } else {
          reject(new Error(`Connection failed after ${MAX_RETRIES} retries (status: ${statusCode})`));
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
