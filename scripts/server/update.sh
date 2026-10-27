#!/bin/bash
# ArbitragePulse — update the server from GitHub. Run ON the server as botadmin:
#   ~/Arbitragepulse/scripts/server/update.sh                 # pull, build, fresh db + log, restart
#   ~/Arbitragepulse/scripts/server/update.sh --keep-data     # same, but keep the db and log
#   ~/Arbitragepulse/scripts/server/update.sh --instance=base-sepolia
#
# Pulls main, builds the dashboard and the engine, then stops the engine, deletes its trade
# database and log, and starts it again. A failed pull or build stops here and the running
# engine keeps going untouched. Needs only the sudoers rule from scripts/aws/provision.sh
# (systemctl start/stop arbitragepulse@*): the log dir is owned by botadmin
# (scripts/server/setup.sh), so the root-owned log file can be removed without sudo.

set -euo pipefail

INSTANCE="base"
CLEAR_DATA=true
for arg in "$@"; do
    case "$arg" in
        --keep-data)  CLEAR_DATA=false ;;
        --instance=*) INSTANCE="${arg#--instance=}" ;;
        *) echo "Unknown argument: $arg" >&2; exit 1 ;;
    esac
done

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
UNIT="arbitragepulse@${INSTANCE}"
ENV_FILE="$REPO/engine/${INSTANCE}.env"
LOG_FILE="/var/log/arbitragepulse/${INSTANCE}.log"
export PATH="$PATH:$HOME/.cargo/bin"

[ -f "$ENV_FILE" ] || { echo "ERROR: $ENV_FILE not found" >&2; exit 1; }
# Only the values this script needs; the env file also holds the private key.
env_get() { sed -n "s/^[[:space:]]*$1=//p" "$ENV_FILE" | tail -1 | tr -d "\"'"; }
DB_PATH="$(env_get DB_PATH)"; DB_PATH="${DB_PATH:-trades.db}"
case "$DB_PATH" in /*) ;; *) DB_PATH="$REPO/$DB_PATH" ;; esac
PORT="$(env_get PORT)"; PORT="${PORT:-3000}"
API_KEY="$(env_get API_KEY)"

cd "$REPO"

echo "[1/4] Pulling main..."
git pull --ff-only
git log --oneline -1

echo "[2/4] Building dashboard..."
npm ci --no-audit --no-fund > /tmp/arbitragepulse-npm.log 2>&1 && npm run build >> /tmp/arbitragepulse-npm.log 2>&1 \
    || { tail -30 /tmp/arbitragepulse-npm.log; echo "Dashboard build FAILED — engine not restarted"; exit 1; }

echo "[3/4] Building engine (about 1.5 min)..."
if ! cargo build --release -p engine > /tmp/arbitragepulse-build.log 2>&1; then
    tail -40 /tmp/arbitragepulse-build.log
    echo "Engine build FAILED — engine not restarted"
    exit 1
fi

echo "[4/4] Restarting $UNIT..."
sudo systemctl stop "$UNIT"
if $CLEAR_DATA; then
    rm -f "$DB_PATH" "$DB_PATH-wal" "$DB_PATH-shm"
    echo "      database cleared ($DB_PATH)"
    if rm -f "$LOG_FILE" 2>/dev/null; then
        echo "      log cleared ($LOG_FILE)"
    else
        echo "      WARN: could not remove $LOG_FILE — as ubuntu: sudo chown botadmin:botadmin /var/log/arbitragepulse"
    fi
fi
sudo systemctl start "$UNIT"

# /health answers 503 while starting; poll every 3 s for up to 90 s.
STATUS=""
for _ in $(seq 1 30); do
    sleep 3
    BODY=$(curl -s --max-time 5 -H "Authorization: Bearer $API_KEY" "http://localhost:${PORT}/health" || true)
    case "$BODY" in *'"status":"ok"'*|*'"status":"paused"'*) STATUS="$BODY"; break ;; esac
done
if [ -n "$STATUS" ]; then
    echo "      healthy: $(printf '%s' "$STATUS" | head -c 200)"
else
    echo "      not healthy after 90 s (pool discovery can take longer) — check: tail -f $LOG_FILE"
fi
echo "Done. Hard-refresh the dashboard (Cmd+Shift+R)."
