#!/usr/bin/env node
// One-shot WhatsApp linking via Baileys.
//
// Flow:
//   1. Open socket with auth dir → QR code printed (qrcode-terminal ASCII).
//   2. User scans on their phone.
//   3. `creds.update` fires once `state.creds.me.id` is set — that's the
//      definitive "link succeeded" signal, regardless of subsequent
//      connection events.
//   4. WhatsApp servers always close the post-scan socket with status 515
//      (DisconnectReason.restartRequired) — this is normal, NOT a failure.
//      We exit cleanly because the creds are already on disk.
//
// Stale-auth recovery: if the socket is logged out *before* a QR is shown
// (statusCode 401, "Logged out"), the auth dir has stale creds from a
// previous half-finished link. We wipe `auth/` and retry once.

import { rm, mkdir } from "fs/promises";
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

async function attempt({ allowAuthReset }) {
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

  let qrShown = false;
  let linked = false;

  return new Promise((resolvePromise, reject) => {
    const timeout = setTimeout(() => {
      try { sock.end(); } catch (_) {}
      reject(new Error("Link timeout (5 min)"));
    }, 300000);

    sock.ev.on("creds.update", async () => {
      await saveCreds();
      if (!linked && state.creds?.me?.id) {
        // First creds.update with `me.id` set = QR was scanned successfully.
        linked = true;
        console.log("LINKED");
        clearTimeout(timeout);
        // Give the server a beat to receive our ack, then exit. The
        // post-link 515 close that always follows is harmless — auth/
        // already has everything we need for `send-text.mjs`.
        setTimeout(() => process.exit(0), 1500);
      }
    });

    sock.ev.on("connection.update", (update) => {
      const { connection, lastDisconnect, qr } = update;

      if (qr) {
        qrShown = true;
        console.log("\nQR-CODE-BEGIN");
        qrcode.generate(qr, { small: true });
        console.log("QR-CODE-END");
        console.log("Scan with WhatsApp → Settings → Linked Devices → Link a Device");
      }

      if (connection === "close") {
        const statusCode = lastDisconnect?.error?.output?.statusCode;
        // If creds.update has already declared success, the close is the
        // expected post-link 515 — let the exit timer fire.
        if (linked) return;
        clearTimeout(timeout);

        // Stale auth: server rejects creds before showing QR. Wipe and
        // retry once.
        if (
          statusCode === DisconnectReason.loggedOut &&
          !qrShown &&
          allowAuthReset
        ) {
          console.log("Stale auth detected — clearing and retrying.");
          rm(AUTH_DIR, { recursive: true, force: true })
            .then(() => mkdir(AUTH_DIR, { recursive: true }))
            .then(() => attempt({ allowAuthReset: false }))
            .then(resolvePromise, reject);
          return;
        }

        // Status 515 (restartRequired) without `linked` means creds.update
        // hasn't fired yet — wait briefly; saveCreds() may still be
        // flushing. If it doesn't arrive in 2 s, give up.
        if (statusCode === DisconnectReason.restartRequired) {
          setTimeout(() => {
            if (linked) return; // creds.update fired in the meantime
            if (state.creds?.me?.id) {
              console.log("LINKED");
              setTimeout(() => process.exit(0), 500);
              return;
            }
            reject(new Error("Restart required, but no credentials saved (try again)"));
          }, 2000);
          return;
        }

        if (statusCode === DisconnectReason.loggedOut) {
          reject(new Error("Logged out before scan (clear auth dir and try again)"));
          return;
        }
        reject(new Error(`Connection closed (status ${statusCode ?? "?"})`));
      }
    });
  });
}

attempt({ allowAuthReset: true })
  .then(() => process.exit(0))
  .catch((err) => {
    console.error("Error:", err.message);
    process.exit(1);
  });
