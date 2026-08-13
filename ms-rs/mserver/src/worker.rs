//! One simulated game (port of `ms/sim-server/worker.js` simulateGame). Runs
//! on a worker-pool OS thread; the mscore solver does the playing and the
//! result is packaged into the same DB row shape the JS server writes.

use crate::config::{SOLVER_FIRST, SOLVER_REFRESH, SOLVER_TIEBREAK, SOLVER_USE_CHORD};
use crate::db::{GameRow, now_sec};
use mscore::mt19937::Mt19937;
use mscore::sim_board::SimBoard;
use mscore::solver::{Strategy, play_game};
use std::time::Instant;

// server/ms_server.py SOLVER_STRATEGY verbatim (config.js).
fn solver_strategy() -> Strategy {
    Strategy {
        tiebreak: SOLVER_TIEBREAK.to_string(),
        first: SOLVER_FIRST.to_string(),
        use_chord: SOLVER_USE_CHORD,
        refresh: SOLVER_REFRESH,
    }
}

pub struct Task {
    pub diff: String,
    pub seed: u64,
    /// When None the game is a requested game: rng = Random(seed ^ run<<32).
    /// When Some it is a broadcast game: rng starts from this decision seed.
    pub decision_seed: Option<u64>,
    /// Broadcast games carry the shared decision RNG as a snapshot so the
    /// producer can resume the exact stream.
    pub rng_state: Option<(Vec<u32>, usize)>,
}

pub struct TaskResult {
    pub g: GameRow,
    pub rng_state: Option<(Vec<u32>, usize)>,
}

pub fn run_game(task: &Task) -> Result<TaskResult, String> {
    let mut board = SimBoard::new(true);
    board.new_game(&task.diff, task.seed).map_err(|e| e)?;
    let mut rng = match &task.rng_state {
        Some((state, index)) => Mt19937::from_state(state, *index),
        None => {
            let mut r = Mt19937::new();
            let seed = task.decision_seed.ok_or("requested game missing decision seed")?;
            r.seed_u64(seed);
            r
        }
    };
    let strategy = solver_strategy();
    let t0 = Instant::now();
    let res = play_game(&mut board, &task.diff, &strategy, &mut rng, 2_000_000);
    let wall_ms = t0.elapsed().as_millis() as usize;

    let g = GameRow {
        ts: now_sec(),
        difficulty: res.difficulty,
        seed: task.seed,
        won: res.win,
        moves: res.moves,
        time_ms: res.time,
        guesses: res.guesses,
        chords: res.chords,
        flags: res.flags,
        deduce_batches: res.deduce_batches,
        frontier: res.frontier,
        wall_ms,
        requester: None,
    };
    let rng_state = task.rng_state.as_ref().map(|_| rng.snapshot());
    Ok(TaskResult { g, rng_state })
}
