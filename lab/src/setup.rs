use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::helpers::{deploy_contract, read_bytecode, token_balance, IERC20};
use crate::mock_router::{deploy_mock_routers, MockRouterState};

// ─── ArbitrageExecutor interface ──────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract ArbitrageExecutor {
        constructor();
        function setAllowedRouter(address router, bool approved) external;
        function setRouterType(address router, uint8 rtype) external;
        function allowedRouters(address router) external view returns (bool);
        function totalTrades() external view returns (uint256);
    }
}

// ─── State file ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct LabState {
    pub contract: String,
    pub usdc: String,
    pub weth: String,
    pub routers: MockRouterState,
    pub deployer: String,
    pub anvil_rpc: String,
}

/// Deploy the ArbitrageExecutor + MockRouters on local Anvil, write .lab-state.json.
pub async fn run_setup<P: Provider>(
    provider: &P,
    deployer: Address,
    contract_dir: &str,
    anvil_rpc: &str,
) -> Result<LabState> {
    info!("=== Lab Setup ===");

    // ── Token addresses on Optimism fork ──
    let usdc: Address = "0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85".parse()?;
    let weth: Address = "0x4200000000000000000000000000000000000006".parse()?;

    // ── Ensure deployer has USDC (impersonate whale if needed) ──
    let usdc_balance = token_balance(provider, usdc, deployer).await?;
    info!("Deployer USDC balance: {} (units)", usdc_balance);
    // Need 500k for RouterB seed + 50k for executor = 550k minimum; re-fund if below 600k
    if usdc_balance < U256::from(600_000_000_000u128) {
        // Fund deployer from a known USDC whale via Anvil impersonation
        let whale: Address = "0xf89d7b9c864f589bbF53a82105107622B35EaA40".parse()?;
        info!("Deployer has no USDC — impersonating whale {:?} to fund deployer", whale);

        // anvil_impersonateAccount
        let _: serde_json::Value = provider
            .raw_request("anvil_impersonateAccount".into(), serde_json::json!([format!("{whale:?}")]))
            .await
            .map_err(|e| anyhow!("anvil_impersonateAccount failed: {}", e))?;

        // Transfer 1,000,000 USDC from whale to deployer (covers RouterB seed + executor)
        let fund_amount = U256::from(1_000_000_000_000u128); // 1,000,000 USDC (6 decimals)
        let calldata = IERC20::transferCall { to: deployer, amount: fund_amount }.abi_encode();
        let _: serde_json::Value = provider
            .raw_request(
                "eth_sendTransaction".into(),
                serde_json::json!([{
                    "from": format!("{whale:?}"),
                    "to":   format!("{usdc:?}"),
                    "data": format!("0x{}", hex::encode(&calldata)),
                    "gas":  "0x30000",
                }]),
            )
            .await
            .map_err(|e| anyhow!("Whale transfer failed: {}", e))?;

        // Wait one block
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        // anvil_stopImpersonatingAccount
        let _: serde_json::Value = provider
            .raw_request("anvil_stopImpersonatingAccount".into(), serde_json::json!([format!("{whale:?}")]))
            .await
            .map_err(|e| anyhow!("anvil_stopImpersonatingAccount failed: {}", e))?;

        let new_bal = token_balance(provider, usdc, deployer).await?;
        info!("Deployer USDC balance after funding: {} (units)", new_bal);
        if new_bal.is_zero() {
            return Err(anyhow!("Failed to fund deployer with USDC from whale"));
        }
    }

    // ── Deploy ArbitrageExecutor ──
    let executor_bytecode = read_bytecode(&format!(
        "{}/out/ArbitrageExecutor.sol/ArbitrageExecutor.json",
        contract_dir
    ))?;

    // Constructor takes no args
    let executor_addr = deploy_contract(provider, executor_bytecode, vec![], deployer).await?;
    info!("ArbitrageExecutor deployed at {:?}", executor_addr);

    // ── Deploy MockRouters ──
    let routers = deploy_mock_routers(provider, contract_dir, usdc, weth, deployer).await?;

    // ── Whitelist routers on executor ──
    let executor = ArbitrageExecutor::new(executor_addr, provider);
    let router_a: Address = routers.router_a.parse()?;
    let router_b: Address = routers.router_b.parse()?;

    executor.setAllowedRouter(router_a, true).send().await?.get_receipt().await?;
    executor.setAllowedRouter(router_b, true).send().await?.get_receipt().await?;
    info!("Routers whitelisted on executor");

    // ── Fund executor with USDC for trading ──
    // Each arb uses 1000 USDC as capital; fund 50,000 USDC to ensure long-running tests
    let trade_amount = U256::from(50_000_000_000u128); // 50,000 USDC
    IERC20::new(usdc, provider)
        .transfer(executor_addr, trade_amount)
        .send()
        .await?
        .get_receipt()
        .await?;
    info!("Funded executor with 50,000 USDC");

    // ── Write state file ──
    let state = LabState {
        contract: format!("{:?}", executor_addr),
        usdc: format!("{:?}", usdc),
        weth: format!("{:?}", weth),
        routers,
        deployer: format!("{:?}", deployer),
        anvil_rpc: anvil_rpc.to_string(),
    };

    let state_json = serde_json::to_string_pretty(&state)?;
    std::fs::write(".lab-state.json", &state_json)?;
    info!("State written to .lab-state.json");

    println!("\n=== Setup Complete ===");
    println!("Contract: {:?}", executor_addr);
    println!("RouterA: {}", state.routers.router_a);
    println!("RouterB: {}", state.routers.router_b);

    Ok(state)
}
