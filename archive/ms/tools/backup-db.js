// backup-db.js - nightly consistent backup of sim.db using node:sqlite.
//
// Replaces server/backup_db.py (which ran under the removed Python venv).
// Safe to run while the server is live (WAL): VACUUM INTO produces a
// consistent snapshot file, gzip-compressed and pruned to the newest KEEP.
//
// Env: MS_DB (default /var/lib/minesweeper-sim/sim.db),
//      MS_BACKUP_DIR (default <db dir>/backups), MS_BACKUP_KEEP (default 14).

import fs from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { DatabaseSync } from "node:sqlite";

const DB = process.env.MS_DB || "/var/lib/minesweeper-sim/sim.db";
const BACKUP_DIR =
  process.env.MS_BACKUP_DIR || path.join(path.dirname(DB), "backups");
const KEEP = Number(process.env.MS_BACKUP_KEEP || 14);

const quote = (p) => "'" + String(p).replace(/'/g, "''") + "'";
const p2 = (n) => String(n).padStart(2, "0");

async function main() {
  fs.mkdirSync(BACKUP_DIR, { recursive: true });
  const d = new Date();
  const stamp =
    "" +
    d.getFullYear() +
    p2(d.getMonth() + 1) +
    p2(d.getDate()) +
    "-" +
    p2(d.getHours()) +
    p2(d.getMinutes()) +
    p2(d.getSeconds());
  const raw = path.join(BACKUP_DIR, `sim-${stamp}.db`);
  const gz = raw + ".gz";

  const conn = new DatabaseSync(DB);
  try {
    conn.exec(`VACUUM INTO ${quote(raw)}`);
  } finally {
    conn.close();
  }

  await compress(raw, gz);
  fs.unlinkSync(raw);

  const old = fs
    .readdirSync(BACKUP_DIR)
    .filter((f) => /^sim-\d{8}-\d{6}\.db\.gz$/.test(f))
    .sort();
  for (const f of old.slice(0, Math.max(0, old.length - KEEP))) {
    fs.unlinkSync(path.join(BACKUP_DIR, f));
  }
  console.log(`backup ok: ${gz} (${Math.min(old.length, KEEP)} backups retained)`);
}

function compress(src, dst) {
  return new Promise((resolve, reject) => {
    fs.createReadStream(src)
      .pipe(zlib.createGzip({ level: 6 }))
      .pipe(fs.createWriteStream(dst))
      .on("close", resolve)
      .on("error", reject);
  });
}

main().catch((e) => {
  console.error("backup FAILED: " + e.message);
  process.exitCode = 1;
});
