//! SQLite persistence, 1:1 port of `ms/sim-server/database.js`
//! (and the original `server/ms_server.py Database`): identical schema,
//! identical PRAGMAs (WAL / synchronous=NORMAL / busy_timeout=5000 /
//! cache_size=-16000), in-memory counters and the auto-migration for
//! `sim_games.requester`.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const SCHEMA: &str = "
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
";

pub fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Clone, Debug)]
pub struct GameRow {
    pub ts: i64,
    pub difficulty: String,
    pub seed: u64,
    pub won: bool,
    pub moves: usize,
    pub time_ms: usize,
    pub guesses: usize,
    pub chords: usize,
    pub flags: usize,
    pub deduce_batches: usize,
    pub frontier: Vec<(usize, usize, f64)>,
    pub wall_ms: usize,
    pub requester: Option<String>,
}

pub struct Database {
    conn: Mutex<Connection>,
    games: AtomicU64,
    wins: AtomicU64,
    metrics: AtomicU64,
}

impl Database {
    pub fn new(db_path: &str) -> rusqlite::Result<Database> {
        if db_path != ":memory:" {
            let p = Path::new(db_path);
            if let Some(dir) = p.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir).ok();
                }
            }
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.busy_timeout(std::time::Duration::from_millis(5000)).ok();
        conn.pragma_update(None, "cache_size", -16000_i64).ok();
        conn.execute_batch(SCHEMA)?;
        // auto-migration for databases created before the requester column
        let migrated = conn
            .execute("ALTER TABLE sim_games ADD COLUMN requester TEXT", [])
            .is_ok();
        let _ = migrated;
        let games = conn
            .query_row(
                "SELECT COUNT(*) FROM sim_games",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let wins = conn
            .query_row(
                "SELECT COALESCE(SUM(won),0) FROM sim_games",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let metrics = conn
            .query_row(
                "SELECT COUNT(*) FROM client_metrics",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        Ok(Database {
            conn: Mutex::new(conn),
            games: AtomicU64::new(games as u64),
            wins: AtomicU64::new(wins as u64),
            metrics: AtomicU64::new(metrics as u64),
        })
    }

    pub fn record_game(&self, g: &GameRow) {
        let frontier_json = frontier_to_json(&g.frontier);
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sim_games(ts,difficulty,seed,won,moves,time_ms,guesses,chords,flags,deduce_batches,frontier,wall_ms,requester) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                g.ts,
                g.difficulty,
                g.seed as i64,
                if g.won { 1 } else { 0 },
                g.moves as i64,
                g.time_ms as i64,
                g.guesses as i64,
                g.chords as i64,
                g.flags as i64,
                g.deduce_batches as i64,
                frontier_json,
                g.wall_ms as i64,
                g.requester.clone().unwrap_or_default()
            ],
        )
        .expect("insert sim_game");
        self.games.fetch_add(1, Ordering::Relaxed);
        if g.won {
            self.wins.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_metric(&self, ts: i64, addr: &str, line: &str) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO client_metrics(ts,addr,line) VALUES(?1,?2,?3)",
            params![ts, addr, line],
        )
        .expect("insert metric");
        self.metrics.fetch_add(1, Ordering::Relaxed);
    }

    pub fn upsert_client(&self, addr: &str, connect_ts: i64, active: bool) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO clients(addr,connect_ts,last_ts,seeds_sent,outcomes_sent,active) VALUES(?1,?2,?2,0,0,?3) ON CONFLICT(addr) DO UPDATE SET active=?3",
            params![addr, connect_ts, if active { 1 } else { 0 }],
        )
        .expect("upsert client");
    }

    pub fn client_touch(&self, addr: &str, seeds: u64, outcomes: u64) {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE clients SET last_ts=?1, seeds_sent=?2, outcomes_sent=?3 WHERE addr=?4",
            params![now_sec(), seeds as i64, outcomes as i64, addr],
        )
        .expect("touch client");    }

    pub fn client_touch_many(&self, rows: &[(String, u64, u64)]) {
        if rows.is_empty() {
            return;
        }
        let now = now_sec();
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction().expect("begin tx");
        for (a, s, o) in rows {
            tx.execute(
                "UPDATE clients SET last_ts=?1, seeds_sent=?2, outcomes_sent=?3 WHERE addr=?4",
                params![now, *s as i64, *o as i64, a],
            )
            .expect("touch client row");
        }
        tx.commit().expect("commit tx");
    }

    pub fn counts(&self) -> (i64, i64, i64, i64) {
        let active = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM clients WHERE active=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0);
        (
            self.games.load(Ordering::Relaxed) as i64,
            self.wins.load(Ordering::Relaxed) as i64,
            self.metrics.load(Ordering::Relaxed) as i64,
            active,
        )
    }

    /// Returns `(improved, rank)` where rank counts strictly-faster scores plus
    /// ties broken by insertion order, exactly like database.js record_score.
    pub fn record_score(&self, name: &str, diff: &str, time_ms: u64) -> (bool, u64) {
        let c = self.conn.lock().unwrap();
        let cur: Option<(i64, u64)> = c
            .query_row(
                "SELECT id, time_ms FROM leaderboard WHERE name=?1 AND difficulty=?2",
                params![name, diff],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()
            .expect("score get");
        let improved;
        let row_id: i64;
        match cur {
            Some((id, cur_ms)) if cur_ms <= time_ms => {
                improved = false;
                row_id = id;
            }
            Some((id, _)) => {
                c.execute(
                    "UPDATE leaderboard SET time_ms=?1, ts=?2 WHERE id=?3",
                    params![time_ms as i64, now_sec(), id],
                )
                .expect("score update");
                improved = true;
                row_id = id;
            }
            None => {
                c.execute(
                    "INSERT INTO leaderboard(name,difficulty,time_ms,ts) VALUES(?1,?2,?3,?4)",
                    params![name, diff, time_ms as i64, now_sec()],
                )
                .expect("score insert");
                row_id = c.last_insert_rowid();
                improved = true;
            }
        }
        let best: (u64, i64) = c
            .query_row(
                "SELECT time_ms, id FROM leaderboard WHERE name=?1 AND difficulty=?2",
                params![name, diff],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .expect("score best");
        let best_ms = best.0 as i64;
        let best_id = best.1;
        let below: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM leaderboard WHERE difficulty=?1 AND time_ms < ?2",
                params![diff, best_ms],
                |r| r.get(0),
            )
            .expect("count below");
        let tied: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM leaderboard WHERE difficulty=?1 AND time_ms = ?2 AND id <= ?3",
                params![diff, best_ms, best_id],
                |r| r.get(0),
            )
            .expect("count tied");
        let _ = row_id;
        (improved, (below + tied) as u64)
    }

    /// `(rank, name, difficulty, time_ms, ts)` per entry, rank counted within
    /// each difficulty like database.js top_scores.
    pub fn top_scores(&self, diff: Option<&str>, limit: u64) -> Vec<(u64, String, String, u64, u64)> {
        let c = self.conn.lock().unwrap();
        let mut rows = Vec::new();
        let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        if let Some(d) = diff {
            let mut stmt = c
                .prepare(
                    "SELECT name, difficulty, time_ms, ts FROM leaderboard WHERE difficulty=?1 ORDER BY time_ms, id LIMIT ?2",
                )
                .expect("top diff stmt");
            let iter = stmt
                .query_map(params![d, limit as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64))
                })
                .expect("query diff");
            for row in iter {
                let (name, d2, ms, ts) = row.expect("row");
                let rank = counts.entry(d2.clone()).or_insert(0);
                *rank += 1;
                rows.push((*rank, name, d2, ms, ts));
            }
        } else {
            let mut stmt = c
                .prepare(
                    "SELECT name, difficulty, time_ms, ts FROM leaderboard ORDER BY difficulty, time_ms, id LIMIT ?1",
                )
                .expect("top all stmt");
            let iter = stmt
                .query_map(params![limit as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64))
                })
                .expect("query all");
            for row in iter {
                let (name, d, ms, ts) = row.expect("row");
                let rank = counts.entry(d.clone()).or_insert(0);
                *rank += 1;
                rows.push((*rank, name, d, ms, ts));
            }
        }
        rows
    }
}

/// JS `JSON.stringify([[a,b,p], ...])`-compatible (shortest round-trip floats).
fn frontier_to_json(frontier: &[(usize, usize, f64)]) -> String {
    let mut out = String::from("[");
    for (i, (a, b, p)) in frontier.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        out.push_str(&a.to_string());
        out.push(',');
        out.push_str(&b.to_string());
        out.push(',');
        out.push_str(&fmt_float(*p));
        out.push(']');
    }
    out.push(']');
    out
}

fn fmt_float(v: f64) -> String {
    if v.is_nan() || v.is_infinite() {
        return "null".to_string();
    }
    format!("{}", v)
}
