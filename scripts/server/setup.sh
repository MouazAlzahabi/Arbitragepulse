#!/bin/bash
# ArbitragePulse Self-Healing Pipeline — Server Installation
# Run as root on the Hetzner server:
#   sudo bash /home/botadmin/Arbitragepulse/scripts/server/setup.sh

set -euo pipefail

SCRIPTS_DIR="/home/botadmin/Arbitragepulse/scripts/server"
LOG_DIR="/var/log/arbitragepulse"
SERVICE_USER="botadmin"

echo "================================================"
echo " ArbitragePulse Self-Healing Pipeline — Install"
echo "================================================"
echo ""

# ── 1. Log directory ──────────────────────────────────────────────────────────
echo "[1/6] Creating log directory..."
mkdir -p "$LOG_DIR"
chown "${SERVICE_USER}:${SERVICE_USER}" "$LOG_DIR"
chmod 755 "$LOG_DIR"
echo "      ${LOG_DIR} ready"

# ── 2. Watchdog script ────────────────────────────────────────────────────────
echo "[2/6] Installing watchdog script..."
cp "${SCRIPTS_DIR}/ap-watchdog" /usr/local/bin/ap-watchdog
chmod +x /usr/local/bin/ap-watchdog
echo "      /usr/local/bin/ap-watchdog installed"

# ── 3. Systemd units ──────────────────────────────────────────────────────────
echo "[3/6] Installing systemd units..."
cp "${SCRIPTS_DIR}/arbitragepulse.service" /etc/systemd/system/arbitragepulse.service
cp "${SCRIPTS_DIR}/ap-watchdog.service"    /etc/systemd/system/ap-watchdog.service
cp "${SCRIPTS_DIR}/ap-watchdog.timer"      /etc/systemd/system/ap-watchdog.timer
echo "      3 units installed"

# ── 4. Log rotation ───────────────────────────────────────────────────────────
echo "[4/6] Installing logrotate config..."
cp "${SCRIPTS_DIR}/logrotate" /etc/logrotate.d/arbitragepulse
echo "      /etc/logrotate.d/arbitragepulse installed"

# ── 5. Reload systemd + enable units ─────────────────────────────────────────
echo "[5/6] Enabling services..."
systemctl daemon-reload
systemctl enable arbitragepulse
systemctl enable ap-watchdog.timer
echo "      arbitragepulse + ap-watchdog.timer enabled"

# ── 6. Start / restart everything ────────────────────────────────────────────
echo "[6/6] Starting services..."
systemctl restart arbitragepulse
sleep 6
systemctl start ap-watchdog.timer
echo "      Services started"

echo ""
echo "================================================"
echo " Installation complete!"
echo "================================================"
echo ""
echo "Useful commands:"
echo "  Engine status:   systemctl status arbitragepulse"
echo "  Engine logs:     journalctl -fu arbitragepulse"
echo "  Watchdog status: systemctl status ap-watchdog.timer"
echo "  Watchdog log:    tail -f /var/log/arbitragepulse/watchdog.log"
echo "  Engine log:      tail -f /var/log/arbitragepulse/engine.log"
echo "  Health check:    source /home/botadmin/Arbitragepulse/engine/.env && curl -s -H \"Authorization: Bearer \$API_KEY\" http://localhost:\${PORT:-3000}/health | python3 -m json.tool"
echo ""
