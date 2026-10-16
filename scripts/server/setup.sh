#!/bin/bash
# ArbitragePulse — Server installation (systemd + logs)
# One engine instance per chain. Run as root on the server:
#   sudo bash /home/botadmin/Arbitragepulse/scripts/server/setup.sh [instance]
# instance defaults to "base" and needs engine/<instance>.env (see
# PRODUCTION_DEPLOYMENT_GUIDE.md). Run again with another name to add an instance,
# e.g. "base-sepolia" for the testing stage.

set -euo pipefail

INSTANCE="${1:-base}"

SCRIPTS_DIR="/home/botadmin/Arbitragepulse/scripts/server"
LOG_DIR="/var/log/arbitragepulse"
SERVICE_USER="botadmin"

echo "================================================"
echo " ArbitragePulse — Server install"
echo "================================================"
echo ""

# ── 1. Log directory ──────────────────────────────────────────────────────────
echo "[1/4] Creating log directory..."
mkdir -p "$LOG_DIR"
chown "${SERVICE_USER}:${SERVICE_USER}" "$LOG_DIR"
chmod 755 "$LOG_DIR"
echo "      ${LOG_DIR} ready"

# ── 2. Systemd unit ───────────────────────────────────────────────────────────
echo "[2/4] Installing systemd template unit..."
cp "${SCRIPTS_DIR}/arbitragepulse@.service" /etc/systemd/system/arbitragepulse@.service
echo "      arbitragepulse@.service installed"
if [ ! -f "/home/botadmin/Arbitragepulse/engine/${INSTANCE}.env" ]; then
    echo "      ERROR: engine/${INSTANCE}.env not found — create it first"; exit 1
fi

# Pre-template single unit (one process for all chains): replace it.
if [ -f /etc/systemd/system/arbitragepulse.service ]; then
    systemctl stop arbitragepulse 2>/dev/null || true
    systemctl disable arbitragepulse 2>/dev/null || true
    rm -f /etc/systemd/system/arbitragepulse.service
    echo "      removed old single-instance arbitragepulse.service"
fi

# Legacy ap-watchdog (removed from this repo): stop, disable, drop unit files
systemctl stop ap-watchdog.timer 2>/dev/null || true
systemctl disable ap-watchdog.timer 2>/dev/null || true
systemctl disable ap-watchdog.service 2>/dev/null || true
rm -f /etc/systemd/system/ap-watchdog.timer /etc/systemd/system/ap-watchdog.service
rm -f /usr/local/bin/ap-watchdog

# ── 3. Log rotation ───────────────────────────────────────────────────────────
echo "[3/4] Installing logrotate config..."
cp "${SCRIPTS_DIR}/logrotate" /etc/logrotate.d/arbitragepulse
echo "      /etc/logrotate.d/arbitragepulse installed"

# ── 4. Reload systemd + enable engine ────────────────────────────────────────
echo "[4/4] Enabling and starting arbitragepulse@${INSTANCE}..."
systemctl daemon-reload
systemctl enable "arbitragepulse@${INSTANCE}"
systemctl restart "arbitragepulse@${INSTANCE}"
sleep 6
echo "      arbitragepulse@${INSTANCE} started"

echo ""
echo "================================================"
echo " Installation complete!"
echo "================================================"
echo ""
echo "Useful commands:"
echo "  Engine status:   systemctl status arbitragepulse@${INSTANCE}"
echo "  Stop engine:     sudo systemctl stop arbitragepulse@${INSTANCE}"
echo "  Engine logs:     journalctl -fu arbitragepulse@${INSTANCE}"
echo "  Engine log file: tail -f /var/log/arbitragepulse/${INSTANCE}.log"
echo "  Health check:    source /home/botadmin/Arbitragepulse/engine/${INSTANCE}.env && curl -s -H \"Authorization: Bearer \$API_KEY\" http://localhost:\${PORT:-3000}/health | python3 -m json.tool"
echo ""
