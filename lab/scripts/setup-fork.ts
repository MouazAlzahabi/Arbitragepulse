/**
 * setup-fork.ts — Prepare the Anvil fork for testing
 *
 * Steps:
 *   1. Check Anvil connection
 *   2. Deploy ArbitrageExecutor contract
 *   3. Fund test wallet with USDC from whale (impersonation)
 *   4. Fund test wallet with WETH
 *   5. Transfer tokens to the contract (it needs capital to arb with)
 *   6. Print summary
 *
 * Usage:
 *   1. Start Anvil:  bun run fork:optimism
 *   2. Run setup:    bun run setup
 */

import { formatEther, formatUnits, parseUnits } from "viem";
import {
  getChain,
  ANVIL_URL,
  CONTRACT_ARTIFACT_PATH,
  ERC20_ABI,
} from "./config";
import {
  getClients,
  impersonateAndTransfer,
  deployContract,
  banner,
  step,
} from "./helpers";

async function main() {
  const chain = getChain();
  banner(`ArbitragePulse Lab — Setup (${chain.chainId === 10 ? "Optimism" : chain.chainId === 8453 ? "Base" : "Gnosis"})`);

  // ── Step 1: Check Anvil ──
  step(1, "Checking Anvil connection");

  const { publicClient, walletClient, testClient, account } = getClients();

  try {
    const blockNumber = await publicClient.getBlockNumber();
    const balance = await publicClient.getBalance({ address: account.address });
    console.log(`  Anvil URL:     ${ANVIL_URL}`);
    console.log(`  Chain ID:      ${chain.chainId}`);
    console.log(`  Block:         ${blockNumber}`);
    console.log(`  Test wallet:   ${account.address}`);
    console.log(`  ETH balance:   ${formatEther(balance)}`);
  } catch (err) {
    console.error(`\n  ❌ Cannot connect to Anvil at ${ANVIL_URL}`);
    console.error(`  Start it first: bun run fork:optimism`);
    process.exit(1);
  }

  // ── Step 2: Deploy contract ──
  step(2, "Deploying ArbitrageExecutor");

  let contractAddress: `0x${string}`;
  try {
    contractAddress = await deployContract(CONTRACT_ARTIFACT_PATH);
  } catch (err) {
    console.error(`  ❌ Deployment failed: ${err}`);
    console.error(`  Make sure to compile first: cd ../contract && forge build`);
    process.exit(1);
  }

  // ── Step 3: Fund wallet with USDC ──
  step(3, "Funding test wallet with USDC (whale impersonation)");

  const usdcAmount = parseUnits("5000", 6); // 5,000 USDC

  await impersonateAndTransfer({
    token: chain.usdc,
    whale: chain.usdcWhale,
    recipient: account.address,
    amount: usdcAmount,
    symbol: "USDC",
    decimals: 6,
  });

  // ── Step 4: Fund wallet with WETH ──
  step(4, "Getting WETH (deposit ETH)");

  // On OP/Base, WETH is at 0x4200...0006. We can deposit ETH to get WETH.
  const wethDepositABI = [
    { type: "function", name: "deposit", inputs: [], outputs: [], stateMutability: "payable" },
  ] as const;

  const depositAmount = parseUnits("5", 18); // 5 WETH
  const depositHash = await walletClient.writeContract({
    address: chain.weth,
    abi: wethDepositABI,
    functionName: "deposit",
    value: depositAmount,
  });
  await publicClient.waitForTransactionReceipt({ hash: depositHash });

  const wethBalance = await publicClient.readContract({
    address: chain.weth,
    abi: ERC20_ABI,
    functionName: "balanceOf",
    args: [account.address],
  });
  console.log(`  ✅ WETH balance: ${formatUnits(wethBalance, 18)}`);

  // ── Step 5: Fund the contract ──
  step(5, "Transferring tokens to ArbitrageExecutor contract");

  // Send 2,000 USDC to contract
  const contractUsdcAmount = parseUnits("2000", 6);
  const tx1 = await walletClient.writeContract({
    address: chain.usdc,
    abi: ERC20_ABI,
    functionName: "transfer",
    args: [contractAddress, contractUsdcAmount],
  });
  await publicClient.waitForTransactionReceipt({ hash: tx1 });

  // Send 1 WETH to contract
  const contractWethAmount = parseUnits("1", 18);
  const tx2 = await walletClient.writeContract({
    address: chain.weth,
    abi: ERC20_ABI,
    functionName: "transfer",
    args: [contractAddress, contractWethAmount],
  });
  await publicClient.waitForTransactionReceipt({ hash: tx2 });

  console.log(`  ✅ Contract funded: 2,000 USDC + 1 WETH`);

  // ── Summary ──
  step(6, "Summary");

  const contractUsdc = await publicClient.readContract({
    address: chain.usdc, abi: ERC20_ABI, functionName: "balanceOf", args: [contractAddress],
  });
  const contractWeth = await publicClient.readContract({
    address: chain.weth, abi: ERC20_ABI, functionName: "balanceOf", args: [contractAddress],
  });
  const walletUsdc = await publicClient.readContract({
    address: chain.usdc, abi: ERC20_ABI, functionName: "balanceOf", args: [account.address],
  });
  const walletWeth = await publicClient.readContract({
    address: chain.weth, abi: ERC20_ABI, functionName: "balanceOf", args: [account.address],
  });
  const walletEth = await publicClient.getBalance({ address: account.address });

  console.log(`  ┌────────────────────────────────────────────┐`);
  console.log(`  │ Contract: ${contractAddress}   │`);
  console.log(`  │   USDC:   ${formatUnits(contractUsdc, 6).padEnd(20)}           │`);
  console.log(`  │   WETH:   ${formatUnits(contractWeth, 18).padEnd(20)}           │`);
  console.log(`  ├────────────────────────────────────────────┤`);
  console.log(`  │ Wallet:   ${account.address}   │`);
  console.log(`  │   ETH:    ${formatEther(walletEth).slice(0, 20).padEnd(20)}           │`);
  console.log(`  │   USDC:   ${formatUnits(walletUsdc, 6).padEnd(20)}           │`);
  console.log(`  │   WETH:   ${formatUnits(walletWeth, 18).padEnd(20)}           │`);
  console.log(`  └────────────────────────────────────────────┘`);

  console.log(`\n  📋 Add to your engine config.yaml or .env:`);
  console.log(`     CONTRACT_ADDRESS=${contractAddress}`);
  console.log(`     HTTP_RPC_URL=${ANVIL_URL}`);
  console.log(`     WS_RPC_URL=ws://127.0.0.1:8545\n`);

  // Write to a file for other scripts to read
  const fs = await import("fs");
  fs.writeFileSync(
    ".lab-state.json",
    JSON.stringify({
      contractAddress,
      chainId: chain.chainId,
      wallet: account.address,
      routers: { a: chain.routerA, b: chain.routerB },
      tokens: { usdc: chain.usdc, weth: chain.weth },
    }, null, 2)
  );
  console.log(`  State saved to .lab-state.json`);
}

main().catch((err) => {
  console.error("Setup failed:", err);
  process.exit(1);
});
