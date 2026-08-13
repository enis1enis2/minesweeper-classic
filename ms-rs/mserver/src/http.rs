//! HTTP(S) endpoint for the simulation/telemetry protocol.
//!
//! This mirrors the `/ms-diag/ingest` pattern: clients POST/GET plain HTTP(S)
//! lines and nginx/Let's Encrypt (or the native `--https-port` rustls
//! listener) terminate TLS in front. The request bodies and response bodies
//! reuse the exact TCP line protocol, so clients parse identical framing.
//!
//! Endpoints (all under `/ms-sim/`):
//!   GET  /ms-sim/healthz            -> {"ok":true}
//!   POST /ms-sim/metrics            body: metric lines -> {"ok":true}
//!   POST /ms-sim/auth               body: `auth <user>` -> authchal <nonce>
//!   POST /ms-sim/req                headers X-MS-User/X-MS-Auth, body:
//!                                   reqseed|reqbatch|requntil -> protocol lines
//!   GET  /ms-sim/seeds?since=N      -> seed/outcome lines, X-Ms-Cursor header
//!   POST /ms-sim/lbscore            body: lbscore ... -> lbstored/lbnotop/lbdenied
//!   GET  /ms-sim/lbtop?count=N&diff=D -> lbtop/lbentry/lbdone lines

use crate::protocol::{Server, handle_lbscore, handle_lbtop, handle_request};
use futures::FutureExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 128 * 1024;

/// Collects protocol reply lines into a response body (instead of writing to
/// a hub client). Always "succeeds": HTTP responses are buffered.
pub struct Collector {
    lines: Mutex<Vec<String>>,
}

impl Collector {
    pub fn new() -> Collector {
        Collector {
            lines: Mutex::new(Vec::new()),
        }
    }

    pub fn into_body(self) -> String {
        let lines = self.lines.into_inner().unwrap_or_default();
        if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        }
    }
}

impl crate::protocol::LineSink for Collector {
    async fn send(&self, line: &str) -> bool {
        self.lines.lock().unwrap_or_else(|p| p.into_inner()).push(line.to_string());
        true
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }
}

#[derive(Debug)]
enum HttpError {
    BadRequest,
    TooLarge,
    NotFound,
    Internal,
}

fn status_line(err: &HttpError) -> &'static str {
    match err {
        HttpError::BadRequest => "400 Bad Request",
        HttpError::TooLarge => "413 Payload Too Large",
        HttpError::NotFound => "404 Not Found",
        HttpError::Internal => "500 Internal Server Error",
    }
}

/// Source IP only (no port), so the HTTP auth challenge spans the separate
/// connections a client uses for `auth` and `req`.
fn peer_ip(addr: &str) -> String {
    match addr.rfind(':') {
        Some(i) => addr[..i].to_string(),
        None => addr.to_string(),
    }
}

pub async fn handle_http_conn<S>(server: Arc<Server>, mut stream: S, addr: String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    if let Err(e) = serve(&server, &mut stream, &addr).await {
        // A protocol/parse failure is not an error worth surfacing loudly;
        // still recover the connection from a panic the way TCP handlers do.
        let _ = e;
    }
}

async fn serve<S>(server: &Arc<Server>, stream: &mut S, addr: &str) -> Result<(), HttpError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = read_request(stream).await?;
    let result = std::panic::AssertUnwindSafe(async { route(server, &req, addr).await })
        .catch_unwind()
        .await;
    match result {
        Ok(resp) => {
            write_response(stream, &resp).await;
            Ok(())
        }
        Err(panic) => {
            eprintln!("  http conn {}: panic in handler: {:?}", addr, panic);
            write_response(stream, &error_response(HttpError::Internal)).await;
            Ok(())
        }
    }
}

struct Response {
    status: &'static str,
    extra_headers: Vec<(String, String)>,
    body: String,
}

fn ok_response(body: String) -> Response {
    Response {
        status: "200 OK",
        extra_headers: Vec::new(),
        body,
    }
}

fn error_response(err: HttpError) -> Response {
    let body = match err {
        HttpError::NotFound => "not found\n".to_string(),
        _ => "error\n".to_string(),
    };
    Response {
        status: status_line(&err),
        extra_headers: Vec::new(),
        body,
    }
}

async fn write_response<S: AsyncWrite + Unpin>(stream: &mut S, resp: &Response) {
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.body.len()
    );
    for (k, v) in &resp.extra_headers {
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(resp.body.as_bytes()).await;
}

async fn route(server: &Arc<Server>, req: &Request, addr: &str) -> Response {
    let method = req.method.as_str();
    let path = req.path.as_str();
    let ip = peer_ip(addr);
    match (method, path) {
        ("GET", "/ms-sim/healthz") => ok_response("{\"ok\":true}\n".to_string()),
        ("POST", "/ms-sim/metrics") => handle_metrics(server, req, addr),
        ("POST", "/ms-sim/auth") => handle_auth(server, req, &ip).await,
        ("POST", "/ms-sim/req") => handle_req(server, req, &ip).await,
        ("GET", "/ms-sim/seeds") => handle_seeds(server, req),
        ("POST", "/ms-sim/lbscore") => {
            let collector = Collector::new();
            let line = String::from_utf8_lossy(&req.body).into_owned();
            handle_lbscore(server, &collector, addr, &line).await;
            ok_response(collector.into_body())
        }
        ("GET", "/ms-sim/lbtop") => {
            let collector = Collector::new();
            let line = format!("lbtop {}", lbtop_args(req));
            handle_lbtop(server, &collector, addr, &line).await;
            ok_response(collector.into_body())
        }
        _ => error_response(HttpError::NotFound),
    }
}

fn handle_metrics(server: &Arc<Server>, req: &Request, addr: &str) -> Response {
    let body = String::from_utf8_lossy(&req.body).into_owned();
    for line in body.lines() {
        let text = line.trim_end_matches('\r').trim();
        if text.starts_with("metric ") {
            if let Err(e) = server.db.record_metric(crate::db::now_sec(), addr, text) {
                eprintln!("  http conn {}: record_metric failed: {}", addr, e);
            }
        }
    }
    ok_response("{\"ok\":true}\n".to_string())
}

async fn handle_auth(server: &Arc<Server>, req: &Request, ip: &str) -> Response {
    let body = String::from_utf8_lossy(&req.body).into_owned();
    let toks: Vec<&str> = body.split_whitespace().collect();
    if toks.len() < 2 || !server.solver_enabled {
        return ok_response("autherr\n".to_string());
    }
    let user = toks[1];
    if !crate::crypto::timing_safe_eq(user, &server.solver_user) {
        eprintln!("  http auth: unknown user {:?} from {}", user, ip);
        return ok_response("autherr\n".to_string());
    }
    match server.auth.auth_begin(ip, user).await {
        Some(nonce) => ok_response(format!("authchal {}\n", nonce)),
        None => ok_response("autherr\n".to_string()),
    }
}

async fn handle_req(server: &Arc<Server>, req: &Request, ip: &str) -> Response {
    let digest = match req.header("X-Ms-Auth") {
        Some(d) => d.to_string(),
        None => return ok_response("autherr\n".to_string()),
    };
    let (nonce, _user) = match server.auth.get(ip).await {
        Some(v) => v,
        None => return ok_response("autherr\n".to_string()),
    };
    let expected = crate::crypto::hmac_sha256_hex(
        server.solver_pass.as_bytes(),
        format!("ms-auth:{}", nonce).as_bytes(),
    );
    let (ok, fails) = server.auth.auth_resolve(ip, &digest, &expected).await;
    if !ok {
        eprintln!("  http auth: FAILED from {} (fails={})", ip, fails);
        if fails >= crate::config::MAX_AUTH_FAILS {
            return error_response(HttpError::BadRequest); // lockout
        }
        return ok_response("autherr\n".to_string());
    }
    let collector = Collector::new();
    let line = String::from_utf8_lossy(&req.body).into_owned();
    handle_request(server, &collector, ip, &line).await;
    ok_response(collector.into_body())
}

fn handle_seeds(server: &Arc<Server>, req: &Request) -> Response {
    let since: u64 = req
        .query
        .split('&')
        .find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            let (k, v) = (it.next()?, it.next()?);
            if k == "since" {
                v.parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let (lines, newest) = server.feed.since(since);
    let body = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    let mut resp = ok_response(body);
    resp.extra_headers
        .push(("X-Ms-Cursor".to_string(), newest.to_string()));
    resp
}

fn lbtop_args(req: &Request) -> String {
    let mut args = String::new();
    let mut parts: Vec<&str> = Vec::new();
    for kv in req.query.split('&') {
        let mut it = kv.splitn(2, '=');
        let (k, v) = match (it.next(), it.next()) {
            (Some(k), Some(v)) => (k, v),
            _ => continue,
        };
        if k == "count" && v.bytes().all(|b| b.is_ascii_digit()) {
            parts.push(v);
        } else if k == "diff" {
            args = v.to_string();
        }
    }
    if !args.is_empty() {
        if parts.is_empty() {
            parts.push("10");
        }
        format!("{} {}", args, parts.join(" "))
    } else if parts.is_empty() {
        "10".to_string()
    } else {
        parts.join(" ")
    }
}

/// Read one HTTP/1.x request (headers up to 16 KB, body up to 128 KB).
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Request, HttpError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_seq(&buf, b"\r\n\r\n") {
            let head = &buf[..pos];
            let body_len = content_length(head)?;
            let body_start = pos + 4;
            while buf.len() < body_start + body_len {
                let n = stream.read(&mut chunk).await.map_err(|_| HttpError::BadRequest)?;
                if n == 0 {
                    return Err(HttpError::BadRequest);
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > MAX_BODY {
                    return Err(HttpError::TooLarge);
                }
            }
            let body = buf[body_start..body_start + body_len].to_vec();
            return parse_head(&buf[..pos], body);
        }
        if buf.len() > MAX_HEADER {
            return Err(HttpError::TooLarge);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| HttpError::BadRequest)?;
        if n == 0 {
            return Err(HttpError::BadRequest);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_seq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn content_length(head: &[u8]) -> Result<usize, HttpError> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines() {
        if line.len() >= 16 && line[..15].eq_ignore_ascii_case("content-length:") {
            let v = line[15..].trim();
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpError::BadRequest);
            }
            let n: usize = v.parse().map_err(|_| HttpError::BadRequest)?;
            if n > MAX_BODY {
                return Err(HttpError::TooLarge);
            }
            return Ok(n);
        }
    }
    Ok(0)
}

fn parse_head(head: &[u8], body: Vec<u8>) -> Result<Request, HttpError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
    let request_line = lines.next().ok_or(HttpError::BadRequest)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(HttpError::BadRequest)?.to_string();
    let target = parts.next().ok_or(HttpError::BadRequest)?;
    let version = parts.next().ok_or(HttpError::BadRequest)?;
    if !version.starts_with("HTTP/1") {
        return Err(HttpError::BadRequest);
    }
    let (path, query) = match target.find('?') {
        Some(i) => (&target[..i], &target[i + 1..]),
        None => (target, ""),
    };
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some(colon) = line.find(':') else {
            return Err(HttpError::BadRequest);
        };
        let name = line[..colon].trim().to_lowercase();
        let value = line[colon + 1..].trim().to_string();
        headers.insert(name, value);
    }
    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head() {
        let head = b"POST /ms-sim/metrics HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nX-Ms-User: alice\r\n\r\n";
        let req = parse_head(head, b"hello".to_vec()).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/ms-sim/metrics");
        assert_eq!(req.query, "");
        assert_eq!(req.header("content-type"), Some("text/plain"));
        assert_eq!(req.header("x-ms-user"), Some("alice"));
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn parses_query_target() {
        let head = b"GET /ms-sim/seeds?since=42&diff=expert HTTP/1.1\r\n\r\n";
        let req = parse_head(head, Vec::new()).unwrap();
        assert_eq!(req.path, "/ms-sim/seeds");
        assert_eq!(req.query, "since=42&diff=expert");
    }

    #[test]
    fn rejects_non_http_versions() {
        let head = b"GET / HTTP/2.0\r\n\r\n";
        assert!(parse_head(head, Vec::new()).is_err());
    }

    #[test]
    fn rejects_malformed_header_lines() {
        let head = b"GET / HTTP/1.1\r\nNoColonHere\r\n\r\n";
        assert!(parse_head(head, Vec::new()).is_err());
    }

    #[test]
    fn lbtop_args_formats_requests() {
        let req = Request {
            method: "GET".into(),
            path: "/ms-sim/lbtop".into(),
            query: "count=5".into(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        assert_eq!(lbtop_args(&req), "5");
        let req = Request {
            query: "diff=expert&count=3".into(),
            ..req
        };
        assert_eq!(lbtop_args(&req), "expert 3");
        let req = Request {
            query: "diff=intermediate".into(),
            ..req
        };
        assert_eq!(lbtop_args(&req), "intermediate 10");
        let req = Request {
            query: "".into(),
            ..req
        };
        assert_eq!(lbtop_args(&req), "10");
        let req = Request {
            query: "count=abc&bogus".into(),
            ..req
        };
        assert_eq!(lbtop_args(&req), "10");
    }

    #[test]
    fn content_length_parses_and_caps() {
        let head = b"POST / HTTP/1.1\r\nContent-Length: 10\r\n";
        assert_eq!(content_length(head).unwrap(), 10);
        let big = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n", MAX_BODY + 1);
        assert!(matches!(content_length(big.as_bytes()), Err(HttpError::TooLarge)));
        let bad = b"POST / HTTP/1.1\r\nContent-Length: nope\r\n";
        assert!(matches!(content_length(bad), Err(HttpError::BadRequest)));
    }

    #[test]
    fn peer_ip_strips_port() {
        assert_eq!(peer_ip("127.0.0.1:55555"), "127.0.0.1");
        assert_eq!(peer_ip("10.0.0.5:80"), "10.0.0.5");
        assert_eq!(peer_ip("bad-addr"), "bad-addr");
    }
}
