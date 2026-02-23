# ArbitragePulse - Quick Command Reference
## Copy-Paste These Commands

---

## 🔧 INITIAL SETUP (On Hetzner Server)

```bash
# Update system
apt update && apt upgrade -y

# Install dependencies
apt install -y build-essential git curl

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Install Foundry
curl -L https://foundry.paradigm.xyz | bash
foundryup

# Clone your repo (or upload via SCP)
git clone https://github.com/YOUR_USERNAME/arbitragepulse.git
cd arbitragepulse

# Build engine
cargo build --release -p engine
```

---

## 💎 CONTRACT DEPLOYMENT

### Linea
```bash
cd contract

# Set environment
export LINEA_RPC="https://linea-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY"
export PRIVATE_KEY="0xYOUR_PRIVATE_KEY"

# Deploy contract
forge create \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  src/ArbitrageExecutor.sol:ArbitrageExecutor

# Save the deployed address → update config.yaml

# Allow Lynex router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  true \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY

# Set Lynex as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  0 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY

# Allow Nile router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0xAAA45c8F5ef92a000a121d102F4e89278a711Faa \
  true \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY

# Set Nile as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0xAAA45c8F5ef92a000a121d102F4e89278a711Faa \
  0 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY

# Fund contract with USDC
cast send 0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  "transfer(address,uint256)" \
  YOUR_CONTRACT_ADDRESS \
  60000000 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY
```

### Scroll
```bash
export SCROLL_RPC="https://scroll-mainnet.g.alchemy.com/v2/YOUR_ALCHEMY_KEY"

# Deploy
forge create \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY \
  src/ArbitrageExecutor.sol:ArbitrageExecutor

# Allow Zebra router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x0122960d6e391478bfe8fb2408ba412d5600f621 \
  true \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY

# Set Zebra as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x0122960d6e391478bfe8fb2408ba412d5600f621 \
  0 \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY

# Allow SyncSwap router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x80e38291e06339d10aab483c65695d004dbd5c69 \
  true \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY

# Set SyncSwap as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x80e38291e06339d10aab483c65695d004dbd5c69 \
  0 \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY

# Fund contract
cast send 0x06efdbff2a14a7c8e15944d1f4a48f9f95f663a4 \
  "transfer(address,uint256)" \
  YOUR_CONTRACT_ADDRESS \
  100000000 \
  --rpc-url $SCROLL_RPC \
  --private-key $PRIVATE_KEY
```

### Gnosis
```bash
export GNOSIS_RPC="https://rpc.gnosischain.com"

# Deploy
forge create \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY \
  src/ArbitrageExecutor.sol:ArbitrageExecutor

# Allow Honeyswap router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x1C232F01118CB8B424793ae03F870aa7D0ac7f77 \
  true \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY

# Set Honeyswap as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x1C232F01118CB8B424793ae03F870aa7D0ac7f77 \
  0 \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY

# Allow SushiSwap router
cast send YOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506 \
  true \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY

# Set SushiSwap as V2
cast send YOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506 \
  0 \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY

# Fund contract with USDC
cast send 0xDDAfbb505ad214D7b80b1f830fcCc89B60fb7A83 \
  "transfer(address,uint256)" \
  YOUR_CONTRACT_ADDRESS \
  100000000 \
  --rpc-url $GNOSIS_RPC \
  --private-key $PRIVATE_KEY
```

---

## 🚀 RUN ENGINE

### Manual Start
```bash
cd /root/arbitragepulse

# Create .env file
cat > engine/.env <<EOF
PRIVATE_KEY=0xYOUR_PRIVATE_KEY
CONFIG_PATH=config.yaml
PORT=3000
LOG_LEVEL=info
EOF

# Run engine
./target/release/engine
```

### Systemd Service (Auto-Restart)
```bash
# Create service file
cat > /etc/systemd/system/arbitragepulse.service <<EOF
[Unit]
Description=ArbitragePulse Multi-Chain Engine
After=network.target

[Service]
Type=simple
User=root
WorkingDirectory=/root/arbitragepulse
Environment="PRIVATE_KEY=0xYOUR_KEY"
Environment="CONFIG_PATH=/root/arbitragepulse/engine/config.yaml"
Environment="LOG_LEVEL=info"
Environment="PORT=3000"
ExecStart=/root/arbitragepulse/target/release/engine
Restart=always
RestartSec=10
StandardOutput=append:/var/log/arbitragepulse/engine.log
StandardError=append:/var/log/arbitragepulse/engine.log

[Install]
WantedBy=multi-user.target
EOF

# Create log directory
mkdir -p /var/log/arbitragepulse

# Enable and start service
systemctl daemon-reload
systemctl enable arbitragepulse
systemctl start arbitragepulse

# Check status
systemctl status arbitragepulse

# View logs
tail -f /var/log/arbitragepulse/engine.log
```

---

## 📊 MONITORING

### Check Engine Status
```bash
# Health check
curl http://localhost:3000/health

# Stats (JSON)
curl http://localhost:3000/stats | jq

# Trades history
curl http://localhost:3000/trades?limit=50 | jq
```

### Control Engine
```bash
# Pause trading (all chains)
curl -X POST http://localhost:3000/engine/pause \
  -H "Content-Type: application/json"

# Resume trading
curl -X POST http://localhost:3000/engine/resume \
  -H "Content-Type: application/json"

# Enable live trading (disable dry-run)
curl -X POST http://localhost:3000/engine/dry-run \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Enable dry-run (safe mode)
curl -X POST http://localhost:3000/engine/dry-run \
  -H "Content-Type: application/json" \
  -d '{"enabled": true}'
```

### Access Dashboard from Local Machine
```bash
# SSH tunnel (maps server:3000 to localhost:3000)
ssh -L 3000:localhost:3000 root@YOUR_SERVER_IP

# In another terminal, start dashboard
cd /path/to/arbitragepulse
npx vite --port 5174

# Open browser: http://localhost:5174
```

---

## 🔍 DEBUGGING

### Check Contract Balance
```bash
# Linea USDC balance
cast call 0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  "balanceOf(address)(uint256)" \
  YOUR_CONTRACT_ADDRESS \
  --rpc-url $LINEA_RPC

# Linea ETH balance (for gas)
cast balance YOUR_CONTRACT_ADDRESS --rpc-url $LINEA_RPC
```

### Verify Router Permissions
```bash
# Check if router is allowed
cast call YOUR_CONTRACT_ADDRESS \
  "allowedRouters(address)(bool)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  --rpc-url $LINEA_RPC

# Check router type (0=V2, 1=V3)
cast call YOUR_CONTRACT_ADDRESS \
  "routerType(address)(uint8)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  --rpc-url $LINEA_RPC
```

### Test RPC Connection
```bash
# Test Alchemy Linea RPC
curl https://linea-mainnet.g.alchemy.com/v2/YOUR_KEY \
  -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'

# Should return: {"jsonrpc":"2.0","id":1,"result":"0x..."}
```

### Withdraw Profits
```bash
# Withdraw USDC from contract
cast send YOUR_CONTRACT_ADDRESS \
  "withdrawToken(address,address,uint256)" \
  0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  YOUR_WALLET_ADDRESS \
  50000000 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY

# Withdraw ETH from contract
cast send YOUR_CONTRACT_ADDRESS \
  "withdraw(address,uint256)" \
  YOUR_WALLET_ADDRESS \
  100000000000000000 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY
```

---

## 🛑 EMERGENCY COMMANDS

### Stop Engine
```bash
systemctl stop arbitragepulse
```

### Pause All Trading
```bash
curl -X POST http://localhost:3000/engine/pause
```

### Kill All Processes
```bash
pkill -f "target/release/engine"
```

### Clear Logs
```bash
rm /var/log/arbitragepulse/engine.log
systemctl restart arbitragepulse
```

---

## 📈 PERFORMANCE TUNING

### Lower Profit Threshold (More Opportunities)
```yaml
# In config.yaml:
min_profit_usd: 0.50  # was 1.50 or 2.0
```

### Increase Profit Threshold (Better Margins)
```yaml
min_profit_usd: 3.0  # only execute if profit > $3
```

### Adjust Dust Filter
```yaml
# Lower = more opportunities (but more noise)
min_swap_amount_filter: 5000000000000000  # 0.005 ETH

# Higher = fewer opportunities (but higher quality)
min_swap_amount_filter: 100000000000000000  # 0.1 ETH
```

### Increase Trade Size
```yaml
# In pairs section:
trade_amount: "500"  # was 100
max_trade: "1000"    # was 500
```

---

## 🔄 UPDATE & REBUILD

```bash
cd /root/arbitragepulse

# Pull latest code
git pull

# Rebuild engine
cargo build --release -p engine

# Restart service
systemctl restart arbitragepulse
```

---

## 📦 BACKUP

```bash
# Backup config
cp engine/config.yaml /root/backup/config.yaml.$(date +%Y%m%d)

# Backup logs
tar -czf /root/backup/logs-$(date +%Y%m%d).tar.gz /var/log/arbitragepulse/

# Backup private key (encrypted)
echo "PRIVATE_KEY=0xYOUR_KEY" | gpg -c > /root/backup/key.gpg
```

---

**Pro Tip**: Create a shell script with your most-used commands:

```bash
cat > /root/arb.sh <<'EOF'
#!/bin/bash
case $1 in
  status)   systemctl status arbitragepulse ;;
  logs)     tail -f /var/log/arbitragepulse/engine.log ;;
  restart)  systemctl restart arbitragepulse ;;
  stop)     systemctl stop arbitragepulse ;;
  start)    systemctl start arbitragepulse ;;
  stats)    curl -s http://localhost:3000/stats | jq ;;
  pause)    curl -X POST http://localhost:3000/engine/pause ;;
  resume)   curl -X POST http://localhost:3000/engine/resume ;;
  live)     curl -X POST http://localhost:3000/engine/dry-run -H "Content-Type: application/json" -d '{"enabled": false}' ;;
  dry)      curl -X POST http://localhost:3000/engine/dry-run -H "Content-Type: application/json" -d '{"enabled": true}' ;;
  *)        echo "Usage: arb.sh {status|logs|restart|stop|start|stats|pause|resume|live|dry}" ;;
esac
EOF

chmod +x /root/arb.sh

# Now you can use:
./arb.sh logs      # View logs
./arb.sh stats     # Get stats
./arb.sh pause     # Pause trading
./arb.sh live      # Enable live mode
```
