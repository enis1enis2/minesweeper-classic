use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::crypt;
use crate::store::{AuthStore, DiagDB};

pub const MAX_BODY: usize = 65536;
const INGEST_LIMIT: usize = 20;
const INGEST_WINDOW_SECS: i64 = 10;
const COOKIE_NAME: &str = "ms_admin";
const VIEWER_ROWS: usize = 200;
const MAX_HEADER_BYTES: usize = 65536;

const EXPECTED_FIELDS: &[&str] = &[
    "machine_id",
    "public_ip",
    "private_ip",
    "network_interface",
    "router_ip",
    "local_gateway",
    "dns_servers",
    "region",
    "hostname",
    "os_version",
    "server_version",
    "version",
    "lang",
    "country_code",
    "timezone",
    "last_boot",
    "total_disk_gb",
    "used_disk_gb",
    "total_ram_gb",
    "used_ram_gb",
    "cpu_model",
    "cpu_cores",
    "ram_mb",
    "uptime_sec",
    "crash_text",
];

const INTEGER_FIELDS: &[&str] = &["cpu_cores", "ram_mb", "uptime_sec"];

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct State {
    pub db: DiagDB,
    pub auth: Mutex<AuthStore>,
    pub key: String,
    pub ingest: Mutex<Vec<i64>>,
}

pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub body_too_large: bool,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|s| s.as_str())
    }
}

pub struct HttpResponse {
    pub code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        423 => "Locked",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

fn simple(code: u16, body: &str, content_type: &str) -> HttpResponse {
    HttpResponse {
        code,
        headers: vec![("Content-Type".to_string(), content_type.to_string())],
        body: body.as_bytes().to_vec(),
    }
}

pub fn ok_text(body: &str) -> HttpResponse {
    simple(200, body, "text/plain; charset=utf-8")
}

pub fn not_found() -> HttpResponse {
    simple(404, "not found\n", "text/plain; charset=utf-8")
}

fn method_not_allowed() -> HttpResponse {
    simple(405, "method not allowed\n", "text/plain; charset=utf-8")
}

fn too_many_requests() -> HttpResponse {
    simple(429, "rate limit exceeded\n", "text/plain; charset=utf-8")
}

fn redirect(location: &str, extra: Vec<(String, String)>) -> HttpResponse {
    let mut headers = vec![("Location".to_string(), location.to_string())];
    headers.extend(extra);
    HttpResponse {
        code: 302,
        headers,
        body: Vec::new(),
    }
}

fn set_cookie(token: &str) -> (String, String) {
    (
        "Set-Cookie".to_string(),
        format!(
            "{COOKIE_NAME}={token}; Path=/ms-admin/; HttpOnly; Secure; SameSite=Lax"
        ),
    )
}

fn clear_cookie() -> (String, String) {
    (
        "Set-Cookie".to_string(),
        format!(
            "{COOKIE_NAME}=; Path=/ms-admin/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"
        ),
    )
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn page(title: &str, body: &str) -> HttpResponse {
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title><style>
body {{ font-family: -apple-system, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif; background: #0d1117; color: #c9d1d9; margin: 0; padding: 16px; }}
h1 {{ color: #f0f6fc; }}
a {{ color: #58a6ff; }}
table {{ border-collapse: collapse; width: 100%; margin-top: 16px; font-size: 14px; }}
th, td {{ border: 1px solid #30363d; padding: 6px 10px; text-align: left; vertical-align: top; }}
th {{ background: #161b22; color: #8b949e; }}
tr:nth-child(even) {{ background: #161b22; }}
input, button {{ font: inherit; padding: 6px 10px; margin: 4px 0; border: 1px solid #30363d; border-radius: 6px; background: #21262d; color: #c9d1d9; }}
button {{ cursor: pointer; }}
button:hover {{ background: #30363d; }}
.notice {{ color: #ff7b72; }}
.inline {{ display: inline; }}
</style></head>\n<body>\n{body}\n</body>\n</html>\n"
    );
    simple(200, &html, "text/html; charset=utf-8")
}

fn login_page(notice: &str) -> HttpResponse {
    let notice_html = if notice.is_empty() {
        String::new()
    } else {
        format!("<p class=\"notice\">{}</p>", escape_html(notice))
    };
    let body = format!(
        "<h1>Minesweeper Diagnostics</h1>\n{notice_html}\n\
<form method=\"post\" action=\"/ms-admin/login\">\n\
<label>Username <input type=\"text\" name=\"username\" required autocomplete=\"username\"></label>\n\
<label>Password <input type=\"password\" name=\"password\" required autocomplete=\"current-password\"></label>\n\
<label>2FA code <input type=\"text\" name=\"totp\" inputmode=\"numeric\" autocomplete=\"one-time-code\"></label>\n\
<button type=\"submit\">Sign in</button>\n\
</form>\n"
    );
    let mut res = page("Minesweeper Diagnostics", &body);
    res.code = 401;
    res
}

fn viewer_page(state: &State, ip: &str) -> HttpResponse {
    let (total, recent) = state.db.stats();
    let rows = state.db.recent_rows(VIEWER_ROWS);
    let mut table = String::new();
    for (ts, _id, addr, blob) in rows {
        let doc: Value = match crypt::decrypt(&state.key, &blob)
            .and_then(|p| serde_json::from_slice::<Value>(&p).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(_) => continue,
        };
        let get = |k: &str| doc.get(k).cloned().unwrap_or(Value::Null);
        let machine_id = get("machine_id")
            .as_str()
            .unwrap_or("")
            .chars()
            .take(16)
            .collect::<String>();
        let ram_mb = number_str(&get("ram_mb"));
        let uptime_sec = number_str(&get("uptime_sec"));
        let cpu_cores = number_str(&get("cpu_cores"));
        let crash = match get("crash_text") {
            Value::String(s) if !s.is_empty() => s,
            _ => String::new(),
        };
        table.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{} <em>({})</em></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            utc_ts(ts),
            escape_html(&addr),
            escape_html(&machine_id),
            escape_html(&addr),
            escape_html(&cpu_cores),
            escape_html(&ram_mb),
            escape_html(&uptime_sec),
            escape_html(&crash),
        ));
    }
    let body = format!(
        "<h1>Minesweeper Diagnostics</h1>\n\
<p>Signed in from: {ip}</p>\n\
<p>Total diagnostics: {total} &mdash; last 24h: {recent}</p>\n\
<form action=\"/ms-admin/logout\" method=\"post\" class=\"inline\"><button type=\"submit\">Log out</button></form>\n\
<form action=\"/ms-admin/revoke-all\" method=\"post\" class=\"inline\"><button type=\"submit\">Revoke all sessions</button></form>\n\
<table>\n\
<thead><tr><th>Time (UTC)</th><th>Node</th><th>Machine ID</th><th>CPU (cores)</th><th>RAM (MB)</th><th>Uptime (s)</th><th>Crash</th></tr></thead>\n\
<tbody>\n{table}\
</tbody>\n\
</table>\n"
    );
    page("Minesweeper Diagnostics", &body)
}

fn number_str(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        _ => "0".to_string(),
    }
}

fn utc_ts(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (hh, mm, ss) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn real_ip(req: &HttpRequest, peer: std::net::SocketAddr) -> String {
    if let Some(v) = req.header("cf-connecting-ip") {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    if let Some(v) = req.header("x-forwarded-for") {
        let first = v.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    peer.ip().to_string()
}

fn cookie_token(req: &HttpRequest) -> Option<String> {
    let cookies = req.header("cookie")?;
    for part in cookies.split(';') {
        let part = part.trim();
        if let Some(eq) = part.find('=') {
            let name = part[..eq].trim();
            if name == COOKIE_NAME {
                return Some(part[eq + 1..].trim().to_string());
            }
        }
    }
    None
}

fn require_session(state: &State, req: &HttpRequest) -> Option<String> {
    let token = cookie_token(req)?;
    let mut auth = state.auth.lock().unwrap();
    auth.validate(unix_now(), &token)
}

fn parse_body(req: &HttpRequest) -> Result<String, u16> {
    if req.body_too_large {
        return Err(413);
    }
    if req.body.is_empty() {
        return Ok(String::new());
    }
    match String::from_utf8(req.body.clone()) {
        Ok(s) => Ok(s),
        Err(_) => Err(400),
    }
}

fn decode_uri_component(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() && hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() => {
                let h = hex(bytes[i + 1]).unwrap();
                let l = hex(bytes[i + 2]).unwrap();
                out.push((h << 4) | l);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_urlencoded(body: &str) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.find('=') {
            Some(eq) => (&pair[..eq], &pair[eq + 1..]),
            None => (pair, ""),
        };
        let key = decode_uri_component(k);
        let val = decode_uri_component(v);
        if !key.is_empty() && !fields.contains_key(&key) {
            fields.insert(key, val);
        }
    }
    fields
}

fn handle_ingest(state: &State, req: &HttpRequest, ip: &str) -> HttpResponse {
    {
        let mut counts = state.ingest.lock().unwrap();
        let now = unix_now();
        counts.retain(|t| *t > now - INGEST_WINDOW_SECS);
        if counts.len() >= INGEST_LIMIT {
            return too_many_requests();
        }
        counts.push(now);
    }
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(code) => {
            if code == 413 {
                return simple(413, "payload too large\n", "text/plain; charset=utf-8");
            }
            return simple(400, "invalid body\n", "text/plain; charset=utf-8");
        }
    };
    if body.is_empty() {
        return simple(400, "empty body\n", "text/plain; charset=utf-8");
    }
    if req.body.len() > MAX_BODY {
        return simple(413, "payload too large\n", "text/plain; charset=utf-8");
    }
    let mut doc: Value = match serde_json::from_str(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return simple(400, "invalid JSON\n", "text/plain; charset=utf-8"),
        Err(_) => return simple(400, "invalid JSON\n", "text/plain; charset=utf-8"),
    };
    for field in EXPECTED_FIELDS {
        if !doc.get(*field).is_some() {
            return simple(400, &format!("missing field: {field}\n"), "text/plain; charset=utf-8");
        }
    }
    match doc.get_mut("crash_text") {
        Some(v) if v.is_string() || v.is_null() => {}
        _ => {
            doc["crash_text"] = Value::Null;
        }
    }
    for field in INTEGER_FIELDS {
        if !is_integer(&doc[*field]) {
            return simple(400, &format!("invalid field: {field}\n"), "text/plain; charset=utf-8");
        }
    }
    let mut sorted = Map::new();
    let mut keys: Vec<&str> = doc.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    for k in keys {
        sorted.insert(k.to_string(), doc[k].clone());
    }
    let plain = match serde_json::to_string(&Value::Object(sorted)) {
        Ok(s) => s,
        Err(_) => return simple(500, "serialize failed\n", "text/plain; charset=utf-8"),
    };
    let blob = match crypt::encrypt(&state.key, plain.as_bytes()) {
        Ok(b) => b,
        Err(_) => return simple(500, "encrypt failed\n", "text/plain; charset=utf-8"),
    };
    let ts = unix_now();
    let id = match state.db.insert(ts, ip, &blob) {
        Ok(id) => id,
        Err(_) => return simple(500, "insert failed\n", "text/plain; charset=utf-8"),
    };
    HttpResponse {
        code: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: format!(r#"{{"ok":true,"id":{id}}}"#).into_bytes(),
    }
}

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_i64() || n.is_u64() || n.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false),
        _ => false,
    }
}

fn handle_admin(state: &State, req: &HttpRequest) -> HttpResponse {
    let session_ip = match require_session(state, req) {
        Some(ip) => ip,
        None => return login_page(""),
    };
    if req.path != "/ms-admin/" {
        return not_found();
    }
    viewer_page(state, &session_ip)
}

fn handle_login(state: &State, req: &HttpRequest, ip: &str) -> HttpResponse {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(_) => return login_page("invalid request"),
    };
    let fields = parse_urlencoded(&body);
    let username = fields.get("username").cloned().unwrap_or_default();
    let password = fields.get("password").cloned().unwrap_or_default();
    let code = fields.get("totp").cloned().unwrap_or_default();
    let result = {
        let mut auth = state.auth.lock().unwrap();
        auth.check_login(unix_now(), ip, &username, &password, &code)
    };
    if !result.ok {
        return login_page(result.reason);
    }
    let (token, _expires) = {
        let mut auth = state.auth.lock().unwrap();
        auth.issue_session(unix_now(), ip)
    };
    redirect("/ms-admin/", vec![set_cookie(&token)])
}

fn handle_logout(state: &State, req: &HttpRequest) -> HttpResponse {
    if let Some(token) = cookie_token(req) {
        let mut auth = state.auth.lock().unwrap();
        auth.revoke(unix_now(), &token);
    }
    redirect("/ms-admin/", vec![clear_cookie()])
}

fn handle_revoke_all(state: &State, _req: &HttpRequest) -> HttpResponse {
    let mut auth = state.auth.lock().unwrap();
    auth.revoke_all();
    redirect("/ms-admin/", Vec::new())
}

pub fn route(state: &State, method: &str, path: &str, req: &HttpRequest, ip: &str) -> HttpResponse {
    if method == "GET" {
        if path == "/ms-admin/healthz" {
            return ok_text("ok\n");
        }
        if path == "/ms-admin/" || path.starts_with("/ms-admin/") {
            return handle_admin(state, req);
        }
        return not_found();
    }
    if method == "POST" {
        match path {
            "/ms-diag/ingest" => return handle_ingest(state, req, ip),
            "/ms-admin/login" => return handle_login(state, req, ip),
            "/ms-admin/logout" => return handle_logout(state, req),
            "/ms-admin/revoke-all" => return handle_revoke_all(state, req),
            _ => return not_found(),
        }
    }
    method_not_allowed()
}

async fn write_response(
    stream: &mut TcpStream,
    res: &HttpResponse,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", res.code, reason(res.code));
    let mut has_length = false;
    for (name, value) in &res.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
    }
    head.push_str("Connection: close\r\n");
    if !has_length {
        head.push_str(&format!("Content-Length: {}\r\n", res.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !head_only && !res.body.is_empty() {
        stream.write_all(&res.body).await?;
    }
    stream.flush().await?;
    Ok(())
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

async fn read_more(
    stream: &mut TcpStream,
    pending: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<bool> {
    let mut tmp = [0u8; 8192];
    let n = stream.read(&mut tmp).await?;
    if n == 0 {
        return Ok(false);
    }
    if pending.len() + n > cap {
        pending.extend_from_slice(&tmp[..cap - pending.len()]);
        return Ok(true);
    }
    pending.extend_from_slice(&tmp[..n]);
    Ok(true)
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<HttpRequest>> {
    let mut pending: Vec<u8> = Vec::new();
    let header_end = loop {
        if let Some(pos) = find_sub(&pending, b"\r\n\r\n") {
            break pos;
        }
        if pending.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
        if !read_more(stream, &mut pending, MAX_HEADER_BYTES).await? {
            return Ok(None);
        }
    };
    let head = String::from_utf8_lossy(&pending[..header_end]);
    let body_start = header_end + 4;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    if method.is_empty() || path.is_empty() {
        return Ok(None);
    }
    let mut headers = HashMap::new();
    for line in lines {
        if let Some(colon) = line.find(':') {
            let name = line[..colon].trim().to_ascii_lowercase();
            let value = line[colon + 1..].trim().to_string();
            headers.insert(name, value);
        }
    }
    let mut req = HttpRequest {
        method,
        path,
        headers,
        body: Vec::new(),
        body_too_large: false,
    };
    let body_bytes = &pending[body_start..];
    if let Some(te) = req.header("transfer-encoding") {
        if te.eq_ignore_ascii_case("chunked") {
            let mut chunked = body_bytes.to_vec();
            loop {
                let crlf = match find_sub(&chunked, b"\r\n") {
                    Some(p) => p,
                    None => {
                        if !read_more(stream, &mut chunked, MAX_BODY * 4).await? {
                            req.body_too_large = true;
                            break;
                        }
                        continue;
                    }
                };
                let size_str = String::from_utf8_lossy(&chunked[..crlf]);
                let size = match usize::from_str_radix(size_str.trim(), 16) {
                    Ok(n) => n,
                    Err(_) => {
                        req.body_too_large = true;
                        break;
                    }
                };
                let chunk_need = crlf + 2 + size + 2;
                while chunked.len() < chunk_need {
                    if !read_more(stream, &mut chunked, MAX_BODY * 4).await? {
                        req.body_too_large = true;
                        break;
                    }
                }
                if req.body_too_large {
                    break;
                }
                let chunk_start = crlf + 2;
                if size == 0 {
                    break;
                }
                if req.body.len() + size > MAX_BODY {
                    req.body_too_large = true;
                    break;
                }
                req.body
                    .extend_from_slice(&chunked[chunk_start..chunk_start + size]);
                chunked.drain(..chunk_need);
            }
        }
    } else if let Some(cl) = req.header("content-length") {
        if let Ok(len) = cl.parse::<usize>() {
            if len > MAX_BODY {
                req.body_too_large = true;
            } else {
                let have = body_bytes.len().min(len);
                req.body.extend_from_slice(&body_bytes[..have]);
                while req.body.len() < len {
                    let mut tmp = [0u8; 8192];
                    let n = stream.read(&mut tmp).await?;
                    if n == 0 {
                        break;
                    }
                    let need = len - req.body.len();
                    let take = n.min(need);
                    req.body.extend_from_slice(&tmp[..take]);
                }
                if req.body.len() > MAX_BODY {
                    req.body_too_large = true;
                }
            }
        }
    }
    Ok(Some(req))
}

pub async fn handle_conn(state: Arc<State>, mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();
    let peer = stream.peer_addr().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
    let request = match timeout(Duration::from_secs(30), read_request(&mut stream)).await {
        Ok(Ok(Some(req))) => req,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(_)) => {
            let _ = write_response(&mut stream, &simple(400, "bad request\n", "text/plain; charset=utf-8"), false).await;
            return Ok(());
        }
        Err(_) => return Ok(()),
    };
    let ip = real_ip(&request, peer);
    println!("{ip} {} {}", request.method, request.path);
    let head_only = request.method == "HEAD";
    let response = route(&state, &request.method, &request.path, &request, &ip);
    write_response(&mut stream, &response, head_only).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_uri_component_handles_plus_and_percent() {
        assert_eq!(decode_uri_component("a+b%20c"), "a b c");
        assert_eq!(decode_uri_component("abc"), "abc");
    }

    #[test]
    fn parse_urlencoded_extracts_fields() {
        let fields = parse_urlencoded("username=admin&password=secret+word&totp=123456");
        assert_eq!(fields.get("username").map(|s| s.as_str()), Some("admin"));
        assert_eq!(fields.get("password").map(|s| s.as_str()), Some("secret word"));
        assert_eq!(fields.get("totp").map(|s| s.as_str()), Some("123456"));
    }

    #[test]
    fn utc_ts_formats_known() {
        assert_eq!(utc_ts(0), "1970-01-01 00:00:00");
        assert_eq!(utc_ts(1_000_000_000), "2001-09-09 01:46:40");
    }

    #[test]
    fn is_integer_checks() {
        assert!(is_integer(&Value::from(4)));
        assert!(is_integer(&Value::from(4.0)));
        assert!(!is_integer(&Value::from(4.5)));
        assert!(!is_integer(&Value::from("4")));
        assert!(!is_integer(&Value::Null));
    }
}
