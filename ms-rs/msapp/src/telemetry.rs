//! Telemetry link — a background task that keeps a non-blocking TCP connection
//! to the simulation server, handles the auth challenge-response, counts
//! broadcast seeds/outcomes, enforces the `g_sim_session` gate, relays metric
//! lines and feeds the leaderboard (a port of `network.c`).
//!
//! The task owns the socket; everything it reports is written back into the
//! shared `Core` under a short mutex critical section.

use crate::core::{Core, AUTH_NONE, AUTH_OK, AUTH_WAIT_CHAL, AUTH_WAIT_OK};
use crate::engine::{EngineEvent, DIFF_NAMES};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::VecDeque;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};

const CONNECT_TO: u64 = 5;
const LOOP_TV_MS: u64 = 50;
const BEAT_MS: u64 = 10_000;
const RETRY_MS: u64 = 3_000;
const AUTH_PREFIX: &str = "ms-auth:";

/// Combined trait so the telemetry socket can be stored as one trait object
/// that is both readable and writable (plaintext or TLS).
trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncReadWrite for T {}

/// The telemetry socket type, erased so the link code is identical for
/// plaintext `TcpStream` and `TlsStream<TcpStream>`.
type DynStream = Box<dyn AsyncReadWrite + Send>;

/// rustls client config: system webpki roots, plus an optional PEM CA bundle
/// (e.g. a self-signed or private CA) for `--tls-ca`.
fn build_client_config(ca_path: Option<&str>) -> std::io::Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_path {
        let data = std::fs::read(path)
            .map_err(|e| std::io::Error::new(e.kind(), format!("read {}: {}", path, e)))?;
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(&data[..]))
                .collect::<Result<_, _>>()
                .map_err(std::io::Error::other)?;
        if certs.is_empty() {
            return Err(std::io::Error::other(format!(
                "no PEM certificates found in {}",
                path
            )));
        }
        for c in certs {
            roots.add(c).map_err(std::io::Error::other)?;
        }
    }
    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

pub enum OutMsg {
    Request(String),
}

#[derive(Clone)]
pub struct Telemetry {
    tx: mpsc::UnboundedSender<OutMsg>,
}

impl Telemetry {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<OutMsg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Telemetry { tx }, rx)
    }

    pub fn request(&self, line: String) {
        let _ = self.tx.send(OutMsg::Request(line));
    }

    pub fn request_lbtop(&self, diff: Option<usize>, count: u32) {
        let line = match diff {
            Some(d) => format!("lbtop {} {}", DIFF_NAMES[d], count),
            None => format!("lbtop {}", count),
        };
        self.request(line);
    }
}

fn hmac_sha256_hex(pass: &str, msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(pass.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

async fn read_line<S>(buf: &mut Vec<u8>, stream: &mut S) -> std::io::Result<Option<String>>
where
    S: AsyncRead + Unpin + ?Sized,
{
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let s = String::from_utf8_lossy(&line).trim().to_string();
            return Ok(Some(s));
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

struct LinkInner {
    core: Arc<Mutex<Core>>,
    stream: Option<DynStream>,
    inbound: Vec<u8>,
    out_lines: VecDeque<String>,
    session: bool,
}

impl LinkInner {
    async fn flush(&mut self, stream: &mut DynStream) {
        let mut written = 0;
        while let Some(line) = self.out_lines.pop_front() {
            if line.starts_with("metric ") {
                written += 1;
            }
            if !write_all_checked(&mut **stream, &line).await {
                self.out_lines.push_front(line);
                break;
            }
        }
        if written > 0 {
            self.core.lock().unwrap().metrics_sent += written;
        }
    }
}

async fn write_all_checked<S>(stream: &mut S, line: &str) -> bool
where
    S: AsyncWrite + Unpin + ?Sized,
{
    stream.write_all(format!("{}\n", line).as_bytes()).await.is_ok()
}

fn parse_diff_token(tok: &str) -> Option<usize> {
    DIFF_NAMES.iter().position(|n| n.eq_ignore_ascii_case(tok))
}

fn handle_inbound(l: &mut LinkInner, line: &str) {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.is_empty() {
        return;
    }
    match toks[0].to_ascii_lowercase().as_str() {
        "seed" if toks.len() == 3 => {
            let mut c = l.core.lock().unwrap();
            c.seeds_recv += 1;
            if l.session {
                if let (Some(d), Ok(seed)) = (parse_diff_token(toks[1]), toks[2].parse::<u64>()) {
                    c.pending_seed_applies.push_back((d, seed));
                }
            }
        }
        "outcome" if toks.len() >= 4 => {
            let mut c = l.core.lock().unwrap();
            c.outcomes_recv += 1;
            if toks[3] == "1" {
                c.wins_recv += 1;
            }
        }
        "authchal" if toks.len() == 2 => {
            let (pass, state) = {
                let c = l.core.lock().unwrap();
                (c.solver_pass.clone(), c.auth_state)
            };
            if state == AUTH_WAIT_CHAL {
                let resp = hmac_sha256_hex(&pass, &format!("{}{}", AUTH_PREFIX, toks[1]));
                l.out_lines.push_back(format!("authresp {}", resp));
                l.core.lock().unwrap().auth_state = AUTH_WAIT_OK;
            }
        }
        "authok" => {
            let mut c = l.core.lock().unwrap();
            if c.auth_state == AUTH_WAIT_OK {
                c.auth_state = AUTH_OK;
            }
        }
        "autherr" => {
            l.core.lock().unwrap().auth_state = AUTH_NONE;
        }
        "reqdone" | "reqdenied" | "lossfound" | "noloss" => {
            l.session = false;
            l.core.lock().unwrap().sim_session = false;
            if toks[0].eq_ignore_ascii_case("reqdenied") {
                l.core.lock().unwrap().solver_denied = true;
            }
        }
        "lbtop" if toks.len() == 2 || toks.len() == 3 => {
            let mut c = l.core.lock().unwrap();
            c.leaderboard.clear();
            c.lb_status = "Loading...".to_string();
        }
        "lbentry" if toks.len() == 6 => {
            if let (Ok(rank), Ok(ms)) = (
                toks[1].parse::<u32>(),
                toks[4].parse::<u32>(),
            ) {
                let mut c = l.core.lock().unwrap();
                c.leaderboard.push(crate::core::LbEntry {
                    rank,
                    diff: toks[2].to_string(),
                    name: toks[3].to_string(),
                    time_ms: ms,
                });
                c.set_lb_status_from_entries();
            }
        }
        "lbdone" => {
            l.core.lock().unwrap().set_lb_status_from_entries();
        }
        _ => {} // welcome/stats/metric/req*/lbstored/lbdenied/lbnotop ignored
    }
}

fn handle_outbound(l: &mut LinkInner, msg: OutMsg) {
    match msg {
        OutMsg::Request(line) => {
            if !l.core.lock().unwrap().solver_ready() {
                return; // mirror net_send_request's refusal
            }
            l.session = true;
            l.core.lock().unwrap().sim_session = true;
            l.out_lines.push_back(line);
        }
    }
}

fn submit_win(l: &mut LinkInner, diff: usize, time: usize) {
    let (auto, name, is_preset) = {
        let c = l.core.lock().unwrap();
        (c.auto_submit, c.player_name.clone(), diff < 3 && c.connected)
    };
    if auto && is_preset && !name.is_empty() && (1..=3_600_000).contains(&(time * 1000)) {
        l.out_lines
            .push_back(format!("lbscore {} {} {}", name, DIFF_NAMES[diff], time * 1000));
    }
}

fn drain_engine_metrics(l: &mut LinkInner) {
    let events: Vec<EngineEvent> = l.core.lock().unwrap().game.drain_events();
    for ev in events {
        match ev {
            EngineEvent::GameStart {
                diff,
                seed,
                seeded,
            } => {
                l.out_lines.push_back(format!(
                    "metric start diff={} seed={} seeded={} t={}",
                    DIFF_NAMES[diff],
                    seed,
                    seeded as i64,
                    now_ms()
                ));
            }
            EngineEvent::GameOver {
                diff,
                won,
                seed,
                seeded,
                time,
                clicks,
                latency,
            } => {
                l.out_lines.push_back(format!(
                    "metric {} diff={} seed={} seeded={} time={} clicks={} latency={:.0} t={}",
                    if won { "win" } else { "loss" },
                    DIFF_NAMES[diff],
                    seed,
                    seeded as i64,
                    time,
                    clicks,
                    latency,
                    now_ms()
                ));
                if won {
                    submit_win(l, diff, time);
                }
            }
        }
    }
}

fn maybe_emit_latency(l: &mut LinkInner, now: std::time::Instant, next_beat: &mut std::time::Instant) {
    if now >= *next_beat {
        l.out_lines
            .push_back(format!("metric heartbeat t={}", now_ms()));
        *next_beat = now + Duration::from_millis(BEAT_MS);
    }
    let (started, over, t, ema, last) = {
        let c = l.core.lock().unwrap();
        (
            c.game.board.started,
            c.game.board.over,
            c.game.time as i64,
            c.game.latency_ema,
            c.last_latency_sent_sec,
        )
    };
    if started != 0 && over == 0 && t > 0 && t % 10 == 0 && t != last {
        l.core.lock().unwrap().last_latency_sent_sec = t;
        l.out_lines
            .push_back(format!("metric latency us={:.0} t={}", ema, now_ms()));
    }
}

async fn run_task(core: Arc<Mutex<Core>>, mut rx: mpsc::UnboundedReceiver<OutMsg>) {
    let mut l = LinkInner {
        core,
        stream: None,
        inbound: Vec::new(),
        out_lines: VecDeque::new(),
        session: false,
    };
    let mut next_beat = std::time::Instant::now() + Duration::from_millis(BEAT_MS);

    // Build the TLS connector once. If TLS was requested but the CA config is
    // invalid, fail closed: disable the link rather than silently downgrading
    // to plaintext.
    let tls_connector: Option<tokio_rustls::TlsConnector> = {
        let c = l.core.lock().unwrap();
        if c.tls {
            match build_client_config(c.tls_ca.as_deref()) {
                Ok(cfg) => Some(tokio_rustls::TlsConnector::from(Arc::new(cfg))),
                Err(e) => {
                    eprintln!("msapp: TLS config error: {}; telemetry disabled", e);
                    l.core.lock().unwrap().telemetry_on = false;
                    l.core.lock().unwrap().connected = false;
                    None
                }
            }
        } else {
            None
        }
    };

    loop {
        if !l.core.lock().unwrap().telemetry_on {
            sleep(Duration::from_millis(LOOP_TV_MS)).await;
            continue;
        }

        if l.stream.is_none() {
            let addr = {
                let c = l.core.lock().unwrap();
                format!("{}:{}", c.host, c.port)
            };
            l.core.lock().unwrap().attempts += 1;
            match timeout(Duration::from_secs(CONNECT_TO), TcpStream::connect(&addr)).await {
                Ok(Ok(s)) => {
                    let _ = s.set_nodelay(true);
                    let (user, wanted, tls_host) = {
                        let c = l.core.lock().unwrap();
                        (c.solver_user.clone(), c.solver_wanted(), c.host.clone())
                    };
                    let stream: DynStream = match &tls_connector {
                        Some(conn) => {
                            let server_name =
                                match rustls::pki_types::ServerName::try_from(tls_host.clone()) {
                                    Ok(n) => n,
                                    Err(e) => {
                                        eprintln!("msapp: invalid TLS server name {:?}: {}", tls_host, e);
                                        l.core.lock().unwrap().connected = false;
                                        sleep(Duration::from_millis(RETRY_MS)).await;
                                        continue;
                                    }
                                };
                            match timeout(
                                Duration::from_secs(CONNECT_TO),
                                conn.connect(server_name, s),
                            )
                            .await
                            {
                                Ok(Ok(tls)) => Box::new(tls),
                                _ => {
                                    l.core.lock().unwrap().connected = false;
                                    sleep(Duration::from_millis(RETRY_MS)).await;
                                    continue;
                                }
                            }
                        }
                        None => Box::new(s),
                    };
                    let mut stream = stream;
                    if wanted && !write_all_checked(&mut stream, &format!("auth {}", user)).await {
                        l.core.lock().unwrap().connected = false;
                        sleep(Duration::from_millis(RETRY_MS)).await;
                        continue;
                    }
                    l.core.lock().unwrap().connected = true;
                    l.core.lock().unwrap().auth_state = if wanted { AUTH_WAIT_CHAL } else { AUTH_NONE };
                    l.core.lock().unwrap().sim_session = false;
                    l.session = false;
                    l.flush(&mut stream).await;
                    l.stream = Some(stream);
                }
                _ => {
                    l.core.lock().unwrap().connected = false;
                    sleep(Duration::from_millis(RETRY_MS)).await;
                    continue;
                }
            }
        }

        let mut disconnected = false;
        {
            let stream = l.stream.as_mut().unwrap();
            let line = match timeout(
                Duration::from_millis(LOOP_TV_MS),
                read_line(&mut l.inbound, &mut **stream),
            )
            .await
            {
                Ok(Ok(Some(line))) => Some(line),
                Ok(Ok(None)) | Ok(Err(_)) => {
                    disconnected = true;
                    None
                }
                Err(_) => None,
            };
            if let Some(line) = line {
                handle_inbound(&mut l, &line);
            }
        }

        while let Ok(msg) = rx.try_recv() {
            handle_outbound(&mut l, msg);
        }
        drain_engine_metrics(&mut l);
        maybe_emit_latency(&mut l, std::time::Instant::now(), &mut next_beat);

        if disconnected {
            l.core.lock().unwrap().connected = false;
            l.core.lock().unwrap().sim_session = false;
            l.core.lock().unwrap().auth_state = AUTH_NONE;
            l.session = false;
            l.stream = None;
        } else if let Some(mut s) = l.stream.take() {
            l.flush(&mut s).await;
            l.stream = Some(s);
        }
    }
}

/// Spawn the telemetry task on the current tokio runtime.
pub fn spawn(core: Arc<Mutex<Core>>, rx: mpsc::UnboundedReceiver<OutMsg>) {
    let http = core.lock().unwrap().http;
    tokio::spawn(async move {
        if http {
            run_http_task(core, rx).await;
        } else {
            run_task(core, rx).await;
        }
    });
}

// ---------------------------------------------------------------------------
// HTTP(S) transport (`--http`): request/response against the /ms-sim/*
// endpoints instead of the streaming protocol. The server closes each
// connection after replying, so every exchange opens a fresh (optionally
// rustls-wrapped) connection; the seed cursor keeps polls lossless.
// ---------------------------------------------------------------------------

const HTTP_POLL_MS: u64 = 1_000; // seed poll interval
const HTTP_FLUSH_MS: u64 = 1_000; // metric flush interval
const METRIC_BUF_MAX: usize = 512;

struct HttpResp {
    status: u16,
    headers: Vec<(String, String)>,
    body_lines: Vec<String>,
}

async fn http_connect(
    core: &Arc<Mutex<Core>>,
    tls_connector: Option<&tokio_rustls::TlsConnector>,
) -> Result<DynStream, String> {
    let (host, port) = {
        let c = core.lock().unwrap();
        (c.host.clone(), c.port)
    };
    let addr = format!("{}:{}", host, port);
    let s = timeout(
        Duration::from_secs(CONNECT_TO),
        TcpStream::connect(&addr),
    )
    .await
    .map_err(|_| format!("connect timeout {}", addr))?
    .map_err(|e| format!("connect {}: {}", addr, e))?;
    let _ = s.set_nodelay(true);
    match tls_connector {
        Some(conn) => {
            let server_name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|e| format!("invalid TLS server name {:?}: {}", host, e))?;
            let tls = timeout(
                Duration::from_secs(CONNECT_TO),
                conn.connect(server_name, s),
            )
            .await
            .map_err(|_| "TLS handshake timeout".to_string())?
            .map_err(|e| format!("TLS handshake: {}", e))?;
            Ok(Box::new(tls))
        }
        None => Ok(Box::new(s)),
    }
}

async fn http_exchange(
    stream: &mut DynStream,
    host: &str,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> Result<HttpResp, String> {
    let mut hdrs = format!(
        "Host: {}\r\nContent-Type: text/plain; charset=utf-8\r\n",
        host
    );
    if !headers.is_empty() {
        hdrs.push_str(headers);
        hdrs.push_str("\r\n");
    }
    if !body.is_empty() {
        hdrs.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    let req = format!("{} {} HTTP/1.1\r\n{}\r\n{}", method, path, hdrs, body);
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {}", e))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let i = l.find(':')?;
            Some((l[..i].trim().to_ascii_lowercase(), l[i + 1..].trim().to_string()))
        })
        .collect();
    let body_lines: Vec<String> = body.lines().map(|l| l.to_string()).collect();
    Ok(HttpResp { status, headers, body_lines })
}

/// One request/response round trip on a fresh connection. Returns the parsed
/// 2xx response, or None on any transport/status failure.
async fn do_exchange(
    core: &Arc<Mutex<Core>>,
    tls_connector: Option<&tokio_rustls::TlsConnector>,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> Option<HttpResp> {
    let (host, _) = {
        let c = core.lock().unwrap();
        (c.host.clone(), c.port)
    };
    let mut stream = http_connect(core, tls_connector).await.ok()?;
    match http_exchange(&mut stream, &host, method, path, headers, body).await {
        Ok(resp) if (200..300).contains(&resp.status) => Some(resp),
        _ => None,
    }
}

/// Full HMAC challenge + request: POST /ms-sim/auth for a fresh nonce, then
/// POST /ms-sim/req with X-Ms-User / X-Ms-Auth. Returns the reply lines.
async fn do_req(
    core: &Arc<Mutex<Core>>,
    tls_connector: Option<&tokio_rustls::TlsConnector>,
    line: &str,
) -> Option<Vec<String>> {
    let (user, pass) = {
        let c = core.lock().unwrap();
        (c.solver_user.clone(), c.solver_pass.clone())
    };
    let chal = do_exchange(core, tls_connector, "POST", "/ms-sim/auth", "", &format!("auth {}", user))
        .await?;
    let nonce = chal
        .body_lines
        .iter()
        .find_map(|l| l.strip_prefix("authchal "))?
        .to_string();
    let digest = hmac_sha256_hex(&pass, &format!("{}{}", AUTH_PREFIX, nonce));
    let headers = format!("X-Ms-User: {}\r\nX-Ms-Auth: {}", user, digest);
    let resp = do_exchange(core, tls_connector, "POST", "/ms-sim/req", &headers, line).await?;
    Some(resp.body_lines)
}

fn lbtop_path(line: &str) -> String {
    let toks: Vec<&str> = line.split_whitespace().collect();
    match toks.len() {
        2 => format!("/ms-sim/lbtop?count={}", toks[1]),
        3 => format!("/ms-sim/lbtop?diff={}&count={}", toks[1], toks[2]),
        _ => "/ms-sim/lbtop".to_string(),
    }
}

async fn run_http_task(core: Arc<Mutex<Core>>, mut rx: mpsc::UnboundedReceiver<OutMsg>) {
    let mut l = LinkInner {
        core: core.clone(),
        stream: None,
        inbound: Vec::new(),
        out_lines: VecDeque::new(),
        session: false,
    };
    let tls_connector: Option<tokio_rustls::TlsConnector> = {
        let c = l.core.lock().unwrap();
        if c.tls {
            match build_client_config(c.tls_ca.as_deref()) {
                Ok(cfg) => Some(tokio_rustls::TlsConnector::from(Arc::new(cfg))),
                Err(e) => {
                    eprintln!("msapp: TLS config error: {}; telemetry disabled", e);
                    l.core.lock().unwrap().telemetry_on = false;
                    l.core.lock().unwrap().connected = false;
                    None
                }
            }
        } else {
            None
        }
    };
    let tls_ref = tls_connector.as_ref();
    let mut next_beat = std::time::Instant::now() + Duration::from_millis(BEAT_MS);
    let mut next_flush = std::time::Instant::now();
    let mut next_seed_poll = std::time::Instant::now();
    let mut cursor: u64 = 0;
    let mut metric_buf: Vec<String> = Vec::new();

    loop {
        if !l.core.lock().unwrap().telemetry_on {
            sleep(Duration::from_millis(LOOP_TV_MS)).await;
            continue;
        }
        while let Ok(msg) = rx.try_recv() {
            handle_outbound(&mut l, msg);
        }
        drain_engine_metrics(&mut l);
        maybe_emit_latency(&mut l, std::time::Instant::now(), &mut next_beat);

        // Partition queued lines by endpoint kind.
        let mut req_line: Option<String> = None;
        let mut lbscore_line: Option<String> = None;
        let mut lbtop_line: Option<String> = None;
        let mut keep: VecDeque<String> = VecDeque::new();
        for line in l.out_lines.drain(..) {
            let kind = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if kind == "metric" {
                if metric_buf.len() < METRIC_BUF_MAX {
                    metric_buf.push(line);
                } else {
                    l.core.lock().unwrap().metrics_dropped += 1;
                }
            } else if req_line.is_none() && kind.starts_with("req") {
                req_line = Some(line);
            } else if lbscore_line.is_none() && kind == "lbscore" {
                lbscore_line = Some(line);
            } else if lbtop_line.is_none() && kind == "lbtop" {
                lbtop_line = Some(line);
            } else {
                keep.push_back(line);
            }
        }
        l.out_lines = keep;

        let now = std::time::Instant::now();

        // Auth probe: after a successful challenge the server keys us by IP
        // (auth_resolve sets authed=true), so AUTH_OK mirrors the TCP authok.
        {
            let (wanted, state, denied) = {
                let c = l.core.lock().unwrap();
                (c.solver_wanted(), c.auth_state, c.solver_denied)
            };
            if wanted && state != AUTH_OK && !denied {
                let user = l.core.lock().unwrap().solver_user.clone();
                if let Some(chal) =
                    do_exchange(&l.core, tls_ref, "POST", "/ms-sim/auth", "", &format!("auth {}", user))
                        .await
                {
                    if chal.body_lines.iter().any(|x| x.starts_with("authchal ")) {
                        l.core.lock().unwrap().auth_state = AUTH_OK;
                        l.core.lock().unwrap().connected = true;
                    }
                }
            }
        }

        // Metrics flush.
        if now >= next_flush && !metric_buf.is_empty() {
            let body = metric_buf.join("\n");
            let n = metric_buf.len();
            if do_exchange(&l.core, tls_ref, "POST", "/ms-sim/metrics", "", &body).await.is_some() {
                l.core.lock().unwrap().metrics_sent += n as u64;
                metric_buf.clear();
                l.core.lock().unwrap().connected = true;
            }
            next_flush = std::time::Instant::now() + Duration::from_millis(HTTP_FLUSH_MS);
        }

        // Request: fresh challenge + req per request (nonces are single-use).
        if let Some(line) = req_line {
            match do_req(&l.core, tls_ref, &line).await {
                Some(replies) => {
                    let denied = replies.iter().any(|r| r == "autherr");
                    for r in replies {
                        handle_inbound(&mut l, &r);
                    }
                    if denied {
                        l.core.lock().unwrap().solver_denied = true;
                        l.core.lock().unwrap().auth_state = AUTH_NONE;
                        l.core.lock().unwrap().connected = false;
                    } else {
                        l.core.lock().unwrap().connected = true;
                    }
                }
                None => {
                    l.core.lock().unwrap().sim_session = false;
                    l.session = false;
                }
            }
        }

        // Leaderboard submit.
        if let Some(line) = lbscore_line {
            if do_exchange(&l.core, tls_ref, "POST", "/ms-sim/lbscore", "", &line).await.is_some() {
                l.core.lock().unwrap().connected = true;
            }
        }

        // Leaderboard fetch.
        if let Some(line) = lbtop_line {
            if let Some(resp) = do_exchange(&l.core, tls_ref, "GET", &lbtop_path(&line), "", "").await {
                for r in resp.body_lines {
                    handle_inbound(&mut l, &r);
                }
                l.core.lock().unwrap().connected = true;
            }
        }

        // Seed poll (liveness): the cursor makes it lossless.
        if now >= next_seed_poll {
            next_seed_poll = std::time::Instant::now() + Duration::from_millis(HTTP_POLL_MS);
            if let Some(resp) =
                do_exchange(&l.core, tls_ref, "GET", &format!("/ms-sim/seeds?since={}", cursor), "", "")
                    .await
            {
                if let Some(c) = resp.headers.iter().find(|(k, _)| k == "x-ms-cursor") {
                    if let Ok(c) = c.1.parse::<u64>() {
                        cursor = c;
                    }
                }
                for r in resp.body_lines {
                    handle_inbound(&mut l, &r);
                }
                l.core.lock().unwrap().connected = true;
            } else {
                l.core.lock().unwrap().connected = false;
                l.core.lock().unwrap().sim_session = false;
                l.core.lock().unwrap().auth_state = AUTH_NONE;
                l.session = false;
            }
            l.core.lock().unwrap().attempts += 1;
        }

        sleep(Duration::from_millis(LOOP_TV_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds_with_public_roots() {
        let cfg = build_client_config(None).expect("build without custom CA");
        assert!(cfg.alpn_protocols.is_empty());
    }

    #[test]
    fn client_config_rejects_missing_ca_file() {
        let err = build_client_config(Some("definitely-not-a-file.pem")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn client_config_rejects_empty_ca_file() {
        let p = std::env::temp_dir().join(format!("msapp-ca-empty-{}.pem", std::process::id()));
        std::fs::write(&p, b"not a certificate\n").unwrap();
        let err = build_client_config(Some(p.to_str().unwrap())).unwrap_err();
        assert!(
            err.to_string().contains("no PEM certificates"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn lbtop_path_builds_query() {
        assert_eq!(lbtop_path("lbtop 5"), "/ms-sim/lbtop?count=5");
        assert_eq!(
            lbtop_path("lbtop expert 3"),
            "/ms-sim/lbtop?diff=expert&count=3"
        );
        assert_eq!(lbtop_path("lbtop"), "/ms-sim/lbtop");
    }

    #[tokio::test]
    async fn http_exchange_builds_request_and_parses_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (server_half, client_half) = tokio::io::duplex(4096);
        let mut client: DynStream = Box::new(client_half);
        let mut server = server_half;

        let client_task = tokio::spawn(async move {
            http_exchange(&mut client, "example.test", "POST", "/ms-sim/req",
                "X-Ms-User: alice\r\nX-Ms-Auth: deadbeef", "reqseed beginner 12345")
                .await
                .expect("http_exchange")
        });

        let expected =
            "POST /ms-sim/req HTTP/1.1\r\nHost: example.test\r\nContent-Type: text/plain; charset=utf-8\r\nX-Ms-User: alice\r\nX-Ms-Auth: deadbeef\r\nContent-Length: 22\r\n\r\nreqseed beginner 12345";
        let mut req = vec![0u8; expected.len()];
        server.read_exact(&mut req).await.unwrap();
        let req = String::from_utf8_lossy(&req).into_owned();
        assert_eq!(req, expected);

        server
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nX-Ms-Cursor: 7\r\n\r\nseed beginner 5\n",
            )
            .await
            .unwrap();
        drop(server);

        let resp = client_task.await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.headers
                .iter()
                .find(|(k, _)| k == "x-ms-cursor")
                .map(|(_, v)| v.as_str()),
            Some("7")
        );
        assert_eq!(resp.body_lines, vec!["seed beginner 5"]);
    }

    #[tokio::test]
    async fn http_exchange_sends_get_without_content_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (server_half, client_half) = tokio::io::duplex(4096);
        let mut client: DynStream = Box::new(client_half);
        let mut server = server_half;

        let client_task = tokio::spawn(async move {
            http_exchange(&mut client, "example.test", "GET", "/ms-sim/seeds?since=3", "", "")
                .await
                .expect("http_exchange")
        });

        let expected = "GET /ms-sim/seeds?since=3 HTTP/1.1\r\nHost: example.test\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n";
        let mut req = vec![0u8; expected.len()];
        server.read_exact(&mut req).await.unwrap();
        let req = String::from_utf8_lossy(&req).into_owned();
        assert_eq!(req, expected);
        assert!(!req.contains("Content-Length"));

        server.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        drop(server);

        let resp = client_task.await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body_lines.is_empty());
    }
}
