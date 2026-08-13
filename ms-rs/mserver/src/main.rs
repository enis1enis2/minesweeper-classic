//! Minesweeper simulation server (CLI entry) — 1:1 port of `server.js`.
//!
//! Usage:
//!   mserver [--host 0.0.0.0] [--port 28571] [--db data/sim.db]
//!       [--rate 5] [--difficulty all|beginner|intermediate|expert]
//!       [--seed 12345] [--max-request 10000] [--max-concurrent 1]
//!       [--solver-user USER --solver-pass PASS | --solver-config FILE]
//!       [--tls-port 28572 --tls-cert cert.pem --tls-key key.pem]
//!   mserver --selfcheck

mod config;
mod crypto;
mod db;
mod http;
mod hub;
mod protocol;
mod worker;
mod worker_pool;

use crate::db::Database;
use crate::hub::{AdmissionGate, AuthStore, ClientHub, FeedBuffer, RequestWorkers};
use crate::protocol::{Server, handle_conn, produce};
use crate::worker_pool::WorkerPool;
use futures::FutureExt;
use mscore::mt19937::Mt19937;
use std::env;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Mutex as TokioMutex;

struct Args {
    host: String,
    port: u16,
    db: String,
    rate: f64,
    difficulty: String,
    seed: Option<u64>,
    max_request: u64,
    max_concurrent: usize,
    solver_user: Option<String>,
    solver_pass: Option<String>,
    solver_config: Option<String>,
    tls_port: Option<u16>,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    http_port: Option<u16>,
    https_port: Option<u16>,
}

fn parse_args() -> Result<Args, i32> {
    let mut host = String::from("0.0.0.0");
    let mut port = String::from("28571");
    let mut db = String::from("data/sim.db");
    let mut rate = String::from("5.0");
    let mut difficulty = String::from("all");
    let mut seed: Option<String> = None;
    let mut max_request = String::from("10000");
    let mut max_concurrent = String::from("1");
    let mut solver_user: Option<String> = None;
    let mut solver_pass: Option<String> = None;
    let mut solver_config: Option<String> = None;
    let mut tls_port: Option<String> = None;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    let mut http_port: Option<String> = None;
    let mut https_port: Option<String> = None;
    let mut selfcheck = false;
    let mut help = false;

    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or_default(),
            "--port" => port = args.next().unwrap_or_default(),
            "--db" => db = args.next().unwrap_or_default(),
            "--rate" => rate = args.next().unwrap_or_default(),
            "--difficulty" => difficulty = args.next().unwrap_or_default(),
            "--seed" => seed = Some(args.next().unwrap_or_default()),
            "--max-request" => max_request = args.next().unwrap_or_default(),
            "--max-concurrent" => max_concurrent = args.next().unwrap_or_default(),
            "--solver-user" => solver_user = Some(args.next().unwrap_or_default()),
            "--solver-pass" => solver_pass = Some(args.next().unwrap_or_default()),
            "--solver-config" => solver_config = Some(args.next().unwrap_or_default()),
            "--tls-port" => tls_port = Some(args.next().unwrap_or_default()),
            "--tls-cert" => tls_cert = Some(args.next().unwrap_or_default()),
            "--tls-key" => tls_key = Some(args.next().unwrap_or_default()),
            "--http-port" => http_port = Some(args.next().unwrap_or_default()),
            "--https-port" => https_port = Some(args.next().unwrap_or_default()),
            "--selfcheck" => selfcheck = true,
            "--help" | "-h" => help = true,
            other => {
                if !other.is_empty() {
                    usage();
                    return Err(2);
                }
            }
        }
    }

    if help {
        usage();
        return Err(0);
    }
    if selfcheck {
        let ok = run_selfcheck();
        std::process::exit(if ok { 0 } else { 1 });
    }

    let int_val = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let float_val = |s: &str| {
        let mut ok = !s.is_empty();
        let mut seen_dot = false;
        for (i, c) in s.bytes().enumerate() {
            match c {
                b'0'..=b'9' => {}
                b'.' if !seen_dot => seen_dot = true,
                b'-' if i == 0 => {}
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        ok
    };

    if !int_val(&port) || port.parse::<u16>().is_err() {
        eprintln!("ms_server: error: invalid --port value: '{}'", port);
        return Err(2);
    }
    if !float_val(&rate) || rate.parse::<f64>().is_err() {
        eprintln!("ms_server: error: invalid --rate value: '{}'", rate);
        return Err(2);
    }
    if !int_val(&max_request) {
        eprintln!("ms_server: error: invalid --max-request value: '{}'", max_request);
        return Err(2);
    }
    if !int_val(&max_concurrent) {
        eprintln!("ms_server: error: invalid --max-concurrent value: '{}'", max_concurrent);
        return Err(2);
    }
    let mut seed_val: Option<u64> = None;
    if let Some(s) = &seed {
        let neg = s.strip_prefix('-').map(|r| r.to_string()).or_else(|| s.strip_prefix('+').map(|r| r.to_string()));
        let digits = neg.as_deref().unwrap_or(s);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            eprintln!("ms_server: error: invalid --seed value: '{}'", s);
            return Err(2);
        }
        if let Ok(i) = digits.parse::<i128>() {
            seed_val = Some(i.unsigned_abs() as u64);
        } else {
            eprintln!("ms_server: error: invalid --seed value: '{}'", s);
            return Err(2);
        }
    }

    // TLS is all-or-nothing: any of --tls-port/--https-port/--tls-cert/
    // --tls-key must come as a complete set (cert+key plus at least one
    // port). The plaintext TCP port stays active either way.
    let tls_any =
        tls_port.is_some() || https_port.is_some() || tls_cert.is_some() || tls_key.is_some();
    if tls_any
        && !(tls_cert.is_some()
            && tls_key.is_some()
            && (tls_port.is_some() || https_port.is_some()))
    {
        eprintln!(
            "ms_server: error: --tls-cert and --tls-key are required together with --tls-port and/or --https-port"
        );
        return Err(2);
    }
    let tls_port_val: Option<u16> = match &tls_port {
        Some(p) => {
            if !int_val(p) || p.parse::<u16>().is_err() {
                eprintln!("ms_server: error: invalid --tls-port value: '{}'", p);
                return Err(2);
            }
            p.parse().ok()
        }
        None => None,
    };
    let parse_http = |name: &str, v: &Option<String>| -> Result<Option<u16>, i32> {
        match v {
            Some(p) => {
                if !int_val(p) || p.parse::<u16>().is_err() {
                    eprintln!("ms_server: error: invalid {} value: '{}'", name, p);
                    return Err(2);
                }
                Ok(p.parse().ok())
            }
            None => Ok(None),
        }
    };
    let http_port_val = parse_http("--http-port", &http_port)?;
    let https_port_val = parse_http("--https-port", &https_port)?;

    Ok(Args {
        host,
        port: port.parse().unwrap(),
        db,
        rate: rate.parse().unwrap(),
        difficulty,
        seed: seed_val,
        max_request: max_request.parse().unwrap(),
        max_concurrent: max_concurrent.parse().unwrap(),
        solver_user,
        solver_pass,
        solver_config,
        tls_port: tls_port_val,
        tls_cert,
        tls_key,
        http_port: http_port_val,
        https_port: https_port_val,
    })
}

fn usage() {
    println!(
        "usage: mserver [--host HOST] [--port PORT] [--db FILE] [--rate G/S] \
[--difficulty all|beginner|intermediate|expert] [--seed N] [--max-request N] \
[--max-concurrent N] [--solver-user USER --solver-pass PASS | --solver-config FILE] \
[--tls-port PORT --tls-cert CERT.pem --tls-key KEY.pem] \
[--http-port PORT] [--https-port PORT --tls-cert CERT.pem --tls-key KEY.pem] [--selfcheck]"
    );
}

fn resolve_solver(args: &Args) -> (Option<String>, Option<String>) {
    let mut user = args.solver_user.clone();
    let mut pw = args.solver_pass.clone();
    if let Some(cfg) = &args.solver_config {
        if let Ok(data) = std::fs::read_to_string(cfg) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                if user.is_none() {
                    user = json.get("user").and_then(|v| v.as_str()).map(String::from);
                }
                if pw.is_none() {
                    pw = json.get("pass").and_then(|v| v.as_str()).map(String::from);
                }
            }
        }
    }
    if user.is_none() {
        user = env::var("MS_SOLVER_USER").ok().filter(|s| !s.is_empty());
    }
    if pw.is_none() {
        pw = env::var("MS_SOLVER_PASS").ok().filter(|s| !s.is_empty());
    }
    (user, pw)
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => std::process::exit(code),
    };

    let (solver_user, solver_pass) = resolve_solver(&args);
    let solver_enabled = solver_user.is_some() && solver_pass.is_some();

    let diffs: Vec<String> = if args.difficulty != "all" {
        args.difficulty.split(',').map(String::from).collect()
    } else {
        config::DIFFS.iter().map(|s| s.to_string()).collect()
    };
    for d in &diffs {
        if !config::is_difficulty(d) {
            eprintln!(
                "unknown difficulty {:?} (use all|beginner|intermediate|expert)",
                d
            );
            std::process::exit(2);
        }
    }

    let db = Arc::new(Database::new(&args.db).expect("open database"));

    let pool = WorkerPool::new(args.max_concurrent + 2);
    let hub = ClientHub::new(Arc::clone(&db));
    let req_workers = RequestWorkers::new();
    let gate = AdmissionGate::new(args.max_concurrent);

    let server = Arc::new(Server {
        stop: AtomicBool::new(false),
        db: Arc::clone(&db),
        hub: Arc::clone(&hub),
        auth: Arc::new(AuthStore::new()),
        feed: FeedBuffer::new(),
        req_workers: Arc::clone(&req_workers),
        gate: Arc::clone(&gate),
        pool: Arc::clone(&pool),
        diffs: diffs.clone(),
        rate: args.rate,
        max_request: args.max_request,
        solver_enabled,
        solver_user: solver_user.unwrap_or_default(),
        solver_pass: solver_pass.unwrap_or_default(),
        lb_hist: std::sync::Mutex::new(std::collections::HashMap::new()),
        base: Instant::now(),
    });

    let rng = Arc::new(TokioMutex::new(make_decision_rng(args.seed)));

    let listener = TcpListener::bind((args.host.as_str(), args.port)).await;
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            eprintln!("listen error: {}", e);
            server.stop.store(true, Ordering::SeqCst);
            std::process::exit(1);
        }
    };
    let bound_port = listener.local_addr().unwrap().port();
    println!(
        "ms_server listening on {}:{}  (db={}  rate={:.1} g/s  max-concurrent={}  solver={})",
        args.host,
        bound_port,
        args.db,
        args.rate,
        args.max_concurrent,
        if solver_enabled { "protected" } else { "disabled" }
    );

    // TLS config is shared by the TLS TCP listener and the HTTPS listener.
    let tls_cfg: Option<rustls::ServerConfig> =
        if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
            Some(match load_tls_config(cert, key) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("ms_server: error: TLS: {}", e);
                    server.stop.store(true, Ordering::SeqCst);
                    std::process::exit(1);
                }
            })
        } else {
            None
        };

    // Optional TLS TCP listener (same wire protocol, encrypted). Plaintext
    // port stays active so legacy C clients can keep connecting.
    if let (Some(tls_port), Some(cfg)) = (&args.tls_port, &tls_cfg) {
        let tls_listener = match TcpListener::bind((args.host.as_str(), *tls_port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ms_server: error: TLS listen: {}", e);
                std::process::exit(1);
            }
        };
        let tls_bound = tls_listener.local_addr().unwrap().port();
        println!("ms_server TLS listening on {}:{}", args.host, tls_bound);
        let acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(cfg.clone())));
        let tls_server = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                match tls_listener.accept().await {
                    Ok((stream, peer)) => {
                        let addr = fmt_peer(&peer);
                        let srv = Arc::clone(&tls_server);
                        let acc = Arc::clone(&acceptor);
                        tokio::spawn(async move {
                            let tls_stream = match acc.accept(stream).await {
                                Ok(s) => s,
                                Err(e) => {
                                    // A failed handshake (wrong protocol, bad
                                    // cert, noise) must not affect the listener.
                                    eprintln!("conn {}: TLS handshake failed: {}", addr, e);
                                    return;
                                }
                            };
                            // One panic in a connection's handler must never
                            // take down the listener loop or other connections.
                            if let Err(panic) =
                                std::panic::AssertUnwindSafe(handle_conn(srv, tls_stream, addr.clone()))
                                    .catch_unwind()
                                    .await
                            {
                                eprintln!("conn {}: panic in TLS connection handler: {:?}", addr, panic);
                            }
                        });
                    }
                    Err(_) => {
                        if tls_server.stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Plaintext HTTP(S) endpoints (sim protocol over HTTP, for nginx/Cloudflare
    // front-proxying). Mirrors the /ms-diag/ingest model.
    if let Some(http_port) = &args.http_port {
        let http_listener = match TcpListener::bind((args.host.as_str(), *http_port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ms_server: error: HTTP listen: {}", e);
                std::process::exit(1);
            }
        };
        let http_bound = http_listener.local_addr().unwrap().port();
        println!("ms_server HTTP listening on {}:{}", args.host, http_bound);
        let http_server = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                match http_listener.accept().await {
                    Ok((stream, peer)) => {
                        let addr = fmt_peer(&peer);
                        let srv = Arc::clone(&http_server);
                        tokio::spawn(async move {
                            if let Err(panic) =
                                std::panic::AssertUnwindSafe(crate::http::handle_http_conn(srv, stream, addr.clone()))
                                    .catch_unwind()
                                    .await
                            {
                                eprintln!("conn {}: panic in HTTP connection handler: {:?}", addr, panic);
                            }
                        });
                    }
                    Err(_) => {
                        if http_server.stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Native TLS HTTP(S) endpoints, terminated by mserver itself.
    if let (Some(https_port), Some(cfg)) = (&args.https_port, &tls_cfg) {
        let https_listener = match TcpListener::bind((args.host.as_str(), *https_port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ms_server: error: HTTPS listen: {}", e);
                std::process::exit(1);
            }
        };
        let https_bound = https_listener.local_addr().unwrap().port();
        println!("ms_server HTTPS listening on {}:{}", args.host, https_bound);
        let acceptor = Arc::new(tokio_rustls::TlsAcceptor::from(Arc::new(cfg.clone())));
        let https_server = Arc::clone(&server);
        tokio::spawn(async move {
            loop {
                match https_listener.accept().await {
                    Ok((stream, peer)) => {
                        let addr = fmt_peer(&peer);
                        let srv = Arc::clone(&https_server);
                        let acc = Arc::clone(&acceptor);
                        tokio::spawn(async move {
                            let tls_stream = match acc.accept(stream).await {
                                Ok(s) => s,
                                Err(e) => {
                                    eprintln!("conn {}: HTTPS handshake failed: {}", addr, e);
                                    return;
                                }
                            };
                            if let Err(panic) =
                                std::panic::AssertUnwindSafe(crate::http::handle_http_conn(srv, tls_stream, addr.clone()))
                                    .catch_unwind()
                                    .await
                            {
                                eprintln!("conn {}: panic in HTTPS connection handler: {:?}", addr, panic);
                            }
                        });
                    }
                    Err(_) => {
                        if https_server.stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                }
            }
        });
    }

    {
        let server2 = Arc::clone(&server);
        tokio::spawn(async move {
            produce(server2, rng).await;
        });
    }

    let status_server = Arc::clone(&server);
    let status_timer = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let (games, wins, metrics, clients) = status_server.db.counts();
            println!("  games={} wins={} metrics={} clients={}", games, wins, metrics, clients);
        }
    });

    let accept_server = Arc::clone(&server);
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let addr = fmt_peer(&peer);
                    let srv = Arc::clone(&accept_server);
                    tokio::spawn(async move {
                        // One panic in a connection's handler must never take
                        // down the listener loop or other connections.
                        if let Err(panic) = std::panic::AssertUnwindSafe(handle_conn(srv, stream, addr.clone()))
                            .catch_unwind()
                            .await
                        {
                            eprintln!("conn {}: panic in connection handler: {:?}", addr, panic);
                        }
                    });
                }
                Err(_) => {
                    if accept_server.stop.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
    });

    // SIGINT/SIGTERM handling (best-effort on Windows).
    let ctrl_server = Arc::clone(&server);
    ctrlc::set_handler(move || {
        ctrl_server.stop.store(true, Ordering::SeqCst);
    })
    .ok();

    while !server.stop.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!("\nshutting down...");
    status_timer.abort();
    std::thread::sleep(Duration::from_millis(200));
    pool.shutdown();
    std::process::exit(0);
}

fn make_decision_rng(seed: Option<u64>) -> Mt19937 {
    let mut r = Mt19937::new();
    match seed {
        Some(s) => r.seed_u64(s),
        None => {
            let mut words = [0u32; 624];
            for w in words.iter_mut() {
                *w = rand::random();
            }
            r.seed_from_words(&words);
        }
    }
    r
}

fn fmt_peer(peer: &SocketAddr) -> String {
    format!("{}", peer)
}

/// Load a PEM certificate chain and PKCS#8/RSA/EC private key into a rustls
/// server config. The key file must not be password-protected.
fn load_tls_config(cert_path: &str, key_path: &str) -> Result<rustls::ServerConfig, String> {
    use std::io::BufReader;
    let cert_bytes = std::fs::read(cert_path).map_err(|e| format!("read {}: {}", cert_path, e))?;
    let key_bytes = std::fs::read(key_path).map_err(|e| format!("read {}: {}", key_path, e))?;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(&cert_bytes[..]))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("parse {}: {}", cert_path, e))?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path));
    }
    let key = rustls_pemfile::private_key(&mut BufReader::new(&key_bytes[..]))
        .map_err(|e| format!("parse {}: {}", key_path, e))?
        .ok_or_else(|| format!("no private key found in {}", key_path))?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("build config: {}", e))
}

/// `--selfcheck`: sanity-run one game per difficulty on a fixed seed and
/// assert the deterministic outcome, exercising DB + pool end to end.
fn run_selfcheck() -> bool {
    let mut ok = true;
    let cases: [(&str, u64, bool); 3] = [
        ("beginner", 1, true),
        ("intermediate", 2, true),
        ("expert", 3, false),
    ];
    for (diff, seed, expect_won) in cases {
        let task = crate::worker::Task {
            diff: diff.to_string(),
            seed,
            decision_seed: Some(seed),
            rng_state: None,
        };
        match crate::worker::run_game(&task) {
            Ok(res) => {
                let won = res.g.won;
                let status = if won == expect_won { "ok" } else { "MISMATCH" };
                println!(
                    "  selfcheck {} seed={} won={} moves={} -> {}",
                    diff, seed, won, res.g.moves, status
                );
                if won != expect_won {
                    ok = false;
                }
            }
            Err(e) => {
                eprintln!("  selfcheck {} FAILED: {}", diff, e);
                ok = false;
            }
        }
    }
    ok
}
