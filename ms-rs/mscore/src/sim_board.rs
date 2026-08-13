//! SimBoard - headless Minesweeper board, bit-for-bit port of
//! `ms/core/sim-engine.js` (which is itself a 1:1 port of
//! `server/sim_engine.py` and `src/minesweeper.c`).
//!
//! Implements exactly:
//!   * `_placeMines`: pool of every cell outside the first click's 3x3, then a
//!     partial Fisher-Yates draw using `k = rng() % n` while n shrinks; tiny
//!     boards fall back to "only the clicked cell is safe".
//!   * `_revealCell` flood fill with the auto-win / auto-flag-mines rules and
//!     the auto-reveal-mines rule on loss.
//!   * The CLI command surface: new/click/flag/chord/state/board (+ping/refresh).

use crate::rng64::{to_u64, Rng64};

pub const PRESET_BEGINNER: (usize, usize, usize) = (8, 8, 10);
pub const PRESET_INTERMEDIATE: (usize, usize, usize) = (16, 16, 40);
pub const PRESET_EXPERT: (usize, usize, usize) = (16, 30, 99);

pub fn preset_rcm(name: &str) -> Option<(usize, usize, usize)> {
    match name {
        "beginner" => Some(PRESET_BEGINNER),
        "intermediate" => Some(PRESET_INTERMEDIATE),
        "expert" => Some(PRESET_EXPERT),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct SimBoard {
    pub marks_enabled: bool,
    pub rows: usize,
    pub cols: usize,
    pub mines: usize,
    pub difficulty: String,
    pub rng: Rng64,
    pub seed: u64,
    pub mine: Vec<u8>,
    pub adj: Vec<u8>,
    pub revealed: Vec<u8>,
    pub mark: Vec<u8>,
    pub opened: usize,
    pub started: usize,
    pub over: i8, // 0 = running, 1 = won, -1 = lost
    pub flags: usize,
    pub time: usize,
    pub paused: usize,
}

impl Default for SimBoard {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SimBoard {
    pub fn new(marks_enabled: bool) -> Self {
        SimBoard {
            marks_enabled,
            rows: 0,
            cols: 0,
            mines: 0,
            difficulty: "beginner".to_string(),
            rng: Rng64::new(0),
            seed: 0,
            mine: Vec::new(),
            adj: Vec::new(),
            revealed: Vec::new(),
            mark: Vec::new(),
            opened: 0,
            started: 0,
            over: 0,
            flags: 0,
            time: 0,
            paused: 0,
        }
    }

    fn reset(&mut self, rows: usize, cols: usize, mines: usize, difficulty: &str, seed: u64) {
        self.rows = rows;
        self.cols = cols;
        self.mines = mines;
        self.difficulty = difficulty.to_string();
        self.seed = to_u64(seed);
        self.rng = Rng64::new(self.seed);
        let n = rows * cols;
        self.mine = vec![0u8; n];
        self.adj = vec![0u8; n];
        self.revealed = vec![0u8; n];
        self.mark = vec![0u8; n];
        self.opened = 0;
        self.started = 0;
        self.over = 0;
        self.flags = 0;
        self.time = 0;
        self.paused = 0;
    }

    /// difficulty: 'beginner' | 'intermediate' | 'expert' | 'custom r c m'.
    pub fn new_game(&mut self, difficulty: &str, seed: u64) -> Result<(), String> {
        let parts: Vec<&str> = difficulty.split_whitespace().collect();
        let diff = parts[0].to_ascii_lowercase();
        if diff == "custom" && parts.len() >= 4 {
            let rows = parts[1].parse::<usize>().map_err(|_| "bad rows".to_string())?;
            let cols = parts[2].parse::<usize>().map_err(|_| "bad cols".to_string())?;
            let mines = parts[3].parse::<usize>().map_err(|_| "bad mines".to_string())?;
            self.reset(rows, cols, mines, "custom", seed);
        } else if let Some((rows, cols, mines)) = preset_rcm(&diff) {
            self.reset(rows, cols, mines, &diff, seed);
        } else {
            return Err(format!("unknown difficulty: {:?}", difficulty));
        }
        Ok(())
    }

    fn idx(&self, r: usize, c: usize) -> usize {
        r * self.cols + c
    }

    fn inb(&self, r: i64, c: i64) -> bool {
        r >= 0 && (r as usize) < self.rows && c >= 0 && (c as usize) < self.cols
    }

    fn compute_adj(&mut self) {
        let mut new_adj = vec![0u8; self.rows * self.cols];
        for r in 0..self.rows {
            for c in 0..self.cols {
                let mut cnt: u8 = 0;
                for dr in -1i64..=1 {
                    for dc in -1i64..=1 {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let rr = r as i64 + dr;
                        let cc = c as i64 + dc;
                        if self.inb(rr, cc) && self.mine[self.idx(rr as usize, cc as usize)] == 1 {
                            cnt += 1;
                        }
                    }
                }
                new_adj[self.idx(r, c)] = cnt;
            }
        }
        self.adj = new_adj;
    }

    /// Partial Fisher-Yates over the pool outside the 3x3 around (sr, sc).
    /// Exactly mirrors JS `_placeMines` / Python `_place_mines` / C `place_mines`.
    fn place_mines(&mut self, sr: usize, sc: usize) {
        let mut pool: Vec<usize> = Vec::new();
        for r in 0..self.rows {
            for c in 0..self.cols {
                let rd = (r as i64 - sr as i64).abs();
                let cd = (c as i64 - sc as i64).abs();
                if rd <= 1 && cd <= 1 {
                    continue;
                }
                pool.push(self.idx(r, c));
            }
        }
        if pool.len() < self.mines {
            // tiny board: only the clicked cell safe
            pool.clear();
            for r in 0..self.rows {
                for c in 0..self.cols {
                    if !(r == sr && c == sc) {
                        pool.push(self.idx(r, c));
                    }
                }
            }
        }
        let mut n = pool.len();
        let mut placed = 0usize;
        while placed < self.mines && n > 0 {
            let k = (self.rng.next() % n as u64) as usize;
            let idx = pool[k];
            pool[k] = pool[n - 1];
            n -= 1;
            if self.mine[idx] == 0 {
                self.mine[idx] = 1;
                placed += 1;
            }
        }
        self.compute_adj();
    }

    fn first_click(&mut self, r: usize, c: usize) -> usize {
        if self.started != 0 {
            return 0;
        }
        self.started = 1;
        self.place_mines(r, c);
        1
    }

    fn end_game_lose(&mut self) {
        if self.over != 0 {
            return;
        }
        self.over = -1;
        for i in 0..self.rows * self.cols {
            if self.mine[i] == 1 {
                self.revealed[i] = 1;
            }
        }
    }

    fn end_game_win(&mut self) {
        if self.over != 0 {
            return;
        }
        self.over = 1;
        for i in 0..self.rows * self.cols {
            if self.mine[i] == 1 && self.mark[i] != 1 {
                self.mark[i] = 1;
                self.flags += 1;
            }
        }
    }

    fn reveal_cell(&mut self, r: i64, c: i64) {
        if !self.inb(r, c) {
            return;
        }
        let i = self.idx(r as usize, c as usize);
        if self.revealed[i] == 1 || self.mark[i] == 1 {
            return;
        }
        if self.mine[i] == 1 {
            self.end_game_lose();
            return;
        }
        if self.over != 0 {
            return;
        }
        self.revealed[i] = 1;
        self.opened += 1;
        if self.adj[i] == 0 {
            for dr in -1i64..=1 {
                for dc in -1i64..=1 {
                    if dr == 0 && dc == 0 {
                        continue;
                    }
                    self.reveal_cell(r + dr, c + dc);
                }
            }
        }
        if self.opened == self.rows * self.cols - self.mines {
            self.end_game_win();
        }
    }

    pub fn click(&mut self, r: i64, c: i64) {
        if !self.inb(r, c) {
            return;
        }
        if self.started == 0 {
            self.first_click(r as usize, c as usize);
        }
        self.reveal_cell(r, c);
    }

    fn cycle_mark(&mut self, cell: usize) {
        if self.over != 0 {
            return;
        }
        if self.mark[cell] == 0 {
            self.mark[cell] = 1;
            self.flags += 1;
        } else if self.mark[cell] == 1 {
            self.flags -= 1;
            self.mark[cell] = if self.marks_enabled { 2 } else { 0 };
        } else {
            self.mark[cell] = 0;
        }
    }

    pub fn flag(&mut self, r: i64, c: i64) {
        if self.inb(r, c) {
            let i = self.idx(r as usize, c as usize);
            self.cycle_mark(i);
        }
    }

    fn do_chord(&mut self, cell: usize) {
        let r = cell / self.cols;
        let c = cell % self.cols;
        let mut cnt = 0usize;
        for dr in -1i64..=1 {
            for dc in -1i64..=1 {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let rr = r as i64 + dr;
                let cc = c as i64 + dc;
                if self.inb(rr, cc) && self.mark[self.idx(rr as usize, cc as usize)] == 1 {
                    cnt += 1;
                }
            }
        }
        if cnt == self.adj[cell] as usize {
            for dr in -1i64..=1 {
                for dc in -1i64..=1 {
                    if dr == 0 && dc == 0 {
                        continue;
                    }
                    let rr = r as i64 + dr;
                    let cc = c as i64 + dc;
                    if self.inb(rr, cc) {
                        self.reveal_cell(rr, cc);
                    }
                }
            }
        }
    }

    pub fn chord(&mut self, r: i64, c: i64) {
        if self.inb(r, c) {
            let i = self.idx(r as usize, c as usize);
            self.do_chord(i);
        }
    }

    /// The `board` CLI reply: one string per row with
    /// hidden='.'/F/?  revealed mine='*'  revealed number='0'..='8'.
    pub fn board(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in 0..self.rows {
            let mut row = String::with_capacity(self.cols);
            for c in 0..self.cols {
                let i = self.idx(r, c);
                let ch = if self.revealed[i] == 0 {
                    match self.mark[i] {
                        1 => 'F',
                        2 => '?',
                        _ => '.',
                    }
                } else if self.mine[i] == 1 {
                    '*'
                } else {
                    char::from(b'0' + self.adj[i])
                };
                row.push(ch);
            }
            out.push(row);
        }
        out
    }

    pub fn state_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("difficulty={}\n", self.difficulty));
        out.push_str(&format!("rows={}\n", self.rows));
        out.push_str(&format!("cols={}\n", self.cols));
        out.push_str(&format!("mines={}\n", self.mines));
        out.push_str(&format!("flags={}\n", self.flags));
        out.push_str(&format!("opened={}\n", self.opened));
        out.push_str(&format!("time={}\n", self.time));
        out.push_str(&format!("started={}\n", self.started));
        out.push_str(&format!("over={}\n", self.over));
        out.push_str(&format!("paused={}\n", self.paused));
        out.push_str(&format!("marks={}\n", if self.marks_enabled { 1 } else { 0 }));
        out.push_str("seeded=1\n");
        out.push_str(&format!("seed={}\n", self.seed));
        out.push_str("END\n");
        out
    }

    /// State map as `(key, value-string)` pairs in the canonical CLI order.
    pub fn state(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in self.state_text().split('\n') {
            if line == "END" {
                continue;
            }
            if let Some(eq) = line.find('=') {
                out.push((line[..eq].to_string(), line[eq + 1..].to_string()));
            }
        }
        out
    }

    /// Handle one CLI-style command line; returns the full reply text.
    pub fn command(&mut self, line: &str) -> String {
        let toks: Vec<&str> = line.trim().split_whitespace().collect();
        if toks.is_empty() {
            return "OK\nEND\n".to_string();
        }
        let cmd = toks[0].to_ascii_lowercase();
        match cmd.as_str() {
            "new" => {
                if toks.len() >= 5 && toks[1].to_ascii_lowercase() == "custom" {
                    let _ = self.new_game(
                        &format!("custom {} {} {}", toks[2], toks[3], toks[4]),
                        self.seed,
                    );
                } else {
                    let d = if toks.len() > 1 { toks[1] } else { "beginner" };
                    let _ = self.new_game(d, self.seed);
                }
                "OK\nEND\n".to_string()
            }
            "click" => {
                if toks.len() >= 3 {
                    if let (Ok(r), Ok(c)) = (toks[1].parse::<i64>(), toks[2].parse::<i64>()) {
                        self.click(r, c);
                        return "OK\nEND\n".to_string();
                    }
                }
                "ERR bad args\nEND\n".to_string()
            }
            "flag" => {
                if toks.len() >= 3 {
                    if let (Ok(r), Ok(c)) = (toks[1].parse::<i64>(), toks[2].parse::<i64>()) {
                        self.flag(r, c);
                        return "OK\nEND\n".to_string();
                    }
                }
                "ERR bad args\nEND\n".to_string()
            }
            "chord" => {
                if toks.len() >= 3 {
                    if let (Ok(r), Ok(c)) = (toks[1].parse::<i64>(), toks[2].parse::<i64>()) {
                        self.chord(r, c);
                        return "OK\nEND\n".to_string();
                    }
                }
                "ERR bad args\nEND\n".to_string()
            }
            "refresh" | "ping" => "OK\nEND\n".to_string(),
            "state" => self.state_text(),
            "board" => {
                let mut out = self.board().join("\n");
                out.push_str("\nEND\n");
                out
            }
            _ => "ERR unknown command\nEND\n".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_golden_seed1_board() {
        // golden-boards.json entry: beginner seed 1 click (4,4).
        let mut b = SimBoard::new(true);
        b.new_game("beginner", 1).unwrap();
        b.click(4, 4);
        assert_eq!(
            b.board(),
            vec![
                "........",
                "........",
                "........",
                "112111..",
                "000001..",
                "011102..",
                "12.102..",
                "...102..",
            ]
        );
        assert_eq!(b.state().iter().find(|(k, _)| k == "seed").unwrap().1, "1");
        assert_eq!(b.opened, 26);
    }

    #[test]
    fn tiny_board_only_click_safe() {
        // custom 3 3 8 seed 2 click (1,1): pool smaller than mines -> fallback
        // where only the clicked cell is safe; every neighbour becomes a mine
        // and the game auto-flags them and wins. Placement is seed-independent.
        let mut b = SimBoard::new(true);
        b.new_game("custom 3 3 8", 2).unwrap();
        b.click(1, 1);
        assert_eq!(b.opened, 1);
        assert_eq!(b.over, 1);
        assert_eq!(b.flags, 8);
        // Verified against ms/core/sim-engine.js command('new custom 3 3 8').
        assert_eq!(b.board(), vec!["FFF", "F8F", "FFF"]);
    }

    #[test]
    fn zero_mines_auto_win_on_click() {
        let mut b = SimBoard::new(true);
        b.new_game("custom 5 5 0", 3).unwrap();
        b.click(2, 2);
        assert_eq!(b.over, 1);
    }

    #[test]
    fn command_cycle_flag_mark() {
        let mut b = SimBoard::new(true);
        b.new_game("beginner", 5).unwrap();
        b.command("flag 0 0");
        assert_eq!(b.board()[0].as_bytes()[0], b'F');
        b.command("flag 0 0");
        assert_eq!(b.board()[0].as_bytes()[0], b'?');
        b.command("flag 0 0");
        assert_eq!(b.board()[0].as_bytes()[0], b'.');
    }
}
