//! Wire-protocol handlers (1:1 ports of `ms/sim-server/protocol.js` plus the
//! connection loop from `server.js`).

use crate::config::{self, HEAVY_CPU_SECONDS, LB_MAX, LB_MAX_IPS, LB_WINDOW, MAX_AUTH_FAILS, MAX_LINE};
use crate::crypto::{hmac_sha256_hex, timing_safe_eq};
use crate::db::{Database, GameRow, now_sec};
use crate::hub::{AdmissionGate, AuthStore, ClientHub, ClientWriter, FeedBuffer, RequestWorkers};
use crate::worker_pool::WorkerPool;
use crate::worker::Task;
use futures::FutureExt;
use mscore::mt19937::Mt19937;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::Mutex as TokioMutex;

/// Python int(): base-10 decimal only (no scientific, hex or underscores).
fn is_int_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    let b = tok.as_bytes();
    let digits = if b[0] == b'+' || b[0] == b'-' { &b[1..] } else { b };
    !digits.is_empty() && digits.iter().all(|x| x.is_ascii_digit())
}

fn parse_seed_token(tok: &str) -> Option<i128> {
    if !is_int_token(tok) {
        return None;
    }
    tok.parse().ok()
}

fn parse_count_token(tok: &str) -> Option<u64> {
    if !is_int_token(tok) {
        return None;
    }
    let i: i128 = tok.parse().ok()?;
    if i < 0 || i > u64::MAX as i128 {
        return None;
    }
    Some(i as u64)
}

/// `/^[A-Za-z0-9_-]{1,16}$/`
fn is_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 16 {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn outcome_line(diff: &str, seed: &str, g: &GameRow) -> String {
    format!(
        "outcome {} {} {} {} {} {}",
        diff,
        seed,
        if g.won { 1 } else { 0 },
        g.moves,
        g.time_ms,
        g.guesses
    )
}

pub struct Server {
    pub stop: AtomicBool,
    pub db: Arc<Database>,
    pub hub: Arc<ClientHub>,
    pub auth: Arc<AuthStore>,
    pub feed: Arc<FeedBuffer>,
    pub req_workers: Arc<RequestWorkers>,
    pub gate: Arc<AdmissionGate>,
    pub pool: Arc<WorkerPool>,
    pub diffs: Vec<String>,
    pub rate: f64,
    pub max_request: u64,
    pub solver_enabled: bool,
    pub solver_user: String,
    pub solver_pass: String,
    pub lb_hist: Mutex<HashMap<String, Vec<f64>>>,
    pub base: Instant,
}

impl Server {
    fn mono_now(&self) -> f64 {
        self.base.elapsed().as_secs_f64()
    }
}

/// Where protocol replies are written. The TCP path writes to the client's
/// hub writer; the HTTP(S) path collects lines into the response body.
pub trait LineSink {
    async fn send(&self, line: &str) -> bool;
}

pub struct HubSink<'a> {
    pub hub: &'a ClientHub,
    pub addr: &'a str,
}

impl LineSink for HubSink<'_> {
    async fn send(&self, line: &str) -> bool {
        self.hub.send_to(self.addr, line).await
    }
}

struct SimOpts {
    requester: Option<String>,
    decision_seed: Option<u64>,
    rng_state: Option<(Vec<u32>, usize)>,
}

async fn simulate_game(
    server: &Server,
    diff: String,
    seed: u64,
    opts: SimOpts,
) -> Result<(GameRow, Option<(Vec<u32>, usize)>), String> {
    let task = Task {
        diff,
        seed,
        decision_seed: opts.decision_seed,
        rng_state: opts.rng_state,
    };
    let mut msg = server.pool.submit(task).await.map_err(|e| {
        eprintln!("  simulate_game: worker pool error: {e}");
        e
    })?;
    msg.g.requester = opts.requester;
    server.db.record_game(&msg.g).map_err(|e| {
        eprintln!("  simulate_game: record_game failed: {}", e);
        "failed to record game".to_string()
    })?;
    Ok((msg.g, msg.rng_state))
}

fn split_tokens(line: &str) -> Vec<String> {
    line.split_whitespace().map(|s| s.to_string()).collect()
}

async fn handle_auth(server: &Server, addr: &str, line: &str) {
    let toks = split_tokens(line);
    if toks.len() < 2 || !server.solver_enabled {
        server.hub.send_to(addr, "autherr").await;
        return;
    }
    let user = &toks[1];
    if !timing_safe_eq(user, &server.solver_user) {
        server.hub.send_to(addr, "autherr").await;
        eprintln!("  auth: unknown user {:?} from {}", user, addr);
        return;
    }
    let nonce = server.auth.auth_begin(addr, user).await;
    match nonce {
        Some(n) => {
            server.hub.send_to(addr, &format!("authchal {}", n)).await;
        }
        None => {
            server.hub.send_to(addr, "autherr").await;
        }
    }
}

/// Returns false when the connection must be dropped (auth lockout).
async fn handle_authresp(server: &Server, addr: &str, line: &str) -> bool {
    let toks = split_tokens(line);
    if toks.len() < 2 {
        server.hub.send_to(addr, "autherr").await;
        return true;
    }
    let (nonce, user) = match server.auth.get(addr).await {
        Some(v) => v,
        None => {
            server.hub.send_to(addr, "autherr").await;
            return true;
        }
    };
    let expected = hmac_sha256_hex(server.solver_pass.as_bytes(), format!("ms-auth:{}", nonce).as_bytes());
    let (ok, fails) = server.auth.auth_resolve(addr, &toks[1], &expected).await;
    if ok {
        server.hub.send_to(addr, "authok").await;
        eprintln!("  auth: ok user={:?} from {}", user, addr);
        true
    } else {
        server.hub.send_to(addr, "autherr").await;
        eprintln!("  auth: FAILED user={:?} from {} (fails={})", user, addr, fails);
        if fails >= MAX_AUTH_FAILS {
            false // lockout: drop the connection
        } else {
            true
        }
    }
}

pub async fn handle_lbscore<S: LineSink>(server: &Server, sink: &S, addr: &str, line: &str) {
    let toks = split_tokens(line);
    if toks.len() != 4 {
        return;
    }
    let name = toks[1].clone();
    let diff = toks[2].to_lowercase();
    if !is_name(&name) || !config::is_difficulty(&diff) {
        return;
    }
    let ms = match parse_count_token(&toks[3]) {
        Some(ms) => ms,
        None => return,
    };
    if ms > 3_600_000 {
        return;
    }
    let ip = match addr.rfind(':') {
        Some(i) => addr[..i].to_string(),
        None => addr.to_string(),
    };
    let now = server.mono_now();
    // The rate-limit bookkeeping must not hold the (non-async) lock across
    // any `.await`; compute the verdict first, then send.
    let verdict = {
        // Recover from a poisoned mutex: rate-limit bookkeeping must not
        // hard-fail a leaderboard submission.
        let mut hist_map = server.lb_hist.lock().unwrap_or_else(|p| p.into_inner());
        let hist = hist_map.entry(ip.clone()).or_default();
        hist.retain(|t| now - t < LB_WINDOW);
        if hist.len() >= LB_MAX {
            Some("lbdenied")
        } else {
            hist.push(now);
            if hist.len() > LB_MAX {
                Some("lbdenied")
            } else {
                if hist_map.len() > LB_MAX_IPS {
                    let stale: Vec<String> = hist_map
                        .keys()
                        .filter(|k| !hist_map[*k].iter().any(|t| now - t < LB_WINDOW))
                        .cloned()
                        .collect();
                    for k in stale {
                        hist_map.remove(&k);
                    }
                }
                while hist_map.len() > LB_MAX_IPS {
                    let oldest_key = hist_map
                        .keys()
                        .min_by(|a, b| {
                            let la = hist_map[*a].last().unwrap_or(&f64::NEG_INFINITY);
                            let lb = hist_map[*b].last().unwrap_or(&f64::NEG_INFINITY);
                            la.partial_cmp(lb).unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .cloned();
                    if let Some(k) = oldest_key {
                        hist_map.remove(&k);
                    } else {
                        break;
                    }
                }
                None
            }
        }
    };
    if let Some(reply) = verdict {
        sink.send(reply).await;
        return;
    }
    let (improved, rank) = match server.db.record_score(&name, &diff, ms) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  lbscore: record_score failed: {}", e);
            sink.send("lbnotop").await;
            return;
        }
    };
    if improved {
        sink.send(&format!("lbstored {} {} {} {}", rank, diff, name, ms)).await;
    } else {
        sink.send("lbnotop").await;
    }
}

pub async fn handle_lbtop<S: LineSink>(server: &Server, sink: &S, _addr: &str, line: &str) {
    let toks = split_tokens(line);
    let mut count: u64 = 10;
    let mut diff: Option<String> = None;
    if toks.len() >= 3 && config::is_difficulty(&toks[1].to_lowercase()) {
        diff = Some(toks[1].to_lowercase());
        count = match parse_count_token(&toks[2]) {
            Some(c) => c,
            None => return,
        };
    } else if toks.len() >= 2 {
        count = match parse_count_token(&toks[1]) {
            Some(c) => c,
            None => return,
        };
    }
    if count < 1 || count > 100 {
        return;
    }
    let entries = match server.db.top_scores(diff.as_deref(), count) {
        Ok(v) => v,
        Err(e) => {
            // Degrade to an empty leaderboard rather than killing the
            // connection; the client still gets its lbtop/lbdone framing.
            eprintln!("  lbtop: top_scores failed: {}", e);
            Vec::new()
        }
    };
    match &diff {
        None => {
            sink.send(&format!("lbtop {}", entries.len())).await;
        }
        Some(d) => {
            sink.send(&format!("lbtop {} {}", d, entries.len())).await;
        }
    }
    for (rank, name, d, ms, ts) in &entries {
        sink.send(&format!("lbentry {} {} {} {} {}", rank, d, name, ms, ts)).await;
    }
    sink.send("lbdone").await;
}

/// handleRequest: a requested batch of games for one client, run strictly in
/// FIFO order on that client's request worker.
pub async fn handle_request<S: LineSink>(server: &Server, sink: &S, addr: &str, line: &str) {
    if !server.solver_enabled || !server.auth.is_authed(addr).await {
        sink.send("reqdenied").await;
        return;
    }
    let toks = split_tokens(line);
    let cmd = toks[0].to_lowercase();
    let diff: Option<String>;
    let mut seed: Option<i128> = None;
    let mut count: u64 = 1;
    let mut until = false;
    let mut seed_required = false;
    if cmd == "reqseed" && toks.len() >= 3 {
        diff = Some(toks[1].to_lowercase());
        seed = parse_seed_token(&toks[2]);
        seed_required = true;
        if toks.len() >= 4 {
            count = match parse_count_token(&toks[3]) {
                Some(c) => c,
                None => return,
            };
        }
    } else if cmd == "reqbatch" && toks.len() >= 3 {
        diff = Some(toks[1].to_lowercase());
        count = match parse_count_token(&toks[2]) {
            Some(c) => c,
            None => return,
        };
    } else if cmd == "requntil" && toks.len() >= 3 {
        diff = Some(toks[1].to_lowercase());
        until = true;
        seed = parse_seed_token(&toks[2]);
        seed_required = true;
        if toks.len() >= 4 {
            count = match parse_count_token(&toks[3]) {
                Some(c) => c,
                None => return,
            };
        }
    } else {
        return;
    }
    if seed_required && seed.is_none() {
        return;
    }
    let diff = match diff {
        Some(d) if config::is_difficulty(&d) => d,
        _ => return,
    };
    if count < 1 {
        return;
    }
    count = count.min(server.max_request);

    let heavy = (count as f64) * config::game_cpu_seconds(&diff) >= HEAVY_CPU_SECONDS;
    if heavy {
        sink.send(
            &format!(
                "reqwait {} {}",
                diff,
                seed.map(|s| s.to_string()).unwrap_or_else(|| count.to_string())
            ),
        )
        .await;
        server.gate.acquire().await;
    }
    let result = run_batch(server, sink, addr, &diff, seed, count, until).await;
    if heavy {
        server.gate.release().await;
    }
    if let Err(e) = result {
        eprintln!("  conn {}: request {} failed: {}", addr, line, e);
    }
}

/// Run one requested batch of games for a single client on its FIFO request
/// worker. Returns Err on a server-side failure (worker pool down / client
/// gone); the admission gate is released by `handle_request` regardless.
async fn run_batch<S: LineSink>(
    server: &Server,
    sink: &S,
    addr: &str,
    diff: &str,
    seed: Option<i128>,
    count: u64,
    until: bool,
) -> Result<(), String> {
    let mut batch_rng = match seed {
        Some(s) => {
            let mut r = Mt19937::new();
            r.seed_u64(s.abs() as u64);
            r
        }
        None => {
            let mut r = Mt19937::new();
            let mut words = [0u32; 624];
            for w in words.iter_mut() {
                *w = rand::random();
            }
            r.seed_from_words(&words);
            r
        }
    };
    let mut played: u64 = 0;
    let mut loss: Option<(u64, u64, u64, u64)> = None;
    for run in 0..count {
        let s = match seed {
            Some(s) => s,
            None => batch_rng.randrange(0, 1u64 << 63) as i128,
        };
        let s_wire = s.to_string();
        let s_u64 = s as u64; // BigInt.asUintN(64): SimBoard.new stores the mask
        // Random(seed) applies abs(); compute abs BEFORE the u64 cast so
        // negative seeds seed the decision RNG like `Random(BigInt(...))`.
        let decision = s ^ (run as i128) << 32;
        let decision_seed = decision.unsigned_abs() as u64;
        if !sink.send(&format!("reqgame {} {}", diff, s_wire)).await {
            return Err("client disconnected".into());
        }
        let (g, _) = simulate_game(
            server,
            diff.to_string(),
            s_u64,
            SimOpts {
                requester: Some(addr.to_string()),
                decision_seed: Some(decision_seed),
                rng_state: None,
            },
        )
        .await?;
        if !sink.send(&format!("seed {} {}", diff, s_wire)).await {
            return Err("client disconnected".into());
        }
        if !sink.send(&outcome_line(diff, &s_wire, &g)).await {
            return Err("client disconnected".into());
        }
        played += 1;
        if until && !g.won {
            loss = Some((if g.won { 1 } else { 0 }, g.moves as u64, g.time_ms as u64, g.guesses as u64));
            break;
        }
    }
    if until {
        match loss {
            Some((w, moves, tms, guesses)) => {
                sink.send(
                    &format!(
                        "lossfound {} {} {} {} {} {} {}",
                        diff,
                        seed.unwrap_or(0),
                        played - 1,
                        w,
                        moves,
                        tms,
                        guesses
                    ),
                )
                .await;
            }
            None => {
                sink.send(&format!("noloss {} {} {}", diff, seed.unwrap_or(0), played)).await;
            }
        }
    }
    sink.send(&format!("reqdone {} {}", diff, played)).await;
    Ok(())
}

/// The shared decision-RNG producer: broadcasts games to all connected
/// clients at the configured rate (port of `produce()`). The per-game body is
/// wrapped in catch_unwind so a panic anywhere inside one broadcast (e.g. a
/// transient DB failure) cannot kill the producer and silently stop the
/// telemetry stream.
pub async fn produce(server: Arc<Server>, rng: Arc<TokioMutex<Mt19937>>) {
    while !server.stop.load(Ordering::SeqCst) {
        while server.hub.count().await == 0 && !server.stop.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if server.stop.load(Ordering::SeqCst) {
            break;
        }
        let _ = std::panic::AssertUnwindSafe(produce_one(&server, &rng))
            .catch_unwind()
            .await;
        if server.rate > 0.0 {
            tokio::time::sleep(Duration::from_millis((1000.0 / server.rate) as u64)).await;
        }
    }
}

async fn produce_one(server: &Server, rng: &Arc<TokioMutex<Mt19937>>) {
    let (diff, seed, snapshot) = {
        let mut r = rng.lock().await;
        let diff = r.choice(&server.diffs);
        let seed = r.randrange(0, 1u64 << 63);
        let snapshot = r.snapshot();
        (diff, seed, snapshot)
    };
    match simulate_game(
        server,
        diff.clone(),
        seed,
        SimOpts {
            requester: None,
            decision_seed: None,
            rng_state: Some(snapshot),
        },
    )
    .await
    {
        Ok((g, rng_state)) => {
            if let Some(st) = rng_state {
                let mut r = rng.lock().await;
                r.restore(&st.0, st.1);
            }
            server.hub.broadcast(&format!("seed {} {}", diff, seed)).await;
            let outcome = outcome_line(&diff, &seed.to_string(), &g);
            server.hub.broadcast(&outcome).await;
            // Mirror the broadcast into the poll buffer so HTTP(S) clients
            // can replay the stream via GET /ms-sim/seeds?since=N.
            server.feed.push(&format!("seed {} {}", diff, seed));
            server.feed.push(&outcome);
        }
        Err(e) => {
            eprintln!("  produce: simulate failed: {}", e);
        }
    }
}

/// Connection loop: accumulate bytes (latin1 -> U+FFFD), split on '\n', trim,
/// and dispatch. Mirrors handleConn in protocol.js plus server.js sockets.
/// Generic over the transport so both plaintext `TcpStream` and
/// `TlsStream<TcpStream>` connections run through the same protocol code.
pub async fn handle_conn<S>(server: Arc<Server>, stream: S, addr: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut read, write) = tokio::io::split(stream);
    let writer: ClientWriter = Arc::new(TokioMutex::new(Box::new(write)));
    server.hub.add(addr.clone(), writer).await;
    let mut buf: String = String::new();
    let mut chunk = [0u8; 8192];
    let mut keep = true;
    while keep {
        match read.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                for &b in &chunk[..n] {
                    if b >= 0x80 {
                        buf.push('\u{FFFD}');
                    } else {
                        buf.push(b as char);
                    }
                }
                if buf.len() > MAX_LINE && !buf.contains('\n') {
                    eprintln!("  conn: {} oversized line, closing", addr);
                    break;
                }
                loop {
                    let Some(nl) = buf.find('\n') else { break };
                    let raw: String = buf.drain(..=nl).collect();
                    let text = raw.trim_end_matches('\r').trim();
                    if text.is_empty() {
                        continue;
                    }
                    keep = dispatch(&server, &addr, text).await;
                    if !keep {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    server.hub.remove(&addr).await;
    server.req_workers.drop_addr(&addr).await;
    server.auth.clear(&addr).await;
}

async fn dispatch(server: &Arc<Server>, addr: &str, text: &str) -> bool {
    if text.starts_with("metric ") {
        if let Err(e) = server.db.record_metric(now_sec(), addr, text) {
            eprintln!("  conn {}: record_metric failed: {}", addr, e);
        }
    } else if text.starts_with("auth ") {
        handle_auth(server, addr, text).await;
    } else if text.starts_with("authresp ") {
        if !handle_authresp(server, addr, text).await {
            return false;
        }
    } else if text.starts_with("lbscore ") {
        let sink = HubSink {
            hub: &server.hub,
            addr,
        };
        handle_lbscore(server, &sink, addr, text).await;
    } else if text == "lbtop" || text.starts_with("lbtop ") {
        let sink = HubSink {
            hub: &server.hub,
            addr,
        };
        handle_lbtop(server, &sink, addr, text).await;
    } else if text.starts_with("reqseed ")
        || text.starts_with("reqbatch ")
        || text.starts_with("requntil ")
    {
        if !server.solver_enabled || !server.auth.is_authed(addr).await {
            server.hub.send_to(addr, "reqdenied").await;
        } else {
            let srv = Arc::clone(server);
            let line = text.to_string();
            server
                .req_workers
                .enqueue(addr.to_string(), line, move |a, l| {
                    let srv = Arc::clone(&srv);
                    async move {
                        let sink = HubSink {
                            hub: &srv.hub,
                            addr: &a,
                        };
                        handle_request(&srv, &sink, &a, &l).await;
                    }
                })
                .await;
        }
    }
    true
}
