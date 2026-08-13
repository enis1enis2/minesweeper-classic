//! Shared mutable client state: the game engine plus the telemetry/link state
//! that the GUI, the `--listen` scripting server and the telemetry task all
//! touch through one `Arc<Mutex<Core>>`.

use crate::engine::Game;
use std::collections::VecDeque;

pub const AUTH_NONE: u8 = 0;
pub const AUTH_WAIT_CHAL: u8 = 1;
pub const AUTH_WAIT_OK: u8 = 2;
pub const AUTH_OK: u8 = 3;

/* Default telemetry endpoint, stored base64 so the deployed server address
 * is not a readable string in the source/binary.  Obfuscation only — the
 * value is recovered at runtime and sent in the clear on the telemetry
 * link; --telemetry / --no-telemetry still override it. */
const DEFAULT_HOST_B64: &str = "MTM1LjEyNS43OS4xNQ=="; /* 135.125.79.15 */
const DEFAULT_PORT_B64: &str = "Mjg1NzE=";              /* 28571 */

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn b64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let Some(d) = b64_val(c) else {
            continue;
        };
        v = (v << 6) | d;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((v >> bits) & 0xFF) as u8);
        }
    }
    out
}

pub fn default_endpoint() -> (String, u16) {
    let host = String::from_utf8_lossy(&b64_decode(DEFAULT_HOST_B64)).into_owned();
    let port = String::from_utf8_lossy(&b64_decode(DEFAULT_PORT_B64))
        .parse::<u16>()
        .unwrap_or(28571);
    (host, port)
}

#[derive(Clone, Debug)]
pub struct LbEntry {
    pub rank: u32,
    pub diff: String,
    pub name: String,
    pub time_ms: u32,
}

pub struct Core {
    pub game: Game,
    pub host: String,
    pub port: u16,
    pub solver_user: String,
    pub solver_pass: String,
    pub telemetry_on: bool,
    /// Use TLS for the telemetry link (`--tls`).
    pub tls: bool,
    /// Use the HTTP(S) `/ms-sim/*` transport instead of the raw streaming
    /// protocol (`--http`). With `--tls` this is HTTPS (native rustls).
    pub http: bool,
    /// Optional PEM CA bundle to trust in addition to the system roots
    /// (`--tls-ca FILE`), e.g. a self-signed or private CA.
    pub tls_ca: Option<String>,
    pub connected: bool,
    pub attempts: u64,
    pub seeds_recv: u64,
    pub outcomes_recv: u64,
    pub wins_recv: u64,
    pub metrics_sent: u64,
    pub metrics_dropped: u64,
    pub sim_session: bool,
    pub auth_state: u8,
    pub leaderboard: Vec<LbEntry>,
    pub lb_status: String,
    pub pending_seed_applies: VecDeque<(usize, u64)>,
    pub solver_denied: bool,
    pub player_name: String,
    pub auto_submit: bool,
    pub last_latency_sent_sec: i64,
}

impl Core {
    pub fn new() -> Self {
        let (host, port) = default_endpoint();
        Core {
            game: Game::new(),
            host,
            port,
            solver_user: String::new(),
            solver_pass: String::new(),
            telemetry_on: true,
            tls: false,
            http: false,
            tls_ca: None,
            connected: false,
            attempts: 0,
            seeds_recv: 0,
            outcomes_recv: 0,
            wins_recv: 0,
            metrics_sent: 0,
            metrics_dropped: 0,
            sim_session: false,
            auth_state: AUTH_NONE,
            leaderboard: Vec::new(),
            lb_status: "Telemetry is off.".to_string(),
            pending_seed_applies: VecDeque::new(),
            solver_denied: false,
            player_name: "Player".to_string(),
            auto_submit: true,
            last_latency_sent_sec: 0,
        }
    }

    pub fn solver_wanted(&self) -> bool {
        !self.solver_user.is_empty() && !self.solver_pass.is_empty()
    }

    pub fn solver_ready(&self) -> bool {
        self.solver_wanted() && self.auth_state == AUTH_OK
    }

    /// Drain server-pushed seeds that arrived during an active remote-sim
    /// session (the g_sim_session gate). Each becomes a persistent Normal
    /// slot and the current board resets, matching the C WM_APP_TELEMETRY_SEED
    /// handler.
    pub fn apply_pending_seeds(&mut self) {
        while let Some((diff, seed)) = self.pending_seed_applies.pop_front() {
            if diff < crate::engine::DIFF_COUNT {
                use crate::engine::{SeedSlot, SEED_NORMAL};
                self.game.slots[diff] = SeedSlot { mode: SEED_NORMAL, value: seed.to_string() };
                self.game.reset(self.game.diff);
            }
        }
    }

    /// Format the leaderboard status line the way the C Hall of Fame does.
    pub fn set_lb_status_from_entries(&mut self) {
        let n = self.leaderboard.len();
        self.lb_status = if !self.connected {
            "Telemetry is off.".to_string()
        } else if n == 0 {
            "No scores yet.".to_string()
        } else if n == 1 {
            "1 entry".to_string()
        } else {
            format!("{} entries", n)
        };
    }
}
