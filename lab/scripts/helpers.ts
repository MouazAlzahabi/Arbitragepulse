import {
  createPublicClient,
  createWalletClient,
  createTestClient,
  http,
  formatEther,
  formatUnits,
  parseUnits,
  type Address,
  type Hex,
  type Chain,
  defineChain,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { ANVIL_URL, TEST_KEY, getChain, ERC20_ABI } from "./config";

// ============================================================
// Client factories
// ============================================================

export function getViemChain(): Chain {
  const c = getChain();
  return defineChain({
    id: c.chainId,
    name: `Fork-${c.chainId}`,
    nativeCurrency: { name: "ETH", symbol: "ETH", decimals: 18 },
    rpcUrls: { default: { http: [ANVIL_URL] } },
  });
}

export function getClients() {
  const chain = getViemChain();
  const account = privateKeyToAccount(TEST_KEY);

  const publicClient = createPublicClient({ chain, transport: http(ANVIL_URL) });
  const walletClient = createWalletClient({ chain, transport: http(ANVIL_URL), account });
  const testClient = createTestClient({ chain, transport: http(ANVIL_URL), mode: "anvil" });

  return { publicClient, walletClient, testClient, account, chain };
}

// ============================================================
// Impersonation — "steal" tokens from whales on the fork
// ============================================================

/**
 * Impersonate a whale address and transfer tokens to our test wallet.
 * Only works on Anvil forks — this is the whole point of the lab.
 */
export async function impersonateAndTransfer(params: {
  token: Address;
  whale: Address;
  recipient: Address;
  amount: bigint;
  symbol: string;
  decimals: number;
}) {
  const { publicClient, testClient } = getClients();
  const chain = getViemChain();

  // Check whale's balance first
  const whaleBalance = await publicClient.readContract({
    address: params.token,
    abi: ERC20_ABI,
    functionName: "balanceOf",
    args: [params.whale],
  });

  console.log(
    `  Whale ${params.whale.slice(0, 10)}… has ${formatUnits(whaleBalance, params.decimals)} ${params.symbol}`
  );

  if (whaleBalance < params.amount) {
    throw new Error(
      `Whale doesn't have enough ${params.symbol}. Has: ${formatUnits(whaleBalance, params.decimals)}, need: ${formatUnits(params.amount, params.decimals)}`
    );
  }

  // Impersonate the whale
  await testClient.impersonateAccount({ address: params.whale });

  // Give whale some ETH for gas (impersonated txns still need gas on fork)
  await testClient.setBalance({ address: params.whale, value: parseUnits("1", 18) });

  // Create a wallet client as the whale
  const whaleWallet = createWalletClient({
    chain,
    transport: http(ANVIL_URL),
    account: params.whale,
  });

  // Transfer tokens
  const hash = await whaleWallet.writeContract({
    address: params.token,
    abi: ERC20_ABI,
    functionName: "transfer",
    args: [params.recipient, params.amount],
  });

  await publicClient.waitForTransactionReceipt({ hash });

  // Stop impersonating
  await testClient.stopImpersonatingAccount({ address: params.whale });

  // Verify
  const newBalance = await publicClient.readContract({
    address: params.token,
    abi: ERC20_ABI,
    functionName: "balanceOf",
    args: [params.recipient],
  });

  console.log(
    `  ✅ Transferred ${formatUnits(params.amount, params.decimals)} ${params.symbol} → ${params.recipient.slice(0, 10)}… (new balance: ${formatUnits(newBalance, params.decimals)})`
  );

  return newBalance;
}

// ============================================================
// Deploy contract from artifact
// ============================================================

export async function deployContract(artifactPath: string): Promise<Address> {
  const { publicClient, walletClient, account } = getClients();

  let artifact: any;
  try {
    artifact = await import(artifactPath, { assert: { type: "json" } });
    // Handle both default export and direct
    artifact = artifact.default || artifact;
  } catch {
    // Try reading as file
    const fs = await import("fs");
    const raw = fs.readFileSync(artifactPath, "utf-8");
    artifact = JSON.parse(raw);
  }

  if (!artifact.bytecode && !artifact.deployedBytecode) {
    throw new Error(`Invalid artifact at ${artifactPath}. Run 'npx hardhat compile' in arb-contract/ first.`);
  }

  const bytecode = artifact.bytecode.startsWith("0x")
    ? artifact.bytecode
    : `0x${artifact.bytecode}`;

  console.log(`  Deploying ArbitrageExecutor as ${account.address.slice(0, 10)}…`);

  const hash = await walletClient.deployContract({
    abi: artifact.abi,
    bytecode: bytecode as Hex,
    args: [], // Constructor takes no args (owner = msg.sender via Ownable)
  });

  const receipt = await publicClient.waitForTransactionReceipt({ hash });
  const contractAddress = receipt.contractAddress!;

  console.log(`  ✅ Contract deployed at: ${contractAddress}`);
  console.log(`  Gas used: ${receipt.gasUsed.toString()}`);

  return contractAddress;
}

// ============================================================
// Formatting helpers
// ============================================================

export function banner(title: string) {
  console.log(`\n${"═".repeat(60)}`);
  console.log(`  ${title}`);
  console.log(`${"═".repeat(60)}\n`);
}

export function step(n: number, msg: string) {
  console.log(`\n── Step ${n}: ${msg} ──`);
}
