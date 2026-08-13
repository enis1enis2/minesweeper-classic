//! msapp — the Minesweeper client: egui frontend + `--listen` scripting server
//! + telemetry link. A clean-room Rust port of the C `minesweeper.exe`, sharing
//! the bit-exact `mscore` board engine with the server.

mod app;
mod core;
mod engine;
mod listen;
mod telemetry;

use app::MinesweeperApp;
use core::Core;
use listen::ListenServer;
use std::sync::{Arc, Mutex};

struct Cli {
    listen_port: u16,
    seed_args: Vec<(String, bool)>, // (arg, custom)
    telemetry_host: String,
    telemetry_port: u16, // 0 = off
    tls: bool,
    http: bool,
    tls_ca: Option<String>,
    solver_user: Option<String>,
    solver_pass: Option<String>,
    solver_config: Option<String>,
}

impl Default for Cli {
    fn default() -> Self {
        let (host, port) = core::default_endpoint();
        Cli {
            listen_port: 0,
            seed_args: Vec::new(),
            telemetry_host: host,
            telemetry_port: port,
            tls: false,
            http: false,
            tls_ca: None,
            solver_user: None,
            solver_pass: None,
            solver_config: None,
        }
    }
}

fn parse_cli() -> Cli {
    let mut cli = Cli::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let take_next = |i: &mut usize, val: &mut Option<String>| {
            if *i + 1 < args.len() {
                *val = Some(args[*i + 1].clone());
                *i += 1;
            }
        };
        if a == "--listen" {
            let mut v = None;
            take_next(&mut i, &mut v);
            if let Some(s) = v {
                cli.listen_port = s.parse().unwrap_or(0);
            }
        } else if let Some(s) = a.strip_prefix("--listen=") {
            cli.listen_port = s.parse().unwrap_or(0);
        } else if a == "--seed" {
            let mut v = None;
            take_next(&mut i, &mut v);
            if let Some(s) = v {
                cli.seed_args.push((s, false));
            }
        } else if let Some(s) = a.strip_prefix("--seed=") {
            cli.seed_args.push((s.to_string(), false));
        } else if a == "--seed-custom" {
            let mut v = None;
            take_next(&mut i, &mut v);
            if let Some(s) = v {
                cli.seed_args.push((s, true));
            }
        } else if let Some(s) = a.strip_prefix("--seed-custom=") {
            cli.seed_args.push((s.to_string(), true));
        } else if a == "--no-telemetry" {
            cli.telemetry_port = 0;
        } else if a == "--telemetry" {
            let mut v = None;
            take_next(&mut i, &mut v);
            if let Some(s) = v {
                parse_telemetry_arg(&s, &mut cli);
            }
        } else if let Some(s) = a.strip_prefix("--telemetry=") {
            parse_telemetry_arg(s, &mut cli);
        } else if a == "--tls" {
            cli.tls = true;
        } else if a == "--http" {
            cli.http = true;
        } else if a == "--tls-ca" {
            let mut v = None;
            take_next(&mut i, &mut v);
            cli.tls_ca = v;
        } else if let Some(s) = a.strip_prefix("--tls-ca=") {
            cli.tls_ca = Some(s.to_string());
        } else if a == "--solver-user" {
            let mut v = None;
            take_next(&mut i, &mut v);
            cli.solver_user = v;
        } else if let Some(s) = a.strip_prefix("--solver-user=") {
            cli.solver_user = Some(s.to_string());
        } else if a == "--solver-pass" {
            let mut v = None;
            take_next(&mut i, &mut v);
            cli.solver_pass = v;
        } else if let Some(s) = a.strip_prefix("--solver-pass=") {
            cli.solver_pass = Some(s.to_string());
        } else if a == "--solver-config" {
            let mut v = None;
            take_next(&mut i, &mut v);
            cli.solver_config = v;
        } else if let Some(s) = a.strip_prefix("--solver-config=") {
            cli.solver_config = Some(s.to_string());
        }
        i += 1;
    }
    cli
}

fn parse_telemetry_arg(arg: &str, cli: &mut Cli) {
    if let Some(colon) = arg.rfind(':') {
        let host = &arg[..colon];
        let port: u16 = arg[colon + 1..].parse().unwrap_or(0);
        if !host.is_empty() && port > 0 {
            cli.telemetry_host = host.to_string();
            cli.telemetry_port = port;
        }
    }
}

fn solver_config_read(path: &str) -> (Option<String>, Option<String>) {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return (None, None),
    };
    let get = |key: &str| -> Option<String> {
        let needle = format!("\"{}\"", key);
        if let Some(pos) = data.find(&needle) {
            let rest = &data[pos + needle.len()..];
            let rest = rest.trim_start();
            if let Some(colon) = rest.strip_prefix(':') {
                let rest = colon.trim_start().trim_start_matches('"');
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_string());
                }
            }
        }
        None
    };
    (get("user"), get("pass"))
}

fn setup_solver(cli: &mut Cli) -> (Option<String>, Option<String>) {
    let mut user = cli.solver_user.clone();
    let mut pass = cli.solver_pass.clone();
    if let Some(path) = &cli.solver_config {
        let (u, p) = solver_config_read(path);
        if user.is_none() {
            user = u;
        }
        if pass.is_none() {
            pass = p;
        }
    }
    if user.is_none() {
        user = std::env::var("MS_SOLVER_USER").ok().filter(|s| !s.is_empty());
    }
    if pass.is_none() {
        pass = std::env::var("MS_SOLVER_PASS").ok().filter(|s| !s.is_empty());
    }
    // Credentials only count when both halves are present (like the C client).
    match (user, pass) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => (Some(u), Some(p)),
        _ => (None, None),
    }
}

fn main() -> eframe::Result {
    let mut cli = parse_cli();
    let (user, pass) = setup_solver(&mut cli);

    let core = Arc::new(Mutex::new(Core::new()));
    {
        let mut c = core.lock().unwrap();
        c.host = cli.telemetry_host.clone();
        c.port = cli.telemetry_port;
        c.telemetry_on = cli.telemetry_port != 0;
        c.tls = cli.tls && cli.telemetry_port != 0;
        c.http = cli.http && cli.telemetry_port != 0;
        c.tls_ca = cli.tls_ca.clone();
        c.solver_user = user.clone().unwrap_or_default();
        c.solver_pass = pass.clone().unwrap_or_default();
        for (arg, custom) in &cli.seed_args {
            c.game.apply_seed_arg(arg, *custom);
        }
        c.game.reset(DIFF_BEGIN);
    }

    let (telemetry, rx) = telemetry::Telemetry::new();
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("msapp-rt")
            .build()
            .expect("tokio runtime"),
    );
    {
        let rt = rt.clone();
        let core = core.clone();
        let telemetry = telemetry.clone();
        let listen_port = cli.listen_port;
        std::thread::spawn(move || {
            rt.block_on(async move {
                telemetry::spawn(core.clone(), rx);
                if listen_port > 0 {
                    match listen::bind(listen_port).await {
                        Ok(listener) => {
                            let server = ListenServer::new(core.clone(), Some(telemetry.clone()));
                            tokio::spawn(server.run(listener));
                        }
                        Err(e) => {
                            eprintln!("listen {}: {}", listen_port, e);
                        }
                    }
                }
                std::future::pending::<()>().await;
            });
        });
    }

    let (board_w, board_h) = {
        let c = core.lock().unwrap();
        (c.game.board.cols as f32 * 24.0, c.game.board.rows as f32 * 24.0 + 90.0)
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([board_w + 16.0, board_h + 40.0])
            .with_resizable(true),
        ..Default::default()
    };
    let telemetry_gui = telemetry.clone();
    let core_gui = core.clone();
    eframe::run_native(
        "Minesweeper",
        options,
        Box::new(move |_cc| Ok(Box::new(MinesweeperApp::new(core_gui.clone(), Some(telemetry_gui.clone()))))),
    )
}

use engine::DIFF_BEGIN;
