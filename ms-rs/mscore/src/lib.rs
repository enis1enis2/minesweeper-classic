//! mscore - the bit-exact core of the Rust Minesweeper rewrite.
//!
//! Every public type here is a 1:1 port of the authoritative engines:
//!   * `rng64`      <- src/minesweeper.c xorshift64 / server/sim_engine.py / ms/core/sim-engine.js
//!   * `sim_board`  <- the same three systems' board placement & click logic
//!   * `mt19937`    <- ms/core/mt19937.js (CPython random.Random compatible)
//!   * `solver`     <- ms/core/solver.js (deduction + exact frontier probabilities)

pub mod mt19937;
pub mod rng64;
pub mod sim_board;
pub mod solver;

pub use mt19937::Mt19937;
pub use rng64::{to_u64, Rng64, ZERO_SEED_FALLBACK};
pub use sim_board::SimBoard;
