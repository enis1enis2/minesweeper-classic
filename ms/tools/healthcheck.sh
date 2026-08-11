#!/usr/bin/env bash
#
# healthcheck.sh - liveness probe for the Node sim server (replaces the
# Python healthcheck.py).  If minesweeper-sim-node is active but not
# accepting TCP connections on the telemetry port, restart it.
#
# Run by minesweeper-health.service (oneshot) on a systemd timer.

set -u

PORT="${MS_HEALTH_PORT:-28571}"
UNIT=minesweeper-sim-node

if systemctl is-active --quiet "$UNIT" && \
   timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT"; then
    echo "health: ok"
    exit 0
fi

echo "health: FAIL -> restarting $UNIT"
systemctl restart "$UNIT"
exit 1
