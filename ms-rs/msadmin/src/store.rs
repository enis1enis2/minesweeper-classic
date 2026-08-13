use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::crypt::{safe_equal, verify_password};
use crate::totp::{random_token, totp_verify};

const FAILURE_WINDOW_SECS: i64 = 900;
const MAX_FAILURES: usize = 5;

pub struct DiagDB {
    conn: Mutex<Connection>,
}

impl DiagDB {
    pub fn new(path: &str) -> Result<DiagDB, String> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()
        } else {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create db dir: {e}"))?;
                }
            }
            Connection::open(path)
        }
        .map_err(|e| format!("cannot open db: {e}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("cannot set WAL: {e}"))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("cannot set synchronous: {e}"))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("cannot set busy timeout: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS device_diagnostics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                addr TEXT NOT NULL,
                blob BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_diag_ts ON device_diagnostics (ts);",
        )
        .map_err(|e| format!("cannot create schema: {e}"))?;
        Ok(DiagDB {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, ts: i64, addr: &str, blob: &[u8]) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO device_diagnostics (ts, addr, blob) VALUES (?1, ?2, ?3)",
            params![ts, addr, blob],
        )
        .map_err(|e| format!("insert failed: {e}"))?;
        Ok(conn.last_insert_rowid())
    }

    pub fn stats(&self) -> (i64, i64) {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM device_diagnostics", [], |r| r.get(0))
            .unwrap_or(0);
        let cutoff = crate::http::unix_now() - 24 * 3600;
        let recent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM device_diagnostics WHERE ts > ?1",
                [cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (total, recent)
    }

    pub fn recent_rows(&self, limit: usize) -> Vec<(i64, i64, String, Vec<u8>)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT ts, id, addr, blob FROM device_diagnostics ORDER BY ts DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map(|it| it.filter_map(|x| x.ok()).collect())
            .unwrap_or_default();
        rows
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LoginResult {
    pub ok: bool,
    pub reason: &'static str,
}

pub struct AuthStore {
    pub username: String,
    pub password_hash: String,
    pub totp_secret_b32: String,
    pub session_ttl_sec: i64,
    sessions: HashMap<String, (i64, String)>,
    epoch: u64,
    failures: HashMap<String, Vec<i64>>,
    locked_until: HashMap<String, i64>,
}

impl AuthStore {
    pub fn new(config_path: &str) -> Result<AuthStore, String> {
        let data = std::fs::read_to_string(config_path)
            .map_err(|e| format!("cannot read config '{config_path}': {e}"))?;
        let v: Value = serde_json::from_str(&data)
            .map_err(|e| format!("invalid config JSON: {e}"))?;
        Ok(AuthStore {
            username: v.get("username").and_then(Value::as_str).unwrap_or("admin").to_string(),
            password_hash: v
                .get("password_hash")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            totp_secret_b32: v
                .get("totp_secret_b32")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            session_ttl_sec: v
                .get("session_ttl_sec")
                .and_then(Value::as_i64)
                .unwrap_or(14400),
            sessions: HashMap::new(),
            epoch: 0,
            failures: HashMap::new(),
            locked_until: HashMap::new(),
        })
    }

    fn prune(&mut self, now: i64) {
        self.sessions.retain(|_, (expires, _)| *expires > now);
        self.failures.retain(|_, list| {
            list.retain(|t| *t > now - FAILURE_WINDOW_SECS);
            !list.is_empty()
        });
        self.locked_until.retain(|_, until| *until > now);
    }

    fn record_failure(&mut self, now: i64, ip: &str) {
        let list = self.failures.entry(ip.to_string()).or_default();
        list.push(now);
        list.retain(|t| *t > now - FAILURE_WINDOW_SECS);
        if list.len() >= MAX_FAILURES {
            if let Some(last) = list.last() {
                self.locked_until.insert(ip.to_string(), last + FAILURE_WINDOW_SECS);
            }
        }
    }

    fn clear_failures(&mut self, ip: &str) {
        self.failures.remove(ip);
        self.locked_until.remove(ip);
    }

    pub fn check_login(
        &mut self,
        now: i64,
        ip: &str,
        username: &str,
        password: &str,
        code: &str,
    ) -> LoginResult {
        self.prune(now);
        let locked = *self.locked_until.get(ip).unwrap_or(&0);
        if locked > now {
            return LoginResult { ok: false, reason: "too many failed attempts (locked out)" };
        }
        if !safe_equal(username, &self.username) {
            self.record_failure(now, ip);
            return LoginResult { ok: false, reason: "invalid credentials" };
        }
        if !verify_password(&self.password_hash, password) {
            self.record_failure(now, ip);
            return LoginResult { ok: false, reason: "invalid credentials" };
        }
        if !totp_verify(&self.totp_secret_b32, code, 1, now) {
            self.record_failure(now, ip);
            return LoginResult { ok: false, reason: "invalid TOTP code" };
        }
        self.clear_failures(ip);
        LoginResult { ok: true, reason: "" }
    }

    pub fn issue_session(&mut self, now: i64, ip: &str) -> (String, i64) {
        self.prune(now);
        let token = random_token();
        let expires = now + self.session_ttl_sec;
        self.epoch += 1;
        self.sessions.insert(token.clone(), (expires, ip.to_string()));
        (token, expires)
    }

    pub fn validate(&mut self, now: i64, token: &str) -> Option<String> {
        self.prune(now);
        match self.sessions.get(token) {
            Some(&(expires, ref ip)) if expires > now => Some(ip.clone()),
            _ => {
                self.sessions.remove(token);
                None
            }
        }
    }

    pub fn revoke(&mut self, now: i64, token: &str) {
        self.prune(now);
        self.sessions.remove(token);
    }

    pub fn revoke_all(&mut self) {
        self.sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypt::hash_password;
    use crate::totp::encode_base32;

    fn config_json(username: &str, pw: &str, secret: &[u8]) -> String {
        format!(
            r#"{{"username":"{username}","password_hash":"{}","totp_secret_b32":"{}","session_ttl_sec":3600}}"#,
            hash_password(pw).unwrap(),
            encode_base32(secret)
        )
    }

    fn write_tmp(dir: &std::path::Path, name: &str, contents: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn diagdb_insert_stats_rows() {
        let dir = std::env::temp_dir().join(format!("msadmin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = DiagDB::new(&dir.join("diag.db").to_str().unwrap()).unwrap();
        let now = crate::http::unix_now();
        db.insert(now - 100_000, "1.2.3.4", b"blob-a").unwrap();
        db.insert(now, "5.6.7.8", b"blob-b").unwrap();
        let (total, recent) = db.stats();
        assert_eq!(total, 2);
        assert_eq!(recent, 1);
        let rows = db.recent_rows(1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "5.6.7.8");
        assert_eq!(rows[0].3, b"blob-b");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn authstore_login_and_session() {
        let dir = std::env::temp_dir().join(format!("msadmin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pw = "correct horse battery staple xyz";
        let secret = b"12345678901234567890";
        let cfg = write_tmp(&dir, "admin.json", &config_json("admin", pw, secret));
        let mut auth = AuthStore::new(&cfg).unwrap();
        let now = 1_000_000i64;
        let code = crate::totp::totp_value(&encode_base32(secret), now, 6).unwrap();

        let bad = auth.check_login(now, "1.2.3.4", "admin", pw, "000000");
        assert!(!bad.ok);
        assert_eq!(bad.reason, "invalid TOTP code");

        let good = auth.check_login(now, "1.2.3.4", "admin", pw, &code);
        assert!(good.ok);

        let (token, expires) = auth.issue_session(now, "1.2.3.4");
        assert!(expires > now);
        assert_eq!(auth.validate(now, &token).as_deref(), Some("1.2.3.4"));
        assert_eq!(auth.validate(now, "bogus"), None);

        auth.revoke(now, &token);
        assert_eq!(auth.validate(now, &token), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn authstore_lockout_after_five_failures() {
        let dir = std::env::temp_dir().join(format!("msadmin-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pw = "correct horse battery staple xyz";
        let secret = b"12345678901234567890";
        let cfg = write_tmp(&dir, "admin.json", &config_json("admin", pw, secret));
        let mut auth = AuthStore::new(&cfg).unwrap();
        let now = 1_000_000i64;

        for _ in 0..5 {
            let r = auth.check_login(now, "9.9.9.9", "admin", "wrong-password", "000000");
            assert!(!r.ok);
            assert_eq!(r.reason, "invalid credentials");
        }
        let r = auth.check_login(now, "9.9.9.9", "admin", pw, "000000");
        assert!(!r.ok);
        assert_eq!(r.reason, "too many failed attempts (locked out)");

        let code = crate::totp::totp_value(&encode_base32(secret), now + 901, 6).unwrap();
        let r = auth.check_login(now + 901, "9.9.9.9", "admin", pw, &code);
        assert!(r.ok);
        std::fs::remove_dir_all(&dir).ok();
    }
}
