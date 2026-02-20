use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::sol;
use anyhow::{anyhow, Result};
use tracing::info;

use crate::helpers::token_balance;
use crate::setup::LabState;

// ─── ArbitrageExecutor interface ──────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract ArbitrageExecutor {
        function executeArbitrage(
            address tokenIn,
            address tokenOut,
            uint256 amountIn,
            address routerA,
            address routerB,
            uint24 feeA,
            uint24 feeB,
            uint256 minProfit,
            uint256 deadline
        ) external;

        function totalProfit(address token) external view returns (uint256);
        function totalTrades() external view returns (uint256);
        function getStats(address token) external view returns (
            uint256 totalProfit,
            uint256 totalTrades,
            uint256 contractBalance
        );
    }
}

/// Run the full E2E arb scenario using state from .lab-state.json or setup.
/// Expected profit: ~125 USDC on a 1000 USDC trade.
pub async fn run_scenario<P: Provider>(provider: &P, state: &LabState) -> Result<()> {
    info!("=== Arb Scenario ===");

    let contract_addr: Address = state.contract.parse()?;
    let usdc: Address = state.usdc.parse()?;
    let weth: Address = state.weth.parse()?;
    let router_a: Address = state.routers.router_a.parse()?;
    let router_b: Address = state.routers.router_b.parse()?;

    let executor = ArbitrageExecutor::new(contract_addr, provider);

    // ── Balance before ──
    let balance_before = token_balance(provider, usdc, contract_addr).await?;
    info!(
        "Executor USDC balance before: {} ({})",
        format_usdc(balance_before),
        balance_before
    );

    // ── Execute arbitrage ──
    let amount_in = U256::from(1_000_000_000u128); // 1000 USDC
    let fee_zero: Uint<24, 1> = Uint::ZERO;

    let deadline = now_plus_secs(120);

    info!("Executing arb: 1000 USDC → WETH (routerA) → USDC (routerB)");

    match executor
        .executeArbitrage(
            usdc, weth,
            amount_in,
            router_a, router_b,
            fee_zero, fee_zero, // V2 fees
            U256::ZERO, // minProfit (set above checks on engine side)
            deadline,
        )
        .send()
        .await
    {
        Ok(pending) => {
            let receipt = pending.get_receipt().await?;
            if !receipt.status() {
                return Err(anyhow!("Arb transaction reverted"));
            }
            info!("Arb tx confirmed: {:?}", receipt.transaction_hash);
        }
        Err(e) => {
            return Err(anyhow!("Arb execution failed: {}", e));
        }
    }

    // ── Balance after ──
    let balance_after = token_balance(provider, usdc, contract_addr).await?;
    info!(
        "Executor USDC balance after: {} ({})",
        format_usdc(balance_after),
        balance_after
    );

    if balance_after <= balance_before {
        return Err(anyhow!("No profit made! before={} after={}", balance_before, balance_after));
    }

    let profit = balance_after - balance_before;
    let profit_display = format_usdc(profit);

    // ── Stats ──
    let stats = executor.getStats(usdc).call().await?;

    println!("\n=== Scenario Result ===");
    println!("Profit: +{} USDC", profit_display);
    println!("Total trades: {}", stats.totalTrades);
    println!("Total profit: {} USDC", format_usdc(stats.totalProfit));

    if profit < U256::from(100_000_000u128) {
        return Err(anyhow!(
            "Profit {} USDC is below expected minimum of 100 USDC",
            profit_display
        ));
    }

    println!("Scenario passed! Profit: +{} USDC", profit_display);
    Ok(())
}

/// Run the 4 revert scenarios.
pub async fn run_test_reverts<P: Provider>(provider: &P, state: &LabState) -> Result<()> {
    info!("=== Test Reverts ===");

    let contract_addr: Address = state.contract.parse()?;
    let usdc: Address = state.usdc.parse()?;
    let weth: Address = state.weth.parse()?;
    let router_a: Address = state.routers.router_a.parse()?;
    let router_b: Address = state.routers.router_b.parse()?;

    let executor = ArbitrageExecutor::new(contract_addr, provider);
    let deadline = now_plus_secs(120);
    let fee_zero: Uint<24, 1> = Uint::ZERO;
    let mut passed = 0;
    let total = 4;

    // Test 1: Unprofitable route (routerB → routerA = loss)
    print!("[1/4] Unprofitable route (B->A) ... ");
    let result = executor
        .executeArbitrage(
            usdc, weth,
            U256::from(1_000_000_000u128),
            router_b, router_a, // Reversed = loss
            fee_zero, fee_zero,
            U256::ZERO,
            deadline,
        )
        .send()
        .await;

    match result {
        Err(_) => {
            println!("Reverted as expected");
            passed += 1;
        }
        Ok(pending) => {
            match pending.get_receipt().await {
                Ok(r) if !r.status() => {
                    println!("Reverted on-chain as expected");
                    passed += 1;
                }
                _ => println!("Did NOT revert (unexpected)"),
            }
        }
    }

    // Test 2: Zero amount
    print!("[2/4] Zero amountIn ... ");
    let result = executor
        .executeArbitrage(
            usdc, weth,
            U256::ZERO,
            router_a, router_b,
            fee_zero, fee_zero,
            U256::ZERO,
            deadline,
        )
        .send()
        .await;

    if result.is_err() {
        println!("Reverted as expected");
        passed += 1;
    } else {
        println!("Did NOT revert (unexpected)");
    }

    // Test 3: Expired deadline
    print!("[3/4] Expired deadline ... ");
    let past_deadline = U256::from(1u64);
    let result = executor
        .executeArbitrage(
            usdc, weth,
            U256::from(1_000_000_000u128),
            router_a, router_b,
            fee_zero, fee_zero,
            U256::ZERO,
            past_deadline,
        )
        .send()
        .await;

    if result.is_err() {
        println!("Reverted as expected");
        passed += 1;
    } else {
        println!("Did NOT revert (unexpected)");
    }

    // Test 4: minProfit too high
    print!("[4/4] minProfit too high (200 USDC > expected 125 USDC) ... ");
    let min_profit_too_high = U256::from(200_000_000u128); // 200 USDC
    let result = executor
        .executeArbitrage(
            usdc, weth,
            U256::from(1_000_000_000u128),
            router_a, router_b,
            fee_zero, fee_zero,
            min_profit_too_high,
            deadline,
        )
        .send()
        .await;

    match result {
        Err(_) => {
            println!("Reverted as expected");
            passed += 1;
        }
        Ok(pending) => {
            match pending.get_receipt().await {
                Ok(r) if !r.status() => {
                    println!("Reverted on-chain as expected");
                    passed += 1;
                }
                _ => println!("Did NOT revert (unexpected)"),
            }
        }
    }

    println!("\n{}/{} revert tests passed", passed, total);
    if passed == total {
        println!("All revert tests passed!");
        Ok(())
    } else {
        Err(anyhow!("{}/{} revert tests passed", passed, total))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn format_usdc(amount: U256) -> String {
    let raw = amount.to::<u128>();
    format!("{:.6}", raw as f64 / 1_000_000.0)
}

fn now_plus_secs(secs: u64) -> U256 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    U256::from(now + secs)
}
