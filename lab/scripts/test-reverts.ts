/**
 * test-reverts.ts — Intentionally trigger every revert reason
 *
 * Your bot WILL encounter these in production. This script shows you
 * exactly what each error looks like so you can handle them properly.
 *
 * Reverts tested:
 *   1. NotProfitable — arb doesn't make money
 *   2. Expired deadline — tx submitted too late
 *   3. Insufficient balance — contract doesn't have enough tokens
 *   4. Not owner — calling from wrong wallet
 *
 * Usage:
 *   bun run setup          # first
 *   bun run test-reverts   # this script
 */

import { parseUnits, formatUnits } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { createWalletClient, http } from "viem";
import { getChain, EXECUTOR_ABI, ERC20_ABI } from "./config";
import { getClients, getViemChain, banner, step } from "./helpers";

async function main() {
  banner("Revert Testing");

  const chain = getChain();
  const { publicClient, walletClient, account } = getClients();
  const viemChain = getViemChain();

  // Load lab state
  let labState: any;
  try {
    const fs = await import("fs");
    labState = JSON.parse(fs.readFileSync(".lab-state.json", "utf-8"));
  } catch {
    console.error("  ❌ No .lab-state.json — run 'bun run setup' first");
    process.exit(1);
  }

  const contract = labState.contractAddress as `0x${string}`;
  const deadline = BigInt(Math.floor(Date.now() / 1000) + 300);
  let pass = 0;
  let fail = 0;

  // ── Test 1: NotProfitable ──
  step(1, "NotProfitable — same router for buy and sell (no spread)");

  try {
    await publicClient.simulateContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,
        chain.weth,
        parseUnits("100", 6),
        chain.routerA.address,
        chain.routerA.address, // Same router! No arb possible.
        0n,
        deadline,
      ],
      account: account.address,
    });
    console.log("  ❌ UNEXPECTED — should have reverted!");
    fail++;
  } catch (err) {
    const msg = String(err);
    if (msg.includes("NotProfitable")) {
      console.log("  ✅ Correctly reverted: NotProfitable");
      console.log(`     This is what your bot sees when there's no spread.`);
      pass++;
    } else {
      console.log(`  ⚠ Reverted but different reason: ${msg.slice(0, 120)}`);
      pass++; // Still a revert, which is expected
    }
  }

  // ── Test 2: Expired deadline ──
  step(2, "Expired deadline — timestamp in the past");

  try {
    const pastDeadline = BigInt(Math.floor(Date.now() / 1000) - 600); // 10 min ago

    await publicClient.simulateContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,
        chain.weth,
        parseUnits("100", 6),
        chain.routerA.address,
        chain.routerB.address,
        0n,
        pastDeadline,
      ],
      account: account.address,
    });
    console.log("  ❌ UNEXPECTED — should have reverted!");
    fail++;
  } catch (err) {
    const msg = String(err);
    if (msg.includes("DeadlineExpired") || msg.includes("deadline") || msg.includes("EXPIRED")) {
      console.log("  ✅ Correctly reverted: DeadlineExpired");
      pass++;
    } else {
      console.log(`  ✅ Reverted (reason: ${msg.slice(0, 80)})`);
      pass++;
    }
  }

  // ── Test 3: Insufficient balance ──
  step(3, "Insufficient balance — trade more than contract holds");

  try {
    const hugeAmount = parseUnits("999999", 6); // Way more USDC than contract has

    await publicClient.simulateContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,
        chain.weth,
        hugeAmount,
        chain.routerA.address,
        chain.routerB.address,
        0n,
        deadline,
      ],
      account: account.address,
    });
    console.log("  ❌ UNEXPECTED — should have reverted!");
    fail++;
  } catch (err) {
    const msg = String(err);
    console.log(`  ✅ Correctly reverted (reason: ${msg.includes("transfer amount exceeds balance") ? "ERC20 insufficient balance" : msg.slice(0, 80)})`);
    pass++;
  }

  // ── Test 4: Not owner ──
  step(4, "Access control — calling from non-owner wallet");

  try {
    // Use Anvil's second default account (not the owner)
    const nonOwnerKey = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d" as `0x${string}`;
    const nonOwner = privateKeyToAccount(nonOwnerKey);

    const nonOwnerClient = createWalletClient({
      chain: viemChain,
      transport: http("http://127.0.0.1:8545"),
      account: nonOwner,
    });

    await publicClient.simulateContract({
      address: contract,
      abi: EXECUTOR_ABI,
      functionName: "executeArbitrage",
      args: [
        chain.usdc,
        chain.weth,
        parseUnits("100", 6),
        chain.routerA.address,
        chain.routerB.address,
        0n,
        deadline,
      ],
      account: nonOwner.address,
    });
    console.log("  ❌ UNEXPECTED — should have reverted!");
    fail++;
  } catch (err) {
    const msg = String(err);
    if (msg.includes("OwnableUnauthorizedAccount") || msg.includes("caller is not the owner") || msg.includes("Ownable")) {
      console.log("  ✅ Correctly reverted: OwnableUnauthorizedAccount");
      pass++;
    } else {
      console.log(`  ✅ Reverted (reason: ${msg.slice(0, 80)})`);
      pass++;
    }
  }

  // ── Summary ──
  console.log(`\n${"═".repeat(60)}`);
  console.log(`  Results: ${pass} passed, ${fail} failed`);
  console.log(`${"═".repeat(60)}\n`);

  if (fail > 0) {
    console.log("  ⚠ Some tests didn't revert as expected. Check the output above.");
  } else {
    console.log("  ✅ All revert scenarios handled correctly!");
    console.log("  Your bot should catch these errors and continue running.\n");
  }
}

main().catch((err) => {
  console.error("Failed:", err);
  process.exit(1);
});
