#!/bin/bash
# ArbitragePulse - Linea Mainnet Deployment Script

set -e  # Exit on any error

echo "🚀 ArbitragePulse - Linea Mainnet Deployment"
echo "============================================="
echo ""

# Load .env if present
if [ -f "engine/.env" ]; then
    set -a; source engine/.env; set +a
fi

# Check environment variables
if [ -z "$PRIVATE_KEY" ]; then
    echo "❌ Error: PRIVATE_KEY not set"
    echo "Add it to engine/.env or run: export PRIVATE_KEY=0xYOUR_PRIVATE_KEY"
    exit 1
fi

if [ -z "$ALCHEMY_KEY" ]; then
    echo "❌ Error: ALCHEMY_KEY not set"
    echo "Add it to engine/.env or run: export ALCHEMY_KEY=your_key"
    exit 1
fi

# Configuration
LINEA_RPC="https://linea-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}"
USDC_ADDRESS="0x176211869ca2b568f2a7d4ee941e073a821ee1ff"
LYNEX_ROUTER="0x1b81d678ffb9c0263b24a97847620c99d213eb14"
NILE_ROUTER="0xAAA45c8F5ef92a000a121d102F4e89278a711Faa"

echo "📋 Configuration:"
echo "   RPC: $LINEA_RPC"
echo "   USDC: $USDC_ADDRESS"
echo ""

# Step 1: Deploy ArbitrageExecutor
echo "📦 Step 1/5: Deploying ArbitrageExecutor contract..."
cd contract

DEPLOY_OUTPUT=$(forge create \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  --broadcast \
  src/ArbitrageExecutor.sol:ArbitrageExecutor 2>&1)

CONTRACT_ADDRESS=$(echo "$DEPLOY_OUTPUT" | grep "Deployed to:" | awk '{print $3}')

if [ -z "$CONTRACT_ADDRESS" ]; then
    echo "❌ Deployment failed!"
    echo "$DEPLOY_OUTPUT"
    exit 1
fi

echo "✅ Contract deployed: $CONTRACT_ADDRESS"
echo ""

# Step 2: Set Lynex router permissions
echo "🔧 Step 2/5: Setting Lynex router permissions..."
cast send $CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  $LYNEX_ROUTER \
  true \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  > /dev/null

cast send $CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  $LYNEX_ROUTER \
  0 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  > /dev/null

echo "✅ Lynex router configured"

# Step 3: Set Nile router permissions
echo "🔧 Step 3/5: Setting Nile router permissions..."
cast send $CONTRACT_ADDRESS \
  "setAllowedRouter(address,bool)" \
  $NILE_ROUTER \
  true \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  > /dev/null

cast send $CONTRACT_ADDRESS \
  "setRouterType(address,uint8)" \
  $NILE_ROUTER \
  0 \
  --rpc-url $LINEA_RPC \
  --private-key $PRIVATE_KEY \
  > /dev/null

echo "✅ Nile router configured"

# Step 4: Verify contract balance
echo "💰 Step 4/5: Checking USDC balance..."
USDC_BALANCE=$(cast call $USDC_ADDRESS \
  "balanceOf(address)(uint256)" \
  $CONTRACT_ADDRESS \
  --rpc-url $LINEA_RPC)

USDC_BALANCE_DECIMAL=$(echo "scale=2; $USDC_BALANCE / 1000000" | bc)
echo "   Contract USDC balance: \$$USDC_BALANCE_DECIMAL"

if [ "$USDC_BALANCE" -lt "10000000" ]; then
    echo ""
    echo "⚠️  Contract needs USDC funding!"
    echo "   Run this command to transfer USDC to the contract:"
    echo ""
    echo "   cast send $USDC_ADDRESS \\"
    echo "     \"transfer(address,uint256)\" \\"
    echo "     $CONTRACT_ADDRESS \\"
    echo "     100000000 \\"
    echo "     --rpc-url $LINEA_RPC \\"
    echo "     --private-key \$PRIVATE_KEY"
    echo ""
fi

# Step 5: Update config.yaml
echo "📝 Step 5/5: Updating config.yaml..."
cd ..
CONFIG_FILE="engine/config.yaml"

# Update contract address (line 103)
sed -i.bak "103s|contract_address:.*|contract_address: \"$CONTRACT_ADDRESS\"|" $CONFIG_FILE

# Enable Linea (line 97)
sed -i.bak "97s|enabled:.*|enabled: true|" $CONFIG_FILE

echo "✅ Config updated"
echo ""

# Summary
echo "🎉 Deployment Complete!"
echo "======================="
echo ""
echo "📋 Deployment Summary:"
echo "   Contract Address: $CONTRACT_ADDRESS"
echo "   Lynex Router: ✅ Configured"
echo "   Nile Router: ✅ Configured"
echo "   Config File: ✅ Updated"
echo ""
echo "🔗 View on LineaScan:"
echo "   https://lineascan.build/address/$CONTRACT_ADDRESS"
echo ""

if [ "$USDC_BALANCE" -lt "10000000" ]; then
    echo "⚠️  Next Step: Fund contract with USDC (see command above)"
else
    echo "✅ Contract funded with \$$USDC_BALANCE_DECIMAL USDC"
    echo ""
    echo "🚀 Ready to run engine!"
    echo "   cd /Users/moaazalthahabee/Documents/Repositories/arbitragepulse"
    echo "   cargo build --release -p engine"
    echo "   PRIVATE_KEY=\$PRIVATE_KEY CONFIG_PATH=\$(pwd)/engine/config.yaml ./target/release/engine"
fi

echo ""
