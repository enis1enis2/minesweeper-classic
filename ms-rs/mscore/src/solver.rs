//! Deterministic Minesweeper player, ported from `ms/core/solver.js` (itself a
//! 1:1 port of `minesweeper_bot/ms_solver.py`). Constraint-propagation deduction
//! plus an exact probabilistic pass over the frontier of unrevealed cells.
//!
//! The JS port deliberately used insertion-ordered Set/Map for determinism; the
//! Rust port uses BTreeSet/BTreeMap (sorted) which keeps every result fully
//! deterministic. Probability values are identical up to f64 rounding order
//! (~1e-16), well inside the 1e-9 tolerance used by the parity harness.

use num_bigint::BigUint;
use num_traits::{One, ToPrimitive};
use std::collections::{BTreeMap, BTreeSet};

fn big_to_f64(b: &BigUint) -> f64 {
    b.to_f64().unwrap_or(f64::INFINITY)
}

use crate::mt19937::Mt19937;
use crate::sim_board::SimBoard;

pub fn comb_big(n: u64, k: u64) -> BigUint {
    if k > n {
        return BigUint::from(0u32);
    }
    let kk = if k > n - k { n - k } else { k };
    let mut c = BigUint::one();
    for i in 1..=kk {
        let numer = BigUint::from(n - kk + i);
        let denom = BigUint::from(i);
        c = (c * numer) / denom;
    }
    c
}

#[derive(Clone, Debug)]
pub struct Board {
    pub rows: usize,
    pub lines: Vec<String>,
    pub cols: usize,
    pub total_mines: usize,
    pub revealed: Vec<bool>,
    pub mine: Vec<bool>,
    pub flagged: Vec<bool>,
    pub q: Vec<bool>,
    pub num: Vec<u8>,
}

impl Board {
    pub fn new(rows: usize, lines: &[String], total_mines: usize) -> Board {
        let cleaned: Vec<String> = lines.iter().map(|l| l.trim_end_matches('\r').to_string()).collect();
        let cols = if rows > 0 { cleaned[0].len() } else { 0 };
        let n = rows * cols;
        let mut b = Board {
            rows,
            lines: cleaned,
            cols,
            total_mines,
            revealed: vec![false; n],
            mine: vec![false; n],
            flagged: vec![false; n],
            q: vec![false; n],
            num: vec![0; n],
        };
        b._parse();
        b
    }

    fn _parse(&mut self) {
        for (r, ln) in self.lines.iter().enumerate() {
            for (c, ch) in ln.chars().enumerate() {
                let i = r * self.cols + c;
                match ch {
                    'F' => self.flagged[i] = true,
                    '?' => self.q[i] = true,
                    '*' => {
                        self.revealed[i] = true;
                        self.mine[i] = true;
                    }
                    '0'..='8' => {
                        self.revealed[i] = true;
                        self.num[i] = ch as u8 - b'0';
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn id(&self, r: usize, c: usize) -> usize {
        r * self.cols + c
    }

    pub fn rc(&self, i: usize) -> (usize, usize) {
        (i / self.cols, i % self.cols)
    }

    pub fn neighbors(&self, r: usize, c: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for dr in -1i64..=1 {
            for dc in -1i64..=1 {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let nr = r as i64 + dr;
                let nc = c as i64 + dc;
                if nr >= 0 && nc >= 0 && (nr as usize) < self.rows && (nc as usize) < self.cols {
                    out.push(self.id(nr as usize, nc as usize));
                }
            }
        }
        out
    }

    pub fn hidden(&self, i: usize) -> bool {
        !self.revealed[i] && !self.flagged[i]
    }

    pub fn flags_around(&self, i: usize) -> usize {
        let (r, c) = self.rc(i);
        let mut sum = 0;
        for j in self.neighbors(r, c) {
            if self.flagged[j] {
                sum += 1;
            }
        }
        sum
    }

    pub fn hidden_around(&self, i: usize) -> Vec<usize> {
        let (r, c) = self.rc(i);
        self.neighbors(r, c).into_iter().filter(|j| self.hidden(*j)).collect()
    }

    pub fn all_hidden(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for i in 0..self.rows * self.cols {
            if self.hidden(i) {
                out.push(i);
            }
        }
        out
    }

    pub fn lost(&self) -> bool {
        self.mine.iter().any(|m| *m)
    }

    pub fn won(&self) -> bool {
        if self.lost() {
            return false;
        }
        for ln in &self.lines {
            for ch in ln.chars() {
                if ch == '.' || ch == '?' {
                    return false;
                }
            }
        }
        true
    }
}

// ---------------------------------------------------------------------
// deterministic deduction
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Constraint {
    pub cells: BTreeSet<usize>,
    pub need: usize,
}

pub fn build_constraints(board: &Board) -> Vec<Constraint> {
    let mut cons = Vec::new();
    for r in 0..board.rows {
        for c in 0..board.cols {
            let i = board.id(r, c);
            if !board.revealed[i] || board.mine[i] {
                continue;
            }
            let hidden = board.hidden_around(i);
            if hidden.is_empty() {
                continue;
            }
            let flags = board.flags_around(i);
            let need = (board.num[i] as usize).wrapping_sub(flags);
            if need > hidden.len() {
                continue;
            }
            let cells: BTreeSet<usize> = hidden.into_iter().collect();
            cons.push(Constraint { cells, need });
        }
    }
    cons
}

pub fn deduce(board: &Board, cons: &[Constraint]) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let mut safes = BTreeSet::new();
    let mut mines = BTreeSet::new();
    let _ = board;

    for cons_item in cons {
        if cons_item.need == 0 {
            for i in &cons_item.cells {
                if board.hidden(*i) {
                    safes.insert(*i);
                }
            }
        } else if cons_item.need == cons_item.cells.len() {
            for i in &cons_item.cells {
                if board.hidden(*i) {
                    mines.insert(*i);
                }
            }
        }
    }

    let subset = |a: &BTreeSet<usize>, b: &BTreeSet<usize>| -> bool {
        if a.len() > b.len() {
            return false;
        }
        a.iter().all(|x| b.contains(x))
    };

    let mut changed = true;
    while changed {
        changed = false;
        for a in cons {
            for b in cons {
                if std::ptr::eq(a, b) || !subset(&a.cells, &b.cells) {
                    continue;
                }
                let diff: Vec<usize> = b.cells.difference(&a.cells).copied().collect();
                let dneed = b.need as i64 - a.need as i64;
                if dneed == 0 && !diff.is_empty() {
                    let fresh: Vec<usize> = diff.into_iter().filter(|i| board.hidden(*i)).collect();
                    if !fresh.is_empty() && !fresh.iter().all(|i| safes.contains(i)) {
                        for i in fresh {
                            safes.insert(i);
                        }
                        changed = true;
                    }
                } else if dneed == diff.len() as i64 && !diff.is_empty() {
                    let fresh: Vec<usize> = diff.into_iter().filter(|i| board.hidden(*i)).collect();
                    if !fresh.is_empty() && !fresh.iter().all(|i| mines.contains(i)) {
                        for i in fresh {
                            mines.insert(i);
                        }
                        changed = true;
                    }
                }
            }
        }
    }
    (safes, mines)
}

// ---------------------------------------------------------------------
// exact frontier probability
// ---------------------------------------------------------------------

pub struct ComponentResult {
    pub cells: BTreeSet<usize>,
    pub comps: Vec<Vec<usize>>,
}

pub fn frontier_components(_board: &Board, cons: &[Constraint]) -> ComponentResult {
    let mut cells = BTreeSet::new();
    for c in cons {
        for i in &c.cells {
            cells.insert(*i);
        }
    }
    if cells.is_empty() {
        return ComponentResult {
            cells,
            comps: vec![],
        };
    }

    let mut parent: BTreeMap<usize, usize> = BTreeMap::new();
    for i in &cells {
        parent.insert(*i, *i);
    }

    fn find(parent: &mut BTreeMap<usize, usize>, mut x: usize) -> usize {
        while parent[&x] != x {
            let g = parent[&parent[&x]];
            parent.insert(x, g);
            x = g;
        }
        x
    }
    fn union(parent: &mut BTreeMap<usize, usize>, a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }

    for c in cons {
        let sl: Vec<usize> = c.cells.iter().copied().collect();
        for i in 1..sl.len() {
            union(&mut parent, sl[0], sl[i]);
        }
    }

    let mut comps_map: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in &cells {
        let root = find(&mut parent, *i);
        comps_map.entry(root).or_default().push(*i);
    }
    ComponentResult {
        cells,
        comps: comps_map.into_values().collect(),
    }
}

/// Solves one frontier component exactly. Returns `(S, T)` where S maps
/// total-mines-in-component -> count of assignments, and T[cell] maps
/// total-mines -> count of assignments with that cell a mine.
/// Returns None if the node budget is exceeded.
pub fn solve_component(
    local_cells: &[usize],
    local_cons: &[Constraint],
    node_budget: u64,
) -> Option<(BTreeMap<usize, u64>, Vec<BTreeMap<usize, u64>>)> {
    let m = local_cells.len();
    if m == 0 {
        let mut s = BTreeMap::new();
        s.insert(0, 1);
        return Some((s, vec![]));
    }
    let cell_pos: BTreeMap<usize, usize> = local_cells.iter().enumerate().map(|(i, c)| (*c, i)).collect();
    let cons: Vec<(Vec<usize>, usize)> = local_cons
        .iter()
        .map(|c| {
            let idx: Vec<usize> = c.cells.iter().map(|x| cell_pos[x]).collect();
            (idx, c.need)
        })
        .collect();

    let mut member: Vec<Vec<usize>> = vec![vec![]; m];
    for (ci, (idx, _)) in cons.iter().enumerate() {
        for li in idx {
            member[*li].push(ci);
        }
    }

    // sort by -member.len() then index (stable; JS `a-b` tie)
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|a, b| {
        member[*b]
            .len()
            .cmp(&member[*a].len())
            .then_with(|| a.cmp(b))
    });
    let mut order_pos = vec![0usize; m];
    for (p, li) in order.iter().enumerate() {
        order_pos[*li] = p;
    }
    let _ = order_pos;

    let assigned = vec![-1i32; m];
    let con_mines = vec![0usize; cons.len()];
    let con_left: Vec<usize> = cons.iter().map(|(idx, _)| idx.len()).collect();

    let s: BTreeMap<usize, u64> = BTreeMap::new();
    let t: Vec<BTreeMap<usize, u64>> = (0..m).map(|_| BTreeMap::new()).collect();

    struct Ctx<'a> {
        m: usize,
        cons: &'a [(Vec<usize>, usize)],
        member: &'a [Vec<usize>],
        order: &'a [usize],
        assigned: Vec<i32>,
        con_mines: Vec<usize>,
        con_left: Vec<usize>,
        s: BTreeMap<usize, u64>,
        t: Vec<BTreeMap<usize, u64>>,
        nodes: u64,
        budget: u64,
        done: bool,
    }

    fn feasible(ctx: &Ctx, li: usize, val: usize) -> bool {
        for ci in &ctx.member[li] {
            let need = ctx.cons[*ci].1;
            let new_mines = ctx.con_mines[*ci] + val;
            if new_mines > need {
                return false;
            }
            let new_left = ctx.con_left[*ci] - 1;
            if need - new_mines > new_left {
                return false;
            }
        }
        true
    }

    fn rec(ctx: &mut Ctx, p: usize) {
        ctx.nodes += 1;
        if ctx.nodes > ctx.budget {
            ctx.done = true;
            return;
        }
        if p == ctx.m {
            let total = ctx.assigned.iter().filter(|v| **v == 1).count();
            *ctx.s.entry(total).or_insert(0) += 1;
            for (li, v) in ctx.assigned.iter().enumerate() {
                if *v == 1 {
                    *ctx.t[li].entry(total).or_insert(0) += 1;
                }
            }
            return;
        }
        let li = ctx.order[p];
        for val in [0usize, 1] {
            if ctx.done {
                return;
            }
            if !feasible(ctx, li, val) {
                continue;
            }
            ctx.assigned[li] = val as i32;
            for ci in ctx.member[li].clone() {
                ctx.con_mines[ci] += val;
                ctx.con_left[ci] -= 1;
            }
            rec(ctx, p + 1);
            if ctx.done {
                return;
            }
            for ci in ctx.member[li].clone() {
                ctx.con_mines[ci] -= val;
                ctx.con_left[ci] += 1;
            }
            ctx.assigned[li] = -1;
        }
    }

    let mut ctx = Ctx {
        m,
        cons: &cons,
        member: &member,
        order: &order,
        assigned,
        con_mines,
        con_left,
        s,
        t,
        nodes: 0,
        budget: node_budget,
        done: false,
    };
    rec(&mut ctx, 0);
    if ctx.nodes > node_budget || ctx.done {
        return None;
    }
    Some((ctx.s, ctx.t))
}

pub fn convolve(
    d1: &BTreeMap<usize, f64>,
    d2: &BTreeMap<usize, f64>,
) -> BTreeMap<usize, f64> {
    let mut out = BTreeMap::new();
    for (k1, v1) in d1 {
        for (k2, v2) in d2 {
            let k = k1 + k2;
            *out.entry(k).or_insert(0.0) += v1 * v2;
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct ProbResult {
    pub probs: BTreeMap<usize, f64>,
    pub nonfrontier_p: Option<f64>,
}

pub fn frontier_probabilities(board: &Board, cons: &[Constraint], node_budget: u64) -> ProbResult {
    let fc = frontier_components(board, cons);
    if fc.cells.is_empty() {
        return ProbResult {
            probs: BTreeMap::new(),
            nonfrontier_p: None,
        };
    }

    let all_s = &fc.cells;
    let mut free: Vec<usize> = Vec::new();
    for i in 0..board.rows * board.cols {
        if board.hidden(i) && !all_s.contains(&i) {
            free.push(i);
        }
    }
    let n_free = free.len();

    let mut comp_data: Vec<(Vec<usize>, BTreeMap<usize, u64>, Vec<BTreeMap<usize, u64>>)> = Vec::new();
    for comp in &fc.comps {
        let comp_set: BTreeSet<usize> = comp.iter().copied().collect();
        let local_cons: Vec<Constraint> = cons
            .iter()
            .filter(|c| c.cells.iter().all(|x| comp_set.contains(x)))
            .cloned()
            .collect();
        if let Some((s, t)) = solve_component(comp, &local_cons, node_budget) {
            comp_data.push((comp.clone(), s, t));
        }
    }
    if comp_data.is_empty() {
        return ProbResult {
            probs: BTreeMap::new(),
            nonfrontier_p: None,
        };
    }

    let mut comps_dist: Vec<BTreeMap<usize, f64>> = Vec::new();
    let mut comps_t: Vec<Vec<BTreeMap<usize, u64>>> = Vec::new();
    let mut comps_cells: Vec<Vec<usize>> = Vec::new();
    let mut comps_tot: Vec<u64> = Vec::new();
    for (comp, s, t) in &comp_data {
        let mut tot: u64 = 0;
        for v in s.values() {
            tot += *v;
        }
        let mut d = BTreeMap::new();
        for (k, v) in s {
            d.insert(*k, *v as f64 / tot as f64);
        }
        comps_dist.push(d);
        comps_t.push(t.clone());
        comps_cells.push(comp.clone());
        comps_tot.push(tot);
    }

    let k = comps_dist.len();
    let mut prefix: Vec<BTreeMap<usize, f64>> = Vec::with_capacity(k + 1);
    let mut single = BTreeMap::new();
    single.insert(0, 1.0f64);
    prefix.push(single.clone());
    for i in 0..k {
        let merged = convolve(&prefix[i], &comps_dist[i]);
        prefix.push(merged);
    }
    let mut suffix: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); k + 1];
    suffix[k] = single.clone();
    for i in (0..k).rev() {
        suffix[i] = convolve(&comps_dist[i], &suffix[i + 1]);
    }

    let m = board.total_mines as u64;
    let d = &prefix[k];

    let mut raw: BTreeMap<usize, BigUint> = BTreeMap::new();
    for t in d.keys() {
        raw.insert(*t, comb_big(n_free as u64, m - *t as u64));
    }
    let mut mx = BigUint::from(0u32);
    for v in raw.values() {
        if *v > mx {
            mx = v.clone();
        }
    }
    let w: BTreeMap<usize, f64> = if mx == BigUint::from(0u32) {
        d.keys().map(|t| (*t, 1.0f64)).collect()
    } else {
        raw.iter()
            .map(|(t, v)| (*t, big_to_f64(v) / big_to_f64(&mx)))
            .collect()
    };

    let mut z: f64 = 0.0;
    for (t, dt) in d {
        z += dt * w[t];
    }
    if z <= 0.0 {
        return ProbResult {
            probs: BTreeMap::new(),
            nonfrontier_p: None,
        };
    }

    let mut e_front: f64 = 0.0;
    for (t, dt) in d {
        e_front += (*t as f64) * dt * w[t];
    }
    e_front /= z;

    let nonfrontier_p = if n_free > 0 {
        Some(((m as f64 - e_front) / n_free as f64).clamp(0.0, 1.0))
    } else {
        None
    };

    let mut probs = BTreeMap::new();
    for ci in 0..comps_cells.len() {
        let comp = &comps_cells[ci];
        let d_except = convolve(&prefix[ci], &suffix[ci + 1]);
        let t = &comps_t[ci];
        let tot_i = comps_tot[ci];
        for (li, cell) in comp.iter().enumerate() {
            let mut num = 0.0f64;
            for (total, cnt) in &t[li] {
                let mut u = 0.0f64;
                for (o, po) in &d_except {
                    u += po * (w.get(&(total + o)).copied().unwrap_or(0.0));
                }
                num += (*cnt as f64 / tot_i as f64) * u;
            }
            probs.insert(*cell, num / z);
        }
    }

    ProbResult {
        probs,
        nonfrontier_p,
    }
}

// ---------------------------------------------------------------------
// move selection
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Strategy {
    pub tiebreak: String, // "minprob" | "random" | "info"
    pub first: String,    // "center" | "corner"
    pub use_chord: bool,
    pub refresh: bool,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy {
            tiebreak: "info".to_string(),
            first: "center".to_string(),
            use_chord: true,
            refresh: false,
        }
    }
}

pub fn choose_move(
    board: &Board,
    probs: &BTreeMap<usize, f64>,
    nonfrontier_p: Option<f64>,
    rng: &mut Mt19937,
    strategy: &Strategy,
) -> Option<usize> {
    let frontier: BTreeSet<usize> = probs.keys().copied().collect();
    let mut free: Vec<usize> = Vec::new();
    for i in 0..board.rows * board.cols {
        if board.hidden(i) && !frontier.contains(&i) {
            free.push(i);
        }
    }

    let mut candidates: Vec<(f64, usize, &str)> = Vec::new();
    for (i, p) in probs {
        candidates.push((*p, *i, "frontier"));
    }
    if !free.is_empty() && nonfrontier_p.is_some() {
        for i in &free {
            candidates.push((nonfrontier_p.unwrap(), *i, "free"));
        }
    }

    if candidates.is_empty() {
        let hidden = board.all_hidden();
        if hidden.is_empty() {
            return None;
        }
        return Some(rng.choice(&hidden));
    }

    let mut minp = f64::INFINITY;
    for (p, _, _) in &candidates {
        if *p < minp {
            minp = *p;
        }
    }
    let best: Vec<(f64, usize, &str)> = candidates
        .into_iter()
        .filter(|c| c.0 <= minp + 1e-12)
        .collect();

    if strategy.tiebreak == "random" {
        let b = rng.choice(&best);
        return Some(b.1);
    }

    let mut info = |c: &(f64, usize, &str)| -> (f64, f64) {
        let (_, i, kind) = c;
        if *kind == "free" {
            return (2.0, rng.random());
        }
        let (r, cc) = board.rc(*i);
        let nbrs = board.neighbors(r, cc);
        let mut rev = 0usize;
        for j in nbrs {
            if board.revealed[j] && !board.mine[j] {
                rev += 1;
            }
        }
        ((9 - rev) as f64 / 9.0, rng.random())
    };

    let mut best_c = best[0];
    let mut best_key = info(&best_c);
    for idx in 1..best.len() {
        let key = info(&best[idx]);
        if key.0 > best_key.0 {
            best_key = key;
            best_c = best[idx];
        }
    }
    Some(best_c.1)
}

fn chordable(board: &Board) -> bool {
    for r in 0..board.rows {
        for c in 0..board.cols {
            let i = board.id(r, c);
            if !board.revealed[i] || board.mine[i] {
                continue;
            }
            if board.num[i] == 0 {
                continue;
            }
            if board.flags_around(i) == board.num[i] as usize && !board.hidden_around(i).is_empty() {
                return true;
            }
        }
    }
    false
}

fn batch(board: &mut SimBoard, cmds: &[(&str, i64, i64)]) {
    for (cmd, r, c) in cmds {
        match *cmd {
            "flag" => board.flag(*r, *c),
            "chord" => board.chord(*r, *c),
            _ => board.click(*r, *c),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GameResult {
    pub win: bool,
    pub time: usize,
    pub moves: usize,
    pub chords: usize,
    pub flags: usize,
    pub guesses: usize,
    pub deduce_batches: usize,
    pub frontier: Vec<(usize, usize, f64)>,
    pub difficulty: String,
}

/// Play one game against a headless board. Mirrors `playGame()` in solver.js.
pub fn play_game(
    board: &mut SimBoard,
    difficulty: &str,
    strategy: &Strategy,
    rng: &mut Mt19937,
    node_budget: u64,
) -> GameResult {
    board.new_game(difficulty, board.seed).expect("difficulty");
    let rows = board.rows;
    let cols = board.cols;
    let mines = board.mines;

    let (r0, c0) = if strategy.first == "corner" {
        (0i64, 0i64)
    } else {
        (rows as i64 / 2, cols as i64 / 2)
    };
    board.click(r0, c0);

    let mut moves = 1usize;
    let mut chords = 0usize;
    let mut flags_placed = 0usize;
    let mut guesses = 0usize;
    let mut deduce_batches = 0usize;
    let mut guess_samples: Vec<(usize, usize, f64)> = Vec::new();

    loop {
        let blines = board.board();
        let b = Board::new(rows, &blines, mines);
        if b.lost() || b.won() {
            break;
        }

        let cons = build_constraints(&b);
        let (safes, mines_set) = deduce(&b, &cons);

        if !safes.is_empty() || !mines_set.is_empty() || (strategy.use_chord && chordable(&b)) {
            deduce_batches += 1;
            let mut cmds: Vec<(&str, i64, i64)> = Vec::new();
            for i in &mines_set {
                let (r, c) = b.rc(*i);
                cmds.push(("flag", r as i64, c as i64));
            }
            if strategy.use_chord {
                for r in 0..b.rows {
                    for c in 0..b.cols {
                        let i = b.id(r, c);
                        if !b.revealed[i] || b.mine[i] {
                            continue;
                        }
                        if b.num[i] == 0 {
                            continue;
                        }
                        if b.flags_around(i) == b.num[i] as usize && !b.hidden_around(i).is_empty() {
                            cmds.push(("chord", r as i64, c as i64));
                        }
                    }
                }
            }
            for i in &safes {
                if !mines_set.contains(i) {
                    let (r, c) = b.rc(*i);
                    cmds.push(("click", r as i64, c as i64));
                }
            }
            flags_placed += mines_set.len();
            chords += cmds.iter().filter(|c| c.0 == "chord").count();
            moves += cmds.len();
            batch(board, &cmds);
            continue;
        }

        let pr = frontier_probabilities(&b, &cons, node_budget);
        let target = choose_move(&b, &pr.probs, pr.nonfrontier_p, rng, strategy);
        let target = match target {
            Some(t) => t,
            None => break,
        };
        let (r, c) = b.rc(target);
        let probs_set: BTreeSet<usize> = pr.probs.keys().copied().collect();
        let free_count = (0..rows * cols)
            .filter(|i| b.hidden(*i) && !probs_set.contains(i))
            .count();
        let p = if pr.probs.contains_key(&target) {
            pr.probs[&target]
        } else {
            pr.nonfrontier_p.unwrap_or(0.0)
        };
        guess_samples.push((pr.probs.len(), free_count, p));
        board.click(r as i64, c as i64);
        moves += 1;
        guesses += 1;
    }

    let b = Board::new(rows, &board.board(), mines);
    GameResult {
        win: b.won() && !b.lost(),
        time: board.time,
        moves,
        chords,
        flags: flags_placed,
        guesses,
        deduce_batches,
        frontier: guess_samples,
        difficulty: difficulty.to_string(),
    }
}
