//! Server constants, 1:1 ports of `ms/sim-server/config.js`.

pub const DIFFS: [&str; 3] = ["beginner", "intermediate", "expert"];

pub const SOLVER_TIEBREAK: &str = "info";
pub const SOLVER_FIRST: &str = "center";
pub const SOLVER_USE_CHORD: bool = true;
pub const SOLVER_REFRESH: bool = false;

// Estimated CPU seconds per simulated game (production box figures, unchanged).
pub const GAME_CPU_SECONDS: [f64; 3] = [0.002, 0.016, 0.076]; // beginner, intermediate, expert
pub const HEAVY_CPU_SECONDS: f64 = 0.25;

pub const NONCE_TTL: i64 = 60;
pub const MAX_AUTH_FAILS: i64 = 5;
pub const MAX_LINE: usize = 65536;
pub const LB_WINDOW: f64 = 60.0;
pub const LB_MAX: usize = 20;
pub const LB_MAX_IPS: usize = 4096;
// NAME_RE = /^[A-Za-z0-9_-]{1,16}$/ (implemented as is_name() in protocol.rs).

pub fn game_cpu_seconds(diff: &str) -> f64 {
    match diff {
        "beginner" => GAME_CPU_SECONDS[0],
        "intermediate" => GAME_CPU_SECONDS[1],
        "expert" => GAME_CPU_SECONDS[2],
        _ => 0.0,
    }
}

pub fn is_difficulty(d: &str) -> bool {
    DIFFS.contains(&d)
}
