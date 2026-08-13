# Security

Security notes, accepted risks and deployment guidance for the Minesweeper
client/server suite (C client, Linux client, `mserver`, `msadmin`).

## Overview

This repository is a two-layer system:

- **Clients** (Win32 `src/`, Linux `linux/`, Rust GUI `ms-rs/msapp`) — local
  games plus a telemetry link to a simulation/telemetry server.
- **Servers** (`ms-rs/mserver` — simulation/telemetry, `ms-rs/msadmin` —
  diagnostics web UI) — operators run these.

The audit-hardening below focuses on making server processes survive
untrusted input and on limiting what a network peer can forge or force.

## Hardening applied

| Ref | Area | Change |
|-----|------|--------|
| C1 | `mserver` robustness | Removed `panic = "abort"`; connection/drain tasks run under `catch_unwind`; `simulate_game` returns `Result` and the batch runner gates on it; malformed input can no longer crash the server or other clients. |
| C2 | `msadmin` IP spoofing | `real_ip()` honors `cf-connecting-ip` / `x-forwarded-for` only from peers matching `--trusted-proxy IP|CIDR`; all other clients are attributed to their socket address. |
| C3 | Linux config shell injection | `linux/ms_ini.c::mkdirs_for_path` rewritten to POSIX `mkdir`/`_WIN32 _mkdir` with no `system()`/`popen()`; also fixed a latent read/write mismatch in `ms_ini_get_str`. |
| C4 | `mserver` panic-free DB/network paths | All DB methods return `rusqlite::Result`; the connection mutex survives poisoning; client-writer races degrade gracefully (`let Some … else`); degraded paths reply with protocol-valid framing. |
| C5 | Telemetry/session hygiene | Linux `telemetry on` restarts against the user-configured endpoint (not the hardcoded production host); verified msadmin sessions are in-memory only, never persisted; documented the plaintext telemetry stream and the `--no-telemetry` opt-out. |

## Known limitations (accepted)

These are deliberate, documented constraints. Do not assume protections that
are not listed here.

- **The mserver protocol is plaintext TCP** — no TLS. Game metric lines,
  leaderboard submissions and the seed stream are readable by anyone who can
  observe the network path. The Windows device-diagnostics report is the
  exception: it is delivered separately over HTTPS (`WinHTTP`,
  `WINHTTP_FLAG_SECURE`).
- **The metric/outcome stream is unauthenticated.** Anyone who can reach an
  `mserver` can connect, stream seeds and inject metric lines. HMAC-SHA256
  challenge authentication covers only the `req*` solver-request path (see
  `--solver-user` / `--solver-pass`). Adding authentication or per-message
  integrity to the whole stream is a wire-format change and is **out of
  scope** until explicitly requested.
- **Telemetry is on by default.** Clients connect to the configured endpoint
  on startup unless run with `--no-telemetry`.
- **`msadmin` is HTTP, not HTTPS.** The login cookie is `HttpOnly; Secure;
  SameSite=Lax`; over plain HTTP browsers will not store a `Secure` cookie,
  so the admin UI effectively requires TLS termination (or localhost).
  `msadmin` binds `127.0.0.1:8444` by default.
- **Client-side secrets** (solver credentials) are passed on the command
  line, in `--solver-config <file.json>`, or via `MS_SOLVER_USER` /
  `MS_SOLVER_PASS`. They are not persisted by the clients.

## Secure deployment guidance

- **Run your own servers.** Point clients at your instance with
  `--telemetry <host>:<port>`. The retired public backend at
  `135.125.79.15:28571` is for the original operator only.
- **Clients:** disable telemetry with `--no-telemetry` when you do not want
  game metrics leaving the machine. Use `--telemetry host:port` to talk to a
  trusted server instead of the default.
- **`msadmin`:** bind to localhost or a private interface; place it behind a
  TLS-terminating reverse proxy; restrict with `--trusted-proxy` so
  forwarded-IP headers are only honored from that proxy; set `--session-ttl`
  to bound session lifetime. Protect `data/admin.json` (password hash, TOTP
  secret) and `data/diag.key` (cookie encryption key) with filesystem
  permissions.
- **`mserver`:** firewall it. Do not expose the protocol port beyond the
  machines whose clients should participate, since the stream is plaintext
  and the write path is unauthenticated.
- **Linux client:** `--no-telemetry` and `--telemetry host:port` are
  supported identically to the Win32 client.

## Reporting

This is a personal project without a dedicated security contact. To report a
vulnerability, open a GitHub issue describing the finding; include the
affected component, a minimal reproduction, and the expected impact.
