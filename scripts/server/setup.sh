#!/bin/bash
# ArbitragePulse — Server installation (systemd + logs)
# Run as root on the server:
#   sudo bash /home/botadmin/Arbitragepulse/scripts/server/setup.sh

set -euo pipefail

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
echo "[2/4] Installing systemd unit..."
cp "${SCRIPTS_DIR}/arbitragepulse.service" /etc/systemd/system/arbitragepulse.service
echo "      arbitragepulse.service installed"

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
echo "[4/4] Enabling and starting engine..."
systemctl daemon-reload
systemctl enable arbitragepulse
systemctl restart arbitragepulse
sleep 6
echo "      arbitragepulse started"

echo ""
echo "================================================"
echo " Installation complete!"
echo "================================================"
echo ""
echo "Useful commands:"
echo "  Engine status:   systemctl status arbitragepulse"
echo "  Stop engine:     sudo systemctl stop arbitragepulse"
echo "  Engine logs:     journalctl -fu arbitragepulse"
echo "  Engine log file: tail -f /var/log/arbitragepulse/engine.log"
echo "  Health check:    source /home/botadmin/Arbitragepulse/engine/.env && curl -s -H \"Authorization: Bearer \$API_KEY\" http://localhost:\${PORT:-3000}/health | python3 -m json.tool"
echo ""
