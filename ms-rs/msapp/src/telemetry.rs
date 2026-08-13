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
use std::sync::Arc;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Duration};

const CONNECT_TO: u64 = 5;
const LOOP_TV_MS: u64 = 50;
const BEAT_MS: u64 = 10_000;
const RETRY_MS: u64 = 3_000;
const AUTH_PREFIX: &str = "ms-auth:";

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

async fn read_line(buf: &mut Vec<u8>, stream: &mut TcpStream) -> std::io::Result<Option<String>> {
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
    stream: Option<TcpStream>,
    inbound: Vec<u8>,
    out_lines: VecDeque<String>,
    session: bool,
}

impl LinkInner {
    async fn flush(&mut self, stream: &mut TcpStream) {
        let mut written = 0;
        while let Some(line) = self.out_lines.pop_front() {
            if line.starts_with("metric ") {
                written += 1;
            }
            if !write_all_checked(stream, &line).await {
                self.out_lines.push_front(line);
                break;
            }
        }
        if written > 0 {
            self.core.lock().unwrap().metrics_sent += written;
        }
    }
}

async fn write_all_checked(stream: &mut TcpStream, line: &str) -> bool {
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
                Ok(Ok(mut s)) => {
                    let _ = s.set_nodelay(true);
                    let (user, wanted) = {
                        let c = l.core.lock().unwrap();
                        (c.solver_user.clone(), c.solver_wanted())
                    };
                    if wanted && !write_all_checked(&mut s, &format!("auth {}", user)).await {
                        l.core.lock().unwrap().connected = false;
                        sleep(Duration::from_millis(RETRY_MS)).await;
                        continue;
                    }
                    l.core.lock().unwrap().connected = true;
                    l.core.lock().unwrap().auth_state = if wanted { AUTH_WAIT_CHAL } else { AUTH_NONE };
                    l.core.lock().unwrap().sim_session = false;
                    l.session = false;
                    l.flush(&mut s).await;
                    l.stream = Some(s);
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
                read_line(&mut l.inbound, stream),
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
    tokio::spawn(async move {
        run_task(core, rx).await;
    });
}
