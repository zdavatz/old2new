#!/usr/bin/env node
// List WhatsApp recipients the linked account knows about — both
// groups (via `groupFetchAllParticipating`) and 1:1 contacts (via
// `messaging-history.set` + `contacts.upsert` events that Baileys
// emits during the post-link app-state-sync).
//
// Output: one `JID|TYPE|NAME` line per recipient on stdout, where
// TYPE is `group` or `contact`. Groups first (alphabetical), then
// contacts (alphabetical). The GUI parses this and shows it in a
// modal picker.
//
// We deliberately leave `syncFullHistory: false` — that flag controls
// whether *message* history downloads (slow, lots of data); contacts
// are pushed regardless via app-state-sync.
//
// Reconnects up to 3 times on any non-loggedOut close — first-fetch
// right after a fresh link can hit a transient 515 (restartRequired)
// or undefined-status close.

import makeWASocket, {
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  fetchLatestBaileysVersion,
} from "@whiskeysockets/baileys";
import pino from "pino";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const AUTH_DIR = resolve(__dirname, "auth");
const logger = pino({ level: "silent" });
// Time to wait after `connection: open` for app-state-sync events to
// flush in contacts. Empirically 5–8 s is plenty on a warm session;
// we use 6 s as a balance between latency and completeness.
const CONTACT_SYNC_MS = 6000;

function pickName(ct) {
  return (ct.name || ct.notify || ct.verifiedName || "").trim();
}

async function fetchOnce() {
  const { state, saveCreds } = await useMultiFileAuthState(AUTH_DIR);
  const { version } = await fetchLatestBaileysVersion();

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

  // Accumulate contacts across all events. Map<jid, displayName>.
  const contacts = new Map();
  const recordContact = (ct) => {
    if (!ct?.id) return;
    if (!ct.id.endsWith("@s.whatsapp.net")) return; // only 1:1
    const name = pickName(ct);
    const prev = contacts.get(ct.id);
    // Prefer non-empty names; otherwise keep whatever's there.
    if (name || !prev) contacts.set(ct.id, name || prev || "");
  };

  sock.ev.on("messaging-history.set", ({ contacts: c }) => {
    for (const ct of c || []) recordContact(ct);
  });
  sock.ev.on("contacts.upsert", (cs) => { for (const ct of cs) recordContact(ct); });
  sock.ev.on("contacts.update", (cs) => { for (const ct of cs) recordContact(ct); });

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
      finish({ ok: false, msg: "Connection timeout (60s)" });
    }, 60000);

    sock.ev.on("connection.update", async (update) => {
      const { connection, lastDisconnect } = update;
      if (connection === "open") {
        // Give app-state-sync time to push contacts in.
        await new Promise((r) => setTimeout(r, CONTACT_SYNC_MS));
        let groups;
        try {
          groups = await sock.groupFetchAllParticipating();
        } catch (e) {
          finish({ ok: false, msg: `groupFetchAllParticipating failed: ${e.message}` });
          setTimeout(() => { try { sock.end(); } catch (_) {} }, 250);
          return;
        }
        const groupList = Object.values(groups).sort((a, b) =>
          (a.subject || "").localeCompare(b.subject || ""),
        );
        const contactList = Array.from(contacts.entries())
          .map(([id, name]) => ({ id, name }))
          .sort((a, b) => {
            // Sort: named contacts first (alphabetical), then unnamed
            // (by JID). Unnamed entries are usually phone numbers we
            // never had in the address book — show them last.
            if (!!a.name === !!b.name) {
              return (a.name || a.id).localeCompare(b.name || b.id);
            }
            return a.name ? -1 : 1;
          });
        finish({ ok: true, groups: groupList, contacts: contactList });
        setTimeout(() => { try { sock.end(); } catch (_) {} }, 250);
        return;
      }
      if (connection === "close") {
        const code = lastDisconnect?.error?.output?.statusCode;
        finish({ ok: false, code, msg: lastDisconnect?.error?.message || "unknown" });
      }
    });
  });
}

async function main() {
  let result;
  for (let attempt = 1; attempt <= 3; attempt++) {
    result = await fetchOnce();
    if (result.ok) break;
    if (result.code === 401 || result.code === 403) break;
    await new Promise((r) => setTimeout(r, 300));
  }
  if (!result.ok) {
    throw new Error(`${result.msg} (code ${result.code ?? "?"})`);
  }
  for (const g of result.groups) {
    const name = (g.subject || "").replace(/[\r\n|]/g, " ");
    console.log(`${g.id}|group|${name}`);
  }
  for (const c of result.contacts) {
    const name = (c.name || "").replace(/[\r\n|]/g, " ");
    console.log(`${c.id}|contact|${name}`);
  }
}

main()
  .then(() => process.exit(0))
  .catch((err) => {
    console.error("Error:", err.message);
    process.exit(1);
  });
