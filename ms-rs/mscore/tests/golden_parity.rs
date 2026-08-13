//! Bit-exact parity tests against the frozen golden fixtures
//! (`ms/test/fixtures/golden-{boards,rng,probs}.json`), the same oracles the
//! Node/Python suites validate against.

use mscore::mt19937::Mt19937;
use mscore::sim_board::SimBoard;
use mscore::solver::{build_constraints, frontier_probabilities, Board};
use std::path::PathBuf;

fn fixture(name: &str) -> serde_json::Value {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("..");
    p.push("..");
    p.push("ms");
    p.push("test");
    p.push("fixtures");
    p.push(name);
    let txt = std::fs::read_to_string(&p).expect("fixture file");
    serde_json::from_str(&txt).expect("fixture json")
}

fn state_get(state: &serde_json::Value, key: &str) -> String {
    state
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

#[test]
fn golden_boards_byte_exact() {
    let data = fixture("golden-boards.json");
    let arr = data.as_array().expect("boards array");
    assert!(arr.len() >= 10, "expected >=10 board fixtures");
    for entry in arr {
        let e = entry.as_array().expect("board entry");
        let difficulty = e[0].as_str().unwrap().to_string();
        let seed = e[1].as_u64().unwrap();
        let click = e[2].as_array().unwrap();
        let cr = click[0].as_u64().unwrap() as usize;
        let cc = click[1].as_u64().unwrap() as usize;
        let expected_lines: Vec<String> = e[3]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        let state = &e[4];

        let mut b = SimBoard::new(true);
        b.new_game(&difficulty, seed).expect("new_game");
        b.click(cr as i64, cc as i64);

        let got_lines = b.board();
        assert_eq!(
            got_lines, expected_lines,
            "board mismatch for difficulty={} seed={} click=({},{})",
            difficulty, seed, cr, cc
        );
        assert_eq!(b.opened.to_string(), state_get(state, "opened"), "opened");
        assert_eq!(b.flags.to_string(), state_get(state, "flags"), "flags");
        assert_eq!(b.mines.to_string(), state_get(state, "mines"), "mines");
        assert_eq!(b.rows.to_string(), state_get(state, "rows"), "rows");
        assert_eq!(b.cols.to_string(), state_get(state, "cols"), "cols");
        assert_eq!(b.over.to_string(), state_get(state, "over"), "over");
        assert_eq!(b.started.to_string(), state_get(state, "started"), "started");
        assert_eq!(b.seed.to_string(), state_get(state, "seed"), "seed");
    }
}

#[test]
fn golden_rng_stream_full_sweep() {
    // Each seed's [floats] and [getrandbits] lists are INDEPENDENT streams,
    // each generated from a fresh seed.
    let data = fixture("golden-rng.json");
    let arr = data.as_array().expect("rng array");
    for entry in arr {
        let seed = entry[0].as_str().unwrap();
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

        // JS/Python seed: BigInt; negative seeds use |seed|.
        let seed_i: i128 = seed.parse().unwrap();
        let seed_u64: u64 = seed_i.unsigned_abs() as u64;

        let mut m = Mt19937 {
            state: vec![0; 624],
            index: 624,
        };
        m.seed_u64(seed_u64);
        for (i, e) in floats.iter().enumerate() {
            let got = m.random();
            assert!(
                (got - e).abs() < 1e-15,
                "seed {} random[{}] mismatch: {} vs {}",
                seed,
                i,
                got,
                e
            );
        }

        let mut m2 = Mt19937 {
            state: vec![0; 624],
            index: 624,
        };
        m2.seed_u64(seed_u64);
        for (i, e) in gb64.iter().enumerate() {
            assert_eq!(
                m2.getrandbits(64).to_string(),
                *e,
                "seed {} getrandbits(64)[{}] mismatch",
                seed,
                i
            );
        }
    }
}

#[test]
fn golden_probs_expert_12345() {
    let data = fixture("golden-probs.json");
    let e = data.as_array().expect("probs entry");
    let lines: Vec<String> = e[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    let expected: Vec<(u64, f64)> = e[1]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            let cell = pair[0].as_str().unwrap().parse().unwrap();
            let prob = pair[1].as_f64().unwrap();
            (cell, prob)
        })
        .collect();
    let expected_nfp = e[2].as_f64().unwrap();

    // Reproduce the shared mid-game board: expert, seed 12345, click (14,14).
    let mut sim = SimBoard::new(true);
    sim.new_game("expert", 12345).expect("new_game");
    for _ in 0..10 {
        sim.click(14, 14);
    }
    assert_eq!(sim.board(), lines, "board reproduction must match fixture");

    let b = Board::new(16, &lines, 99);
    let cons = build_constraints(&b);
    let pr = frontier_probabilities(&b, &cons, 2_000_000);

    let mut got: Vec<(u64, f64)> = pr
        .probs
        .iter()
        .map(|(c, p)| (*c as u64, *p))
        .collect();
    got.sort_by_key(|(c, _)| *c);

    let mut expected_sorted = expected.clone();
    expected_sorted.sort_by_key(|(c, _)| *c);

    assert_eq!(got.len(), expected_sorted.len(), "frontier size");
    let tol = 1e-9;
    for ((gc, gp), (ec, ep)) in got.iter().zip(expected_sorted.iter()) {
        assert_eq!(gc, ec, "cell index");
        assert!(
            (gp - ep).abs() <= tol,
            "prob mismatch cell {}: {} vs {}",
            gc,
            gp,
            ep
        );
    }
    match pr.nonfrontier_p {
        Some(nfp) => assert!(
            (nfp - expected_nfp).abs() <= tol,
            "nonfrontierP mismatch: {} vs {}",
            nfp,
            expected_nfp
        ),
        None => panic!("expected Some nonfrontierP, got None"),
    }
}
