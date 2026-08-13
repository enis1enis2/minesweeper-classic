//! End-to-end TLS test of the compiled `mserver` binary.
//!
//! Verifies that the TLS listener terminates a rustls handshake with a
//! self-signed cert, runs the *same* wire protocol as the plaintext port
//! (auth + reqseed parity), and that plaintext clients hitting the TLS port
//! are rejected without harming the server.

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

/// Self-signed end-entity cert with SANs for `localhost` and 127.0.0.1,
/// written to unique temp files. Returns (cert path, key path, cert PEM).
fn make_test_cert() -> (String, String, String) {
    let certified = generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])
    .unwrap();
    let cert_pem = certified.cert.pem();
    let key_pem = certified.key_pair.serialize_pem();
    let unique = format!(
        "mserver-tls-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    );
    let cert_path = std::env::temp_dir().join(format!("{}.cert.pem", unique));
    let key_path = std::env::temp_dir().join(format!("{}.key.pem", unique));
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();
    (cert_path.display().to_string(), key_path.display().to_string(), cert_pem)
}

struct ServerProc {
    child: Child,
    tls_port: Option<u16>,
    ca_pem: Option<String>,
}

fn spawn_server(with_tls: bool) -> ServerProc {
    let exe = env!("CARGO_BIN_EXE_mserver");
    let mut cmd = Command::new(exe);
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--rate",
        "0",
        "--seed",
        "12345",
        "--solver-user",
        "alice",
        "--solver-pass",
        "secret",
        "--db",
        ":memory:",
    ]);
    let ca_pem = if with_tls {
        let (cert, key, pem) = make_test_cert();
        cmd.arg("--tls-port").arg("0");
        cmd.arg("--tls-cert").arg(&cert);
        cmd.arg("--tls-key").arg(&key);
        Some(pem)
    } else {
        None
    };

    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mserver");

    // Parse the bound ports from the "listening on" / "TLS listening on" lines.
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut plain_port = 0u16;
    let mut tls_port = 0u16;
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
        if tls_port == 0 {
            if let Some(idx) = first.find("TLS listening on ") {
                let rest = &first[idx + "TLS listening on ".len()..];
                if let Some(colon) = rest.find(':') {
                    let end = rest[colon + 1..]
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len() - colon - 1);
                    tls_port = rest[colon + 1..colon + 1 + end].parse().unwrap_or(0);
                }
            }
        }
        if plain_port > 0 && (!with_tls || tls_port > 0) {
            break;
        }
        first.clear();
    }
    assert!(plain_port > 0, "server did not report a plaintext port; first line: {:?}", first);
    if with_tls {
        assert!(tls_port > 0, "server did not report a TLS port; first line: {:?}", first);
    }
    ServerProc { child, tls_port: if with_tls { Some(tls_port) } else { None }, ca_pem }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read one complete line (trimmed, \r stripped) from the TLS read half.
async fn tls_read_line<R>(rd: &mut R, buf: &mut Vec<u8>) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let mut line = String::from_utf8_lossy(&raw[..raw.len().saturating_sub(1)]).into_owned();
            line = line.trim_end_matches('\r').trim().to_string();
            return Some(line);
        }
        let mut chunk = [0u8; 8192];
        let n = rd.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// TLS client harness: full auth + a fixed reqseed round trip.
fn tls_round_trip(port: u16, ca_pem: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use tokio::io::AsyncWriteExt;
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
        let mut buf = Vec::new();

        tls.write_all(b"auth alice\n").await.unwrap();
        let chal = loop {
            let line = tls_read_line(&mut tls, &mut buf).await.expect("eof in auth");
            if let Some(n) = line.strip_prefix("authchal ") {
                break n.to_string();
            }
        };
        let digest = hmac_sha256_hex("secret", &format!("ms-auth:{}", chal));
        tls.write_all(format!("authresp {}\n", digest).as_bytes()).await.unwrap();
        loop {
            let line = tls_read_line(&mut tls, &mut buf).await.expect("eof in authresp");
            if line == "authok" {
                break;
            }
            assert!(!line.starts_with("autherr"), "auth rejected over TLS: {}", line);
        }

        // Parity expectation matches sim_roundtrip's plaintext run: the TLS
        // transport must not change game outcomes.
        tls.write_all(b"reqseed beginner 12345\n").await.unwrap();
        let mut saw_outcome = false;
        let mut saw_reqdone = false;
        let mut saw_seed = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match tls_read_line(&mut tls, &mut buf).await {
                Some(line) => {
                    saw_outcome |= line == "outcome beginner 12345 1 19 0 1";
                    saw_seed |= line == "seed beginner 12345";
                    saw_reqdone |= line == "reqdone beginner 1";
                    if saw_outcome && saw_seed && saw_reqdone {
                        return;
                    }
                }
                None => break,
            }
        }
        panic!(
            "did not receive expected reqseed result over TLS (outcome={} seed={} reqdone={})",
            saw_outcome, saw_seed, saw_reqdone
        );
    });
}

#[test]
fn tls_client_round_trip_matches_plaintext_protocol() {
    let server = spawn_server(true);
    let ca_pem = server.ca_pem.as_ref().unwrap().clone();
    tls_round_trip(server.tls_port.unwrap(), &ca_pem);
}

#[test]
fn plaintext_client_rejected_on_tls_port() {
    let server = spawn_server(true);
    let port = server.tls_port.unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => panic!("connect failed: {}", e),
        }
    };
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let _ = stream.write_all(b"auth alice\n");
    // The TLS server reads these bytes as a bogus ClientHello, fails the
    // handshake and closes the socket — never a valid protocol reply.
    let mut probe = [0u8; 256];
    let n = stream.read(&mut probe).unwrap_or(0);
    let text = String::from_utf8_lossy(&probe[..n]);
    assert!(
        !text.contains("authchal"),
        "plaintext client received a protocol reply on the TLS port: {:?}",
        text
    );
}

#[test]
fn tls_flags_require_all_three() {
    let exe = env!("CARGO_BIN_EXE_mserver");
    let out = Command::new(exe)
        .args([
            "--host", "127.0.0.1", "--port", "0", "--rate", "0", "--seed", "1",
            "--solver-user", "alice", "--solver-pass", "secret", "--db", ":memory:",
        ])
        .arg("--tls-cert")
        .arg("nope.pem")
        .output()
        .expect("run mserver");
    assert_eq!(out.status.code(), Some(2), "expected exit code 2 for partial TLS args");
}
