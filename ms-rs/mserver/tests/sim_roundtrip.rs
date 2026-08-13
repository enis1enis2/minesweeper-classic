//! End-to-end test of the compiled `mserver` binary over TCP.
//!
//! The expected values below were captured from the reference JS server
//! (`ms/sim-server/server.js`) running with the same seed and commands, so
//! this test pins wire-format and game-outcome parity without needing Node at
//! test time.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn hmac_sha256_hex(key: &str, msg: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

struct ServerProc {
    child: Child,
    port: u16,
}

fn spawn_server(seed: &str, rate: &str) -> ServerProc {
    let exe = env!("CARGO_BIN_EXE_mserver");
    let mut child = Command::new(exe)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--rate",
            rate,
            "--seed",
            seed,
            "--solver-user",
            "alice",
            "--solver-pass",
            "secret",
            "--db",
            ":memory:",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mserver");

    // Parse the port from the "ms_server listening on ..." line.
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut port = 0u16;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first = String::new();
    while Instant::now() < deadline {
        if reader.read_line(&mut first).unwrap_or(0) == 0 {
            break;
        }
        if let Some(idx) = first.find("listening on ") {
            let rest = &first[idx + "listening on ".len()..];
            if let Some(colon) = rest.find(':') {
                let end = rest[colon + 1..].find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len() - colon - 1);
                port = rest[colon + 1..colon + 1 + end].parse().unwrap_or(0);
                break;
            }
        }
        first.clear();
    }
    assert!(port > 0, "server did not report a bound port; first line: {:?}", first);
    ServerProc { child, port }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn connect(port: u16) -> Client {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
                    return Client { stream: s, buf: Vec::new() };
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("connect failed: {}", e),
            }
        }
    }

    fn send(&mut self, line: &str) {
        self.stream.write_all(format!("{}\n", line).as_bytes()).unwrap();
    }

    /// Read lines until `needle` is seen; returns all complete lines read.
    fn read_until(&mut self, needle: &str, timeout: Duration) -> Vec<String> {
        let mut lines = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = find_nl(&self.buf) {
                let raw: Vec<u8> = self.buf.drain(..=pos).collect();
                let mut line = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]).into_owned();
                line = line.trim_end_matches('\r').trim().to_string();
                if !line.is_empty() {
                    lines.push(line.clone());
                    if line.contains(needle) {
                        return lines;
                    }
                }
                continue;
            }
            if Instant::now() >= deadline {
                panic!("timeout waiting for {:?}; got so far: {:?}", needle, lines);
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("server closed while waiting for {:?}", needle),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => panic!("read error: {}", e),
            }
        }
    }
}

fn find_nl(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == b'\n')
}

fn auth(client: &mut Client) {
    client.send("auth alice");
    let lines = client.read_until("authchal ", Duration::from_secs(10));
    let nonce = lines
        .iter()
        .find_map(|l| l.strip_prefix("authchal "))
        .expect("authchal")
        .to_string();
    let digest = hmac_sha256_hex("secret", &format!("ms-auth:{}", nonce));
    client.send(&format!("authresp {}", digest));
    let lines = client.read_until("authok", Duration::from_secs(10));
    assert!(lines.iter().any(|l| l == "authok"));
}

#[test]
fn auth_handshake_and_broadcast_parity() {
    let server = spawn_server("12345", "1");
    let mut client = Client::connect(server.port);
    auth(&mut client);

    // Producer's first broadcast (decision RNG seeded from --seed 12345).
    // Verified bit-identical against the reference JS server.
    let lines = client.read_until("outcome intermediate 187587978314466106 1 164 0 0", Duration::from_secs(30));
    assert!(lines.iter().any(|l| l == "seed intermediate 187587978314466106"));
}

#[test]
fn reqseed_fixed_seed_parity() {
    let server = spawn_server("12345", "0");
    let mut client = Client::connect(server.port);
    auth(&mut client);

    client.send("reqseed beginner 12345");
    let lines = client.read_until("reqdone beginner 1", Duration::from_secs(30));
    assert!(lines.contains(&"reqgame beginner 12345".to_string()));
    assert!(lines.contains(&"seed beginner 12345".to_string()));
    assert!(lines.contains(&"outcome beginner 12345 1 19 0 1".to_string()));

    client.send("reqseed intermediate 99");
    let lines = client.read_until("reqdone intermediate 1", Duration::from_secs(30));
    assert!(lines.contains(&"outcome intermediate 99 1 142 0 0".to_string()));

    client.send("reqseed expert 4242 3");
    let lines = client.read_until("reqdone expert 3", Duration::from_secs(60));
    assert_eq!(lines.iter().filter(|l| *l == "reqgame expert 4242").count(), 3);
    assert!(lines.contains(&"outcome expert 4242 1 389 0 3".to_string()));

    // Negative seeds: board seed wraps (toU64), decision RNG uses abs.
    client.send("reqseed beginner -5");
    let lines = client.read_until("reqdone beginner 1", Duration::from_secs(30));
    assert!(lines.contains(&"reqgame beginner -5".to_string()));
    assert!(lines.contains(&"outcome beginner -5 1 39 0 0".to_string()));
}

#[test]
fn requntil_loss_path_and_lb() {
    let server = spawn_server("7", "0");
    let mut client = Client::connect(server.port);
    auth(&mut client);

    client.send("requntil expert 9999 3");
    let lines = client.read_until("reqdone expert 1", Duration::from_secs(60));
    assert!(lines.contains(&"outcome expert 9999 0 334 0 3".to_string()));
    assert!(lines.contains(&"lossfound expert 9999 0 0 334 0 3".to_string()));

    client.send("lbscore mallory intermediate 2500");
    let lines = client.read_until("lbstored 1 intermediate mallory 2500", Duration::from_secs(10));
    assert!(lines.iter().any(|l| l == "lbstored 1 intermediate mallory 2500"));

    client.send("lbtop intermediate 5");
    let lines = client.read_until("lbdone", Duration::from_secs(10));
    assert!(lines.contains(&"lbtop intermediate 1".to_string()));
    assert!(lines.iter().any(|l| l.starts_with("lbentry 1 intermediate mallory 2500 ")));
}

#[test]
fn unauthenticated_requests_denied() {
    let server = spawn_server("1", "0");
    let mut client = Client::connect(server.port);
    client.send("reqseed beginner 5");
    let lines = client.read_until("reqdenied", Duration::from_secs(10));
    assert!(lines.iter().any(|l| l == "reqdenied"));
}

#[test]
fn bad_auth_responses_without_reauth_stay_open() {
    let server = spawn_server("1", "0");
    let mut client = Client::connect(server.port);
    client.send("auth alice");
    let lines = client.read_until("authchal ", Duration::from_secs(10));
    let nonce = lines
        .iter()
        .find_map(|l| l.strip_prefix("authchal "))
        .expect("authchal")
        .to_string();
    // Matches the reference JS server: repeated wrong authresp WITHOUT a fresh
    // `auth` do not reach the lockout threshold (nonce is null after the first
    // failed resolve), so the connection stays usable.
    let digest = hmac_sha256_hex("WRONG", &format!("ms-auth:{}", nonce));
    for _ in 0..5 {
        client.send(&format!("authresp {}", digest));
        let lines = client.read_until("autherr", Duration::from_secs(10));
        assert!(lines.iter().any(|l| l == "autherr"));
    }
    // Connection is still usable: a fresh auth + correct response succeeds.
    auth(&mut client);
}

#[test]
fn bad_auth_locks_out_when_each_attempt_reauths() {
    let server = spawn_server("1", "0");
    let mut client = Client::connect(server.port);
    for _ in 0..5 {
        client.send("auth alice");
        let lines = client.read_until("authchal ", Duration::from_secs(10));
        let nonce = lines
            .iter()
            .find_map(|l| l.strip_prefix("authchal "))
            .expect("authchal")
            .to_string();
        let digest = hmac_sha256_hex("WRONG", &format!("ms-auth:{}", nonce));
        client.send(&format!("authresp {}", digest));
        let lines = client.read_until("autherr", Duration::from_secs(10));
        assert!(lines.iter().any(|l| l == "autherr"));
    }
    // MAX_AUTH_FAILS reached -> server drops the connection.
    let mut probe = [0u8; 16];
    assert_eq!(client.stream.read(&mut probe).unwrap(), 0);
}

#[test]
fn malformed_input_does_not_crash_server_or_affect_other_clients() {
    let server = spawn_server("12345", "0");

    // Client A fires a barrage of malformed / borderline protocol lines.
    let mut a = Client::connect(server.port);
    a.send("metric ");
    a.send("auth");
    a.send("authresp");
    a.send("authresp deadbeef");
    a.send("lbscore");
    a.send("lbscore x nope 5");
    a.send("lbscore mallory beginner nope");
    a.send("lbscore mallory beginner 9999999999");
    a.send("lbtop");
    a.send("lbtop xyz 99999");
    a.send("lbtop 999999");
    a.send("lbtop 0");
    a.send("reqseed");
    a.send("reqseed beginner");
    a.send("reqseed bogus 5");
    a.send("reqseed beginner nope");
    a.send("reqbatch beginner nope");
    a.send("reqbatch beginner 0");
    a.send("requntil intermediate -");
    a.send("requntil expert");
    a.send("   ");
    a.send("\t\t");

    // Client B is completely unaffected: auth + a full request round-trip.
    let mut b = Client::connect(server.port);
    auth(&mut b);
    b.send("reqseed beginner 12345");
    let lines = b.read_until("reqdone beginner 1", Duration::from_secs(30));
    assert!(lines.contains(&"outcome beginner 12345 1 19 0 1".to_string()));

    // Client A is still connected and fully usable too.
    auth(&mut a);
    a.send("reqseed beginner 12345");
    let lines = a.read_until("reqdone beginner 1", Duration::from_secs(30));
    assert!(lines.contains(&"outcome beginner 12345 1 19 0 1".to_string()));
}
