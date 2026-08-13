//! Game engine state machine — a faithful port of the C client's board model,
//! seed slots and scripting semantics (`minesweeper.c`).
//!
//! Board generation/click/flag/chord parity is delegated to `mscore::SimBoard`
//! (itself a bit-exact port of the C board engine); this module adds the
//! client-side state the C app owns: difficulty slots, the one-shot seed
//! override, FNV-1a custom-seed derivation, seeding bookkeeping and metrics.

use mscore::SimBoard;
use rand::Rng;
use std::collections::VecDeque;

pub const DIFF_BEGIN: usize = 0;
pub const DIFF_INTERMEDIATE: usize = 1;
pub const DIFF_EXPERT: usize = 2;
pub const DIFF_CUSTOM: usize = 3;
pub const DIFF_COUNT: usize = 4;

pub const DIFF_NAMES: [&str; DIFF_COUNT] = ["beginner", "intermediate", "expert", "custom"];
pub const DIFF_SALTS: [&str; DIFF_COUNT] = ["beginner", "intermediate", "expert", "custom"];
pub const PRESETS: [(usize, usize, usize); 3] = [(8, 8, 10), (16, 16, 40), (16, 30, 99)];
pub const MAX_ROWS: usize = 30;
pub const MAX_COLS: usize = 30;
pub const CUSTOM_SEED_INPUT_MAX: usize = 64;
pub const CUSTOM_SEED_TARGET_DIGITS: usize = 19;
pub const CUSTOM_SEED_MAX_STEPS: u32 = 32;
pub const RNG_ZERO_SEED_FALLBACK: u64 = 0x9E3779B97F4A7C15;

pub const SEED_OFF: u8 = 0;
pub const SEED_NORMAL: u8 = 1;
pub const SEED_CUSTOM: u8 = 2;

pub fn parse_diff_name(s: &str) -> Option<usize> {
    DIFF_NAMES.iter().position(|n| n.eq_ignore_ascii_case(s))
}

/// Faithful port of C `strtoull(s, NULL, 10)`: skips whitespace, honours a
/// leading +/- sign, reads decimal digits with u64 wraparound (so "-5" becomes
/// 2^64-5, matching the C client), returns 0 when no digits are present.
pub fn strtoull_10(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] as char).is_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut v: u64 = 0;
    let mut any = false;
    while i < b.len() && b[i].is_ascii_digit() {
        v = v.wrapping_mul(10).wrapping_add((b[i] - b'0') as u64);
        any = true;
        i += 1;
    }
    if !any {
        return 0;
    }
    if neg {
        v = 0u64.wrapping_sub(v);
    }
    v
}

/// Port of C `printf("%.12g")`: 12 significant digits, %e when the exponent
/// is < -4 or >= 12, trailing zeros stripped.
pub fn fmt_g12(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let neg = v < 0.0;
    let av = v.abs();
    let s = format!("{:.11e}", av);
    let (mant, exp_str) = s.split_once('e').unwrap();
    let exp: i32 = exp_str.parse().unwrap();
    let (int_part, frac_part) = mant.split_once('.').unwrap();
    let mut digits: Vec<char> = format!("{}{}", int_part, frac_part).chars().collect();
    while digits.last() == Some(&'0') {
        digits.pop();
    }
    if digits.is_empty() {
        return "0".to_string();
    }
    if exp >= -4 && exp < 12 {
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        let dp = exp + 1; // number of integer digits
        if dp <= 0 {
            out.push('0');
            out.push('.');
            for _ in 0..(-dp) {
                out.push('0');
            }
            for d in &digits {
                out.push(*d);
            }
        } else if (dp as usize) >= digits.len() {
            for d in &digits {
                out.push(*d);
            }
            for _ in digits.len()..(dp as usize) {
                out.push('0');
            }
        } else {
            for (i, d) in digits.iter().enumerate() {
                out.push(*d);
                if i + 1 == dp as usize {
                    out.push('.');
                }
            }
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
                out.pop();
            }
        }
        out
    } else {
        let mut out = String::new();
        if neg {
            out.push('-');
        }
        out.push(digits[0]);
        if digits.len() > 1 {
            out.push('.');
            for d in &digits[1..] {
                out.push(*d);
            }
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let ae = exp.abs();
        if ae < 10 {
            out.push('0');
        }
        out.push_str(&ae.to_string());
        out
    }
}

/// FNV-1a 64-bit (verbatim constants from the C client).
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn str_mul(buf: &str, m: u64) -> String {
    let bytes: Vec<u8> = buf.bytes().collect();
    let mut tmp: Vec<u8> = Vec::with_capacity(bytes.len() + 12);
    let mut carry: u64 = 0;
    for i in (0..bytes.len()).rev() {
        let v = (bytes[i] - b'0') as u64 * m + carry;
        tmp.push(b'0' + (v % 10) as u8);
        carry = v / 10;
    }
    while carry > 0 {
        tmp.push(b'0' + (carry % 10) as u8);
        carry /= 10;
    }
    tmp.reverse();
    String::from_utf8(tmp).unwrap()
}

/// `custom_seed_generate`: returns `(value, steps, truncated)` or None.
pub fn custom_seed_generate(input: &str) -> Option<(u64, u32, bool)> {
    if input.is_empty() {
        return None;
    }
    let mut buf: String;
    if input.bytes().all(|b| b.is_ascii_digit()) {
        buf = input.chars().take(159).collect();
    } else {
        let hv = fnv1a64(input);
        buf = hv.to_string();
    }
    let trimmed = buf.trim_start_matches('0');
    buf = if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() };
    if buf == "0" {
        return Some((0, 0, false));
    }
    let mut steps: u32 = 0;
    let mut truncated = false;
    if buf.len() >= CUSTOM_SEED_TARGET_DIGITS {
        if buf.len() > CUSTOM_SEED_TARGET_DIGITS {
            buf.truncate(CUSTOM_SEED_TARGET_DIGITS);
            truncated = true;
        }
    } else {
        while buf.len() < CUSTOM_SEED_TARGET_DIGITS && steps < CUSTOM_SEED_MAX_STEPS {
            let m = 1u64 << (steps + 1);
            buf = str_mul(&buf, m);
            steps += 1;
            if buf.len() > CUSTOM_SEED_TARGET_DIGITS {
                buf.truncate(CUSTOM_SEED_TARGET_DIGITS);
                truncated = true;
                break;
            }
        }
    }
    let mut v: u64 = 0;
    for b in buf.bytes() {
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u64);
    }
    Some((v, steps, truncated))
}

/// Per-difficulty CUSTOM derivation: pure numbers unsalted, text salted with
/// `<difficulty>:<value>` before hashing.
pub fn diff_custom_seed_generate(diff: usize, input: &str) -> Option<(u64, u32, bool)> {
    if input.is_empty() {
        return None;
    }
    if input.bytes().all(|b| b.is_ascii_digit()) {
        return custom_seed_generate(input);
    }
    let salt = DIFF_SALTS.get(diff).copied().unwrap_or("");
    custom_seed_generate(&format!("{}:{}", salt, input))
}

fn seed_or_fallback(seed: u64) -> u64 {
    if seed == 0 {
        RNG_ZERO_SEED_FALLBACK
    } else {
        seed
    }
}

#[derive(Clone)]
pub struct SeedSlot {
    pub mode: u8,
    pub value: String,
}

impl SeedSlot {
    fn new() -> Self {
        SeedSlot { mode: SEED_OFF, value: String::new() }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum EngineEvent {
    GameStart { diff: usize, seed: u64, seeded: bool },
    GameOver { diff: usize, won: bool, seed: u64, seeded: bool, time: usize, clicks: u64, latency: f64 },
}

pub struct Game {
    pub board: SimBoard,
    pub diff: usize,
    pub custom: (usize, usize, usize),
    pub slots: [SeedSlot; DIFF_COUNT],
    pub override_active: bool,
    pub override_val: u64,
    pub seed_active: bool,
    pub seed_active_val: u64,
    pub marks_enabled: bool,
    pub paused: bool,
    pub time: usize,
    pub cli_refresh: bool,
    pub clicks: u64,
    pub latency_ema: f64,
    pub latency_n: u64,
    pub events: VecDeque<EngineEvent>,
    pub last_over: i8,
}

impl Game {
    pub fn new() -> Self {
        let mut g = Game {
            board: SimBoard::new(true),
            diff: DIFF_BEGIN,
            custom: PRESETS[DIFF_INTERMEDIATE],
            slots: [SeedSlot::new(), SeedSlot::new(), SeedSlot::new(), SeedSlot::new()],
            override_active: false,
            override_val: 0,
            seed_active: false,
            seed_active_val: 0,
            marks_enabled: true,
            paused: false,
            time: 0,
            cli_refresh: true,
            clicks: 0,
            latency_ema: 0.0,
            latency_n: 0,
            events: VecDeque::new(),
            last_over: 0,
        };
        g.reset(DIFF_BEGIN);
        g
    }

    fn metrics_reset(&mut self) {
        self.clicks = 0;
        self.latency_ema = 0.0;
        self.latency_n = 0;
    }

    pub fn note_ui_latency(&mut self, us: i64) {
        let us = if us < 0 { 0.0 } else { us as f64 };
        if self.latency_n == 0 {
            self.latency_ema = us;
        } else {
            self.latency_ema = 0.8 * self.latency_ema + 0.2 * us;
        }
        self.latency_n += 1;
    }

    /// `resolve_board_seed` — pending override wins and is consumed.
    pub fn resolve_board_seed(&mut self, diff: Option<usize>) -> Option<u64> {
        if self.override_active {
            self.override_active = false;
            let v = seed_or_fallback(self.override_val);
            return Some(v);
        }
        let diff = diff?;
        if diff >= DIFF_COUNT {
            return None;
        }
        let slot = &self.slots[diff];
        match slot.mode {
            SEED_NORMAL => {
                let v = slot.value.parse::<u64>().unwrap_or(0);
                Some(seed_or_fallback(v))
            }
            SEED_CUSTOM => diff_custom_seed_generate(diff, &slot.value)
                .map(|(v, _, _)| seed_or_fallback(v)),
            _ => None,
        }
    }

    /// Start a new game at a preset or custom difficulty. Mirrors reset_game().
    pub fn reset(&mut self, diff: usize) {
        let diff = diff.min(DIFF_CUSTOM);
        self.diff = diff;
        let name = if diff == DIFF_CUSTOM {
            "custom"
        } else {
            DIFF_NAMES[diff]
        };
        self.last_over = self.board.over;
        let resolved = self.resolve_board_seed(Some(diff));
        match resolved {
            Some(v) => {
                self.seed_active = true;
                self.seed_active_val = v;
                let _ = self.board.new_game(name, v);
            }
            None => {
                self.seed_active = false;
                let rnd: u64 = rand::thread_rng().gen();
                let _ = self.board.new_game(name, rnd);
            }
        }
        self.time = 0;
        self.paused = false;
        self.metrics_reset();
        self.events.push_back(EngineEvent::GameStart {
            diff,
            seed: self.seed_active_val,
            seeded: self.seed_active,
        });
    }

    /// `new custom <r> <c> <m>` clamping (8..30 rows/cols, mines 1..r*c-9).
    pub fn reset_custom(&mut self, rows: i64, cols: i64, mines: i64) {
        let rows = rows.clamp(8, MAX_ROWS as i64) as usize;
        let cols = cols.clamp(8, MAX_COLS as i64) as usize;
        let mut mines = mines;
        if mines < 1 {
            mines = 1;
        }
        let max_mines = (rows * cols) as i64 - 9;
        if mines > max_mines {
            mines = max_mines;
        }
        self.custom = (rows, cols, mines as usize);
        self.reset(DIFF_CUSTOM);
    }

    pub fn inb(&self, r: i64, c: i64) -> bool {
        r >= 0 && (r as usize) < self.board.rows && c >= 0 && (c as usize) < self.board.cols
    }

    /// Click: first click places mines, then reveal. Tracks the win/loss
    /// transition and enqueues events. Returns Err text on out-of-bounds.
    pub fn click(&mut self, r: i64, c: i64) -> Result<(), String> {
        if !self.inb(r, c) {
            return Err("ERR out of bounds".to_string());
        }
        self.clicks += 1;
        let before = self.board.over;
        self.board.click(r, c);
        self.after_transition(before);
        Ok(())
    }

    pub fn flag(&mut self, r: i64, c: i64) -> Result<(), String> {
        if !self.inb(r, c) {
            return Err("ERR out of bounds".to_string());
        }
        let before = self.board.over;
        self.board.flag(r, c);
        self.after_transition(before);
        Ok(())
    }

    pub fn chord(&mut self, r: i64, c: i64) -> Result<(), String> {
        if !self.inb(r, c) {
            return Err("ERR out of bounds".to_string());
        }
        self.clicks += 1;
        let before = self.board.over;
        self.board.chord(r, c);
        self.after_transition(before);
        Ok(())
    }

    fn after_transition(&mut self, before: i8) {
        if self.board.over != before && self.board.over != 0 {
            let won = self.board.over == 1;
            self.events.push_back(EngineEvent::GameOver {
                diff: self.diff,
                won,
                seed: self.seed_active_val,
                seeded: self.seed_active,
                time: self.time,
                clicks: self.clicks,
                latency: self.latency_ema,
            });
        }
    }

    pub fn drain_events(&mut self) -> Vec<EngineEvent> {
        self.events.drain(..).collect()
    }

    pub fn set_marks(&mut self, on: bool) {
        self.marks_enabled = on;
        self.board.marks_enabled = on;
    }

    // ------------------------------------------------------------------
    // Scripting command replies (all without the trailing END marker)
    // ------------------------------------------------------------------

    pub fn cmd_state(&self) -> String {
        let b = &self.board;
        format!(
            "difficulty={}\nrows={}\ncols={}\nmines={}\nflags={}\nopened={}\n\
             time={}\nstarted={}\nover={}\npaused={}\nmarks={}\n\
             seeded={}\nseed={}",
            DIFF_NAMES[self.diff],
            b.rows,
            b.cols,
            b.mines,
            b.flags,
            b.opened,
            self.time,
            b.started,
            b.over,
            self.paused as i64,
            self.marks_enabled as i64,
            self.seed_active as i64,
            self.seed_active_val,
        )
    }

    pub fn cmd_board(&self) -> String {
        self.board.board().join("\n")
    }

    pub fn cmd_seeds(&self) -> String {
        let mut out = String::new();
        for i in 0..DIFF_COUNT {
            let slot = &self.slots[i];
            match slot.mode {
                SEED_OFF => out.push_str(&format!("{}=off\n", DIFF_NAMES[i])),
                SEED_NORMAL => out.push_str(&format!("{}=normal:{}\n", DIFF_NAMES[i], slot.value)),
                _ => match diff_custom_seed_generate(i, &slot.value) {
                    Some((v, _, _)) => out.push_str(&format!("{}=custom:{}\n", DIFF_NAMES[i], v)),
                    None => out.push_str(&format!("{}=custom:invalid\n", DIFF_NAMES[i])),
                },
            }
        }
        if self.override_active {
            out.push_str(&format!("pending={}\n", self.override_val));
        } else {
            out.push_str("pending=off\n");
        }
        out.pop();
        out
    }

    /// `seed` command. `toks` = [seed, ...]. Returns reply without END.
    pub fn cmd_seed(&mut self, toks: &[String]) -> String {
        match toks.len() {
            2 => {
                if toks[1].eq_ignore_ascii_case("off") {
                    self.override_active = false;
                    "OK seed off".to_string()
                } else {
                    let v = strtoull_10(&toks[1]);
                    self.override_active = true;
                    self.override_val = v;
                    format!("OK seed={}", v)
                }
            }
            3 => {
                match parse_diff_name(&toks[1]) {
                    None => "ERR unknown difficulty".to_string(),
                    Some(d) => {
                        if toks[2].eq_ignore_ascii_case("off") {
                            self.slots[d] = SeedSlot::new();
                            "OK seed off".to_string()
                        } else {
                            let v = strtoull_10(&toks[2]);
                            self.slots[d] = SeedSlot { mode: SEED_NORMAL, value: v.to_string() };
                            format!("OK seed={}", v)
                        }
                    }
                }
            }
            _ => "ERR bad args".to_string(),
        }
    }

    /// `seedcustom` command. Returns reply without END.
    pub fn cmd_seedcustom(&mut self, toks: &[String]) -> String {
        match toks.len() {
            2 => {
                if toks[1].eq_ignore_ascii_case("off") {
                    self.override_active = false;
                    "OK seed off".to_string()
                } else {
                    match custom_seed_generate(&toks[1]) {
                        Some((v, steps, truncated)) => {
                            self.override_active = true;
                            self.override_val = v;
                            format!("OK seed={} steps={} truncated={}", v, steps, truncated as i64)
                        }
                        None => "ERR bad seed input".to_string(),
                    }
                }
            }
            3 => match parse_diff_name(&toks[1]) {
                None => "ERR unknown difficulty".to_string(),
                Some(d) => {
                    if toks[2].eq_ignore_ascii_case("off") {
                        self.slots[d] = SeedSlot::new();
                        "OK seed off".to_string()
                    } else {
                        match diff_custom_seed_generate(d, &toks[2]) {
                            Some((v, steps, truncated)) => {
                                let mut value = toks[2].clone();
                                value.truncate(CUSTOM_SEED_INPUT_MAX);
                                self.slots[d] = SeedSlot { mode: SEED_CUSTOM, value };
                                format!("OK seed={} steps={} truncated={}", v, steps, truncated as i64)
                            }
                            None => "ERR bad seed input".to_string(),
                        }
                    }
                }
            },
            _ => "ERR bad args".to_string(),
        }
    }

    /// Set the one-shot or a per-difficulty seed from CLI args (`--seed`).
    pub fn apply_seed_arg(&mut self, arg: &str, custom: bool) {
        let (diff, val) = match arg.find(':') {
            Some(i) if parse_diff_name(&arg[..i]).is_some() => {
                (parse_diff_name(&arg[..i]), &arg[i + 1..])
            }
            _ => (None, arg),
        };
        match diff {
            Some(d) => {
                if custom {
                    let mut value = val.to_string();
                    value.truncate(CUSTOM_SEED_INPUT_MAX);
                    self.slots[d] = SeedSlot { mode: SEED_CUSTOM, value };
                } else {
                    let v = strtoull_10(val);
                    self.slots[d] = SeedSlot { mode: SEED_NORMAL, value: v.to_string() };
                }
            }
            None => {
                if custom {
                    if let Some((v, _, _)) = custom_seed_generate(val) {
                        self.override_active = true;
                        self.override_val = v;
                    }
                } else {
                    let v = strtoull_10(val);
                    self.override_active = true;
                    self.override_val = v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_known_values() {
        assert_eq!(fnv1a64(""), 14695981039346656037);
        assert_eq!(fnv1a64("a"), 12638187200555641996);
    }

    #[test]
    fn seed_or_fallback_zero() {
        assert_eq!(seed_or_fallback(0), RNG_ZERO_SEED_FALLBACK);
        assert_eq!(seed_or_fallback(12345), 12345);
    }

    #[test]
    fn custom_seed_numeric_identity() {
        // A pure number grows via the x2/x4/x8... multiplier loop to 19 digits
        // (verified against the C client: seedcustom 12345 -> steps=10).
        assert_eq!(custom_seed_generate("12345"), Some((4447754991991101849, 10, true)));
        assert_eq!(custom_seed_generate("0"), Some((0, 0, false)));
        assert_eq!(custom_seed_generate(""), None);
    }

    #[test]
    fn custom_seed_fnv_hashing() {
        // "hello" hashes to an FNV-1a value then trims to 19 digits
        // (verified against the C client: seedcustom hello -> steps=0).
        assert_eq!(custom_seed_generate("hello"), Some((1183119401842027649, 0, true)));
        assert_eq!(fnv1a64("hello"), 11831194018420276491);
    }

    #[test]
    fn custom_seed_long_truncates() {
        let input = "123456789012345678901234"; // 24 digits
        let (v, _steps, truncated) = custom_seed_generate(input).unwrap();
        assert!(truncated);
        assert_eq!(v.to_string().len(), 19);
    }

    #[test]
    fn diff_custom_salted() {
        let (a, _, _) = diff_custom_seed_generate(DIFF_BEGIN, "hello").unwrap();
        let (b, _, _) = diff_custom_seed_generate(DIFF_EXPERT, "hello").unwrap();
        assert_eq!(a, custom_seed_generate("beginner:hello").unwrap().0);
        assert_ne!(a, b);
    }

    #[test]
    fn override_consumed_on_next_resolve() {
        let mut g = Game::new();
        g.override_active = true;
        g.override_val = 77;
        assert_eq!(g.resolve_board_seed(None), Some(seed_or_fallback(77)));
        assert!(!g.override_active);
        // Next resolve falls through to the difficulty slot (none set) -> None.
        assert_eq!(g.resolve_board_seed(Some(DIFF_BEGIN)), None);
    }

    #[test]
    fn state_matches_c_layout() {
        let mut g = Game::new();
        g.reset(DIFF_BEGIN);
        g.override_active = true;
        g.override_val = 12345;
        g.reset(DIFF_BEGIN);
        let st = g.cmd_state();
        assert!(st.contains("difficulty=beginner"));
        assert!(st.contains("rows=8"));
        assert!(st.contains("cols=8"));
        assert!(st.contains("mines=10"));
        assert!(st.contains("started=0"));
        assert!(st.contains("over=0"));
        assert!(st.contains("marks=1"));
        assert!(st.contains("seeded=1"));
        assert!(st.contains("seed=12345"));
    }

    #[test]
    fn board_dump_chars() {
        let mut g = Game::new();
        g.override_active = true;
        g.override_val = 12345;
        g.reset(DIFF_BEGIN);
        let dump = g.cmd_board();
        let rows: Vec<&str> = dump.split('\n').collect();
        assert_eq!(rows.len(), 8);
        assert!(rows.iter().all(|r| r.len() == 8));
    }

    #[test]
    fn oob_click_reports_error() {
        let mut g = Game::new();
        assert_eq!(g.click(99, 99).unwrap_err(), "ERR out of bounds");
        assert_eq!(g.flag(-1, 0).unwrap_err(), "ERR out of bounds");
    }

    #[test]
    fn seeded_board_matches_mscore() {
        // The engine must produce the exact same board as a bare SimBoard for
        // the same seed, so listen-driven bots see bit-identical boards.
        let mut g = Game::new();
        g.override_active = true;
        g.override_val = 12345;
        g.reset(DIFF_BEGIN);
        g.click(4, 4).unwrap();
        let mut s = SimBoard::new(true);
        s.new_game("beginner", 12345).unwrap();
        s.click(4, 4);
        assert_eq!(g.board.board(), s.board());
        assert_eq!(g.board.opened, s.opened);
        assert_eq!(g.board.adj, s.adj);
    }
}
