#!/usr/bin/env bash
#
# deploy.sh - install the Node Minesweeper stack on Debian/Ubuntu:
#   * minesweeper-sim-node.service  - sim/telemetry server (the ONLY sim server;
#                                     replaces the old Python one),
#   * minesweeper-admin.service     - diagnostics admin (loopback :8444, nginx
#                                     fronts it; only started after --init).
#
# Run as root (or with sudo):
#
#     sudo bash ms/sim-server/deploy.sh /path/to/ms [PORT]
#
# Optional overrides (env):
#   DB  - sim DB file to serve.  Default $DATA/sim.db.  To take over the
#         production DB the Python server used:  DB=/var/lib/minesweeper-sim/sim.db
#   RW  - space-separated dirs the services may write (ReadWritePaths).
#         Default $DATA.  With the production-DB takeover this must also
#         include /var/lib/minesweeper-sim.
#   ADMIN_PASSWORD - if set, run `admin/admin.js --init` non-interactively
#         (password must be >= 20 chars) and enable the admin service.  It
#         prints a TOTP otpauth:// URI you must add to your authenticator app.
#
# What it does:
#   1. copies ms/ sources into /opt/minesweeper-sim-node (core, sim-server,
#      tools, test, cli, analyze, admin),
#   2. copies the node binary as a real file (ProtectHome=true blocks the nvm
#      symlink that /usr/local/bin/node points at),
#   3. reuses the 'msim' system user with data dir /var/lib/minesweeper-sim-node,
#   4. installs minesweeper-sim-node.service (TCP $PORT, solver creds from
#      /etc/minesweeper-server/ms-solver.env),
#   5. installs minesweeper-admin.service (loopback :8444),
#   6. opens $PORT/tcp in UFW,
#   7. runs the deployed self-check.

set -euo pipefail

PORT="${2:-28571}"
SRC="${1:-$(cd "$(dirname "$0")/../.." && pwd)}"
DEST=/opt/minesweeper-sim-node
DATA=/var/lib/minesweeper-sim-node
SERVICE=minesweeper-sim-node
ADMIN_SERVICE=minesweeper-admin
USER=msim
NODE_SRC="${NODE_SRC:-/usr/local/bin/node}"
ENV_FILE=/etc/minesweeper-server/ms-solver.env
DB="${DB:-$DATA/sim.db}"
RW="${RW:-$DATA}"

if [[ $EUID -ne 0 ]]; then
    echo "run as root: sudo bash $0 [ms-dir] [port]" >&2
    exit 1
fi

for d in core sim-server admin; do
    if [[ ! -d "$SRC/$d" ]]; then
        echo "missing $SRC/$d (pass the ms/ source dir as arg 1)" >&2
        exit 1
    fi
done

echo "==> installing files to $DEST"
install -d "$DEST"
cp -a "$SRC/core" "$SRC/sim-server" "$SRC/tools" "$SRC/test" "$SRC/cli" \
    "$SRC/analyze" "$SRC/admin" "$DEST/"
install -m 0644 "$SRC/package.json" "$DEST/package.json"
chown -R root:root "$DEST"
chmod -R a+rX "$DEST"

echo "==> copying node binary as a real file (not the nvm symlink)"
cp -L "$NODE_SRC" "$DEST/node"
chmod 0755 "$DEST/node"
echo "    node version: $("$DEST/node" --version)"

echo "==> using user '$USER' (data dir $DATA)"
id "$USER" >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin "$USER"
install -d -o "$USER" -g "$USER" "$DATA"

if [[ ! -f "$ENV_FILE" ]]; then
    echo "missing $ENV_FILE (solver creds) - creating empty file" >&2
    : > "$ENV_FILE"
    chmod 0600 "$ENV_FILE"
fi

echo "==> installing systemd unit $SERVICE.service (db=$DB)"
cat > /etc/systemd/system/$SERVICE.service <<EOF
[Unit]
Description=Minesweeper simulation server (Node)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER
Group=$USER
WorkingDirectory=$DEST
EnvironmentFile=$ENV_FILE
LimitNOFILE=65535
ExecStart=$DEST/node $DEST/sim-server/server.js --host 0.0.0.0 --port $PORT --db $DB --rate 5
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$RW
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

echo "==> installing systemd unit $ADMIN_SERVICE.service"
cat > /etc/systemd/system/$ADMIN_SERVICE.service <<EOF
[Unit]
Description=Minesweeper diagnostics admin (ingest + viewer, loopback only)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER
Group=$USER
WorkingDirectory=$DEST
LimitNOFILE=65535
ExecStart=$DEST/node $DEST/admin/admin.js --host 127.0.0.1 --port 8444 --db $DATA/diag.db --config /etc/minesweeper-server/admin.json --key /etc/minesweeper-server/diag.key
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now $SERVICE

if [[ -n "${ADMIN_PASSWORD:-}" ]]; then
    echo "==> initializing admin credentials (--init)"
    printf '%s\n%s\n' "$ADMIN_PASSWORD" "$ADMIN_PASSWORD" | \
        "$DEST/node" "$DEST/admin/admin.js" --init \
        --db "$DATA/diag.db" \
        --config /etc/minesweeper-server/admin.json \
        --key /etc/minesweeper-server/diag.key
    chown "$USER:$USER" /etc/minesweeper-server/admin.json /etc/minesweeper-server/diag.key
    chmod 0600 /etc/minesweeper-server/admin.json
    chmod 0400 /etc/minesweeper-server/diag.key
    systemctl enable --now $ADMIN_SERVICE
else
    echo "==> admin unit installed but NOT initialized"
    echo "    run:  ADMIN_PASSWORD='<pw>' bash $0 $SRC $PORT"
    echo "    (the generated otpauth:// URI must be added to your authenticator)"
fi

echo "==> opening TCP $PORT in UFW"
ufw allow "$PORT/tcp" >/dev/null

echo "==> running deployed self-check"
"$DEST/node" "$DEST/sim-server/server.js" --selfcheck

echo
echo "Installed. Status:"
echo "  systemctl status $SERVICE $ADMIN_SERVICE"
echo "  journalctl -u $SERVICE -u $ADMIN_SERVICE -f"
echo
echo "Clients connect with:"
echo "  minesweeper.exe --telemetry <this-host>:${PORT}"
echo
echo "Admin (loopback :8444, TLS terminated by nginx):"
echo "  https://admin.jellyfiner.dpdns.org/ms-admin/"
