use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use msadmin::crypt::hash_password;
use msadmin::totp::{encode_base32, totp_value};

const PASSWORD: &str = "correct horse battery staple xyz";

struct HttpResp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn cookie(&self) -> Option<String> {
        let sc = self.header("set-cookie")?;
        for part in sc.split(';') {
            if let Some(eq) = part.find('=') {
                if part[..eq].trim() == "ms_admin" {
                    return Some(part[eq + 1..].trim().to_string());
                }
            }
        }
        None
    }
}

fn http_raw(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    content_length: usize,
    body: &[u8],
) -> HttpResp {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {content_length}\r\n\r\n"));
    stream.write_all(req.as_bytes()).expect("write request");
    stream.write_all(body).expect("write body");

    // Read the response head, then exactly Content-Length bytes. The server
    // closes without consuming an oversized request body, which makes the OS
    // send an RST after the response; that is expected for 4xx responses.
    let mut head_buf = Vec::new();
    loop {
        if head_buf.windows(4).position(|w| w == b"\r\n\r\n").is_some() {
            break;
        }
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => panic!("eof before headers: {:?}", String::from_utf8_lossy(&head_buf)),
            Ok(_) => head_buf.push(byte[0]),
            Err(e) => panic!("read headers: {e}: {:?}", String::from_utf8_lossy(&head_buf)),
        }
    }
    let head_end = head_buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&head_buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut parsed_headers = Vec::new();
    let mut response_length = 0usize;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim().to_string();
            let value = line[colon + 1..].trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                response_length = value.parse().unwrap_or(0);
            }
            parsed_headers.push((name, value));
        }
    }
    let mut response_body = Vec::with_capacity(response_length);
    response_body.extend_from_slice(&head_buf[head_end + 4..]);
    while response_body.len() < response_length {
        let mut buf = [0u8; 8192];
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response_body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let body = String::from_utf8_lossy(&response_body).into_owned();
    HttpResp { status, headers: parsed_headers, body }
}

fn http(addr: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> HttpResp {
    http_raw(addr, method, path, headers, body.len(), body)
}

fn get(addr: &str, path: &str, cookie: Option<&str>) -> HttpResp {
    let mut headers: Vec<String> = Vec::new();
    if let Some(c) = cookie {
        headers.push(format!("Cookie: ms_admin={c}"));
    }
    http_from_lines(addr, "GET", path, &headers, b"")
}

fn post(addr: &str, path: &str, cookie: Option<&str>, body: &str) -> HttpResp {
    let mut headers: Vec<String> = Vec::new();
    headers.push("Content-Type: application/x-www-form-urlencoded".to_string());
    if let Some(c) = cookie {
        headers.push(format!("Cookie: ms_admin={c}"));
    }
    http_from_lines(addr, "POST", path, &headers, body.as_bytes())
}

fn http_from_lines(
    addr: &str,
    method: &str,
    path: &str,
    header_lines: &[String],
    body: &[u8],
) -> HttpResp {
    let header_pairs: Vec<(&str, &str)> = header_lines
        .iter()
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap();
            (k, v.trim())
        })
        .collect();
    http_raw(addr, method, path, &header_pairs, body.len(), body)
}

fn ingest_doc(machine_id: &str, crash_text: &str) -> String {
    format!(
        r#"{{"machine_id":"{machine_id}","os":"debian-12","cpu":"Xeon E5","cpu_cores":8,"gpu":"NVIDIA","ram_mb":32768,"display":"1920x1080","game_version":"1.0.0","uptime_sec":123456,"crash_text":{crash_text}}}"#
    )
}

fn wait_for_port(child: &mut Child) -> (String, String) {
    let stdout = child.stdout.take().expect("capture stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for listening line");
        }
        line.clear();
        if reader.read_line(&mut line).expect("read line") == 0 {
            panic!("child exited before listening");
        }
        if line.starts_with("ms-admin listening on ") {
            let rest = line.trim();
            let parts: Vec<&str> = rest.split_whitespace().collect();
            // "ms-admin listening on 127.0.0.1:PORT (db=... config=...)"
            let addr = parts[3].to_string();
            std::thread::spawn(move || {
                for _ in reader.lines() {
                    // drain server stdout (request log) to avoid pipe backpressure
                }
            });
            return (addr, line);
        }
    }
}

fn start_server(tmp: &std::path::Path, extra_args: &[&str]) -> Child {
    let key = msadmin::crypt::generate_key();
    let secret_bytes: Vec<u8> = (0u8..20).collect();
    let secret = encode_base32(&secret_bytes);
    let config = format!(
        r#"{{"username":"admin","password_hash":"{}","totp_secret_b32":"{}","session_ttl_sec":3600}}"#,
        hash_password(PASSWORD).unwrap(),
        secret
    );
    std::fs::write(tmp.join("diag.key"), format!("{key}\n")).unwrap();
    std::fs::write(tmp.join("admin.json"), config).unwrap();
    let config_path = tmp.join("admin.json").to_str().unwrap().to_string();
    let key_path = tmp.join("diag.key").to_str().unwrap().to_string();
    let mut args: Vec<String> = vec![
        "--host".into(),
        "127.0.0.1".into(),
        "--port".into(),
        "0".into(),
        "--db".into(),
        ":memory:".into(),
        "--config".into(),
        config_path,
        "--key".into(),
        key_path,
    ];
    for a in extra_args {
        args.push(a.to_string());
    }
    Command::new(env!("CARGO_BIN_EXE_msadmin"))
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn msadmin")
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.0.kill().ok();
        let _ = self.0.wait();
    }
}

fn login(addr: &str) -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let code = totp_value(&encode_base32(&(0u8..20).collect::<Vec<u8>>()), now, 6).unwrap();
    let body = format!(
        "username=admin&password={}&totp={code}",
        PASSWORD.replace(' ', "%20")
    );
    let resp = post(addr, "/ms-admin/login", None, &body);
    assert_eq!(resp.status, 302, "login should redirect: {:?}", resp.headers);
    let token = resp.cookie().expect("login sets cookie");
    (token, body)
}

#[test]
fn admin_e2e_over_tcp() {
    let tmp = std::env::temp_dir().join(format!("msadmin-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut guard = ChildGuard(start_server(&tmp, &[]));
    let (addr, _line) = wait_for_port(&mut guard.0);
    eprintln!("[e2e] server at {addr}");

    // healthz is public
    let r = get(&addr, "/ms-admin/healthz", None);
    eprintln!("[e2e] healthz -> {}", r.status);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "ok\n");

    // ingest: valid -> {"ok":true}\n (Node sends no id)
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        ingest_doc("MACHINE-0001", "null").as_bytes(),
    );
    eprintln!("[e2e] ingest -> {}", r.status);
    assert_eq!(r.status, 200, "ingest: {}", r.body);
    assert_eq!(r.body, "{\"ok\":true}\n", "ingest body: {}", r.body);

    // ingest: missing field (first missing is "os") -> JSON error
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        r#"{"machine_id":"x"}"#.as_bytes(),
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"missing field os\"}\n", "got: {}", r.body);

    // ingest: bad json
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        r#"{"machine_id":"x","os":"y","cpu":"z","cpu_cores":1,"gpu":"g","ram_mb":1,"display":"d","game_version":"v","uptime_sec":1,"crash_text":null,"#.as_bytes(),
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"bad json\"}\n", "got: {}", r.body);

    // ingest: expected object (array body)
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        b"[1,2,3]",
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"expected object\"}\n", "got: {}", r.body);

    // ingest: empty body
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        b"",
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"empty body\"}\n", "got: {}", r.body);

    // ingest: invalid integer field
    let r = http(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        ingest_doc("MACHINE-0001", "null").replace("\"cpu_cores\":8", "\"cpu_cores\":\"eight\"").as_bytes(),
    );
    assert_eq!(r.status, 400);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"bad field cpu_cores\"}\n", "got: {}", r.body);

    // ingest: payload too large (rejected on content-length header alone,
    // like the Node admin — no body is sent so the close stays clean)
    let r = http_raw(
        &addr,
        "POST",
        "/ms-diag/ingest",
        &[("Content-Type", "application/json")],
        70000,
        b"",
    );
    assert_eq!(r.status, 413);
    assert_eq!(r.body, "{\"ok\":false,\"error\":\"payload too large\"}\n", "got: {}", r.body);

    // viewer requires login
    let r = get(&addr, "/ms-admin/", None);
    assert_eq!(r.status, 401);
    assert!(r.body.contains("Please sign in."), "got: {}", r.body);

    // bad login
    let r = post(
        &addr,
        "/ms-admin/login",
        None,
        "username=admin&password=wrong-password&totp=000000",
    );
    assert_eq!(r.status, 401);
    assert!(r.body.contains("Invalid credentials."), "got: {}", r.body);

    // good login
    let (token, _) = login(&addr);

    // viewer shows decrypted diagnostics
    let r = get(&addr, "/ms-admin/", Some(&token));
    assert_eq!(r.status, 200);
    assert!(r.body.contains("MACHINE-0001"), "viewer body: {}", r.body);
    assert!(r.body.contains("signed in from"), "viewer body: {}", r.body);

    // unknown admin path with valid session -> 404
    let r = get(&addr, "/ms-admin/nope", Some(&token));
    assert_eq!(r.status, 404);
    assert_eq!(r.body, "not found\n");

    // logout
    let r = post(&addr, "/ms-admin/logout", Some(&token), "");
    assert_eq!(r.status, 302);
    let r = get(&addr, "/ms-admin/", Some(&token));
    assert_eq!(r.status, 401);

    // re-login, then revoke-all invalidates session
    let (token2, _) = login(&addr);
    let r = post(&addr, "/ms-admin/revoke-all", Some(&token2), "");
    assert_eq!(r.status, 302);
    let r = get(&addr, "/ms-admin/", Some(&token2));
    assert_eq!(r.status, 401);

    // lockout: five failures, then the sixth is 423 with a retry message
    for _ in 0..5 {
        let r = post(
            &addr,
            "/ms-admin/login",
            None,
            "username=admin&password=wrong-password&totp=000000",
        );
        assert_eq!(r.status, 401);
    }
    let r = post(
        &addr,
        "/ms-admin/login",
        None,
        "username=admin&password=wrong-password&totp=000000",
    );
    assert_eq!(r.status, 423, "got: {}", r.body);
    assert!(r.body.contains("Too many failed attempts"), "got: {}", r.body);

    // unsupported methods -> 501 "Unsupported method\n" (Node behavior,
    // including HEAD)
    let r = http(&addr, "PUT", "/ms-admin/healthz", &[], b"");
    assert_eq!(r.status, 501);
    assert_eq!(r.body, "Unsupported method\n");
    let r = http(&addr, "HEAD", "/ms-admin/healthz", &[], b"");
    assert_eq!(r.status, 501);
    assert_eq!(r.body, "Unsupported method\n");

    // healthz still reachable after lockout
    let r = get(&addr, "/ms-admin/healthz", None);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "ok\n");

    drop(guard);
    std::fs::remove_dir_all(&tmp).ok();
}

fn failed_login(addr: &str, extra_headers: &[(&str, &str)]) -> HttpResp {
    http(
        addr,
        "POST",
        "/ms-admin/login",
        extra_headers,
        "username=admin&password=wrong-password&totp=000000".as_bytes(),
    )
}

#[test]
fn untrusted_peer_cannot_spoof_lockout_ip() {
    // No --trusted-proxy configured: the socket peer is authoritative, so an
    // attacker cannot lock out a victim (or evade their own lockout) by
    // sending forwarding headers.
    let tmp = std::env::temp_dir().join(format!("msadmin-spoof-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut guard = ChildGuard(start_server(&tmp, &[]));
    let (addr, _) = wait_for_port(&mut guard.0);

    // Five failed logins, each claiming a different victim IP.
    for _ in 0..5 {
        let r = failed_login(&addr, &[("X-Forwarded-For", "198.51.100.7")]);
        assert_eq!(r.status, 401);
    }
    // The lockout was recorded under the real peer (127.0.0.1), so the next
    // attempt from that peer -- without any forwarded header -- is locked.
    let r = failed_login(&addr, &[]);
    assert_eq!(r.status, 423, "got: {}", r.body);

    drop(guard);
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn trusted_proxy_honors_forwarded_lockout_ip() {
    // When the peer is a configured trusted proxy, the forwarded IP is
    // honored (real-world deployment sits behind the proxy).
    let tmp = std::env::temp_dir().join(format!("msadmin-trust-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut guard = ChildGuard(start_server(&tmp, &["--trusted-proxy", "127.0.0.1"]));
    let (addr, _) = wait_for_port(&mut guard.0);

    for _ in 0..5 {
        let r = failed_login(&addr, &[("X-Forwarded-For", "198.51.100.7")]);
        assert_eq!(r.status, 401);
    }
    // Failures were attributed to the forwarded IP, so the real peer is NOT locked...
    let r = failed_login(&addr, &[]);
    assert_eq!(r.status, 401, "got: {}", r.body);
    // ...but that forwarded IP is now locked.
    let r = failed_login(&addr, &[("X-Forwarded-For", "198.51.100.7")]);
    assert_eq!(r.status, 423, "got: {}", r.body);

    drop(guard);
    std::fs::remove_dir_all(&tmp).ok();
}