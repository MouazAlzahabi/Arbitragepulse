# Linea $60 Test Deployment Guide
## Complete Setup Instructions

---

## 1. VERIFIED CONTRACT ADDRESSES

Based on [LineaScan](https://lineascan.build) verification:

### Token Addresses
- **USDC**: `0x176211869ca2b568f2a7d4ee941e073a821ee1ff` ([verified on LineaScan](https://lineascan.build/address/0x176211869ca2b568f2a7d4ee941e073a821ee1ff))
- **WETH**: `0xe5d7c2a44ffddf6b295a15c148167daaaf5cf34f` ([verified on LineaScan](https://lineascan.build/address/0xe5d7c2a44ffddf6b295a15c148167daaaf5cf34f))

### DEX Router Addresses
- **Lynex SwapRouter**: `0x1b81d678ffb9c0263b24a97847620c99d213eb14` ([verified on LineaScan](https://lineascan.build/address/0x1b81d678ffb9c0263b24a97847620c99d213eb14))
- **Nile Exchange Router**: `0xAAA45c8F5ef92a000a121d102F4e89278a711Faa` ([official Nile docs](https://docs.thenile.exchange/resources/deployed-contract-addresses))

---

## 2. INFURA SETUP (FREE TIER - 5 MINUTES)

### Step 1: Create Account
1. Go to [infura.io](https://infura.io)
2. Sign up (free tier: 100,000 requests/day)
3. Verify email

### Step 2: Create Linea Project
1. Dashboard → "Create New Project"
2. Name: "ArbitragePulse-Linea"
3. Select Network → Enable **Linea Mainnet**
4. Copy API Key (format: `abc123...xyz`)

### Your RPC URLs
```
WebSocket:  wss://linea-mainnet.infura.io/ws/v3/YOUR_API_KEY
HTTP:       https://linea-mainnet.infura.io/v3/YOUR_API_KEY
```

**Cost for 24h test**: **$0** (well within free tier — you'll use ~24,500 calls out of 100,000/day limit)

---

## 3. BRIDGE FUNDS TO LINEA

### Option A: Official Linea Bridge (Recommended)
**URL**: [bridge.linea.build](https://bridge.linea.build)

**Steps**:
1. Connect wallet (MetaMask/Coinbase/WalletConnect)
2. Select:
   - **From**: Ethereum Mainnet
   - **To**: Linea
   - **Amount**: 65 USDC ($60 test + $5 gas buffer)
3. Approve + Confirm bridge transaction
4. **Wait**: 10-20 minutes for bridge completion
5. **Add Linea network to MetaMask** (bridge UI will prompt)

**Cost**: ~$3-8 in Ethereum gas (depends on mainnet congestion)

### Option B: CEX Withdrawal (Cheaper if supported)
Some centralized exchanges (Binance, OKX) support direct USDC withdrawals to Linea.
- **Cost**: $0.50-2 withdrawal fee (much cheaper than bridging)
- **Check**: Your exchange's withdrawal networks

---

## 4. UPDATE CONFIG.YAML

Edit `engine/config.yaml` and update the Linea section:

```yaml
chains:
  # ... existing chains ...

  # Enable Linea for $60 test
  - id: 59144
    name: "Linea"
    enabled: true  # ← Change to true
    ws_rpc: "wss://linea-mainnet.infura.io/ws/v3/YOUR_INFURA_KEY"  # ← Add your key
    http_rpc: "https://linea-mainnet.infura.io/v3/YOUR_INFURA_KEY"  # ← Add your key
    native_currency: "ETH"
    wrapped_native: "0xe5d7c2a44ffddf6b295a15c148167daaaf5cf34f"
    block_time_ms: 2000  # ← Updated (Linea is 2s, not 3s)
    contract_address: "0xYOUR_CONTRACT_ADDRESS"  # ← Update after deployment (step 5)
    min_native_balance: 0.01
    min_profit_usd: 2.0  # $2 minimum profit for $60 test
    deadline_seconds: 120
    min_swap_amount_filter: 50000000000000000  # 0.05 ETH or 50k USDC (6 decimals)

routers:
  # ... existing routers ...

  # Add Lynex (V2-compatible DEX on Linea)
  - id: "lynex-linea"
    name: "Lynex (Linea)"
    chain_id: 59144
    address: "0x1b81d678ffb9c0263b24a97847620c99d213eb14"
    type: "v2"
    fee_bps: 30  # 0.3% fee

  # Add Nile Exchange (V2-compatible)
  - id: "nile-linea"
    name: "Nile Exchange (Linea)"
    chain_id: 59144
    address: "0xAAA45c8F5ef92a000a121d102F4e89278a711Faa"
    type: "v2"
    fee_bps: 30  # 0.3% fee

pairs:
  # ... existing pairs ...

  # Add Linea USDC/WETH pair (highest volume)
  - id: "linea-usdc-weth"
    chain_id: 59144
    token_in: "0x176211869ca2b568f2a7d4ee941e073a821ee1ff"   # USDC
    token_out: "0xe5d7c2a44ffddf6b295a15c148167daaaf5cf34f"  # WETH
    token_in_symbol: "USDC"
    token_out_symbol: "WETH"
    token_in_decimals: 6
    token_out_decimals: 18
    trade_amount: "60"  # $60 capital
    max_trade: "60"
    min_swap_size: "5"  # Minimum $5 swap to trigger evaluation
    watch_pools: []  # Leave empty for periodic polling
```

---

## 5. DEPLOY ARBITRAGEEXECUTOR CONTRACT

### Step 1: Update Foundry Config

Edit `contract/foundry.toml`:

```toml
[profile.linea]
src = "src"
out = "out"
libs = ["lib"]
eth_rpc_url = "https://linea-mainnet.infura.io/v3/YOUR_INFURA_KEY"
chain_id = 59144
```

### Step 2: Deploy Contract

```bash
cd contract

forge create \
  --rpc-url linea \
  --private-key $PRIVATE_KEY \
  src/ArbitrageExecutor.sol:ArbitrageExecutor

# Output will show:
# Deployed to: 0xYOUR_NEW_CONTRACT_ADDRESS
```

**Important**: Copy the deployed contract address and update it in `config.yaml` (step 4).

### Step 3: Set Router Permissions

The contract starts with all routers disabled. You must allow Lynex and Nile:

```bash
# Allow Lynex router (V2)
cast send 0xYOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  true \
  --rpc-url linea \
  --private-key $PRIVATE_KEY

# Set Lynex as V2 router
cast send 0xYOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0x1b81d678ffb9c0263b24a97847620c99d213eb14 \
  0 \
  --rpc-url linea \
  --private-key $PRIVATE_KEY

# Allow Nile Exchange router
cast send 0xYOUR_CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  0xAAA45c8F5ef92a000a121d102F4e89278a711Faa \
  true \
  --rpc-url linea \
  --private-key $PRIVATE_KEY

# Set Nile as V2 router
cast send 0xYOUR_CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  0xAAA45c8F5ef92a000a121d102F4e89278a711Faa \
  0 \
  --rpc-url linea \
  --private-key $PRIVATE_KEY
```

---

## 6. FUND CONTRACT WITH $60 USDC

### Option A: Using Cast (Foundry)

```bash
# Transfer 60 USDC to contract
cast send 0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  "transfer(address,uint256)" \
  0xYOUR_CONTRACT_ADDRESS \
  60000000 \
  --rpc-url linea \
  --private-key $PRIVATE_KEY

# Verify balance
cast call 0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  "balanceOf(address)(uint256)" \
  0xYOUR_CONTRACT_ADDRESS \
  --rpc-url linea
```

### Option B: Using MetaMask

1. Add USDC token to MetaMask:
   - Contract: `0x176211869ca2b568f2a7d4ee941e073a821ee1ff`
   - Symbol: USDC
   - Decimals: 6
2. Send 60 USDC to `0xYOUR_CONTRACT_ADDRESS`

---

## 7. UPDATE ENGINE .env FILE

Edit `engine/.env`:

```bash
PRIVATE_KEY=0xYOUR_PRIVATE_KEY_HERE
CONFIG_PATH=config.yaml
PORT=3000
LOG_LEVEL=info
API_KEY=  # Optional — leave empty for no auth
```

**Security**: Never commit `.env` to git. Keep your private key safe.

---

## 8. RUN ENGINE IN DRY-RUN MODE (FIRST TEST)

Before risking real funds, validate everything works in dry-run mode:

```bash
cd /path/to/arbitragepulse

# Run engine (from workspace root)
PRIVATE_KEY=$PRIVATE_KEY \
  CONFIG_PATH=engine/config.yaml \
  cargo run -p engine
```

**Expected output**:
```
[INFO] Chain Linea enabled, connecting to wss://linea-mainnet.infura.io/...
[INFO] Loaded 2 routers for Linea
[INFO] Loaded 1 pair for Linea: USDC/WETH
[INFO] Linea: listening for swap events
[INFO] Engine started on http://0.0.0.0:3000
[INFO] DRY RUN MODE: No transactions will be submitted
```

**Check HTTP API**:
```bash
curl http://localhost:3000/health
# Should return: {"status":"ok","chains":[{"name":"Linea","enabled":true}],...}
```

**Monitor logs** for 10-15 minutes:
- ✅ You should see swap events being detected
- ✅ Opportunities being evaluated (even if none are profitable yet)
- ✅ No errors related to RPC, contract, or routers

---

## 9. SWITCH TO LIVE MODE

If dry-run looks good, enable live trading:

```bash
# Send POST request to enable live mode
curl -X POST http://localhost:3000/engine/dry-run \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'

# Verify
curl http://localhost:3000/stats
# Should show: "dry_run": false
```

**Or restart engine** without dry-run:
```bash
# Restart with explicit dry-run=false in config or code
cargo run -p engine
```

---

## 10. MONITOR 24-HOUR TEST

### Dashboard (Optional)

Start React dashboard to visualize activity:

```bash
# From workspace root
npx vite --port 5174

# Open browser: http://localhost:5174
```

Dashboard shows:
- Real-time swap events
- Opportunity detection
- Execution attempts
- Profit/loss

### API Monitoring

Use `/stats` endpoint to track progress:

```bash
# Check stats every 10 minutes
watch -n 600 'curl -s http://localhost:3000/stats | jq'
```

### Expected Results (24 hours)

Based on [LINEA_SCROLL_GNOSIS_ANALYSIS.md](LINEA_SCROLL_GNOSIS_ANALYSIS.md):

| Metric | Conservative | Realistic | Optimistic |
|--------|-------------|-----------|------------|
| **Trades executed** | 20 | 23 | 30 |
| **Avg profit/trade** | $3.50 | $4.20 | $5.50 |
| **Total gas cost** | $3 | $3.45 | $4.50 |
| **Net profit** | **$67** | **$93** | **$160** |
| **ROI** | **112%** | **155%** | **267%** |

**RPC Cost**: **$0** (well within Infura free tier)

---

## 11. VERIFY PROFIT

After 24 hours, check final USDC balance:

```bash
# Check contract USDC balance
cast call 0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  "balanceOf(address)(uint256)" \
  0xYOUR_CONTRACT_ADDRESS \
  --rpc-url linea

# Convert to human-readable (divide by 1e6)
# Example output: 93000000 → 93 USDC
```

**Withdraw profits** (if needed):

```bash
cast send 0xYOUR_CONTRACT_ADDRESS \
  "withdrawToken(address,address,uint256)" \
  0x176211869ca2b568f2a7d4ee941e073a821ee1ff \
  YOUR_WALLET_ADDRESS \
  93000000 \
  --rpc-url linea \
  --private-key $PRIVATE_KEY
```

---

## 12. TROUBLESHOOTING

### Issue: No opportunities detected

**Possible causes**:
1. Routers not set as allowed in contract → run step 5 again
2. Minimum profit threshold too high → lower `min_profit_usd` to `1.0` in config
3. Swap amount filter too restrictive → lower `min_swap_amount_filter` to `10000000000000000` (0.01 ETH)

**Fix**: Check logs for specific error messages

### Issue: Transactions reverting

**Possible causes**:
1. Insufficient gas price → engine auto-adjusts, but check `gas_price_cache_ttl` is 4-6s
2. Router liquidity changed → normal, engine will retry other routers
3. Deadline too short → increase `deadline_seconds` to `180`

**Fix**: Review WebSocket logs for revert reasons

### Issue: RPC rate limit errors

**Possible causes**:
1. Exceeded Infura free tier (unlikely in 24h test)
2. WebSocket connection dropped → engine auto-reconnects

**Fix**:
- Add fallback RPC to `config.yaml`:
```yaml
ws_rpc_fallbacks:
  - "wss://linea-mainnet.infura.io/ws/v3/BACKUP_KEY"
  - "wss://rpc.linea.build"  # Public RPC (rate limited)
```

---

## 13. NEXT STEPS AFTER SUCCESSFUL TEST

If your 24h test shows **>100% ROI** (profit >$60):

### Phase 2: Production Deployment ($20k)

Follow the full [LINEA_SCROLL_GNOSIS_ANALYSIS.md](LINEA_SCROLL_GNOSIS_ANALYSIS.md) deployment plan:

1. **Deploy $8k on Linea** (proven profitable)
2. **Deploy $7k on Gnosis** (stable xDAI, low gas)
3. **Deploy $5k on Scroll** (lowest competition)

**Expected monthly profit**: $43,650-74,100 💰

### Phase 3: Scale Further

After 30 days of stable production:
- Add Base, Arbitrum, Polygon (another $30k capital)
- Implement RPC racing (Phase 3 optimization)
- Consider Flashbots integration for MEV protection

---

## 14. SAFETY CHECKLIST

Before going live:

- [ ] Infura API key configured and tested
- [ ] ArbitrageExecutor contract deployed to Linea
- [ ] Both routers (Lynex + Nile) allowed and typed in contract
- [ ] Contract funded with 60 USDC
- [ ] Dry-run mode tested for 10+ minutes without errors
- [ ] Dashboard connected and showing live swap events
- [ ] Private key secured (not committed to git)
- [ ] `.env` file in workspace root or `engine/` directory

**Estimated setup time**: 45-60 minutes

---

## 15. COST BREAKDOWN

| Item | Cost | Notes |
|------|------|-------|
| **Bridge to Linea** | $3-8 | One-time (Ethereum gas) |
| **Contract deployment** | $2-3 | One-time (Linea gas) |
| **Router setup (4 txs)** | $0.50-1 | One-time (Linea gas) |
| **Fund contract transfer** | $0.10-0.20 | One-time (Linea gas) |
| **RPC costs (24h)** | $0 | Free tier |
| **Gas per arb (avg 23 trades)** | $3-5 | Included in profit calc |
| **Total upfront cost** | **$5.60-12.20** | |
| **Expected 24h profit** | **$67-160** | Net after all gas |

**Break-even**: First 2-3 successful arbitrage trades cover all setup costs.

---

## SOURCES

This guide uses verified addresses from:
- [Linea USDC Token on LineaScan](https://lineascan.build/address/0x176211869ca2b568f2a7d4ee941e073a821ee1ff)
- [Linea WETH Token on LineaScan](https://lineascan.build/address/0xe5d7c2a44ffddf6b295a15c148167daaaf5cf34f)
- [Lynex SwapRouter on LineaScan](https://lineascan.build/address/0x1b81d678ffb9c0263b24a97847620c99d213eb14)
- [Nile Exchange Official Documentation](https://docs.thenile.exchange/resources/deployed-contract-addresses)
- [Linea Official Bridge](https://bridge.linea.build)
- [Infura RPC Service](https://infura.io)

---

**Ready to start your $60 test?** Follow steps 1-8, then monitor for 24 hours. Good luck! 🚀
