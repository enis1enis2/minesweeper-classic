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

// Mirror of Node server.js EXPECTED_FIELDS.
const EXPECTED_FIELDS: &[&str] = &[
    "machine_id",
    "os",
    "cpu",
    "cpu_cores",
    "gpu",
    "ram_mb",
    "display",
    "game_version",
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
    /// Proxies (IP or CIDR) whose forwarding headers are trusted. Requests
    /// from any other peer are attributed to the socket peer address.
    pub trusted_proxies: Vec<String>,
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

// Node server.js sendJson(): {"ok":false,"error":"..."}\n with
// Content-Type: application/json.
fn send_json_error(code: u16, error: &str) -> HttpResponse {
    HttpResponse {
        code,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: format!("{{\"ok\":false,\"error\":\"{error}\"}}\n").into_bytes(),
    }
}

// Node server.js sendJson(res, 200, {ok:true}) — no id field.
fn ok_json() -> HttpResponse {
    HttpResponse {
        code: 200,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: b"{\"ok\":true}\n".to_vec(),
    }
}

// Node send() for redirects sets Content-Type: text/plain; charset=utf-8
// before the extra headers (Location, Set-Cookie).
fn redirect(location: &str, extra: Vec<(String, String)>) -> HttpResponse {
    let mut headers = vec![(
        "Content-Type".to_string(),
        "text/plain; charset=utf-8".to_string(),
    )];
    headers.push(("Location".to_string(), location.to_string()));
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
        format!("{COOKIE_NAME}={token}; Path=/ms-admin/; HttpOnly; Secure; SameSite=Lax"),
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

// Node server.js escapeHtml(): ' becomes &#x27;.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

// Node server.js page(title, body) — byte-for-byte HTML shell.
fn page(title: &str, body: &str) -> HttpResponse {
    let html = format!(
        "<!doctype html><html><head><meta charset='utf-8'><title>{}</title><style>\
body{{font-family:system-ui,sans-serif;margin:2rem;color:#1c1c1c;background:#f7f7f7}}\
h1{{font-size:1.25rem}}.card{{background:#fff;border:1px solid #ddd;border-radius:6px;padding:1rem;margin:1rem 0}}\
table{{border-collapse:collapse;width:100%;font-size:0.85rem}}\
th,td{{border:1px solid #e3e3e3;padding:4px 8px;text-align:left}}\
th{{background:#efefef}}pre{{white-space:pre-wrap;word-break:break-all;max-width:60ch;margin:0;font-size:0.8rem}}\
label{{display:block;margin:.5rem 0 .15rem}}input{{width:20rem}}\
button,form{{display:inline;margin-right:.5rem}}\
.mono{{font-family:ui-monospace,monospace}}\
</style></head><body>{}</body></html>",
        escape_html(title),
        body
    );
    simple(200, &html, "text/html; charset=utf-8")
}

// Node server.js loginPage(notice); code is 401 (bad creds) or 423 (locked).
fn login_page(notice: &str, code: u16) -> HttpResponse {
    let body = format!(
        "<h1>Minesweeper diagnostics admin</h1><p>{}</p>\
<form method='POST' action='/ms-admin/login'>\
<label>Username</label><input type='text' name='username' autocomplete='username'>\
<label>Password</label><input type='password' name='password' autocomplete='current-password'>\
<label>TOTP code</label><input type='text' name='totp' autocomplete='one-time-code' inputmode='numeric' pattern='[0-9]{{6}}' maxlength='6'>\
<br><br><button type='submit'>Sign in</button></form>",
        escape_html(notice)
    );
    let mut res = page("Sign in", &body);
    res.code = code;
    res
}

// JS Number(x) string coercion, matching the viewer's Number(d.ram_mb ?? 0)
// and Number(d.uptime_sec ?? 0) rendering.
fn js_number(v: &Value) -> String {
    let f = match v {
        Value::Null => 0.0,
        Value::Bool(b) => return if *b { "1".to_string() } else { "0".to_string() },
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => s.parse::<f64>().unwrap_or(f64::NAN),
        _ => f64::NAN,
    };
    if f.is_nan() {
        return "NaN".to_string();
    }
    format!("{}", f)
}

// JS String(v) for the viewer's String(d.os ?? "")-style coercions.
fn str_coerce(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(_) => js_number(v),
        Value::Bool(b) => b.to_string(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

// JS truthiness for the viewer's `d.crash_text || ""`.
fn js_crash(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f == 0.0 {
                String::new()
            } else {
                js_number(v)
            }
        }
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

// Node server.js viewerPage(state, ip).
fn viewer_page(state: &State, ip: &str) -> HttpResponse {
    let (total, recent) = state.db.stats();
    let rows = state.db.recent_rows(VIEWER_ROWS);
    let mut cards = format!(
        "<div class='card'><b>{total}</b> total rows &middot; <b>{recent}</b> in the last 24h &middot; signed in from <span class='mono'>{}</span></div>",
        escape_html(ip)
    );
    if rows.is_empty() {
        cards.push_str("<div class='card'>No diagnostics yet.</div>");
    } else {
        cards.push_str(
            "<table><tr><th>id</th><th>when (UTC)</th><th>addr</th><th>machine</th>\
<th>os</th><th>cpu</th><th>gpu</th><th>ram</th><th>display</th>\
<th>game</th><th>uptime</th><th>crash</th></tr>",
        );
        for (rid, ts, addr, blob) in rows {
            let doc: Option<Value> = crypt::decrypt(&state.key, &blob)
                .ok()
                .and_then(|p| serde_json::from_slice::<Value>(&p).ok());
            match doc {
                None => {
                    cards.push_str(&format!(
                        "<tr><td>{rid}</td><td>{}</td><td>{}</td><td colspan='9'>unable to decrypt (key mismatch?)</td></tr>",
                        utc_ts(ts),
                        escape_html(&addr)
                    ));
                }
                Some(d) => {
                    let machine: String =
                        escape_html(&str_coerce(d.get("machine_id").unwrap_or(&Value::Null)))
                            .chars()
                            .take(16)
                            .collect();
                    let os = escape_html(&str_coerce(d.get("os").unwrap_or(&Value::Null)));
                    let cpu = escape_html(&str_coerce(d.get("cpu").unwrap_or(&Value::Null)));
                    let gpu = escape_html(&str_coerce(d.get("gpu").unwrap_or(&Value::Null)));
                    let ram = js_number(&d.get("ram_mb").unwrap_or(&Value::Null));
                    let display =
                        escape_html(&str_coerce(d.get("display").unwrap_or(&Value::Null)));
                    let game =
                        escape_html(&str_coerce(d.get("game_version").unwrap_or(&Value::Null)));
                    let uptime = js_number(&d.get("uptime_sec").unwrap_or(&Value::Null));
                    let crash = escape_html(&js_crash(&d.get("crash_text").unwrap_or(&Value::Null)));
                    cards.push_str(&format!(
                        "<tr><td>{rid}</td><td>{}</td><td>{}</td><td class='mono'>{machine}</td><td>{os}</td><td>{cpu}</td><td>{gpu}</td><td>{ram}</td><td>{display}</td><td>{game}</td><td>{uptime}s</td><td><pre>{crash}</pre></td></tr>",
                        utc_ts(ts),
                        escape_html(&addr)
                    ));
                }
            }
        }
        cards.push_str("</table>");
    }
    let actions = "<form method='POST' action='/ms-admin/logout'>\
<button type='submit'>Log out</button></form>\
<form method='POST' action='/ms-admin/revoke-all'>\
<button type='submit' onclick=\"return confirm('Revoke all sessions?');\">\
Revoke all sessions</button></form>\
<a href='/ms-admin/'>Refresh</a>";
    let body = format!("<h1>Minesweeper diagnostics</h1>{actions}{cards}");
    page("Diagnostics", &body)
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

/// Validate a `--trusted-proxy` value: a bare IP or a CIDR prefix.
pub fn validate_trusted_proxy(entry: &str) -> Result<(), String> {
    let err = || format!("bad --trusted-proxy '{entry}': expected an IP address or CIDR (e.g. 203.0.113.4 or 10.0.0.0/8)");
    match entry.split_once('/') {
        Some((base, prefix)) => {
            let bits = match base.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V4(_)) => 32u8,
                Ok(std::net::IpAddr::V6(_)) => 128u8,
                Err(_) => return Err(err()),
            };
            let p: u8 = prefix
                .parse()
                .map_err(|_| err())?;
            if p > bits {
                return Err(err());
            }
            Ok(())
        }
        None => match entry.parse::<std::net::IpAddr>() {
            Ok(_) => Ok(()),
            Err(_) => Err(err()),
        },
    }
}

/// True when `ip` matches any entry in the trusted-proxy list.
fn peer_is_trusted(ip: &std::net::IpAddr, trusted: &[String]) -> bool {
    trusted.iter().any(|e| ip_in_entry(ip, e))
}

fn ip_in_entry(ip: &std::net::IpAddr, entry: &str) -> bool {
    match entry.split_once('/') {
        Some((base, prefix)) => {
            let prefix: u8 = match prefix.parse() {
                Ok(p) => p,
                Err(_) => return false,
            };
            match (ip, base.parse::<std::net::IpAddr>()) {
                (std::net::IpAddr::V4(a), Ok(std::net::IpAddr::V4(b))) => {
                    if prefix > 32 {
                        return false;
                    }
                    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
                    (u32::from(*a) & mask) == (u32::from(b) & mask)
                }
                (std::net::IpAddr::V6(a), Ok(std::net::IpAddr::V6(b))) => {
                    if prefix > 128 {
                        return false;
                    }
                    let mask = if prefix == 0 {
                        0
                    } else {
                        u128::MAX << (128 - prefix)
                    };
                    (u128::from(*a) & mask) == (u128::from(b) & mask)
                }
                _ => false,
            }
        }
        None => match (ip, entry.parse::<std::net::IpAddr>()) {
            (a, Ok(b)) => a == &b,
            _ => false,
        },
    }
}

/// Resolve the client IP for a request. Forwarding headers
/// (`cf-connecting-ip`, `x-forwarded-for`) are ONLY honored when the socket
/// peer is a configured trusted proxy; for every other peer the socket
/// address is authoritative, so a remote attacker cannot spoof the value
/// used for ingest attribution and login-lockout accounting.
fn real_ip(req: &HttpRequest, peer: std::net::SocketAddr, trusted: &[String]) -> String {
    if !peer_is_trusted(&peer.ip(), trusted) {
        return peer.ip().to_string();
    }
    if let Some(v) = req.header("cf-connecting-ip") {
        let first = v.split(',').next().unwrap_or("").trim();
        if !first.is_empty() && first.parse::<std::net::IpAddr>().is_ok() {
            return first.to_string();
        }
    }
    if let Some(v) = req.header("x-forwarded-for") {
        let first = v.split(',').next().unwrap_or("").trim();
        if !first.is_empty() && first.parse::<std::net::IpAddr>().is_ok() {
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

// Node readBody() + JSON.parse: body bytes are decoded lossily, so invalid
// UTF-8 simply becomes garbage JSON ("bad json"), never a separate error.
fn parse_body(req: &HttpRequest) -> Result<String, u16> {
    if req.body_too_large {
        return Err(413);
    }
    Ok(String::from_utf8_lossy(&req.body).into_owned())
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

fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64() || n.is_u64() || n.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false)
        }
        _ => false,
    }
}

// Mirror JS JSON.stringify for numbers: an f64 with a zero fraction
// serializes as an integer ("4.0" -> "4").
fn normalize_number(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0
                    && f.is_finite()
                    && f >= i64::MIN as f64
                    && f <= i64::MAX as f64
                {
                    return Value::from(f as i64);
                }
            }
            v.clone()
        }
        _ => v.clone(),
    }
}

fn handle_ingest(state: &State, req: &HttpRequest, ip: &str) -> HttpResponse {
    let now = unix_now();
    {
        let mut counts = state.ingest.lock().unwrap();
        counts.retain(|t| *t > now - INGEST_WINDOW_SECS);
        if counts.len() >= INGEST_LIMIT {
            return send_json_error(429, "rate limited");
        }
        counts.push(now);
    }
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(_) => return send_json_error(413, "payload too large"),
    };
    if body.is_empty() {
        return send_json_error(400, "empty body");
    }
    let mut doc: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return send_json_error(400, "bad json"),
    };
    if !doc.is_object() {
        return send_json_error(400, "expected object");
    }
    let obj = doc.as_object_mut().unwrap();
    for field in EXPECTED_FIELDS {
        if !obj.contains_key(*field) {
            return send_json_error(400, &format!("missing field {field}"));
        }
    }
    match obj.get_mut("crash_text") {
        Some(v) if v.is_string() || v.is_null() => {}
        _ => {
            obj.insert("crash_text".to_string(), Value::Null);
        }
    }
    for field in INTEGER_FIELDS {
        if !is_integer(&doc[*field]) {
            return send_json_error(400, &format!("bad field {field}"));
        }
    }
    // serde_json's Map sorts keys (BTreeMap), matching Object.keys(doc).sort().
    let mut sorted = Map::new();
    for (k, v) in doc.as_object().unwrap() {
        sorted.insert(k.clone(), normalize_number(v));
    }
    let plain = match serde_json::to_string(&Value::Object(sorted)) {
        Ok(s) => s,
        Err(_) => return send_json_error(500, "server error"),
    };
    let blob = match crypt::encrypt(&state.key, plain.as_bytes()) {
        Ok(b) => b,
        Err(_) => return send_json_error(500, "server error"),
    };
    if state.db.insert(now, ip, &blob).is_err() {
        return send_json_error(500, "server error");
    }
    ok_json()
}

fn handle_admin(state: &State, req: &HttpRequest) -> HttpResponse {
    let session_ip = match require_session(state, req) {
        Some(ip) => ip,
        None => return login_page("Please sign in.", 401),
    };
    if req.path != "/ms-admin/" {
        return not_found();
    }
    viewer_page(state, &session_ip)
}

fn handle_login(state: &State, req: &HttpRequest, ip: &str) -> HttpResponse {
    let now = unix_now();
    let lock_until = {
        let mut auth = state.auth.lock().unwrap();
        auth.locked_until(now, ip)
    };
    if lock_until > 0 {
        let retry = (lock_until - now).max(1);
        return login_page(&format!("Too many failed attempts. Retry in ~{retry}s."), 423);
    }
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(_) => String::new(),
    };
    let fields = parse_urlencoded(&body);
    let username = fields.get("username").cloned().unwrap_or_default();
    let password = fields.get("password").cloned().unwrap_or_default();
    let code = fields.get("totp").cloned().unwrap_or_default();
    let result = {
        let mut auth = state.auth.lock().unwrap();
        auth.check_login(now, ip, &username, &password, &code)
    };
    if !result.ok {
        let mut auth = state.auth.lock().unwrap();
        auth.record_failure(now, ip);
        return login_page("Invalid credentials.", 401);
    }
    let (token, _expires) = {
        let mut auth = state.auth.lock().unwrap();
        auth.issue_session(now, ip)
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

fn handle_revoke_all(state: &State, req: &HttpRequest) -> HttpResponse {
    let session_ip = match require_session(state, req) {
        Some(ip) => ip,
        None => return login_page("Please sign in.", 401),
    };
    {
        let mut auth = state.auth.lock().unwrap();
        auth.revoke_all();
    }
    println!("revoke-all issued by ip={session_ip}");
    redirect("/ms-admin/", vec![clear_cookie()])
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
    simple(501, "Unsupported method\n", "text/plain; charset=utf-8")
}

async fn write_response(stream: &mut TcpStream, res: &HttpResponse) -> std::io::Result<()> {
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
    if !res.body.is_empty() {
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
    let raw_path = parts.next().unwrap_or("").to_string();
    // Node routes on req.url.split("?", 1)[0].
    let path = raw_path.split('?').next().unwrap_or("").to_string();
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
            let _ = write_response(
                &mut stream,
                &simple(400, "bad request\n", "text/plain; charset=utf-8"),
            )
            .await;
            return Ok(());
        }
        Err(_) => return Ok(()),
    };
    let ip = real_ip(&request, peer, &state.trusted_proxies);
    println!("{ip} {} {}", request.method, request.path);
    let response = route(&state, &request.method, &request.path, &request, &ip);
    write_response(&mut stream, &response).await
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

    #[test]
    fn js_number_matches_js_rendering() {
        assert_eq!(js_number(&Value::from(4096)), "4096");
        assert_eq!(js_number(&Value::from(4096.0)), "4096");
        assert_eq!(js_number(&Value::Null), "0");
        assert_eq!(js_number(&Value::from("4096")), "4096");
        assert_eq!(js_number(&Value::from("abc")), "NaN");
        assert_eq!(js_number(&Value::Bool(true)), "1");
    }

    #[test]
    fn normalize_number_handles_integral_floats() {
        assert_eq!(normalize_number(&Value::from(4.0)), Value::from(4));
        assert_eq!(normalize_number(&Value::from(4.5)), Value::from(4.5));
        assert_eq!(normalize_number(&Value::from(4)), Value::from(4));
    }

    fn req_with_headers(headers: &[(&str, &str)]) -> HttpRequest {
        let mut map = HashMap::new();
        for (k, v) in headers {
            map.insert(k.to_string(), v.to_string());
        }
        HttpRequest {
            method: "POST".to_string(),
            path: "/ms-diag/ingest".to_string(),
            headers: map,
            body: Vec::new(),
            body_too_large: false,
        }
    }

    const PEER: std::net::SocketAddr =
        std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 9)), 4444);

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        // Attackers who are NOT a configured proxy cannot influence the
        // recorded/locked IP by sending headers.
        let req = req_with_headers(&[
            ("cf-connecting-ip", "203.0.113.66"),
            ("x-forwarded-for", "203.0.113.66, 10.0.0.2"),
        ]);
        assert_eq!(real_ip(&req, PEER, &[]), "198.51.100.9");
        assert_eq!(real_ip(&req, PEER, &["10.0.0.0/8".to_string()]), "198.51.100.9");
    }

    #[test]
    fn trusted_peer_honors_forwarded_headers() {
        let trusted = vec!["198.51.100.0/24".to_string()];
        let req = req_with_headers(&[("cf-connecting-ip", "203.0.113.66")]);
        assert_eq!(real_ip(&req, PEER, &trusted), "203.0.113.66");
        let req = req_with_headers(&[("x-forwarded-for", "203.0.113.66, 10.0.0.2")]);
        // leftmost entry is the original client, appended by the proxy.
        assert_eq!(real_ip(&req, PEER, &trusted), "203.0.113.66");
        // cf-connecting-ip takes precedence over x-forwarded-for.
        let req = req_with_headers(&[
            ("cf-connecting-ip", "203.0.113.66"),
            ("x-forwarded-for", "198.51.100.200"),
        ]);
        assert_eq!(real_ip(&req, PEER, &trusted), "203.0.113.66");
    }

    #[test]
    fn trusted_peer_ignores_garbage_forwarded_values() {
        let trusted = vec!["198.51.100.0/24".to_string()];
        let req = req_with_headers(&[
            ("cf-connecting-ip", "not-an-ip"),
            ("x-forwarded-for", "!!"),
        ]);
        assert_eq!(real_ip(&req, PEER, &trusted), "198.51.100.9");
        let empty = req_with_headers(&[("cf-connecting-ip", ""), ("x-forwarded-for", " , ")]);
        assert_eq!(real_ip(&empty, PEER, &trusted), "198.51.100.9");
    }

    #[test]
    fn trusted_peer_match_is_exact_or_cidr() {
        assert!(peer_is_trusted(&"198.51.100.9".parse().unwrap(), &["198.51.100.9".to_string()]));
        assert!(!peer_is_trusted(&"198.51.100.9".parse().unwrap(), &["198.51.100.8".to_string()]));
        assert!(peer_is_trusted(&"10.0.0.7".parse().unwrap(), &["10.0.0.0/8".to_string()]));
        assert!(!peer_is_trusted(&"11.0.0.7".parse().unwrap(), &["10.0.0.0/8".to_string()]));
        assert!(peer_is_trusted(&"10.0.0.7".parse().unwrap(), &["0.0.0.0/0".to_string()]));
        assert!(peer_is_trusted(
            &"2001:db8::1".parse().unwrap(),
            &["2001:db8::/32".to_string()]
        ));
        assert!(!peer_is_trusted(
            &"2001:db9::1".parse().unwrap(),
            &["2001:db8::/32".to_string()]
        ));
        // garbage entries never match
        assert!(!peer_is_trusted(&"10.0.0.7".parse().unwrap(), &["bogus".to_string()]));
        assert!(!peer_is_trusted(&"10.0.0.7".parse().unwrap(), &["10.0.0.0/33".to_string()]));
    }

    #[test]
    fn validate_trusted_proxy_accepts_ips_and_cidrs() {
        assert!(validate_trusted_proxy("203.0.113.4").is_ok());
        assert!(validate_trusted_proxy("10.0.0.0/8").is_ok());
        assert!(validate_trusted_proxy("2001:db8::1").is_ok());
        assert!(validate_trusted_proxy("2001:db8::/32").is_ok());
        assert!(validate_trusted_proxy("203.0.113.4/33").is_err());
        assert!(validate_trusted_proxy("203.0.113.4/xx").is_err());
        assert!(validate_trusted_proxy("203.0.113.0/24/5").is_err());
        assert!(validate_trusted_proxy("not-an-ip").is_err());
        assert!(validate_trusted_proxy("").is_err());
    }
}
