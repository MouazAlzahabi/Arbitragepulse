#!/bin/bash
# ArbitragePulse — Local → Hetzner Deploy Script
#
# Usage:
#   ./scripts/deploy.sh                       # deploys to default server
#   ./scripts/deploy.sh botadmin@1.2.3.4      # override server
#   ./scripts/deploy.sh --code-only           # skip rebuild (config/script changes only)
#   ./scripts/deploy.sh --build-only          # rebuild + restart, no rsync
#
# Requires: ssh key auth already set up (run ssh-copy-id first)

set -euo pipefail

# ── Config ─────────────────────────────────────────────────────────────────────
SERVER="botadmin@5.161.193.123"
REMOTE_PATH="/home/botadmin/Arbitragepulse"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Flags ──────────────────────────────────────────────────────────────────────
DO_SYNC=true
DO_BUILD=true

for arg in "$@"; do
    case "$arg" in
        --code-only)  DO_BUILD=false ;;
        --build-only) DO_SYNC=false ;;
        botadmin@*|root@*)  SERVER="$arg" ;;
    esac
done

echo "================================================"
echo " ArbitragePulse Deploy → ${SERVER}"
echo "================================================"
echo "  Sync:  $DO_SYNC"
echo "  Build: $DO_BUILD"
echo ""

# ── Step 1: Sync code ─────────────────────────────────────────────────────────
if [ "$DO_SYNC" = true ]; then
    echo "[1/3] Syncing code..."
    rsync -az --progress \
        --exclude 'target/' \
        --exclude '.git/' \
        --exclude 'contract/lib/' \
        --exclude 'lab/' \
        --exclude '*.log' \
        --exclude '*.db' \
        "$PROJECT_ROOT/" "$SERVER:$REMOTE_PATH/"
    echo "      Sync complete"
else
    echo "[1/3] Sync skipped"
fi

echo ""

# ── Step 2: Build on server ───────────────────────────────────────────────────
if [ "$DO_BUILD" = true ]; then
    echo "[2/3] Building on server (this takes ~2-3 min first time, ~30s after)..."
    ssh "$SERVER" "
        set -e
        cd $REMOTE_PATH
        export PATH=\$PATH:\$HOME/.cargo/bin
        cargo build --release -p engine 2>&1 | tail -5
        echo 'Build OK'
    "
    echo ""

    echo "[3/3] Restarting service + health check..."
    ssh "$SERVER" "
        sudo systemctl restart arbitragepulse
        sleep 6
        source $REMOTE_PATH/engine/.env
        STATUS=\$(curl -sf --max-time 5 \
            -H \"Authorization: Bearer \$API_KEY\" \
            \"http://localhost:\${PORT:-3000}/health\" | python3 -c \"
import sys, json
d = json.load(sys.stdin)
chains = ', '.join(c['chain_name'] + ':' + ('OK' if c['rpc_ok'] else 'FAIL') for c in d.get('chains',[]))
print(f\\\"status={d['status']} chains=[{chains}] uptime={d['uptime_seconds']}s\\\")
\" 2>/dev/null || echo 'health check failed — check journalctl -u arbitragepulse')
        echo \"\$STATUS\"
    "
else
    echo "[2/3] Build skipped"
    echo "[3/3] Restarting service..."
    ssh "$SERVER" "sudo systemctl restart arbitragepulse && echo 'Restarted'"
fi

echo ""
echo "================================================"
echo " Deploy complete!"
echo "================================================"
echo ""
echo "Monitor:"
echo "  ssh $SERVER 'journalctl -fu arbitragepulse'"
echo "  ssh $SERVER 'tail -f /var/log/arbitragepulse/watchdog.log'"
echo ""
