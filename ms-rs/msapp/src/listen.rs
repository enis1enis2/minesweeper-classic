//! `--listen <port>` scripting server — a faithful port of the C client's
//! localhost CLI protocol (one newline-delimited command per line, every reply
//! terminated by a line containing exactly `END`, loopback only, TCP_NODELAY,
//! max 8 tokens, max 512-byte lines).
//!
//! The game state lives in the shared `Core`, so commands are dispatched
//! directly against it (the C client marshals them onto the UI thread; here
//! the mutex plays that role).

use crate::core::Core;
use crate::engine::{self, fmt_g12, strtoull_10, DIFF_CUSTOM, DIFF_NAMES};
use crate::telemetry::Telemetry;
use mscore::solver::{build_constraints, frontier_probabilities, Board};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const HELP: &str = "commands: ping | help | new <beginner|intermediate|expert|custom [r c m]> | \
 click <r> <c> | flag <r> <c> | chord <r> <c> | state | board | \
 marks [0|1] | pause | resume | seed [<diff>] <n> | seedcustom [<diff>] <value> | \
 seeds | telemetry [on|off] | reqseed <diff> <n> [count] | \
 reqbatch <diff> <count> | scenarios | refresh [0|1] | quit";

/// Node budget handed to the mscore solver for `scenarios` enumeration.
const SCENARIOS_NODE_BUDGET: u64 = 200_000;

#[derive(Clone)]
pub struct ListenServer {
    core: Arc<Mutex<Core>>,
    telemetry: Option<Telemetry>,
}

impl ListenServer {
    pub fn new(core: Arc<Mutex<Core>>, telemetry: Option<Telemetry>) -> Self {
        ListenServer { core, telemetry }
    }

    /// Serve connections forever (one client at a time, like the C thread).
    pub async fn run(self, listener: TcpListener) {
        loop {
            let (s, _) = match listener.accept().await {
                Ok(x) => x,
                Err(_) => continue,
            };
            let _ = s.set_nodelay(true);
            self.handle_client(s).await;
        }
    }

    async fn handle_client(&self, mut s: TcpStream) {
        let mut line_buf: Vec<u8> = Vec::with_capacity(512);
        loop {
            let mut chunk = [0u8; 256];
            let mut line: Option<Vec<u8>> = None;
            loop {
                let n = match tokio::io::AsyncReadExt::read(&mut s, &mut chunk).await {
                    Ok(n) => n,
                    Err(_) => return,
                };
                if n == 0 {
                    return;
                }
                for &b in &chunk[..n] {
                    if b == b'\n' {
                        line = Some(std::mem::take(&mut line_buf));
                        break;
                    } else if line_buf.len() < 512 {
                        line_buf.push(b);
                    }
                }
                if line.is_some() {
                    break;
                }
            }
            let raw = String::from_utf8_lossy(&line.unwrap()).into_owned();
            let mut toks: Vec<String> = raw
                .split(|c: char| c == ' ' || c == '\t' || c == '\r')
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect();
            if toks.is_empty() {
                continue;
            }
            toks.truncate(8);
            if toks[0].eq_ignore_ascii_case("quit") {
                let _ = s.write_all(b"OK\nEND\n").await;
                return;
            }
            let reply = self.dispatch(&toks);
            let mut out = reply;
            out.push_str("END\n");
            if s.write_all(out.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn dispatch(&self, toks: &[String]) -> String {
        match toks[0].to_ascii_lowercase().as_str() {
            "ping" => "OK\n".to_string(),
            "help" => format!("{}\n", HELP),
            "state" => format!("{}\n", self.core.lock().unwrap().game.cmd_state()),
            "board" => format!("{}\n", self.core.lock().unwrap().game.cmd_board()),
            "marks" => {
                let mut c = self.core.lock().unwrap();
                if toks.len() >= 2 {
                    let on = strtoull_10(&toks[1]) != 0;
                    c.game.set_marks(on);
                }
                format!("marks={}\n", c.game.marks_enabled as i64)
            }
            "pause" => {
                self.core.lock().unwrap().game.paused = true;
                "OK\n".to_string()
            }
            "resume" => {
                self.core.lock().unwrap().game.paused = false;
                "OK\n".to_string()
            }
            "refresh" => {
                let mut c = self.core.lock().unwrap();
                if toks.len() >= 2 {
                    c.game.cli_refresh = strtoull_10(&toks[1]) != 0;
                }
                format!("refresh={}\n", c.game.cli_refresh as i64)
            }
            "new" => self.cmd_new(toks),
            "click" => self.cmd_cell(toks, CellOp::Click),
            "flag" => self.cmd_cell(toks, CellOp::Flag),
            "chord" => self.cmd_cell(toks, CellOp::Chord),
            "seed" => format!(
                "{}\n",
                self.core.lock().unwrap().game.cmd_seed(toks)
            ),
            "seedcustom" => format!(
                "{}\n",
                self.core.lock().unwrap().game.cmd_seedcustom(toks)
            ),
            "seeds" => format!("{}\n", self.core.lock().unwrap().game.cmd_seeds()),
            "telemetry" => self.cmd_telemetry(toks),
            "reqseed" => self.cmd_reqseed(toks),
            "reqbatch" => self.cmd_reqbatch(toks),
            "scenarios" => self.cmd_scenarios(),
            _ => "ERR unknown command\n".to_string(),
        }
    }

    fn cmd_new(&self, toks: &[String]) -> String {
        if toks.len() < 2 {
            return "ERR unknown difficulty\n".to_string();
        }
        let diff = engine::parse_diff_name(&toks[1]);
        match diff {
            Some(DIFF_CUSTOM) if toks.len() >= 5 => {
                let r = strtoull_10(&toks[2]) as i64;
                let c = strtoull_10(&toks[3]) as i64;
                let m = strtoull_10(&toks[4]) as i64;
                self.core.lock().unwrap().game.reset_custom(r, c, m);
                "OK\n".to_string()
            }
            Some(DIFF_CUSTOM) => {
                self.core.lock().unwrap().game.reset(DIFF_CUSTOM);
                "OK\n".to_string()
            }
            Some(d) => {
                self.core.lock().unwrap().game.reset(d);
                "OK\n".to_string()
            }
            None => "ERR unknown difficulty\n".to_string(),
        }
    }

    fn cmd_cell(&self, toks: &[String], op: CellOp) -> String {
        let r = toks.get(1).map(|t| strtoull_10(t) as i64).unwrap_or(i64::MIN);
        let c = toks.get(2).map(|t| strtoull_10(t) as i64).unwrap_or(i64::MIN);
        let mut g = self.core.lock().unwrap();
        let res = match op {
            CellOp::Click => g.game.click(r, c),
            CellOp::Flag => g.game.flag(r, c),
            CellOp::Chord => g.game.chord(r, c),
        };
        match res {
            Ok(()) => "OK\n".to_string(),
            Err(e) => format!("{}\n", e),
        }
    }

    fn cmd_telemetry(&self, toks: &[String]) -> String {
        if toks.len() >= 2 {
            match toks[1].to_ascii_lowercase().as_str() {
                "on" => self.core.lock().unwrap().telemetry_on = true,
                "off" => self.core.lock().unwrap().telemetry_on = false,
                _ => return "ERR arg\n".to_string(),
            }
        }
        let c = self.core.lock().unwrap();
        format!(
            "telemetry={} host={} port={} connected={} attempts={} seeds={} \
             outcomes={} wins={} sent={} dropped={}\n",
            c.telemetry_on as i64,
            c.host,
            c.port,
            c.connected as i64,
            c.attempts,
            c.seeds_recv,
            c.outcomes_recv,
            c.wins_recv,
            c.metrics_sent,
            c.metrics_dropped
        )
    }

    fn cmd_reqseed(&self, toks: &[String]) -> String {
        let diff = toks.get(1).and_then(|t| engine::parse_diff_name(t));
        let Some(diff) = diff else {
            return "ERR unknown difficulty\n".to_string();
        };
        if toks.len() < 3 {
            return "ERR reqseed needs a seed: reqseed <diff> <n> [count]\n".to_string();
        }
        let u = strtoull_10(&toks[2]);
        if !self.core.lock().unwrap().telemetry_on {
            return "ERR telemetry off\n".to_string();
        }
        let count = toks
            .get(3)
            .map(|t| strtoull_10(t) as i64)
            .unwrap_or(1)
            .max(1);
        if !self.core.lock().unwrap().solver_ready() {
            return "ERR reqseed not queued (solver auth pending or credentials not configured)\n"
                .to_string();
        }
        let line = if count > 1 {
            format!("reqseed {} {} {}", DIFF_NAMES[diff], u, count)
        } else {
            format!("reqseed {} {}", DIFF_NAMES[diff], u)
        };
        if let Some(t) = &self.telemetry {
            t.request(line);
        }
        format!("OK reqseed {} {} count={}\n", DIFF_NAMES[diff], u, count)
    }

    fn cmd_reqbatch(&self, toks: &[String]) -> String {
        let diff = toks.get(1).and_then(|t| engine::parse_diff_name(t));
        let Some(diff) = diff else {
            return "ERR unknown difficulty\n".to_string();
        };
        if !self.core.lock().unwrap().telemetry_on {
            return "ERR telemetry off\n".to_string();
        }
        let count = toks
            .get(2)
            .map(|t| strtoull_10(t) as i64)
            .unwrap_or(1)
            .max(1);
        if !self.core.lock().unwrap().solver_ready() {
            return "ERR reqbatch not queued (solver auth pending or credentials not configured)\n"
                .to_string();
        }
        let line = format!("reqbatch {} {}", DIFF_NAMES[diff], count);
        if let Some(t) = &self.telemetry {
            t.request(line);
        }
        format!("OK reqbatch {} count={}\n", DIFF_NAMES[diff], count)
    }

    fn cmd_scenarios(&self) -> String {
        let (rows, cols, mines, lines) = {
            let g = self.core.lock().unwrap();
            (g.game.board.rows, g.game.board.cols, g.game.board.mines, g.game.board.board())
        };
        if rows == 0 || cols == 0 {
            return "ERR invalid board\n".to_string();
        }
        let board = Board::new(rows, &lines, mines);
        let hidden: Vec<usize> = (0..rows * cols).filter(|&i| board.hidden(i)).collect();
        if hidden.is_empty() {
            return "ERR no hidden cells\n".to_string();
        }
        let cons = build_constraints(&board);
        let pr = frontier_probabilities(&board, &cons, SCENARIOS_NODE_BUDGET);
        let uniform = (mines as f64) / (hidden.len() as f64);
        let n_free = if pr.nonfrontier_p.is_some() {
            hidden.len() - pr.probs.len()
        } else {
            hidden.len()
        };
        let nonfrontier_p = pr.nonfrontier_p.unwrap_or(uniform).clamp(0.0, 1.0);

        let mut sc: Vec<Scenario> = Vec::with_capacity(hidden.len());
        for &i in &hidden {
            let frontier = pr.probs.contains_key(&i);
            let mut p_mine = if frontier { pr.probs[&i] } else { nonfrontier_p };
            let p_safe = (1.0 - p_mine).clamp(0.0, 1.0);
            p_mine = 1.0 - p_safe;
            let reveals = {
                let g = self.core.lock().unwrap();
                flood_reveals(&g.game.board, i)
            };
            sc.push(Scenario {
                cell: i,
                r: i / cols,
                c: i % cols,
                p_mine,
                p_safe,
                frontier,
                reveals,
            });
        }
        sc.sort_by(|a, b| {
            b.p_safe
                .partial_cmp(&a.p_safe)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.reveals.cmp(&a.reveals))
                .then(a.r.cmp(&b.r))
                .then(a.c.cmp(&b.c))
        });

        let mut out = format!(
            "hidden={} free={} nonfrontier_p={} solved={}\n",
            hidden.len(),
            n_free,
            fmt_g12(nonfrontier_p),
            1
        );
        for s in &sc {
            out.push_str(&format!(
                "cell {} r {} c {} p_mine {} p_safe {} frontier {} reveals {}\n",
                s.cell,
                s.r,
                s.c,
                fmt_g12(s.p_mine),
                fmt_g12(s.p_safe),
                s.frontier as i64,
                s.reveals
            ));
        }
        out
    }
}

enum CellOp {
    Click,
    Flag,
    Chord,
}

struct Scenario {
    cell: usize,
    r: usize,
    c: usize,
    p_mine: f64,
    p_safe: f64,
    frontier: bool,
    reveals: usize,
}

/// Port of the C `flood_reveals` helper: how many hidden (non-flagged) cells
/// would open if `cell` were revealed, stopping the flood at revealed-number
/// boundaries.
fn flood_reveals(b: &mscore::SimBoard, cell: usize) -> usize {
    let n = b.rows * b.cols;
    let mut scratch = vec![false; n];
    let mut stack = Vec::with_capacity(n);
    scratch[cell] = true;
    stack.push(cell);
    let mut cnt = 0;
    while let Some(x) = stack.pop() {
        cnt += 1;
        if b.adj[x] != 0 {
            continue;
        }
        let r = x / b.cols;
        let c = x % b.cols;
        for dr in -1i64..=1 {
            for dc in -1i64..=1 {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let rr = r as i64 + dr;
                let cc = c as i64 + dc;
                if rr < 0 || rr >= b.rows as i64 || cc < 0 || cc >= b.cols as i64 {
                    continue;
                }
                let i = (rr as usize) * b.cols + (cc as usize);
                if b.revealed[i] == 0 && b.mark[i] != 1 && !scratch[i] {
                    scratch[i] = true;
                    stack.push(i);
                }
            }
        }
    }
    cnt
}

// Re-exported for main.rs: bind a loopback listener like the C cli_start().
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(format!("127.0.0.1:{}", port)).await
}
