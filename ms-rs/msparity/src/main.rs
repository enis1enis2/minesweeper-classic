//! msparity — byte-parity harness for the Minesweeper Rust port.
//!
//! Usage:
//!   msparity [--c-exe PATH] [--rust-exe PATH] [--seeds N] [--fast]
//!            [--port N] [--no-fixtures] [--no-live] [--help]
//!
//! Checks (all byte-exact unless noted):
//!   A. fixtures — mscore against `ms/test/fixtures/golden-{boards,rng,probs}.json`
//!                 (board dumps byte-exact, RNG streams, frontier probabilities
//!                 within 1e-9). No executables needed.
//!   B. live     — the same scripted command transcript driven against the C
//!                 client (`build/minesweeper-x64.exe`) and the Rust client
//!                 (`ms-rs/target/debug/msapp.exe --no-telemetry`) over the
//!                 listen protocol. Every reply (all lines through `END`) must
//!                 be byte-identical between the two clients.

use mscore::mt19937::Mt19937;
use mscore::sim_board::SimBoard;
use mscore::solver::{build_constraints, frontier_probabilities, Board};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE_DIR: &str = "ms/test/fixtures";
const DEFAULT_C_EXE: &str = "build/minesweeper-x64.exe";
const DEFAULT_RUST_EXE: &str = "ms-rs/target/debug/msapp.exe";

const DIFFS: [&str; 3] = ["beginner", "intermediate", "expert"];
const SIZES: [(usize, usize); 3] = [(8, 8), (16, 16), (16, 30)];
const FIRST_CLICKS: [&str; 3] = ["center", "corner", "edge"];

struct Args {
    c_exe: PathBuf,
    rust_exe: PathBuf,
    seeds: usize,
    fast: bool,
    port: u16,
    fixtures: bool,
    live: bool,
}

fn usage() {
    println!(
        "usage: msparity [--c-exe PATH] [--rust-exe PATH] [--seeds N] [--fast] \
         [--port N] [--no-fixtures] [--no-live] [--help]"
    );
}

fn parse_args() -> Args {
    let mut a = Args {
        c_exe: PathBuf::from(DEFAULT_C_EXE),
        rust_exe: PathBuf::from(DEFAULT_RUST_EXE),
        seeds: 8,
        fast: false,
        port: 31350,
        fixtures: true,
        live: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        match k.as_str() {
            "--c-exe" => a.c_exe = PathBuf::from(it.next().unwrap_or_default()),
            "--rust-exe" => a.rust_exe = PathBuf::from(it.next().unwrap_or_default()),
            "--seeds" => {
                a.seeds = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8)
            }
            "--port" => {
                a.port = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(31350)
            }
            "--fast" => a.fast = true,
            "--no-fixtures" => a.fixtures = false,
            "--no-live" => a.live = false,
            "--help" | "-h" => {
                usage();
                std::process::exit(0);
            }
            _ => {
                eprintln!("unknown argument: {k}");
                usage();
                std::process::exit(2);
            }
        }
    }
    a
}

fn fixture_path(name: &str) -> PathBuf {
    // CWD is the repo root; fall back to the fixture dir relative to this file.
    let from_cwd = Path::new(FIXTURE_DIR).join(name);
    if from_cwd.is_file() {
        return from_cwd;
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("..");
    p.push("..");
    p.push(FIXTURE_DIR);
    p.push(name);
    p
}

fn fixture_json(name: &str) -> serde_json::Value {
    let txt = std::fs::read_to_string(fixture_path(name)).unwrap_or_else(|e| {
        panic!("cannot read fixture {name}: {e}");
    });
    serde_json::from_str(&txt).unwrap_or_else(|e| panic!("bad fixture json {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Check A — golden fixtures vs mscore
// ---------------------------------------------------------------------------

fn check_fixtures() -> usize {
    let mut fails = 0;

    // boards: byte-exact dump + state keys
    {
        let arr = fixture_json("golden-boards.json");
        let data = arr.as_array().expect("boards array");
        let mut ok = 0usize;
        for entry in data {
            let e = entry.as_array().expect("board entry");
            let difficulty = e[0].as_str().unwrap().to_string();
            let seed = e[1].as_u64().unwrap();
            let click = e[2].as_array().unwrap();
            let cr = click[0].as_u64().unwrap() as i64;
            let cc = click[1].as_u64().unwrap() as i64;
            let expected_lines: Vec<String> = e[3]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_str().unwrap().to_string())
                .collect();
            let st = &e[4];

            let mut b = SimBoard::new(true);
            if let Err(err) = b.new_game(&difficulty, seed) {
                println!("  FAIL boards {difficulty} seed={seed}: new_game {err}");
                fails += 1;
                continue;
            }
            b.click(cr, cc);

            let got = b.board();
            if got != expected_lines {
                println!(
                    "  FAIL boards {difficulty} seed={seed} click=({cr},{cc}): dump differs"
                );
                for i in 0..got.len().max(expected_lines.len()) {
                    let g = got.get(i).cloned().unwrap_or_default();
                    let ex = expected_lines.get(i).cloned().unwrap_or_default();
                    if g != ex {
                        println!("    live: {g}");
                        println!("    gold: {ex}");
                        break;
                    }
                }
                fails += 1;
                continue;
            }
            let mut mism = Vec::new();
            for (key, want) in [
                ("opened", st["opened"].as_str()),
                ("flags", st["flags"].as_str()),
                ("mines", st["mines"].as_str()),
                ("rows", st["rows"].as_str()),
                ("cols", st["cols"].as_str()),
                ("over", st["over"].as_str()),
                ("started", st["started"].as_str()),
                ("seed", st["seed"].as_str()),
            ] {
                if let Some(w) = want {
                    let got_v = match key {
                        "opened" => b.opened.to_string(),
                        "flags" => b.flags.to_string(),
                        "mines" => b.mines.to_string(),
                        "rows" => b.rows.to_string(),
                        "cols" => b.cols.to_string(),
                        "over" => b.over.to_string(),
                        "started" => b.started.to_string(),
                        "seed" => b.seed.to_string(),
                        _ => unreachable!(),
                    };
                    if got_v != w {
                        mism.push(format!("{key} {got_v}!={w}"));
                    }
                }
            }
            if mism.is_empty() {
                ok += 1;
            } else {
                println!("  FAIL boards {difficulty} seed={seed}: {}", mism.join("; "));
                fails += 1;
            }
        }
        println!("  golden-boards.json : {ok}/{} PASS (dumps + state)", data.len());
    }

    // rng: floats and getrandbits(64) streams, fresh seed each
    {
        let arr = fixture_json("golden-rng.json");
        let data = arr.as_array().expect("rng array");
        let mut ok = 0usize;
        for entry in data {
            let seed_s = entry[0].as_str().unwrap();
            let floats: Vec<f64> = entry[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap())
                .collect();
            let gb64: Vec<String> = entry[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let seed_u64: u64 = seed_s.parse::<i128>().unwrap().unsigned_abs() as u64;

            let mut bad: Vec<String> = Vec::new();
            let mut m = Mt19937::new();
            m.seed_u64(seed_u64);
            for (i, e) in floats.iter().enumerate() {
                let g = m.random();
                if (g - e).abs() >= 1e-15 {
                    bad.push(format!("random[{i}] {g}!={e}"));
                }
            }
            let mut m2 = Mt19937::new();
            m2.seed_u64(seed_u64);
            for (i, e) in gb64.iter().enumerate() {
                if m2.getrandbits(64).to_string() != *e {
                    bad.push(format!("getrandbits(64)[{i}] mismatch"));
                }
            }
            if bad.is_empty() {
                ok += 1;
            } else {
                println!("  FAIL rng seed={seed_s}: {}", bad.join("; "));
                fails += 1;
            }
        }
        println!("  golden-rng.json    : {ok}/{} PASS (floats + getrandbits)", data.len());
    }

    // probs: frontier probabilities within 1e-9
    {
        let e = fixture_json("golden-probs.json");
        let arr = e.as_array().expect("probs entry");
        let lines: Vec<String> = arr[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        let expected: Vec<(u64, f64)> = arr[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|pair| {
                (
                    pair[0].as_str().unwrap().parse().unwrap(),
                    pair[1].as_f64().unwrap(),
                )
            })
            .collect();
        let expected_nfp = arr[2].as_f64().unwrap();

        let mut sim = SimBoard::new(true);
        sim.new_game("expert", 12345).expect("new_game");
        for _ in 0..10 {
            sim.click(14, 14);
        }
        if sim.board() != lines {
            println!("  FAIL probs: board reproduction differs");
            fails += 1;
        } else {
            let b = Board::new(16, &lines, 99);
            let cons = build_constraints(&b);
            let pr = frontier_probabilities(&b, &cons, 2_000_000);
            let mut got: Vec<(u64, f64)> =
                pr.probs.iter().map(|(c, p)| (*c as u64, *p)).collect();
            got.sort_by_key(|(c, _)| *c);
            let mut expected_sorted = expected.clone();
            expected_sorted.sort_by_key(|(c, _)| *c);

            let mut mism: Vec<String> = Vec::new();
            if got.len() != expected_sorted.len() {
                mism.push(format!("frontier size {}!={}", got.len(), expected_sorted.len()));
            }
            let tol = 1e-9;
            for (i, ((gc, gp), (ec, ep))) in
                got.iter().zip(expected_sorted.iter()).enumerate()
            {
                if gc != ec {
                    mism.push(format!("cell[{}] {}!={}", i, gc, ec));
                }
                if (gp - ep).abs() > tol {
                    mism.push(format!("prob[{gc}] {gp}!={ep}"));
                }
            }
            match pr.nonfrontier_p {
                Some(nfp) if (nfp - expected_nfp).abs() <= tol => {}
                other => mism.push(format!(
                    "nonfrontierP {:?}!={expected_nfp}",
                    other.map(|v| (v - expected_nfp).abs())
                )),
            }
            if mism.is_empty() {
                println!(
                    "  golden-probs.json  : PASS (frontier {} cells, nonfrontierP {:.6}, tol 1e-9)",
                    got.len(),
                    pr.nonfrontier_p.unwrap_or(f64::NAN)
                );
            } else {
                println!("  FAIL probs: {}", mism.join("; "));
                fails += 1;
            }
        }
    }

    fails
}

// ---------------------------------------------------------------------------
// Check B — live C client vs Rust client transcript parity
// ---------------------------------------------------------------------------

fn free_port(base: u16) -> u16 {
    for p in base..base + 500 {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
            let port = l.local_addr().unwrap().port();
            drop(l);
            return port;
        }
    }
    panic!("no free port near {base}");
}

fn spawn_exe(exe: &Path, port: u16, extra: &[&str]) -> Child {
    let mut cmd = Command::new(exe);
    cmd.arg("--listen")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for e in extra {
        cmd.arg(e);
    }
    cmd.spawn()
        .unwrap_or_else(|e| panic!("cannot spawn {}: {e}", exe.display()))
}

fn wait_port(port: u16, child: &mut Child, timeout: Duration) -> std::io::Result<()> {
    let t0 = Instant::now();
    let addr = ("127.0.0.1", port)
        .to_socket_addrs()
        .unwrap()
        .next()
        .unwrap();
    loop {
        if let Some(code) = child.try_wait()? {
            return Err(std::io::Error::other(format!(
                "exe exited early with code {code:?}"
            )));
        }
        match TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            Ok(s) => {
                drop(s);
                return Ok(());
            }
            Err(_) => {
                if t0.elapsed() > timeout {
                    return Err(std::io::Error::other("port never opened"));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

struct Cli {
    reader: BufReader<TcpStream>,
}

impl Cli {
    fn connect(port: u16) -> std::io::Result<Self> {
        let addr = ("127.0.0.1", port)
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap();
        let s = TcpStream::connect(addr)?;
        s.set_nodelay(true).ok();
        Ok(Cli {
            reader: BufReader::new(s),
        })
    }

    /// Send one command line; return every reply line including the `END`
    /// terminator, in order.
    fn cmd(&mut self, line: &str) -> Vec<String> {
        {
            let w = self.reader.get_mut();
            write!(w, "{line}\n").expect("write command");
            w.flush().expect("flush command");
        }
        let mut reply = Vec::new();
        loop {
            let mut l = String::new();
            let n = self.reader.read_line(&mut l).expect("read reply line");
            assert!(n > 0, "EOF before END for {line:?}");
            let l = l.trim_end_matches(['\r', '\n']).to_string();
            reply.push(l.clone());
            if l == "END" {
                break;
            }
        }
        reply
    }
}

fn first_cell(diff: usize, pos: &str) -> (i64, i64) {
    let (rows, cols) = SIZES[diff];
    match pos {
        "center" => (rows as i64 / 2, cols as i64 / 2),
        "corner" => (0, 0),
        _ => (rows as i64 / 2, 0),
    }
}

fn run_script(cli: &mut Cli, script: &[String]) -> Vec<Vec<String>> {
    script.iter().map(|line| cli.cmd(line)).collect()
}

fn combos(args: &Args) -> Vec<(usize, usize, u64)> {
    let mut rng = Mt19937::new();
    rng.seed_u64(20260209);
    let mut out = Vec::new();
    for (di, name) in DIFFS.iter().enumerate() {
        let positions: Vec<usize> = if args.fast && *name == "expert" {
            vec![0]
        } else {
            (0..FIRST_CLICKS.len()).collect()
        };
        for &pi in &positions {
            for _ in 0..args.seeds {
                let seed = rng.randrange(0, 1u64 << 63);
                out.push((di, pi, seed));
            }
        }
    }
    out
}

fn check_live(args: &Args) -> usize {
    if !args.c_exe.is_file() {
        println!("  SKIP live: C client not found at {}", args.c_exe.display());
        return 0;
    }
    if !args.rust_exe.is_file() {
        println!(
            "  SKIP live: Rust client not found at {} (cargo build -p msapp first)",
            args.rust_exe.display()
        );
        return 0;
    }

    let c_port = free_port(args.port);
    let r_port = free_port(args.port + 100);

    let mut c = spawn_exe(&args.c_exe, c_port, &[]);
    let mut r = spawn_exe(&args.rust_exe, r_port, &["--no-telemetry"]);

    let ok_c = wait_port(c_port, &mut c, Duration::from_secs(20));
    let ok_r = wait_port(r_port, &mut r, Duration::from_secs(20));
    if ok_c.is_err() || ok_r.is_err() {
        println!(
            "  FAIL live: cannot reach listeners (c: {:?}, rust: {:?})",
            ok_c.err(),
            ok_r.err()
        );
        let _ = c.kill();
        let _ = r.kill();
        return 1;
    }

    let mut cc = Cli::connect(c_port).expect("connect C");
    let mut rc = Cli::connect(r_port).expect("connect Rust");

    let mut fails = 0usize;
    let mut total = 0usize;
    let mut replies_cmp = 0usize;

    for (di, pi, seed) in combos(args) {
        let name = DIFFS[di];
        let (r0, c0) = first_cell(di, FIRST_CLICKS[pi]);

        // Normal-seed script.
        let script = vec![
            "ping".to_string(),
            format!("seed {name} {seed}"),
            "seeds".to_string(),
            format!("new {name}"),
            "state".to_string(),
            "board".to_string(),
            format!("click {r0} {c0}"),
            "state".to_string(),
            "board".to_string(),
        ];
        // Custom-seed (FNV path) script for the same combo.
        let cscript = vec![
            format!("seedcustom {name} txt{seed}"),
            "seeds".to_string(),
            format!("new {name}"),
            "state".to_string(),
            "board".to_string(),
        ];

        total += 1;
        let mut mismatches: Vec<String> = Vec::new();
        let c_replies = run_script(&mut cc, &script);
        let r_replies = run_script(&mut rc, &script);
        replies_cmp += c_replies.len() + r_replies.len();
        if c_replies != r_replies {
            mismatches.push("normal-seed transcript differs".to_string());
        }
        let cc_replies = run_script(&mut cc, &cscript);
        let rc_replies = run_script(&mut rc, &cscript);
        replies_cmp += cc_replies.len() + rc_replies.len();
        if cc_replies != rc_replies {
            mismatches.push("custom-seed transcript differs".to_string());
        }

        if !mismatches.is_empty() {
            fails += 1;
            println!(
                "  MISMATCH {} seed={} pos={} first=({r0},{c0}): {}",
                name,
                seed,
                FIRST_CLICKS[pi],
                mismatches.join("; ")
            );
            // show the first differing command, C side then Rust side
            for (label, a, b) in [
                ("normal", &c_replies, &r_replies),
                ("custom", &cc_replies, &rc_replies),
            ] {
                if a != b {
                    for i in 0..a.len().max(b.len()) {
                        let x = a.get(i).cloned().unwrap_or_default();
                        let y = b.get(i).cloned().unwrap_or_default();
                        if x != y {
                            println!("    [{label}] C   : {}", x.join("\n           "));
                            println!("    [{label}] rust: {}", y.join("\n           "));
                            break;
                        }
                    }
                }
            }
        }
    }

    let _ = c.kill();
    let _ = r.kill();
    let _ = c.wait();
    let _ = r.wait();

    println!(
        "  live (C vs Rust)   : {}/{} combos PASS, {} replies byte-identical",
        total - fails,
        total,
        replies_cmp
    );
    fails
}

fn main() {
    let args = parse_args();
    let mut fails = 0usize;

    println!("msparity — Minesweeper byte-parity harness\n");

    if args.fixtures {
        println!("-- fixture checks (mscore vs golden) --");
        fails += check_fixtures();
        println!();
    }
    if args.live {
        println!("-- live checks (C client vs Rust client, listen protocol) --");
        fails += check_live(&args);
        println!();
    }

    if fails == 0 {
        println!("RESULT: ALL PASS");
        std::process::exit(0);
    } else {
        println!("RESULT: {fails} check group(s) failed");
        std::process::exit(1);
    }
}
