import {
  type PublicClient,
  type WalletClient,
  type Address,
  type Hash,
  encodeFunctionData,
  formatEther,
  formatUnits,
  parseEther,
} from "viem";
import { ArbitrageExecutorABI } from "../abi";
import type { ChainConfig } from "../config";
import type { ArbOpportunity } from "../strategies";
import { log, NonceManager, Mutex } from "../utils";

export interface ExecutionResult {
  success: boolean;
  txHash?: Hash;
  profit?: bigint;
  error?: string;
  gasUsed?: bigint;
  opportunity: ArbOpportunity;
  dryRun: boolean;
}

export class ArbExecutor {
  private nonceMgr: NonceManager;
  private mutex = new Mutex();
  private paused = false;
  private stats = {
    totalAttempts: 0,
    totalSuccess: 0,
    totalFailed: 0,
    totalProfitWei: 0n,
    lastExecution: 0,
  };

  constructor(
    private publicClient: PublicClient,
    private walletClient: WalletClient,
    private chain: ChainConfig,
    private walletAddress: Address
  ) {
    this.nonceMgr = new NonceManager(publicClient, walletAddress);
  }

  async init() {
    await this.nonceMgr.init();

    // Verify contract
    const code = await this.publicClient.getCode({ address: this.chain.contractAddress });
    if (!code || code === "0x") {
      throw new Error(
        `[${this.chain.name}] No contract at ${this.chain.contractAddress}. Deploy ARB-001 first.`
      );
    }

    try {
      const owner = await this.publicClient.readContract({
        address: this.chain.contractAddress,
        abi: ArbitrageExecutorABI,
        functionName: "owner",
      });
      if (owner.toLowerCase() !== this.walletAddress.toLowerCase()) {
        throw new Error(
          `[${this.chain.name}] Wallet ${this.walletAddress} is not contract owner. Owner: ${owner}`
        );
      }
    } catch (err) {
      if (String(err).includes("not contract owner")) throw err;
      log("warn", `[${this.chain.name}] Could not verify ownership`, { error: String(err) });
    }

    log("info", `[${this.chain.name}] Executor ready`, {
      contract: this.chain.contractAddress,
      wallet: this.walletAddress,
    });
  }

  async execute(opp: ArbOpportunity, dryRun: boolean = false): Promise<ExecutionResult> {
    const release = await this.mutex.acquire();

    try {
      this.stats.totalAttempts++;

      if (!(await this.preflight(opp))) {
        return { success: false, error: "Preflight failed", opportunity: opp, dryRun };
      }

      return dryRun ? await this.simulate(opp) : await this.live(opp);
    } catch (err) {
      const error = err instanceof Error ? err.message : String(err);
      log("error", `[${this.chain.name}] Execution failed: ${error}`);
      await this.nonceMgr.handleError(err);
      this.stats.totalFailed++;
      return { success: false, error, opportunity: opp, dryRun };
    } finally {
      release();
    }
  }

  private async preflight(opp: ArbOpportunity): Promise<boolean> {
    if (this.paused) {
      log("warn", `[${this.chain.name}] Executor paused`);
      return false;
    }

    // Check gas balance
    const balance = await this.publicClient.getBalance({ address: this.walletAddress });
    const minBal = parseEther(this.chain.minNativeBalance.toString());

    if (balance < minBal) {
      log("error", `[${this.chain.name}] ${this.chain.nativeCurrency} balance too low: ${formatEther(balance)}`);
      this.paused = true;
      return false;
    }

    // Check contract not paused
    try {
      const contractPaused = await this.publicClient.readContract({
        address: this.chain.contractAddress,
        abi: ArbitrageExecutorABI,
        functionName: "paused",
      });
      if (contractPaused) {
        log("warn", `[${this.chain.name}] Contract is paused`);
        return false;
      }
    } catch {}

    if (!opp.netProfitable) return false;
    return true;
  }

  private async simulate(opp: ArbOpportunity): Promise<ExecutionResult> {
    const deadline = BigInt(Math.floor(Date.now() / 1000) + this.chain.deadlineSeconds);

    try {
      await this.publicClient.call({
        to: this.chain.contractAddress,
        data: encodeFunctionData({
          abi: ArbitrageExecutorABI,
          functionName: "executeArbitrage",
          args: [opp.tokenIn, opp.tokenOut, opp.amountIn, opp.routerA.address, opp.routerB.address, 0n, deadline],
        }),
        account: this.walletAddress,
      });

      log("trade", `[${this.chain.name}] [DRY RUN] ✅ Simulation passed`, {
        pair: `${opp.pair.tokenInSymbol}/${opp.pair.tokenOutSymbol}`,
        profit: `$${opp.profitUsd.toFixed(4)}`,
        buyOn: opp.routerA.name,
        sellOn: opp.routerB.name,
      });

      this.stats.totalSuccess++;
      return { success: true, profit: opp.profit, opportunity: opp, dryRun: true };
    } catch (err) {
      const error = String(err);
      const isExpected = error.includes("NotProfitable");
      log(isExpected ? "debug" : "error", `[${this.chain.name}] [DRY RUN] Simulation ${isExpected ? "reverted (expected)" : "failed"}`);
      this.stats.totalFailed++;
      return { success: false, error, opportunity: opp, dryRun: true };
    }
  }

  private async live(opp: ArbOpportunity): Promise<ExecutionResult> {
    const deadline = BigInt(Math.floor(Date.now() / 1000) + this.chain.deadlineSeconds);
    const nonce = await this.nonceMgr.acquire();

    try {
      // Gas estimate
      let gasLimit: bigint;
      try {
        gasLimit = await this.publicClient.estimateGas({
          to: this.chain.contractAddress,
          data: encodeFunctionData({
            abi: ArbitrageExecutorABI,
            functionName: "executeArbitrage",
            args: [opp.tokenIn, opp.tokenOut, opp.amountIn, opp.routerA.address, opp.routerB.address, 0n, deadline],
          }),
          account: this.walletAddress,
        });
        gasLimit = (gasLimit * 120n) / 100n;
      } catch (e) {
        if (String(e).includes("NotProfitable")) {
          this.nonceMgr.release();
          return { success: false, error: "NotProfitable at gas estimation", opportunity: opp, dryRun: false };
        }
        gasLimit = 500_000n;
      }

      const txHash = await this.walletClient.writeContract({
        address: this.chain.contractAddress,
        abi: ArbitrageExecutorABI,
        functionName: "executeArbitrage",
        args: [opp.tokenIn, opp.tokenOut, opp.amountIn, opp.routerA.address, opp.routerB.address, 0n, deadline],
        gas: gasLimit,
        nonce,
        chain: this.walletClient.chain,
        account: this.walletClient.account!,
      });

      this.nonceMgr.release();

      log("trade", `[${this.chain.name}] 📤 TX sent: ${txHash}`);

      const receipt = await this.publicClient.waitForTransactionReceipt({ hash: txHash, timeout: 30_000 });

      if (receipt.status === "success") {
        this.stats.totalSuccess++;
        this.stats.totalProfitWei += opp.profit;
        this.stats.lastExecution = Date.now();
        log("trade", `[${this.chain.name}] ✅ Confirmed! $${opp.profitUsd.toFixed(4)} profit`, {
          txHash,
          gas: receipt.gasUsed.toString(),
        });
        return { success: true, txHash, profit: opp.profit, gasUsed: receipt.gasUsed, opportunity: opp, dryRun: false };
      } else {
        this.stats.totalFailed++;
        log("error", `[${this.chain.name}] ❌ Reverted: ${txHash}`);
        return { success: false, txHash, error: "Reverted", gasUsed: receipt.gasUsed, opportunity: opp, dryRun: false };
      }
    } catch (err) {
      this.nonceMgr.release();
      throw err;
    }
  }

  pause() { this.paused = true; log("warn", `[${this.chain.name}] Executor paused`); }
  resume() { this.paused = false; log("info", `[${this.chain.name}] Executor resumed`); }
  getStats() { return { ...this.stats, totalProfitWei: this.stats.totalProfitWei.toString() }; }
}
