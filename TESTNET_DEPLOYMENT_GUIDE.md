# ArbitragePulse - Testnet Deployment Guide

## 🧪 TESTNET-FIRST APPROACH

**Testing on testnets first eliminates ALL financial risk and validates your entire system before deploying to mainnet.**

**Benefits**:
- ✅ No real money at risk
- ✅ Free testnet tokens from faucets
- ✅ Validate contract deployment
- ✅ Test router permissions
- ✅ Verify engine functionality
- ✅ Debug issues without cost
- ✅ Practice full deployment workflow

---

## 📋 TESTNET OVERVIEW

| Testnet | Chain ID | Block Time | Faucet | DEX Availability |
|---------|----------|------------|--------|------------------|
| **Linea Sepolia** | 59141 | 2s | [Link](https://faucet.sepolia.linea.build) | ❌ Need MockRouters |
| **Scroll Sepolia** | 534351 | 3s | [Link](https://sepolia.scroll.io/faucet) | ❌ Need MockRouters |
| **Optimism Sepolia** | 11155420 | 2s | [Link](https://app.optimism.io/faucet) | ✅ Uniswap V3 |
| **Arbitrum Sepolia** | 421614 | 250ms | [Link](https://faucet.quicknode.com/arbitrum/sepolia) | ✅ Uniswap V3 |
| **Polygon Amoy** | 80002 | 2s | [Link](https://faucet.polygon.technology) | ❌ Need MockRouters |
| **Gnosis Chiado** | 10200 | 5s | [Link](https://gnosisfaucet.com) | ❌ Need MockRouters |

**Recommendation**: Start with **Optimism Sepolia** or **Arbitrum Sepolia** (both have Uniswap V3 deployed).

---

## 🚀 QUICK START (Optimism Sepolia)

### 1. Get Testnet ETH (5 min)

```bash
# Visit Optimism Sepolia faucet
# https://app.optimism.io/faucet
# Connect wallet → Request testnet ETH
# Wait 1-2 min for confirmation
```

### 2. Deploy ArbitrageExecutor (2 min)

```bash
cd contract

export TESTNET_RPC="https://sepolia.optimism.io"
export PRIVATE_KEY="0xYOUR_TESTNET_PRIVATE_KEY"

forge create \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY \
  src/ArbitrageExecutor.sol:ArbitrageExecutor

# OUTPUT EXAMPLE:
# Deployed to: 0x1234567890abcdef1234567890abcdef12345678
# ↑ SAVE THIS ADDRESS
```

### 3. Update config-testnet.yaml (1 min)

```bash
cd ../engine
nano config-testnet.yaml

# Line 23: Update contract address for OP Sepolia
contract_address: "0x1234567890abcdef1234567890abcdef12345678"
```

### 4. Set Router Permissions (2 min)

```bash
CONTRACT="0x1234567890abcdef1234567890abcdef12345678"  # Your deployed address
UNISWAP_V3="0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4"  # Uniswap V3 on OP Sepolia

# Allow Uniswap V3 router
cast send $CONTRACT \
  "setAllowedRouter(address,bool)" \
  $UNISWAP_V3 \
  true \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY

# Set router type to V3 (1 = V3)
cast send $CONTRACT \
  "setRouterType(address,uint8)" \
  $UNISWAP_V3 \
  1 \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY
```

### 5. Get Testnet USDC (3 min)

```bash
# OP Sepolia USDC faucet (if available) or:
# Deploy MockERC20 as USDC:

forge create \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY \
  lib/openzeppelin-contracts/contracts/token/ERC20/presets/ERC20PresetMinterPauser.sol:ERC20PresetMinterPauser \
  --constructor-args "Mock USDC" "USDC"

# SAVE USDC ADDRESS → Update config-testnet.yaml pairs

# Mint 10,000 USDC to your wallet
cast send $USDC_ADDRESS \
  "mint(address,uint256)" \
  $YOUR_WALLET \
  10000000000 \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY

# Transfer 1,000 USDC to contract
cast send $USDC_ADDRESS \
  "transfer(address,uint256)" \
  $CONTRACT \
  1000000000 \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY
```

### 6. Run Engine (2 min)

```bash
cd /path/to/arbitragepulse

# Build
cargo build --release -p engine

# Run with testnet config
PRIVATE_KEY=$PRIVATE_KEY \
  CONFIG_PATH=/path/to/engine/config-testnet.yaml \
  LOG_LEVEL=debug \
  ./target/release/engine
```

### 7. Monitor Dashboard (1 min)

```bash
# In another terminal
npx vite --port 5174

# Open http://localhost:5174
# Dashboard connects to engine at http://localhost:3000
```

---

## 📦 DEPLOYING MOCKROUTERS (For chains without DEXes)

For testnets like Linea Sepolia, Scroll Sepolia, Polygon Amoy, and Gnosis Chiado that don't have DEX deployments, you can deploy MockRouters to simulate arbitrage opportunities.

### Step 1: Create MockRouter Deployment Script

Create `lab/src/testnet_setup.rs`:

```rust
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use std::str::FromStr;

// Copy MockRouterV2 deployment code from lab/src/setup.rs
// Modify to accept RPC URL as parameter

pub async fn deploy_testnet_mocks(
    rpc_url: &str,
    private_key: &str,
) -> Result<(Address, Address)> {
    // 1. Connect to testnet
    let signer = PrivateKeySigner::from_str(private_key)?;
    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http(rpc_url.parse()?);

    // 2. Deploy MockERC20 tokens (USDC, WETH)
    let usdc = deploy_mock_token(&provider, "Mock USDC", "USDC", 6).await?;
    let weth = deploy_mock_token(&provider, "Mock WETH", "WETH", 18).await?;

    // 3. Deploy MockRouterV2 A
    let router_a = deploy_mock_router(&provider).await?;

    // 4. Deploy MockRouterV2 B
    let router_b = deploy_mock_router(&provider).await?;

    // 5. Seed routers with liquidity
    // RouterA: 1 WETH = 2222 USDC (cheap)
    // RouterB: 1 WETH = 2500 USDC (market)
    seed_router(&provider, router_a, usdc, weth, ...).await?;
    seed_router(&provider, router_b, usdc, weth, ...).await?;

    Ok((router_a, router_b))
}
```

### Step 2: Deploy to Testnet

```bash
# Add to lab/src/main.rs:
# cargo run -p lab -- testnet-setup --rpc <RPC_URL>

cargo run -p lab -- testnet-setup \
  --rpc https://rpc.sepolia.linea.build

# Outputs:
# MockRouter A: 0xabc...
# MockRouter B: 0xdef...
# USDC: 0x123...
# WETH: 0x456...
```

### Step 3: Update config-testnet.yaml

```yaml
routers:
  - id: "mock-router-a-linea-sepolia"
    name: "MockRouter A (Linea Sepolia)"
    chain_id: 59141
    address: "0xabc..."  # From deployment
    type: "v2"
    fee_bps: 0

  - id: "mock-router-b-linea-sepolia"
    name: "MockRouter B (Linea Sepolia)"
    chain_id: 59141
    address: "0xdef..."  # From deployment
    type: "v2"
    fee_bps: 0

pairs:
  - id: "linea-sepolia-usdc-weth"
    chain_id: 59141
    token_in: "0x123..."  # Mock USDC
    token_out: "0x456..."  # Mock WETH
    # ...
```

---

## 🧪 TESTNET DEPLOYMENT WORKFLOW

### Phase 1: Single Testnet Validation (Day 1)
**Goal**: Prove the system works end-to-end

1. ✅ Deploy to **Optimism Sepolia** (easiest - Uniswap V3 available)
2. ✅ Run in dry-run mode for 2-4 hours
3. ✅ Verify opportunities detected in logs
4. ✅ Enable live mode
5. ✅ Execute 5-10 test arbitrages
6. ✅ Verify all succeeded (or failed gracefully)

**Success criteria**: At least 1 profitable arbitrage executed on testnet

---

### Phase 2: Multi-Testnet Validation (Day 2-3)
**Goal**: Test multi-chain coordination

1. ✅ Deploy to **Arbitrum Sepolia**
2. ✅ Deploy MockRouters to **Scroll Sepolia**
3. ✅ Run all 3 testnets in parallel
4. ✅ Monitor for cross-chain conflicts
5. ✅ Verify each chain operates independently

**Success criteria**: All 3 chains running simultaneously without errors

---

### Phase 3: Stress Testing (Day 4-5)
**Goal**: Validate edge cases and error handling

1. ✅ Deplete contract balance (verify "insufficient funds" error)
2. ✅ Pause engine (verify execution stops)
3. ✅ Resume engine (verify execution resumes)
4. ✅ Kill engine mid-trade (verify restart recovery)
5. ✅ Deploy new contract (verify migration works)
6. ✅ Revoke router permissions (verify "router not allowed" error)

**Success criteria**: All error scenarios handled gracefully

---

### Phase 4: Mainnet Migration (Day 6)
**Goal**: Deploy to production with confidence

1. ✅ Get Alchemy API key for mainnet
2. ✅ Deploy contracts to Linea/Scroll/Gnosis mainnet
3. ✅ Start with **$100-500 capital per chain**
4. ✅ Run in dry-run mode for 24h
5. ✅ Enable live mode
6. ✅ Monitor for 48h before scaling

**Success criteria**: Positive ROI on mainnet after 48h

---

## 📊 TESTNET VS MAINNET DIFFERENCES

| Aspect | Testnet | Mainnet |
|--------|---------|---------|
| **Capital** | Free (faucets) | Real money |
| **Gas Costs** | Free testnet ETH | Real ETH ($0.01-1 per tx) |
| **Liquidity** | Low/zero (MockRouters) | High (real DEXes) |
| **Spreads** | Artificial (12.5% mock) | Real (0.05-0.5%) |
| **Competition** | None (no bots) | High (many bots) |
| **ROI** | Not applicable | 5-15% daily realistic |
| **Risk** | Zero | Contract bugs, gas costs |

**Key insight**: Testnet validates your **system**, not your **profit potential**. Mainnet liquidity and competition are vastly different.

---

## 🐛 COMMON TESTNET ISSUES

### Issue 1: "Insufficient funds for gas"
**Solution**: Get more testnet ETH from faucet

```bash
# Most testnets allow 1 request per 24h
# Use multiple wallets if needed
```

### Issue 2: "No opportunities detected after 1 hour"
**Cause**: Testnet DEX pools have zero liquidity

**Solution A**: Deploy MockRouters with artificial spread
**Solution B**: Switch to OP/Arb Sepolia (Uniswap V3 available)

### Issue 3: "Transaction reverted: Router not allowed"
**Cause**: Forgot to set router permissions

**Solution**:
```bash
cast send $CONTRACT \
  "setAllowedRouter(address,bool)" \
  $ROUTER \
  true \
  --rpc-url $TESTNET_RPC \
  --private-key $PRIVATE_KEY
```

### Issue 4: "Uniswap V3 pool doesn't exist"
**Cause**: Testnet pools are sparse

**Solution**: Check pool existence first:
```bash
# Query Uniswap V3 Factory for pool address
cast call 0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24 \
  "getPool(address,address,uint24)(address)" \
  $TOKEN_A \
  $TOKEN_B \
  3000 \
  --rpc-url $TESTNET_RPC

# If returns 0x0000...0000 → pool doesn't exist
# Create pool or use different tokens
```

### Issue 5: "WebSocket connection failed"
**Cause**: Some testnets don't have reliable WS endpoints

**Solution**: Use HTTP polling instead:
```yaml
# In config-testnet.yaml
ws_rpc: ""  # Leave empty
http_rpc: "https://sepolia.optimism.io"  # Engine will poll
```

---

## ✅ TESTNET DEPLOYMENT CHECKLIST

### Pre-Deployment
- [ ] Get testnet ETH from faucet (0.1+ ETH)
- [ ] Get testnet private key (NEVER use mainnet key!)
- [ ] Install Foundry: `foundryup`
- [ ] Install Rust: `rustup`
- [ ] Build engine: `cargo build --release -p engine`

### Contract Deployment (Per Testnet)
- [ ] Deploy ArbitrageExecutor
- [ ] Set router permissions (2 cast commands per router)
- [ ] Fund contract with testnet tokens
- [ ] Verify contract has gas (0.01+ ETH)

### Engine Configuration
- [ ] Update `config-testnet.yaml` with contract address
- [ ] Update router addresses (if using MockRouters)
- [ ] Update token addresses
- [ ] Enable ONLY 1 chain initially

### Testing
- [ ] Run engine in dry-run mode (2+ hours)
- [ ] Check dashboard shows chain stats
- [ ] Verify opportunities detected in logs
- [ ] Enable live mode
- [ ] Execute 5+ test arbitrages
- [ ] Verify transactions on block explorer

### Multi-Chain
- [ ] Enable 2nd testnet
- [ ] Run both chains in parallel (2+ hours)
- [ ] Verify no cross-chain conflicts
- [ ] Enable 3rd testnet
- [ ] Run all 3 chains (4+ hours)

### Mainnet Migration
- [ ] All testnet tests passed
- [ ] No errors in 24h testnet run
- [ ] Dashboard works correctly
- [ ] Error handling validated
- [ ] Ready to deploy to mainnet with $100-500

---

## 🎯 SUCCESS CRITERIA

Before moving to mainnet, ensure:

1. ✅ **Contract Deployment**: Successfully deployed to 3+ testnets
2. ✅ **Opportunity Detection**: Engine detects arbitrage opportunities (even if rare on testnet)
3. ✅ **Execution**: At least 5 successful arbitrage executions (even with MockRouters)
4. ✅ **Multi-Chain**: Engine runs 3+ chains simultaneously without conflicts
5. ✅ **Error Handling**: All error scenarios tested and handled gracefully
6. ✅ **Dashboard**: Real-time updates working correctly
7. ✅ **Uptime**: Engine runs 24h+ without crashes
8. ✅ **Logs**: All log levels (info/warn/error) working correctly

**If all 8 criteria met → READY FOR MAINNET**

---

## 📚 NEXT STEPS

After testnet validation:

1. **Mainnet Deployment**
   - Follow [PRODUCTION_READINESS.md](./PRODUCTION_READINESS.md)
   - Start with $100-500 per chain
   - Run dry-run for 24h before going live

2. **Scaling**
   - Increase capital to $1k-5k after 1 week
   - Add more chains (Base, Polygon, etc.)
   - Optimize profit thresholds

3. **Monitoring**
   - Track daily ROI, success rate, gas costs
   - Set up alerts for errors
   - Review logs weekly

---

## 🔗 RESOURCES

### Testnet Faucets
- Linea Sepolia: https://faucet.sepolia.linea.build
- Scroll Sepolia: https://sepolia.scroll.io/faucet
- Optimism Sepolia: https://app.optimism.io/faucet
- Arbitrum Sepolia: https://faucet.quicknode.com/arbitrum/sepolia
- Polygon Amoy: https://faucet.polygon.technology
- Gnosis Chiado: https://gnosisfaucet.com

### Block Explorers
- Linea Sepolia: https://sepolia.lineascan.build
- Scroll Sepolia: https://sepolia.scrollscan.com
- Optimism Sepolia: https://sepolia-optimism.etherscan.io
- Arbitrum Sepolia: https://sepolia.arbiscan.io
- Polygon Amoy: https://amoy.polygonscan.com
- Gnosis Chiado: https://gnosis-chiado.blockscout.com

### Uniswap V3 Deployments
- OP Sepolia SwapRouter: `0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4`
- OP Sepolia QuoterV2: `0xC5290058841028F1614F3A6F0F5816cAd0df5E27`
- Arb Sepolia SwapRouter: `0x101F443B4d1b059569D643917553c771E1b9663E`
- Arb Sepolia QuoterV2: `0x2779a0CC1c3e0E44D2542EC3e79e3864Ae93Ef0B`

---

**Testing on testnets FIRST is the most important step. It costs you nothing but saves you from potentially expensive mainnet mistakes.** 🚀
