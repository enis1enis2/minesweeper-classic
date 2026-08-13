use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use msadmin::crypt;
use msadmin::http::{self, State};
use msadmin::store::{AuthStore, DiagDB};
use msadmin::totp;

#[derive(Clone)]
struct Opts {
    init: bool,
    selfcheck: bool,
    help: bool,
    host: String,
    port: String,
    db: String,
    config: String,
    key: String,
    session_ttl: i64,
    username: String,
    trusted_proxies: Vec<String>,
}

impl Default for Opts {
    fn default() -> Opts {
        Opts {
            init: false,
            selfcheck: false,
            help: false,
            host: "127.0.0.1".to_string(),
            port: "8444".to_string(),
            db: "data/diag.db".to_string(),
            config: "data/admin.json".to_string(),
            key: "data/diag.key".to_string(),
            session_ttl: 14400,
            username: "admin".to_string(),
            trusted_proxies: Vec::new(),
        }
    }
}

fn usage() -> String {
    "usage: msadmin [--init | --selfcheck | --help]\n       msadmin [--host HOST] [--port PORT] [--db PATH] [--config PATH] [--key PATH] [--session-ttl SECS] [--username NAME] [--trusted-proxy IP|CIDR]\n\noptions:\n  --trusted-proxy IP|CIDR   proxy whose forwarding headers (cf-connecting-ip,\n                            x-forwarded-for) are honored; repeatable. Requests\n                            from any other peer are attributed to the socket\n                            address, so unconfigured clients cannot spoof the\n                            recorded/locked IP.\n"
        .to_string()
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--init" => opts.init = true,
            "--selfcheck" => opts.selfcheck = true,
            "--help" => opts.help = true,
            "--host" | "--port" | "--db" | "--config" | "--key" | "--session-ttl" | "--username"
            | "--trusted-proxy" => {
                let value = it
                    .next()
                    .ok_or_else(|| format!("missing value for {arg}"))?;
                match arg.as_str() {
                    "--host" => opts.host = value.clone(),
                    "--port" => opts.port = value.clone(),
                    "--db" => opts.db = value.clone(),
                    "--config" => opts.config = value.clone(),
                    "--key" => opts.key = value.clone(),
                    "--session-ttl" => {
                        opts.session_ttl = value
                            .parse::<i64>()
                            .map_err(|_| format!("invalid --session-ttl value: '{value}'"))?
                    }
                    "--trusted-proxy" => {
                        http::validate_trusted_proxy(value)?;
                        opts.trusted_proxies.push(value.clone());
                    }
                    _ => opts.username = value.clone(),
                }
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(opts)
}

fn read_key_file(path: &str) -> Result<String, String> {
    let mut data = String::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut data))
        .map_err(|e| format!("cannot read key file '{path}': {e}"))?;
    Ok(data.trim().to_string())
}

fn write_key_file(path: &str, key: &str) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o400);
    }
    let mut f = options
        .open(path)
        .map_err(|e| format!("cannot write key file '{path}': {e}"))?;
    f.write_all(key.as_bytes())
        .and_then(|_| f.write_all(b"\n"))
        .map_err(|e| format!("cannot write key file '{path}': {e}"))
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("cannot flush stdout: {e}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read input: {e}"))?;
    let mut out = line.trim_end_matches(['\r', '\n']).to_string();
    if let Some(stripped) = out.strip_prefix('\u{feff}') {
        out = stripped.to_string();
    }
    Ok(out)
}

fn cmd_init(opts: &Opts) -> Result<i32, String> {
    if let Some(dir) = Path::new(&opts.key).parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create dir '{:?}': {e}", dir))?;
        }
    }
    if let Some(dir) = Path::new(&opts.config).parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create dir '{:?}': {e}", dir))?;
        }
    }
    let _key = match read_key_file(&opts.key) {
        Ok(k) if !k.is_empty() => {
            println!("using existing key at {}", opts.key);
            k
        }
        _ => {
            let k = crypt::generate_key();
            write_key_file(&opts.key, &k)?;
            println!("generated new diagnostics key at {}", opts.key);
            k
        }
    };
    let password = prompt_line("Password (>= 20 chars): ")?;
    let password2 = prompt_line("Repeat password: ")?;
    if password != password2 {
        return Err("passwords do not match".to_string());
    }
    if password.chars().count() < 20 {
        return Err("password too short (min 20 characters)".to_string());
    }
    let cfg = serde_json::json!({
        "username": opts.username,
        "password_hash": crypt::hash_password(&password)?,
        "totp_secret_b32": totp::generate_secret_b32(),
        "session_ttl_sec": opts.session_ttl,
    });
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options
        .open(&opts.config)
        .map_err(|e| format!("cannot write config '{:?}': {e}", opts.config))?;
    f.write_all(
        serde_json::to_string_pretty(&cfg)
            .map_err(|e| format!("cannot serialize config: {e}"))?
            .as_bytes(),
    )
    .and_then(|_| f.write_all(b"\n"))
    .map_err(|e| format!("cannot write config '{:?}': {e}", opts.config))?;
    let username = cfg["username"].as_str().unwrap_or("admin");
    let secret = cfg["totp_secret_b32"].as_str().unwrap_or("");
    println!("wrote admin config to {}", opts.config);
    println!("TOTP seed (add to your authenticator app):");
    println!("{}", totp::otpauth_uri(username, "Minesweeper", secret));
    Ok(0)
}

fn cmd_selfcheck() -> i32 {
    let mut failures = 0;
    let mut fail = |msg: &str| {
        eprintln!("FAIL: {msg}");
        failures += 1;
    };

    let seed = b"12345678901234567890";
    let secret = totp::encode_base32(seed);
    let vectors: [(i64, &str); 6] = [
        (59, "94287082"),
        (1111111109, "07081804"),
        (1111111111, "14050471"),
        (1234567890, "89005924"),
        (2000000000, "69279037"),
        (20000000000, "65353130"),
    ];
    for (t, want) in vectors {
        let got = totp::totp_value(&secret, t, 8).unwrap_or_default();
        if got != want {
            fail(&format!("totp {t}: got {got}, want {want}"));
        }
    }
    let got6 = totp::totp_value(&secret, 59, 6).unwrap_or_default();
    if got6 != "287082" {
        fail(&format!("totp 59/6: got {got6}, want 287082"));
    }
    if !totp::totp_verify(&secret, "287082", 0, 59) {
        fail("totpVerify rejected a valid code");
    }
    if totp::totp_verify(&secret, "000000", 0, 59) {
        fail("totpVerify accepted a wrong code");
    }

    let key = crypt::generate_key();
    let plain = b"hello, selfcheck";
    let ct = match crypt::encrypt(&key, plain) {
        Ok(c) => c,
        Err(e) => {
            fail(&format!("aes encrypt: {e}"));
            vec![0; 1]
        }
    };
    match crypt::decrypt(&key, &ct) {
        Ok(rt) if rt == plain => {}
        _ => fail("aes roundtrip mismatch"),
    }
    if !ct.is_empty() {
        let mut tampered = ct.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        if crypt::decrypt(&key, &tampered).is_ok() {
            fail("aes tamper not detected");
        }
        let other_key = crypt::generate_key();
        if crypt::decrypt(&other_key, &ct).is_ok() {
            fail("aes wrong key not detected");
        }
    }

    let hash = match crypt::hash_password("selfcheck-password") {
        Ok(h) => h,
        Err(e) => {
            fail(&format!("scrypt hash: {e}"));
            String::new()
        }
    };
    if !hash.is_empty() {
        if !crypt::verify_password(&hash, "selfcheck-password") {
            fail("scrypt verify failed");
        }
        if crypt::verify_password(&hash, "nope") {
            fail("scrypt accepted wrong password");
        }
    }

    if failures > 0 {
        eprintln!("{failures} selfcheck failure(s)");
        1
    } else {
        println!("selfcheck: all checks passed");
        0
    }
}

async fn cmd_run(opts: &Opts) -> Result<i32, String> {
    let key = read_key_file(&opts.key)?;
    if key.is_empty() {
        return Err(format!("no key at {} -- run --init first", opts.key));
    }
    let db = DiagDB::new(&opts.db)
        .map_err(|e| format!("cannot load state: {e}"))?;
    let auth = AuthStore::new(&opts.config)
        .map_err(|e| format!("cannot load state: {e}"))?;
    let state = Arc::new(State {
        db,
        auth: std::sync::Mutex::new(auth),
        key,
        ingest: std::sync::Mutex::new(Vec::new()),
        trusted_proxies: opts.trusted_proxies.clone(),
    });
    let addr = format!("{}:{}", opts.host, opts.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("server error: {e}"))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("server error: {e}"))?;
    println!(
        "ms-admin listening on {}  (db={} config={})",
        bound, opts.db, opts.config
    );
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            res = listener.accept() => {
                match res {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = http::handle_conn(state, stream).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("msadmin: error: accept: {e}");
                    }
                }
            }
        }
    }
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("msadmin: error: {e}");
            eprintln!("{}", usage());
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    if opts.init && opts.selfcheck {
        eprintln!("msadmin: error: --init and --selfcheck are mutually exclusive");
        return ExitCode::from(2);
    }
    let result = if opts.init {
        cmd_init(&opts)
    } else if opts.selfcheck {
        Ok(cmd_selfcheck())
    } else {
        // run mode: no-op here; real work in async below
        Ok(-1)
    };
    if opts.init || opts.selfcheck {
        return match result {
            Ok(0) => ExitCode::SUCCESS,
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                eprintln!("msadmin: error: {e}");
                ExitCode::from(2)
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("msadmin: error: {e}");
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(cmd_run(&opts)) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("msadmin: error: {e}");
            ExitCode::from(2)
        }
    }
}
