/**
 * full-scenario.ts — Complete E2E simulation
 *
 * Runs everything in sequence:
 *   1. Deploy contract
 *   2. Fund wallet + contract
 *   3. Record initial balances
 *   4. Create arb opportunity (whale swap on Router A)
 *   5. Execute arb on our contract (buy on B, sell on A)
 *   6. Verify profit in contract
 *   7. Withdraw profit
 *   8. Print P&L report
 *
 * This is what you run to prove the entire system works before going live.
 *
 * Usage:
 *   Start anvil:    bun run fork:optimism
 *   Run scenario:   bun run scenario
 */

import {
  formatEther,
  formatUnits,
  parseUnits,
  createWalletClient,
  http,
  encodeFunctionData,
} from "viem";
import {
  getChain,
  ANVIL_URL,
  CONTRACT_ARTIFACT_PATH,
  ERC20_ABI,
  ROUTER_ABI,
  EXECUTOR_ABI,
} from "./config";
import {
  getClients,
  getViemChain,
  impersonateAndTransfer,
  deployContract,
  banner,
  step,
} from "./helpers";

async function main() {
  const chain = getChain();
  banner(`Full E2E Scenario — ${chain.chainId === 10 ? "Optimism" : chain.chainId === 8453 ? "Base" : "Gnosis"}`);

  const { publicClient, walletClient, testClient, account } = getClients();
  const viemChain = getViemChain();

  // ═══════════════════════════════════════════
  // 1. DEPLOY
  // ═══════════════════════════════════════════
  step(1, "Deploy ArbitrageExecutor");

  let contract: `0x${string}`;
  try {
    contract = await deployContract(CONTRACT_ARTIFACT_PATH);
  } catch (err) {
    console.error(`  ❌ ${err}`);
    process.exit(1);
  }

  // ═══════════════════════════════════════════
  // 2. FUND
  // ═══════════════════════════════════════════
  step(2, "Fund contract with USDC and WETH");

  // Get USDC from whale
  await impersonateAndTransfer({
    token: chain.usdc,
    whale: chain.usdcWhale,
    recipient: account.address,
    amount: parseUnits("10000", 6),
    symbol: "USDC",
    decimals: 6,
  });

  // Get WETH by depositing ETH
  const wethDeposit = [{ type: "function", name: "deposit", inputs: [], outputs: [], stateMutability: "payable" }] as const;
  await walletClient.writeContract({ address: chain.weth, abi: wethDeposit, functionName: "deposit", value: parseUnits("5", 18) });

  // Fund contract
  const fundUsdc = parseUnits("3000", 6);
  const fundWeth = parseUnits("1", 18);

  await walletClient.writeContract({ address: chain.usdc, abi: ERC20_ABI, functionName: "transfer", args: [contract, fundUsdc] }).then((h) => publicClient.waitForTransactionReceipt({ hash: h }));
  await walletClient.writeContract({ address: chain.weth, abi: ERC20_ABI, functionName: "transfer", args: [contract, fundWeth] }).then((h) => publicClient.waitForTransactionReceipt({ hash: h }));

  console.log(`  ✅ Contract funded: ${formatUnits(fundUsdc, 6)} USDC + ${formatUnits(fundWeth, 18)} WETH`);

  // ═══════════════════════════════════════════
  // 3. RECORD INITIAL STATE
  // ═══════════════════════════════════════════
  step(3, "Record initial balances");

  const initialUsdc = await publicClient.readContract({ address: chain.usdc, abi: ERC20_ABI, functionName: "balanceOf", args: [contract] });
  const initialWeth = await publicClient.readContract({ address: chain.weth, abi: ERC20_ABI, functionName: "balanceOf", args: [contract] });

  console.log(`  Contract USDC: ${formatUnits(initialUsdc, 6)}`);
  console.log(`  Contract WETH: ${formatUnits(initialWeth, 18)}`);

  // Check prices before
  console.log(`\n  Pre-swap prices:`);
  for (const router of [chain.routerA, chain.routerB]) {
    try {
      const quote = await publicClient.readContract({
        address: router.address, abi: ROUTER_ABI, functionName: "getAmountsOut",
        args: [parseUnits("100", 6), [chain.usdc, chain.weth]],
      });
      console.log(`    ${router.name}: 100 USDC → ${formatUnits(quote[1], 18)} WETH`);
    } catch {
      console.log(`    ${router.name}: ❌ Not available`);
    }
  }

  // ═══════════════════════════════════════════
  // 4. CREATE PRICE IMBALANCE
  // ═══════════════════════════════════════════
  step(4, `Whale swap on ${chain.routerA.name} (create imbalance)`);

  const whaleSwapAmount = parseUnits("80000", 6);

  await testClient.impersonateAccount({ address: chain.usdcWhale });
  await testClient.setBalance({ address: chain.usdcWhale, value: parseUnits("10", 18) });

  const whaleWallet = createWalletClient({ chain: viemChain, transport: http(ANVIL_URL), account: chain.usdcWhale });

  await whaleWallet.writeContract({
    address: chain.usdc, abi: ERC20_ABI, functionName: "approve",
    args: [chain.routerA.address, whaleSwapAmount],
  }).then((h) => publicClient.waitForTransactionReceipt({ hash: h }));

  try {
    const deadline = BigInt(Math.floor(Date.now() / 1000) + 600);
    await whaleWallet.writeContract({
      address: chain.routerA.address, abi: ROUTER_ABI, functionName: "swapExactTokensForTokens",
      args: [whaleSwapAmount, 0n, [chain.usdc, chain.weth], chain.usdcWhale, deadline],
    }).then((h) => publicClient.waitForTransactionReceipt({ hash: h }));

    console.log(`  ✅ Whale swapped ${formatUnits(whaleSwapAmount, 6)} USDC → WETH on ${chain.routerA.name}`);
  } catch (err) {
    console.error(`  ❌ Whale swap failed: ${String(err).slice(0, 100)}`);
    await testClient.stopImpersonatingAccount({ address: chain.usdcWhale });
    process.exit(1);
  }

  await testClient.stopImpersonatingAccount({ address: chain.usdcWhale });

  // Show post-swap prices
  console.log(`\n  Post-swap prices:`);
  for (const router of [chain.routerA, chain.routerB]) {
    try {
      const quote = await publicClient.readContract({
        address: router.address, abi: ROUTER_ABI, functionName: "getAmountsOut",
        args: [parseUnits("100", 6), [chain.usdc, chain.weth]],
      });
      console.log(`    ${router.name}: 100 USDC → ${formatUnits(quote[1], 18)} WETH`);
    } catch {
      console.log(`    ${router.name}: ❌ Not available`);
    }
  }

  // ═══════════════════════════════════════════
  // 5. EXECUTE ARB ON CONTRACT
  // ═══════════════════════════════════════════
  step(5, "Execute arbitrage via contract");

  const arbAmount = parseUnits("1000", 6); // 1,000 USDC
  const arbDeadline = BigInt(Math.floor(Date.now() / 1000) + 300);

  // Strategy: buy WETH on Router B (still cheap), sell on Router A (now expensive after whale dump)
  // Wait — the whale bought WETH on A (USDC → WETH), so WETH is now MORE expensive on A.
  // For USDC arb: Router B has cheaper WETH, Router A has expensive WETH.
  // So: send USDC to B → get WETH → send WETH to A → get more USDC back.
  // Contract path: tokenIn=USDC, tokenOut=WETH, routerA=B(buy), routerB=A(sell)

  try {
    // Simulate first
    const simResult = await publicClient.simulateContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,             // tokenIn
        chain.weth,             // tokenOut
        arbAmount,              // amountIn
        chain.routerB.address,  // routerA (buy WETH here — cheaper)
        chain.routerA.address,  // routerB (sell WETH here — expensive)
        0n,                     // minProfit
        arbDeadline,
      ],
      account: account.address,
    });

    console.log(`  ✅ Simulation passed! Executing...`);

    // Actually execute
    const execHash = await walletClient.writeContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,
        chain.weth,
        arbAmount,
        chain.routerB.address,
        chain.routerA.address,
        0n,
        arbDeadline,
      ],
    });

    const receipt = await publicClient.waitForTransactionReceipt({ hash: execHash });
    console.log(`  ✅ Arbitrage executed!`);
    console.log(`  TX: ${receipt.transactionHash}`);
    console.log(`  Gas: ${receipt.gasUsed.toString()} (${receipt.status})`);

  } catch (err) {
    const msg = String(err);
    if (msg.includes("NotProfitable")) {
      console.log(`  ⚠ NotProfitable — the spread isn't large enough for this amount.`);
      console.log(`  Try: smaller arbAmount, larger whaleSwapAmount, or different routers.`);

      // Try the opposite direction
      console.log(`\n  Trying opposite direction (WETH as tokenIn)...`);
      try {
        await publicClient.simulateContract({
          address: contract, abi: EXECUTOR_ABI, functionName: "executeArbitrage",
          args: [chain.weth, chain.usdc, parseUnits("0.5", 18), chain.routerA.address, chain.routerB.address, 0n, arbDeadline],
          account: account.address,
        });
        console.log(`  ✅ Opposite direction works! Executing...`);

        const h = await walletClient.writeContract({
          address: contract, abi: EXECUTOR_ABI, functionName: "executeArbitrage",
          args: [chain.weth, chain.usdc, parseUnits("0.5", 18), chain.routerA.address, chain.routerB.address, 0n, arbDeadline],
        });
        const r = await publicClient.waitForTransactionReceipt({ hash: h });
        console.log(`  ✅ TX: ${r.transactionHash} (gas: ${r.gasUsed})`);
      } catch (err2) {
        console.log(`  ❌ Opposite direction also failed: ${String(err2).slice(0, 80)}`);
        console.log(`\n  The whale swap may not have created enough spread between these routers.`);
        console.log(`  This is normal — not every swap creates an arb opportunity.\n`);
      }
    } else {
      console.error(`  ❌ Execution failed: ${msg.slice(0, 150)}`);
    }
  }

  // ═══════════════════════════════════════════
  // 6. CHECK PROFIT
  // ═══════════════════════════════════════════
  step(6, "P&L Report");

  const finalUsdc = await publicClient.readContract({ address: chain.usdc, abi: ERC20_ABI, functionName: "balanceOf", args: [contract] });
  const finalWeth = await publicClient.readContract({ address: chain.weth, abi: ERC20_ABI, functionName: "balanceOf", args: [contract] });

  const usdcDiff = finalUsdc - initialUsdc;
  const wethDiff = finalWeth - initialWeth;

  console.log(`\n  ┌────────────────────────────────────────────┐`);
  console.log(`  │            P & L   R E P O R T              │`);
  console.log(`  ├────────────────────────────────────────────┤`);
  console.log(`  │  USDC  Before: ${formatUnits(initialUsdc, 6).padEnd(12)} After: ${formatUnits(finalUsdc, 6).padEnd(12)}│`);
  console.log(`  │  WETH  Before: ${formatUnits(initialWeth, 18).slice(0, 12).padEnd(12)} After: ${formatUnits(finalWeth, 18).slice(0, 12).padEnd(12)}│`);
  console.log(`  ├────────────────────────────────────────────┤`);
  console.log(`  │  USDC Δ: ${usdcDiff >= 0n ? "+" : ""}${formatUnits(usdcDiff, 6).padEnd(30)}    │`);
  console.log(`  │  WETH Δ: ${wethDiff >= 0n ? "+" : ""}${formatUnits(wethDiff, 18).slice(0, 30).padEnd(30)}    │`);
  console.log(`  └────────────────────────────────────────────┘`);

  // Get stats from contract
  try {
    const stats = await publicClient.readContract({
      address: contract, abi: EXECUTOR_ABI, functionName: "getStats", args: [chain.usdc],
    });
    console.log(`\n  Contract stats: totalProfit=${formatUnits(stats[0], 6)} USDC, totalTrades=${stats[1]}`);
  } catch {}

  if (usdcDiff > 0n || wethDiff > 0n) {
    console.log(`\n  🎉 PROFIT! The system works. You're ready to test on mainnet (dry-run first).`);
  } else if (usdcDiff === 0n && wethDiff === 0n) {
    console.log(`\n  ↔ No change — the arb may not have executed (check errors above).`);
  } else {
    console.log(`\n  ⚠ Loss detected — check the arb direction and router selection.`);
  }

  console.log("");
}

main().catch((err) => {
  console.error("Scenario failed:", err);
  process.exit(1);
});
