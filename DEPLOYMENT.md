# Deployment guide: TLS for the telemetry/simulation link

This guide explains how to encrypt the `mserver` telemetry link end to end, and
how Cloudflare, Let's Encrypt and TLS front-proxies fit together. It covers the
Rust clients (`ms-rs/msapp`) with native TLS, and the legacy C clients (Win32 +
Linux) which still connect plaintext.

Read `SECURITY.md` first for the threat model.

## Quick summary

| Component | TLS support | How |
|---|---|---|
| `mserver` | **Native** | `--tls-port 28572 --tls-cert cert.pem --tls-key key.pem` (plaintext `--port` stays active) |
| `msapp` (Rust GUI) | **Native** | `--tls` plus `--tls-ca FILE` when the cert is not from a public CA |
| C clients (Win32/Linux) | **None today** | plaintext; encrypt the wire with a local `stunnel` client relay (see below) |

TLS wraps the existing line protocol — seeds, metrics, leaderboard and the
`req*` solver requests all travel over the same messages, just encrypted. No
protocol versioning is needed.

## Option A — Let's Encrypt on the origin (recommended)

Run TLS straight on the `mserver` process. You need a **domain name** pointing
at your VPS (the cert must carry the name your clients connect to).

### 1. Get a certificate with certbot

```sh
sudo apt install certbot
sudo certbot certonly --standalone -d ms.example.com --non-interactive --agree-tos -m you@example.com
```

`certbot certonly --standalone` binds port 80 briefly (fine on a server that
does not yet run a web server). For headless VPSes without inbound port 80,
use DNS-01, which also allows wildcards:

```sh
sudo certbot certonly --manual --preferred-challenges dns -d 'ms.example.com' \
  --agree-tos -m you@example.com
# add the printed TXT record, wait for propagation, confirm
```

Certificates land in `/etc/letsencrypt/live/ms.example.com/`:

- `fullchain.pem` — the certificate (pass this to `--tls-cert`)
- `privkey.pem` — the private key (pass this to `--tls-key`)

### 2. Run mserver with TLS

```sh
mserver --host 0.0.0.0 --port 28571 \
        --tls-port 28572 \
        --tls-cert /etc/letsencrypt/live/ms.example.com/fullchain.pem \
        --tls-key  /etc/letsencrypt/live/ms.example.com/privkey.pem
```

The key must be unencrypted PEM. `mserver` reads both files **once at
startup**, so it must be restarted after every renewal.

Systemd unit (`/etc/systemd/system/mserver.service`), restarting on renewal via
a certbot deploy hook:

```ini
[Unit]
Description=minesweeper simulation server
After=network-online.target

[Service]
ExecStart=/opt/ms/mserver --host 0.0.0.0 --port 28571 \
    --tls-port 28572 \
    --tls-cert /etc/letsencrypt/live/ms.example.com/fullchain.pem \
    --tls-key  /etc/letsencrypt/live/ms.example.com/privkey.pem
Restart=on-failure
User=mserver

[Install]
WantedBy=multi-user.target
```

```sh
# /etc/letsencrypt/renewal-hooks/deploy/mserver-restart.sh
#!/bin/sh
systemctl restart mserver
```

```sh
sudo chmod +x /etc/letsencrypt/renewal-hooks/deploy/mserver-restart.sh
sudo systemctl daemon-reload && sudo systemctl enable --now mserver
```

### 3. Connect from the Rust client

```sh
msapp --telemetry ms.example.com:28572 --tls --solver-user alice --solver-pass secret
```

Because the cert is from a public CA (Let's Encrypt), the client verifies it
against its bundled webpki roots — no `--tls-ca` needed. **The host in
`--telemetry` must match the certificate's name** (or an IP SAN in the cert).

## Option B — self-signed / private CA (testing, or internal deployments)

Generate a self-signed cert with any tool (OpenSSL example):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout key.pem -out cert.pem -days 3650 -nodes \
  -subj "/CN=ms.example.com" \
  -addext "subjectAltName=DNS:ms.example.com,IP:127.0.0.1"
```

Run `mserver` as in Option A pointing at these files. Clients must trust the
cert explicitly:

```sh
msapp --telemetry ms.example.com:28572 --tls --tls-ca cert.pem --solver-user alice --solver-pass secret
```

`--tls-ca` appends the given PEM bundle to the client's trust roots. The
client **fails closed**: if the CA file is missing or invalid, the link is
disabled rather than silently downgraded to plaintext.

## Option C — TLS front-proxy (stunnel or haproxy) instead of native TLS

Use this when you do not want `mserver` to own the TLS keys, or to terminate
TLS for several backends at one point. `mserver` then listens on localhost and
the proxy accepts the public, encrypted port.

stunnel server (`/etc/stunnel/ms.conf`):

```
[mserver-tls]
accept = 28572
connect = 127.0.0.1:28571
cert = /etc/letsencrypt/live/ms.example.com/fullchain.pem
key  = /etc/letsencrypt/live/ms.example.com/privkey.pem
```

haproxy equivalent (`/etc/haproxy/haproxy.cfg`):

```
frontend ms_tls
    bind :28572 ssl crt /etc/letsencrypt/live/ms.example.com/fullchain.pem
    default_backend ms_plain
backend ms_plain
    server ms1 127.0.0.1:28571
```

The client side is identical to Option A (`--tls` / `--tls-ca`), since the
proxy speaks the same TLS. Restart the proxy on cert renewal instead of
`mserver`.

## Cloudflare — what actually proxies what

Cloudflare's **free** plan is an **HTTP/HTTPS reverse proxy**: it can only
proxy web traffic (ports 80/443 and a few HTTP(S) alt ports). The mserver
telemetry protocol is a **raw TCP protocol on a custom port (28571)**, not
HTTP, so the free CDN will not carry it. A free-zone A record set to "DNS
only" (grey cloud) makes Cloudflare act purely as a DNS host — the connection
goes **directly** to your VPS and Cloudflare is not in the data path at all.
That is the correct free configuration:

- DNS-only record `ms.example.com → <VPS IP>` (grey cloud).
- Let's Encrypt cert (Option A) terminates at the origin.
- Clients reach `ms.example.com:28572` directly; TLS is end-to-end between the
  client and your origin. Cloudflare's TLS/SSL settings do not apply to this
  port.

If you specifically want **Cloudflare in the data path** for a raw TCP port,
that is **Cloudflare Spectrum** (a paid add-on, per app/port). With Spectrum:

1. Create a Spectrum app on port 28572 (protocol TCP, HTTPS or TCP+TLS).
2. Cloudflare terminates TLS at the edge with a cert of your choice.
3. Origin traffic re-encrypts to your VPS. Use a cert the origin trusts:
   a free **Cloudflare Origin CA certificate** (issued from the dashboard,
   never public), or keep using Let's Encrypt.
4. `mserver` then serves Spectrum's traffic: enable `--tls-port` with the
   origin cert, or point Spectrum at the plaintext `--port` if you accept a
   plaintext WAN leg to Cloudflare (not recommended) — prefer the TLS port.

"Let's Encrypt to proxy the connection before going to Cloudflare" maps to
this layout: Let's Encrypt provides the certificate **your origin (or its
front proxy) presents**, so the client→origin / client→Cloudflare leg is
encrypted. The same cert can be uploaded to Cloudflare as a custom ("bring
your own") edge certificate. With a DNS-only record there is no Cloudflare in
the path, so the Let's Encrypt cert simply terminates at the origin, which is
the simplest correct setup.

## The C clients (Win32 + Linux) stay plaintext

The Win32 and Linux C clients have no TLS stack in the build. Options to keep
them safe on the wire:

1. **Local stunnel client relay.** Run a stunnel client on each machine that
   listens on `127.0.0.1:28571` and forwards over TLS to the server
   (`ms.example.com:28572`). The game keeps talking plaintext to localhost;
   the WAN leg is encrypted. `--telemetry 127.0.0.1:28571` in every client.
   ```
   # /etc/stunnel/ms-client.conf  (client machine)
   client = yes
   [ms-relay]
   accept = 127.0.0.1:28571
   connect = ms.example.com:28572
   ```
2. **Restrict the plaintext port.** Firewall the server's `--port` (28571) so
   only the C-client subnets (or the stunnel relays) can reach it, and keep
   the TLS port open for Rust clients.
3. **`--no-telemetry`** on clients that do not need the link at all.

## Firewall

Debian/Ubuntu (UFW), allowing only what you need:

```sh
sudo ufw allow 28572/tcp comment 'telemetry TLS (Rust clients + stunnel relays)'
sudo ufw allow from 10.0.0.0/24 to any port 28571/tcp comment 'plaintext telemetry (C clients)'
# or close plaintext entirely:
# sudo ufw deny 28571/tcp
```

`msadmin` binds `127.0.0.1:8444` by default — do not expose it publicly (see
`SECURITY.md`).

## Verify

From any machine that can reach the server:

```sh
# TLS handshake + cert chain
openssl s_client -connect ms.example.com:28572 -servername ms.example.com \
  -showcerts </dev/null 2>/dev/null | openssl x509 -noout -subject -issuer -dates

# plaintext port (if still open) answers with the protocol
nc -vz ms.example.com 28571
```

Then connect a client with `--tls` and confirm `telemetry` shows
`connected=1` and rising `seeds=` counts. In the repo test suite,
`ms-rs/mserver/tests/tls_roundtrip.rs` exercises a full TLS auth + `reqseed`
round trip against a self-signed cert.
