//! Shared mutable client state: the game engine plus the telemetry/link state
//! that the GUI, the `--listen` scripting server and the telemetry task all
//! touch through one `Arc<Mutex<Core>>`.

use crate::engine::Game;
use std::collections::VecDeque;

pub const AUTH_NONE: u8 = 0;
pub const AUTH_WAIT_CHAL: u8 = 1;
pub const AUTH_WAIT_OK: u8 = 2;
pub const AUTH_OK: u8 = 3;

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
        Core {
            game: Game::new(),
            host: "135.125.79.15".to_string(),
            port: 28571,
            solver_user: String::new(),
            solver_pass: String::new(),
            telemetry_on: true,
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
