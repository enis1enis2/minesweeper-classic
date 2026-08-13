//! End-to-end tests of the `mserver` HTTP(S) endpoints.
//!
//! Verifies the `/ms-sim/*` endpoints over raw HTTP (the nginx/Cloudflare
//! front-proxy topology) and over native TLS (the `--https-port` listener),
//! including the auth challenge flow, the cursor-based seed poll and the
//! leaderboard, with the same wire values as `sim_roundtrip.rs`.

use hmac::{Hmac, Mac};
use rcgen::generate_simple_self_signed;
use sha2::Sha256;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
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
    plain_port: u16,
    http_port: Option<u16>,
    https_port: Option<u16>,
    ca_pem: Option<String>,
}

fn spawn_server(with_http: bool, with_https: bool, rate: &str) -> ServerProc {
    let exe = env!("CARGO_BIN_EXE_mserver");
    let mut cmd = Command::new(exe);
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--rate",
        rate,
        "--seed",
        "12345",
        "--solver-user",
        "alice",
        "--solver-pass",
        "secret",
        "--db",
        ":memory:",
    ]);
    let ca_pem = if with_https {
        let certified = generate_simple_self_signed(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])
        .unwrap();
        let cert_pem = certified.cert.pem();
        let key_pem = certified.key_pair.serialize_pem();
        let unique = format!(
            "mserver-http-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        let cert_path = std::env::temp_dir().join(format!("{}.cert.pem", unique));
        let key_path = std::env::temp_dir().join(format!("{}.key.pem", unique));
        std::fs::write(&cert_path, &cert_pem).unwrap();
        std::fs::write(&key_path, &key_pem).unwrap();
        cmd.arg("--tls-cert").arg(cert_path);
        cmd.arg("--tls-key").arg(key_path);
        Some(cert_pem)
    } else {
        None
    };
    if with_http {
        cmd.arg("--http-port").arg("0");
    }
    if with_https {
        cmd.arg("--https-port").arg("0");
    }

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mserver");

    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut plain_port = 0u16;
    let mut http_port = 0u16;
    let mut https_port = 0u16;
    let mut first = String::new();
    while Instant::now() < deadline {
        if reader.read_line(&mut first).unwrap_or(0) == 0 {
            break;
        }
        if plain_port == 0 {
            if let Some(idx) = first.find("listening on ") {
                let rest = &first[idx + "listening on ".len()..];
                if let Some(colon) = rest.find(':') {
                    let end = rest[colon + 1..]
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len() - colon - 1);
                    plain_port = rest[colon + 1..colon + 1 + end].parse().unwrap_or(0);
                }
            }
        }
        for (needle, slot) in [
            ("HTTP listening on ", &mut http_port),
            ("HTTPS listening on ", &mut https_port),
        ] {
            if *slot == 0 {
                if let Some(idx) = first.find(needle) {
                    let rest = &first[idx + needle.len()..];
                    if let Some(colon) = rest.find(':') {
                        let end = rest[colon + 1..]
                            .find(|c: char| !c.is_ascii_digit())
                            .unwrap_or(rest.len() - colon - 1);
                        *slot = rest[colon + 1..colon + 1 + end].parse().unwrap_or(0);
                    }
                }
            }
        }
        let need_https = if with_https { https_port > 0 } else { true };
        let need_http = if with_http { http_port > 0 } else { true };
        if plain_port > 0 && need_http && need_https {
            break;
        }
        first.clear();
    }
    assert!(plain_port > 0, "server did not report a plaintext port; first line: {:?}", first);
    if with_http {
        assert!(http_port > 0, "server did not report an HTTP port; first line: {:?}", first);
    }
    if with_https {
        assert!(https_port > 0, "server did not report an HTTPS port; first line: {:?}", first);
    }
    ServerProc { child, plain_port, http_port: if with_http { Some(http_port) } else { None }, https_port: if with_https { Some(https_port) } else { None }, ca_pem }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct HttpResponse {
    status: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        let lname = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&lname))
            .map(|(_, v)| v.as_str())
    }

    fn status_code(&self) -> &str {
        self.status.split_whitespace().nth(1).unwrap_or("")
    }
}

fn http_req(port: u16, request: &str) -> HttpResponse {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("connect failed: {}", e),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut resp = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => panic!("read error: {}", e),
        }
    }
    let text = String::from_utf8_lossy(&resp).into_owned();
    let mut lines = text.splitn(2, "\r\n\r\n");
    let head = lines.next().unwrap_or("");
    let body = lines.next().unwrap_or("");
    let mut parts = head.splitn(2, "\r\n");
    let status = parts.next().unwrap_or("").to_string();
    let headers = parts
        .next()
        .unwrap_or("")
        .lines()
        .filter_map(|l| {
            let idx = l.find(':')?;
            Some((l[..idx].trim().to_string(), l[idx + 1..].trim().to_string()))
        })
        .collect();
    HttpResponse { status, headers, body: body.to_string() }
}

fn post(port: u16, path: &str, body: &str, extra_headers: &str) -> HttpResponse {
    let req = format!(
        "POST {} HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: {}{}\r\n\r\n{}",
        path,
        body.len(),
        extra_headers,
        body
    );
    http_req(port, &req)
}

fn get(port: u16, path: &str) -> HttpResponse {
    let req = format!("GET {} HTTP/1.1\r\n\r\n", path);
    http_req(port, &req)
}

/// Two-step HMAC challenge: POST /ms-sim/auth for a nonce, then POST
/// /ms-sim/req with the digest. Returns the response body lines.
fn auth_and_req(port: u16, cmd: &str) -> Vec<String> {
    let resp = post(port, "/ms-sim/auth", "auth alice", "");
    assert!(resp.status_code() == "200", "auth status: {}", resp.status);
    let nonce = resp
        .body
        .lines()
        .find_map(|l| l.strip_prefix("authchal "))
        .expect("authchal")
        .to_string();
    let digest = hmac_sha256_hex("secret", &format!("ms-auth:{}", nonce));
    let resp = post(
        port,
        "/ms-sim/req",
        cmd,
        &format!("\r\nX-Ms-User: alice\r\nX-Ms-Auth: {}", digest),
    );
    assert!(resp.status_code() == "200", "req status: {}", resp.status);
    resp.body.lines().map(|l| l.to_string()).collect()
}

#[test]
fn healthz_and_metrics_ok() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();

    let resp = get(port, "/ms-sim/healthz");
    assert!(resp.status_code() == "200");
    assert_eq!(resp.body, "{\"ok\":true}\n");

    let body = "metric game_start beginner\nmetric ui_latency_ms 12\nnot-a-metric ignored\n";
    let resp = post(port, "/ms-sim/metrics", body, "");
    assert!(resp.status_code() == "200");
    assert_eq!(resp.body, "{\"ok\":true}\n");
}

#[test]
fn auth_req_round_trip_over_http() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();

    let lines = auth_and_req(port, "reqseed beginner 12345");
    assert!(lines.contains(&"reqgame beginner 12345".to_string()));
    assert!(lines.contains(&"seed beginner 12345".to_string()));
    assert!(lines.contains(&"outcome beginner 12345 1 19 0 1".to_string()));
    assert!(lines.contains(&"reqdone beginner 1".to_string()));

    // requntil loss path over HTTP.
    let lines = auth_and_req(port, "requntil expert 9999 3");
    assert!(lines.contains(&"outcome expert 9999 0 334 0 3".to_string()));
    assert!(lines.contains(&"lossfound expert 9999 0 0 334 0 3".to_string()));
}

#[test]
fn unauthenticated_req_is_denied() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();

    // No X-Ms-Auth header at all -> autherr.
    let resp = post(port, "/ms-sim/req", "reqseed beginner 5", "");
    assert!(resp.status_code() == "200");
    assert_eq!(resp.body, "autherr\n");

    // A valid challenge is required too: an arbitrary digest without a prior
    // /ms-sim/auth challenge is rejected.
    let resp = post(
        port,
        "/ms-sim/req",
        "reqseed beginner 5",
        "\r\nX-Ms-User: alice\r\nX-Ms-Auth: deadbeef",
    );
    assert_eq!(resp.body, "autherr\n");

    // An authed request for a losing seed runs the batch and reports reqdone
    // (the "reqdenied" line is reserved for the unauthenticated path).
    let lines = auth_and_req(port, "reqseed beginner 5");
    assert!(lines.contains(&"reqdone beginner 1".to_string()));
    assert!(!lines.contains(&"reqdenied".to_string()));
}

#[test]
fn unknown_user_gets_autherr() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();
    let resp = post(port, "/ms-sim/auth", "auth mallory", "");
    assert_eq!(resp.body, "autherr\n");
}

#[test]
fn seeds_poll_with_cursor() {
    let server = spawn_server(true, false, "1");
    let port = server.http_port.unwrap();

    // The producer only runs while at least one client is connected; hold a
    // TCP connection so broadcasts (and the feed) keep flowing.
    let hold = TcpStream::connect(("127.0.0.1", server.plain_port))
        .expect("hold connection");
    hold.set_read_timeout(Some(Duration::from_millis(100))).unwrap();

    // First poll returns nothing until the producer has broadcast once.
    let mut cursor = 0u64;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let resp = get(port, &format!("/ms-sim/seeds?since={}", cursor));
        assert!(resp.status_code() == "200");
        for line in resp.body.lines() {
            if line.starts_with("seed ") {
                cursor = resp.header("X-Ms-Cursor").unwrap_or("0").parse().unwrap();
                seen.push(line.to_string());
                break;
            }
        }
        if !seen.is_empty() || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!seen.is_empty(), "feed never produced a seed line");

    // The cursor-0 response must have carried the newest id.
    let first_cursor: u64 = get(port, "/ms-sim/seeds?since=0")
        .header("X-Ms-Cursor")
        .unwrap_or("0")
        .parse()
        .unwrap();
    assert!(first_cursor >= 1);

    // Asking since=cursor must NOT repeat the line we already consumed.
    let resp = get(port, &format!("/ms-sim/seeds?since={}", first_cursor));
    assert!(
        !resp.body.contains(&seen[0]),
        "cursor poll repeated an already-consumed line: {}",
        seen[0]
    );

    drop(hold);
}

#[test]
fn leaderboard_over_http() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();

    let resp = post(port, "/ms-sim/lbscore", "lbscore mallory intermediate 2500", "");
    assert!(resp.status_code() == "200");
    assert!(resp.body.contains("lbstored 1 intermediate mallory 2500"));

    let resp = get(port, "/ms-sim/lbtop?diff=intermediate");
    assert!(resp.status_code() == "200");
    assert!(resp.body.contains("lbtop intermediate 1"));
    assert!(resp.body.contains("lbentry 1 intermediate mallory 2500 "));
    assert!(resp.body.contains("lbdone"));
}

#[test]
fn unknown_path_returns_404() {
    let server = spawn_server(true, false, "0");
    let port = server.http_port.unwrap();
    let resp = get(port, "/ms-sim/nope");
    assert!(resp.status_code() == "404");
}

#[test]
fn https_healthz_over_tls() {
    let server = spawn_server(true, true, "0");
    let port = server.https_port.unwrap();
    let ca_pem = server.ca_pem.as_ref().unwrap().clone();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream as TokioTcp;

        let mut roots = rustls::RootCertStore::empty();
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(ca_pem.as_bytes()))
                .collect::<Result<_, _>>()
                .unwrap();
        for c in certs {
            roots.add(c).unwrap();
        }
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match TokioTcp::connect(("127.0.0.1", port)).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => panic!("connect failed: {}", e),
            }
        };
        let mut tls = connector
            .connect(server_name, stream)
            .await
            .expect("TLS handshake");
        tls.write_all(b"GET /ms-sim/healthz HTTP/1.1\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        let mut chunk = [0u8; 4096];
        loop {
            match tls.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => resp.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(_) => break,
            }
        }
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "HTTPS healthz status: {:?}",
            resp
        );
        assert!(resp.contains("{\"ok\":true}"));
    });
}

#[test]
fn http_flags_require_tls_cert_pair() {
    let exe = env!("CARGO_BIN_EXE_mserver");
    let out = Command::new(exe)
        .args([
            "--host", "127.0.0.1", "--port", "0", "--rate", "0", "--seed", "1",
            "--solver-user", "alice", "--solver-pass", "secret", "--db", ":memory:",
        ])
        .arg("--https-port")
        .arg("0")
        .output()
        .expect("run mserver");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 when --https-port is given without --tls-cert/--tls-key"
    );
}
