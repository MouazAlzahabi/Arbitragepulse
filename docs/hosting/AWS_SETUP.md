# AWS hosting runbook — ArbitragePulse engine

**Reader:** the operator, or an AI agent executing on the operator's behalf. Phases run in order, and each ends with a **STOP** that needs the operator's explicit "go". Commands run on the operator's Mac unless marked `[server]`.

**Target:** one AWS EC2 `t4g.small` (2 vCPU / 2 GB Graviton, Ubuntu 24.04 arm64) in **us-east-1**, about **$18/month**:

| Item | Cost/mo |
|---|---|
| Instance | ≈ $12.3 |
| Public IPv4 / Elastic IP | ≈ $3.7 |
| 20 GB gp3 disk | ≈ $1.6 |

These prices were checked 2026-10-23. The engine is I/O-bound, so this size is enough. Move to `t4g.medium` (≈ $30) if memory or CPU credits run short.

## Safety rules (an executing agent must follow all of them)

1. **Secrets never leave the operator's hands.**
   - `PRIVATE_KEY`, `ALCHEMY_KEY` and `API_KEY` must not be typed into this chat or an agent prompt, put in EC2 user-data, written to the repo, or printed.
   - The operator writes `engine/base.env` on the server themselves (Phase 3). An agent may check that the file exists and has mode 600, but never `cat` it.
2. **Every paid AWS create needs the operator's "go".** `provision.sh` asks before launching the instance and before allocating the Elastic IP. Don't pass `--yes` unless the operator said so.
3. **Only port 22 is open, and only from the operator's IP.**
   - Never open 3000 (API/WebSocket). The dashboard goes through an SSH tunnel.
   - Never add `0.0.0.0/0` rules.
4. **The engine runs only through systemd on the server, with `DRY_RUN=true`, until Phase 5.**
   - Never start the engine on the Mac from the repo root: the root `.env` holds a live key with `DRY_RUN=false`.
   - Never flip `DRY_RUN=false` without the operator's explicit go-live.
5. **The live wallet's nonce must not change before go-live.** Check it read-only:
   ```bash
   cast nonce 0xf2f422DAe16146DDEB9C888aACDdB23e3EA02327 --rpc-url https://mainnet.base.org
   ```
6. **Use least privilege.** Use short-lived SSO credentials scoped to EC2, not root and not long-lived access keys.

---

## Phase 0 — AWS account (operator, in the browser)

1. Sign up at https://aws.amazon.com. Check that every page's address ends in `.amazon.com` / `.aws.amazon.com`.
2. Turn on **MFA for the root user**: account menu → Security credentials.
3. Set a **budget alert**: Billing → Budgets → Create budget → monthly cost budget of **$30**, with an email at 80%.
4. In the region selector (top right), choose **US East (N. Virginia) us-east-1**.
5. Create a user for daily and CLI work:
   1. Open **IAM Identity Center** → Enable.
   2. Create user `arb-admin`.
   3. Create a permission set with `AmazonEC2FullAccess` and `AmazonSSMReadOnlyAccess`. The SSM policy is needed for the Ubuntu AMI lookup.
   4. Assign it to the account.
   5. Note the **AWS access portal URL**.
6. On the Mac:
   ```bash
   brew install awscli
   aws configure sso            # SSO start URL = access portal URL, region us-east-1, profile name: arb
   export AWS_PROFILE=arb
   aws sts get-caller-identity  # must print the account id
   ```
   Credentials are short-lived. Run `aws sso login --profile arb` when they expire.

**STOP — operator confirms:** account ready, MFA on, budget set, `aws sts get-caller-identity` works.

## Phase 1 — Provision (agent or operator)

```bash
export AWS_PROFILE=arb
scripts/aws/provision.sh --dry-run    # read-only lookups; prints every create
scripts/aws/provision.sh              # creates; asks before the instance and the Elastic IP
```

The script is idempotent (a re-run reuses what exists), and everything is tagged `Project=arbitragepulse`. It creates:

| Resource | Settings |
|---|---|
| Key pair `arbitragepulse` | From `~/.ssh/id_ed25519.pub` (override with `SSH_PUBKEY`) |
| Security group `arbitragepulse-sg` | Inbound TCP 22 from your current IP/32 only (override with `SSH_CIDR`) |
| Instance: `t4g.small` | Latest Ubuntu 24.04 arm64 AMI (from SSM), 20 GB encrypted gp3, IMDSv2 required, `CpuCredits=standard` (no surprise CPU-credit charges) |
| Elastic IP | Associated with the instance |

Cloud-init on first boot:
- installs build tools, git, rsync, sqlite3, chrony and unattended-upgrades;
- adds user **`botadmin`** with your SSH key;
- adds a sudoers rule that lets `botadmin` only start, stop, restart or check `arbitragepulse@*`;
- turns off SSH password and root login;
- creates a 4 GB swapfile.

There are no secrets in user-data.

Then:
```bash
IP=<printed public IP>
ssh ubuntu@$IP cloud-init status --wait     # ~3 min the first time
ssh botadmin@$IP 'free -h; swapon --show; timedatectl | grep synchronized'
```

**STOP — operator confirms** the instance and IP (billing has started).

## Phase 2 — Measure latency from the server (read-only)

Sync the code and install Node 22 (`preconf-lag.mjs` needs Node ≥ 22). `--code-only` = rsync only, no build or restart:
```bash
ARB_SERVER=botadmin@$IP scripts/deploy.sh --code-only
ssh ubuntu@$IP 'curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs'
```

Run the probes `[server]`:
```bash
ssh botadmin@$IP
cd ~/Arbitragepulse
read -rs ALCHEMY_KEY && export ALCHEMY_KEY          # typed by the operator; not echoed
N=100 scripts/rpc-latency.sh                         # Base public, Base preconf, Alchemy
node scripts/preconf-lag.mjs 120                     # does mainnet.base.org `pending` lag the pendingLogs feed?
# Sequencer (accepts sends only): connect + TLS + round trip of a cheap request, 20×
for i in $(seq 20); do curl -so /dev/null -w '%{time_connect} %{time_appconnect} %{time_total}\n' \
  -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
  https://mainnet-sequencer.base.org; done | sort -n -k3 | sed -n '10p'   # median
unset ALCHEMY_KEY
```

Record the p50/p95 values in **Results** below. Then set, in `engine/config.yaml`:
- `http_rpc` / `preconf_http_rpc`: the fastest error-free endpoint that serves Flashblocks `pending` state;
- `submission_rpcs`: keep the sequencer plus the fastest others.

If `preconf-lag.mjs` shows `pending` lagging, see the "Flashblocks HTTP `pending` lagging" entry in `docs/review-loop/OPEN_QUESTIONS.md`.

**STOP — operator reviews the numbers.** If they're poor (e.g. p50 > 30 ms to Base endpoints), discuss another host before continuing.

## Phase 3 — Install the engine

Install Rust and Foundry `[server]`:
```bash
ssh botadmin@$IP
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup   # `cast` for checks
```

Build `[server]`. The first build takes ~10–15 min on a t4g.small; swap absorbs the memory peak:
```bash
cd ~/Arbitragepulse && ~/.cargo/bin/cargo build --release -p engine
```

**Operator only** — create the env file `[server]` (the agent waits; see `engine/base.env.example`):
```bash
cp engine/base.env.example engine/base.env && chmod 600 engine/base.env && nano engine/base.env
# PRIVATE_KEY=…  ALCHEMY_KEY=…  API_KEY=$(openssl rand -hex 32)
# CONFIG_PATH=engine/config.yaml  PORT=3000  DB_PATH=trades-base.db  DRY_RUN=true
```

Agent check (no contents printed):
```bash
stat -c '%a %U' engine/base.env                                 # → 600 botadmin
grep -c '^DRY_RUN=true$' engine/base.env                       # → 1
```

Install systemd and logrotate, then start (as `ubuntu`, which has full sudo):
```bash
ssh ubuntu@$IP 'sudo bash /home/botadmin/Arbitragepulse/scripts/server/setup.sh base'
```

Later deploys from the Mac (rsync, build on the server, restart, health check):
```bash
ARB_SERVER=botadmin@$IP scripts/deploy.sh
```

## Phase 4 — Verify in dry-run (≈ 1 day)

**Dry-run tests trades even while the executor is unfunded.** When the contract holds less
than a trade's `amount_in`, the simulation runs with an *assumed balance*: an `eth_call`
state override that sets the contract's USDC/USDT balance to `amount_in` for that one call
only. The engine finds each token's balance slot at startup and logs it, e.g.
`Dry-run assumed balance: USDC slot 9, USDT slot 0`. Results show as
`DRY-RUN ok … | assumed balance` (or `Simulation failed`), so you can judge the routes
before funding anything. Live mode never uses it: an unfunded trade is skipped, nothing is
sent. Redeploy the executor (Phase 5, step 1–2) first, otherwise Slipstream routes fail
their simulation.

Service and health:
```bash
ssh botadmin@$IP 'systemctl status arbitragepulse@base --no-pager | head -5'
ssh -N -L 3000:localhost:3000 botadmin@$IP &          # tunnel; dashboard → ws://localhost:3000
curl -s -H "Authorization: Bearer <API_KEY>" localhost:3000/health   # 200 + "status":"ok"
```

Check the heartbeat in `ssh botadmin@$IP 'journalctl -u arbitragepulse@base -n 200 | grep ♥'`:
- `v2=` / `v3=` show pool counts (fork reference: v2=30 v3=146);
- `preconf_logs=` is above 0 (Flashblocks feed live);
- `unfunded=7/7` is expected until Phase 5 funding.

Exposure: from the Mac, `nmap -Pn -p 1-65535 $IP` should show only 22 open.

Backups:
```bash
ssh botadmin@$IP 'mkdir -p ~/backups && (crontab -l 2>/dev/null; \
  echo "15 3 * * * sqlite3 ~/Arbitragepulse/trades-base.db \".backup ~/backups/trades-\$(date +\%F).db\" && find ~/backups -mtime +30 -delete") | crontab -'
```

Optionally copy them off the box: `rsync botadmin@$IP:backups/ ./server-backups/`.

Live wallet nonce check (safety rule 5) — still unchanged.

**STOP — operator reviews a day of dry-run:**
- opportunities and DRY-RUN ok lines in the dashboard;
- no restart loops (`systemctl show arbitragepulse@base -p NRestarts`);
- CPU credits not exhausted (EC2 console → Monitoring → CPUCreditBalance).

## Phase 5 — Go live (operator-driven, separate approval)

1. Redeploy the executor. The current one predates the Slipstream router type:
   ```bash
   cd contract && forge script script/Deploy.s.sol --rpc-url base --broadcast --verify
   ```
2. Allowlist the routers (`PRODUCTION_DEPLOYMENT_GUIDE.md` §3):
   - Uniswap V3 `0x2626…e481` → type 1;
   - PancakeSwap V3 `0x678A…fa86` → type 1;
   - Aerodrome `0xcF77…4E43` → type 4;
   - Slipstream `0xBE6D…18a5` → type 5.
3. Set `contract_address` in `engine/config.yaml`, then deploy with `ARB_SERVER=botadmin@$IP scripts/deploy.sh`.
4. Fund the executor with **≥ 1000 USDC and ≥ 1000 USDT**, and give the signer a little ETH for gas.
5. On the server, set `DRY_RUN=false` in `engine/base.env`, then run `sudo systemctl restart arbitragepulse@base`. The restart re-runs the owner and router checks; exit 2 means a check failed, and the engine stays stopped.
6. Watch the first trades, `arb_lost_race_total` and `arb_gas_spent_usd_total` on `/metrics`.

## Operations

| Task | Command |
|---|---|
| Update from GitHub (on the server, as botadmin) | `~/Arbitragepulse/scripts/server/update.sh`: pulls main, builds dashboard + engine, deletes `trades-base.db` and the log, restarts. `--keep-data` keeps them. A failed build leaves the running engine alone |
| Logs | `ssh botadmin@$IP 'journalctl -fu arbitragepulse@base'` |
| Restart | `ssh botadmin@$IP 'sudo systemctl restart arbitragepulse@base'` |
| Your IP changed (SSH refused) | `aws ec2 authorize-security-group-ingress --group-id <sg> --protocol tcp --port 22 --cidr $(curl -s https://checkip.amazonaws.com)/32` — then remove the old rule |
| Stop billing for compute | `aws ec2 stop-instances --instance-ids <id>`. The Elastic IP (≈ $3.7) and disk keep billing |
| Delete everything | `scripts/aws/teardown.sh --dry-run`, then `scripts/aws/teardown.sh`. This destroys the disk and `trades-base.db`; back them up and withdraw contract funds first |

## Results (fill in during Phase 2)

| Endpoint | quote @latest p50 / p95 | quote @pending p50 / p95 | Flashblocks state |
|---|---|---|---|
| https://mainnet.base.org | | | |
| https://mainnet-preconf.base.org | | | |
| Alchemy | | | |
| mainnet-sequencer.base.org (connect / total, median) | | — | — |
| preconf-lag.mjs: pre-swap results seen | | | |
