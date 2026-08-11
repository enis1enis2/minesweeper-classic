// database.js - SQLite persistence for the simulation server.
//
// 1:1 port of server/ms_server.py Database.  Same schema, same PRAGMAs
// (WAL / synchronous=NORMAL / busy_timeout=5000 / cache_size=-16000), same
// in-memory counters, same auto-migration for the sim_games.requester column.

import fs from "node:fs";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";

export const SCHEMA = `
CREATE TABLE IF NOT EXISTS sim_games(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    difficulty TEXT NOT NULL,
    seed INTEGER NOT NULL,
    won INTEGER NOT NULL,
    moves INTEGER NOT NULL,
    time_ms INTEGER NOT NULL,
    guesses INTEGER NOT NULL,
    chords INTEGER NOT NULL,
    flags INTEGER NOT NULL,
    deduce_batches INTEGER NOT NULL,
    frontier TEXT,
    wall_ms INTEGER NOT NULL,
    requester TEXT
);
CREATE TABLE IF NOT EXISTS leaderboard(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    difficulty TEXT NOT NULL,
    time_ms INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    UNIQUE(name, difficulty)
);
CREATE INDEX IF NOT EXISTS idx_leaderboard_diff ON
    leaderboard(difficulty, time_ms);
CREATE TABLE IF NOT EXISTS client_metrics(
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    addr TEXT NOT NULL,
    line TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_metrics_ts ON client_metrics(ts);
CREATE TABLE IF NOT EXISTS clients(
    addr TEXT PRIMARY KEY,
    connect_ts INTEGER NOT NULL,
    last_ts INTEGER NOT NULL,
    seeds_sent INTEGER NOT NULL,
    outcomes_sent INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1
);
`;

const nowSec = () => Math.floor(Date.now() / 1000);

export class Database {
  constructor(dbPath) {
    if (dbPath !== ":memory:") {
      const dir = path.dirname(dbPath);
      if (dir) fs.mkdirSync(dir, { recursive: true });
    }
    this.conn = new DatabaseSync(dbPath);
    this.conn.exec("PRAGMA journal_mode=WAL");
    this.conn.exec("PRAGMA synchronous=NORMAL");
    this.conn.exec("PRAGMA busy_timeout=5000");
    this.conn.exec("PRAGMA cache_size=-16000");
    this.conn.exec(SCHEMA);
    // migration for databases created before the requester column
    try {
      this.conn.exec("ALTER TABLE sim_games ADD COLUMN requester TEXT");
    } catch (e) {
      // only the "already migrated" duplicate-column error is expected;
      // anything else must propagate (Python: except sqlite3.OperationalError)
      if (!(e && /duplicate column name/i.test(String(e.message)))) throw e;
    }
    const g = this.conn
      .prepare("SELECT COUNT(*) AS n, COALESCE(SUM(won),0) AS w FROM sim_games")
      .get();
    const m = this.conn.prepare("SELECT COUNT(*) AS n FROM client_metrics").get();
    this._games = g.n;
    this._wins = g.w;
    this._metrics = m.n;

    this._stmtGame = this.conn.prepare(
      "INSERT INTO sim_games(ts,difficulty,seed,won,moves,time_ms," +
        "guesses,chords,flags,deduce_batches,frontier,wall_ms,requester) " +
        "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)"
    );
    this._stmtMetric = this.conn.prepare(
      "INSERT INTO client_metrics(ts,addr,line) VALUES(?,?,?)"
    );
    this._stmtUpsertClient = this.conn.prepare(
      "INSERT INTO clients(addr,connect_ts,last_ts,seeds_sent," +
        "outcomes_sent,active) VALUES(?,?,?,0,0,?) " +
        "ON CONFLICT(addr) DO UPDATE SET active=?"
    );
    this._stmtTouch = this.conn.prepare(
      "UPDATE clients SET last_ts=?, seeds_sent=?, outcomes_sent=? WHERE addr=?"
    );
    this._stmtClientsActive = this.conn.prepare(
      "SELECT COUNT(*) AS n FROM clients WHERE active=1"
    );
    this._stmtScoreGet = this.conn.prepare(
      "SELECT id, time_ms FROM leaderboard WHERE name=? AND difficulty=?"
    );
    this._stmtScoreUpdate = this.conn.prepare(
      "UPDATE leaderboard SET time_ms=?, ts=? WHERE id=?"
    );
    this._stmtScoreInsert = this.conn.prepare(
      "INSERT INTO leaderboard(name,difficulty,time_ms,ts) VALUES(?,?,?,?)"
    );
    this._stmtScoreBest = this.conn.prepare(
      "SELECT time_ms, id FROM leaderboard WHERE name=? AND difficulty=?"
    );
    this._stmtScoreBelow = this.conn.prepare(
      "SELECT COUNT(*) AS n FROM leaderboard WHERE difficulty=? AND time_ms < ?"
    );
    this._stmtScoreTied = this.conn.prepare(
      "SELECT COUNT(*) AS n FROM leaderboard WHERE difficulty=? AND time_ms = ? AND id <= ?"
    );
    this._stmtTopAll = this.conn.prepare(
      "SELECT name, difficulty, time_ms, ts FROM leaderboard " +
        "ORDER BY difficulty, time_ms, id LIMIT ?"
    );
    this._stmtTopDiff = this.conn.prepare(
      "SELECT name, difficulty, time_ms, ts FROM leaderboard " +
        "WHERE difficulty=? ORDER BY time_ms, id LIMIT ?"
    );
  }

  record_game(g) {
    this._stmtGame.run(
      g.ts,
      g.difficulty,
      g.seed,
      g.won ? 1 : 0,
      g.moves,
      g.time_ms,
      g.guesses,
      g.chords,
      g.flags,
      g.deduce_batches,
      JSON.stringify(g.frontier),
      g.wall_ms,
      g.requester ?? null
    );
    this._games += 1;
    this._wins += g.won ? 1 : 0;
  }

  record_metric(ts, addr, line) {
    this._stmtMetric.run(ts, addr, line);
    this._metrics += 1;
  }

  upsert_client(addr, connectTs, active = true) {
    this._stmtUpsertClient.run(
      addr,
      connectTs,
      connectTs,
      active ? 1 : 0,
      active ? 1 : 0
    );
  }

  client_touch(addr, seeds, outcomes) {
    this._stmtTouch.run(nowSec(), seeds, outcomes, addr);
  }

  client_touch_many(rows) {
    if (!rows.length) return;
    const now = nowSec();
    this.conn.exec("BEGIN");
    try {
      for (const [a, s, o] of rows) this._stmtTouch.run(now, s, o, a);
      this.conn.exec("COMMIT");
    } catch (e) {
      this.conn.exec("ROLLBACK");
      throw e;
    }
  }

  counts() {
    const c = this._stmtClientsActive.get();
    return [[this._games, this._wins], [this._metrics], c.n];
  }

  record_score(name, diff, time_ms) {
    const cur = this._stmtScoreGet.get(name, diff);
    let improved;
    let rowId;
    if (cur !== undefined && cur.time_ms <= time_ms) {
      improved = false;
      rowId = cur.id;
    } else {
      if (cur !== undefined) {
        this._stmtScoreUpdate.run(time_ms, nowSec(), cur.id);
        rowId = cur.id;
      } else {
        const r = this._stmtScoreInsert.run(name, diff, time_ms, nowSec());
        rowId = r.lastInsertRowid;
      }
      improved = true;
    }
    const best = this._stmtScoreBest.get(name, diff);
    const bestMs = best.time_ms;
    const bestId = best.id;
    const below = this._stmtScoreBelow.get(diff, bestMs).n;
    const tied = this._stmtScoreTied.get(diff, bestMs, bestId).n;
    return [improved, below + tied];
  }

  top_scores(diff, limit) {
    const rows =
      diff === null
        ? this._stmtTopAll.all(limit)
        : this._stmtTopDiff.all(diff, limit);
    const out = [];
    const counts = {};
    for (const r of rows) {
      counts[r.difficulty] = (counts[r.difficulty] || 0) + 1;
      out.push([counts[r.difficulty], r.name, r.difficulty, r.time_ms, r.ts]);
    }
    return out;
  }

  close() {
    this.conn.close();
  }
}
