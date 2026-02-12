/**
 * create-arb-opportunity.ts — Create an artificial arbitrage opportunity
 *
 * How it works:
 *   1. Impersonate a whale with lots of USDC
 *   2. Perform a LARGE swap on Router A (USDC → WETH)
 *   3. This moves Router A's price significantly
 *   4. Router B still has the old (pre-swap) price
 *   5. Now there's a price gap: WETH is expensive on A, cheap on B
 *   6. The arb: buy WETH cheap on B, sell expensive on A → profit
 *
 * The bot should detect this and execute on the contract.
 *
 * Usage:
 *   bun run setup          # first
 *   bun run create-arb     # this script
 */

import { formatUnits, parseUnits } from "viem";
import { createWalletClient, http } from "viem";
import {
  getChain,
  ANVIL_URL,
  ROUTER_ABI,
  ERC20_ABI,
} from "./config";
import {
  getClients,
  getViemChain,
  banner,
  step,
} from "./helpers";

async function main() {
  const chain = getChain();
  banner("Create Arbitrage Opportunity");

  const { publicClient, testClient } = getClients();
  const viemChain = getViemChain();

  // ── Step 1: Check current prices on both routers ──
  step(1, "Current prices on both routers");

  const testAmountUsdc = parseUnits("100", 6); // 100 USDC → how much WETH?
  const testAmountWeth = parseUnits("0.05", 18); // 0.05 WETH → how much USDC?

  for (const router of [chain.routerA, chain.routerB]) {
    try {
      const usdcToWeth = await publicClient.readContract({
        address: router.address,
        abi: ROUTER_ABI,
        functionName: "getAmountsOut",
        args: [testAmountUsdc, [chain.usdc, chain.weth]],
      });
      const wethToUsdc = await publicClient.readContract({
        address: router.address,
        abi: ROUTER_ABI,
        functionName: "getAmountsOut",
        args: [testAmountWeth, [chain.weth, chain.usdc]],
      });

      console.log(`  ${router.name}:`);
      console.log(`    100 USDC → ${formatUnits(usdcToWeth[1], 18)} WETH`);
      console.log(`    0.05 WETH → ${formatUnits(wethToUsdc[1], 6)} USDC`);
    } catch (err) {
      console.log(`  ${router.name}: ❌ Not available (${String(err).slice(0, 60)})`);
    }
  }

  // ── Step 2: Impersonate whale and do a BIG swap on Router A ──
  step(2, `Large swap on ${chain.routerA.name} to move price`);

  const swapAmount = parseUnits("50000", 6); // 50,000 USDC — big enough to move the pool

  // Impersonate whale
  await testClient.impersonateAccount({ address: chain.usdcWhale });
  await testClient.setBalance({ address: chain.usdcWhale, value: parseUnits("10", 18) });

  const whaleWallet = createWalletClient({
    chain: viemChain,
    transport: http(ANVIL_URL),
    account: chain.usdcWhale,
  });

  // Approve Router A to spend USDC
  console.log(`  Approving ${chain.routerA.name} to spend whale's USDC...`);
  const approveHash = await whaleWallet.writeContract({
    address: chain.usdc,
    abi: ERC20_ABI,
    functionName: "approve",
    args: [chain.routerA.address, swapAmount],
  });
  await publicClient.waitForTransactionReceipt({ hash: approveHash });

  // Swap USDC → WETH on Router A (makes WETH more expensive on A)
  console.log(`  Swapping ${formatUnits(swapAmount, 6)} USDC → WETH on ${chain.routerA.name}...`);

  try {
    const deadline = BigInt(Math.floor(Date.now() / 1000) + 600);
    const swapHash = await whaleWallet.writeContract({
      address: chain.routerA.address,
      abi: ROUTER_ABI,
      functionName: "swapExactTokensForTokens",
      args: [
        swapAmount,
        0n, // amountOutMin = 0 (we don't care, this is a test)
        [chain.usdc, chain.weth],
        chain.usdcWhale,
        deadline,
      ],
    });

    const receipt = await publicClient.waitForTransactionReceipt({ hash: swapHash });
    console.log(`  ✅ Whale swap tx: ${receipt.transactionHash}`);
    console.log(`  Gas used: ${receipt.gasUsed.toString()}`);
  } catch (err) {
    console.error(`  ❌ Swap failed: ${err}`);
    console.log(`  The pool might not have enough liquidity, or the router interface is different.`);
    console.log(`  Try with a different swap amount or router.`);
    await testClient.stopImpersonatingAccount({ address: chain.usdcWhale });
    process.exit(1);
  }

  await testClient.stopImpersonatingAccount({ address: chain.usdcWhale });

  // ── Step 3: Check prices AFTER the swap ──
  step(3, "Prices AFTER whale swap (look for the gap!)");

  for (const router of [chain.routerA, chain.routerB]) {
    try {
      const usdcToWeth = await publicClient.readContract({
        address: router.address,
        abi: ROUTER_ABI,
        functionName: "getAmountsOut",
        args: [testAmountUsdc, [chain.usdc, chain.weth]],
      });
      const wethToUsdc = await publicClient.readContract({
        address: router.address,
        abi: ROUTER_ABI,
        functionName: "getAmountsOut",
        args: [testAmountWeth, [chain.weth, chain.usdc]],
      });

      console.log(`  ${router.name}:`);
      console.log(`    100 USDC → ${formatUnits(usdcToWeth[1], 18)} WETH`);
      console.log(`    0.05 WETH → ${formatUnits(wethToUsdc[1], 6)} USDC`);
    } catch {
      console.log(`  ${router.name}: ❌ Not available`);
    }
  }

  // ── Step 4: Verify opportunity exists ──
  step(4, "Checking if arb opportunity exists");

  // Read .lab-state.json for contract address
  let labState: any;
  try {
    const fs = await import("fs");
    labState = JSON.parse(fs.readFileSync(".lab-state.json", "utf-8"));
  } catch {
    console.log("  ⚠ No .lab-state.json found — run 'bun run setup' first");
    console.log("  Skipping contract simulation check");
    return;
  }

  // Try to simulate the arb on the contract
  try {
    const { EXECUTOR_ABI } = await import("./config");
    const deadline = BigInt(Math.floor(Date.now() / 1000) + 300);

    // Try: buy WETH on B (cheap), sell on A (expensive)
    await publicClient.simulateContract({
      address: labState.contractAddress,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,           // tokenIn: start with USDC
        chain.weth,           // tokenOut: buy WETH
        parseUnits("500", 6), // 500 USDC
        chain.routerB.address, // buy on B (still cheap)
        chain.routerA.address, // sell on A (now expensive after whale dump)
        0n,                   // minProfit
        deadline,
      ],
      account: labState.wallet,
    });

    console.log(`  🎯 OPPORTUNITY CONFIRMED — the arb would succeed!`);
    console.log(`  Your bot should detect this and execute automatically.`);
  } catch (err) {
    const msg = String(err);
    if (msg.includes("NotProfitable")) {
      console.log(`  ⚠ Arb exists but not profitable after fees (NotProfitable revert)`);
      console.log(`  Try a larger whale swap or different token amounts`);
    } else {
      console.log(`  ⚠ Simulation failed: ${msg.slice(0, 100)}`);
    }
  }

  console.log(`\n  🧪 Now start your engine pointed at Anvil to test detection!`);
  console.log(`     WS_RPC_URL=ws://127.0.0.1:8545`);
  console.log(`     HTTP_RPC_URL=http://127.0.0.1:8545\n`);
}

main().catch((err) => {
  console.error("Failed:", err);
  process.exit(1);
});
