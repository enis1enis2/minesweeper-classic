#!/usr/bin/env bash
#
# deploy.sh - install the Minesweeper simulation/telemetry server on
# Debian/Ubuntu as a systemd service with a UFW firewall rule.
#
# Run as root (or with sudo):
#
#     sudo bash server/deploy.sh
#
# What it does:
#   1. installs python3, python3-venv, ufw (apt),
#   2. copies the server + solver into /opt/minesweeper-server,
#   3. creates a dedicated system user 'msim' with /var/lib/minesweeper-sim,
#   4. creates a python3 venv and runs the built-in self-check,
#   5. installs minesweeper-sim.service (TCP 28571),
#   6. opens 28571/tcp in UFW.
#
# After install:  systemctl enable --now minesweeper-sim

set -euo pipefail

PORT="${PORT:-28571}"
DEST=/opt/minesweeper-server
DATA=/var/lib/minesweeper-sim
SERVICE=minesweeper-sim
USER=msim
SERVER_DIR="$(cd "$(dirname "$0")" && pwd)"
BOT_DIR="$(dirname "$SERVER_DIR")/minesweeper_bot"

if [[ $EUID -ne 0 ]]; then
    echo "run as root: sudo bash $0" >&2
    exit 1
fi

for f in sim_engine.py ms_server.py selfcheck.py verify_parity.py; do
    if [[ ! -f "$SERVER_DIR/$f" ]]; then
        echo "missing $SERVER_DIR/$f" >&2
        exit 1
    fi
done
if [[ ! -f "$BOT_DIR/ms_solver.py" ]]; then
    echo "missing $BOT_DIR/ms_solver.py" >&2
    exit 1
fi

echo "==> installing packages (python3, python3-venv, ufw)"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3 python3-venv ufw

echo "==> installing files to $DEST"
install -d "$DEST" "$DEST/minesweeper_bot"
install -m 0644 "$SERVER_DIR"/sim_engine.py "$SERVER_DIR"/ms_server.py \
    "$SERVER_DIR"/selfcheck.py "$SERVER_DIR"/verify_parity.py "$DEST"/
install -m 0644 "$BOT_DIR/ms_solver.py" "$DEST/minesweeper_bot/"

echo "==> creating user '$USER' (data dir $DATA)"
id "$USER" >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin "$USER"
install -d -o "$USER" -g "$USER" "$DATA"

echo "==> creating python3 venv"
python3 -m venv "$DEST/.venv"
"$DEST/.venv/bin/python" -m ensurepip --upgrade >/dev/null 2>&1 || true

echo "==> running self-check"
"$DEST/.venv/bin/python" "$DEST/selfcheck.py"

echo "==> installing systemd unit $SERVICE.service"
cat > /etc/systemd/system/$SERVICE.service <<EOF
[Unit]
Description=Minesweeper simulation/telemetry server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$USER
Group=$USER
WorkingDirectory=$DEST
ExecStart=$DEST/.venv/bin/python $DEST/ms_server.py --host 0.0.0.0 --port $PORT --db $DATA/sim.db --rate 5
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

echo "==> opening TCP $PORT in UFW (and enabling the firewall)"
ufw --force enable >/dev/null
ufw allow "$PORT/tcp" >/dev/null

echo
echo "Installed. Start now with:"
echo "  systemctl enable --now $SERVICE"
echo "  journalctl -u $SERVICE -f"
echo
echo "Clients connect with:"
echo "  minesweeper.exe --telemetry <this-host>:${PORT}"
echo
echo "Manual UFW commands (already applied above, shown for reference):"
echo "  ufw allow ${PORT}/tcp                      # open the telemetry port"
echo "  ufw --force enable                          # turn the firewall on"
echo "  ufw status verbose                          # verify the rule"
echo "  ufw allow from <client-ip> to any port ${PORT} proto tcp   # single client"
echo "  ufw delete allow ${PORT}/tcp                # remove the rule again"
