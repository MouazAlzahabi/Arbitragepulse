use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolValue;
use anyhow::{anyhow, Result};
use tracing::info;

use crate::helpers::{deploy_contract, read_bytecode};

// ─── MockRouter ABI (V2-style) ────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IMockRouter {
        function rateNumerator() external view returns (uint256);
        function rateDenominator() external view returns (uint256);
    }
}

// ─── MockV3Router ABI ─────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IMockV3Router {
        function rateNumerator() external view returns (uint256);
        function rateDenominator() external view returns (uint256);
    }
}

// ─── Deployed mock router state ───────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MockRouterState {
    pub router_a: String,  // address hex
    pub router_b: String,
    pub router_a_type: String, // "v2" or "v3"
    pub router_b_type: String,
}

/// Deploy two MockRouters (V2 and V2) with a 12.5% price spread.
///
/// MockRouter A: 450_000_000 / 1 → 1 USDC → 0.45 WETH (2222 USDC/WETH, cheap)
/// MockRouter B: 2_500_000_000 / 1e18 → 1 WETH → 2500 USDC (market price)
///
/// Arb flow: 1000 USDC → 0.45 WETH (routerA) → 1125 USDC (routerB) → +125 profit
pub async fn deploy_mock_routers<P: Provider>(
    provider: &P,
    contract_dir: &str,
    usdc: Address,
    weth: Address,
    deployer: Address,
) -> Result<MockRouterState> {
    // ── Deploy MockRouter A (V2, 450_000_000 / 1) ──
    let bytecode_a = read_bytecode(&format!(
        "{}/out/MockRouter.sol/MockRouter.json",
        contract_dir
    ))?;

    // Constructor args: (uint256 rateNumerator, uint256 rateDenominator)
    // MockRouter A: USDC→WETH direction: 450_000_000 USDC units → 1 WETH unit
    // This means: input = amountIn in USDC (1e6 units), output = amountOut in WETH (1e18 units)
    // Rate: 450_000_000 / 1e18 ≈ 0.00000000045 per unit — but MockRouter uses integer math
    // Actual rates from the working lab: routerA (450_000_000, 1), routerB (2_500_000_000, 1e18)
    let args_a = (U256::from(450_000_000u128), U256::from(1u128)).abi_encode();
    let router_a = deploy_contract(provider, bytecode_a, args_a, deployer).await?;
    info!("MockRouter A deployed at {:?}", router_a);

    // ── Deploy MockRouter B (V2, 2_500_000_000 / 1e18) ──
    let bytecode_b = read_bytecode(&format!(
        "{}/out/MockRouter.sol/MockRouter.json",
        contract_dir
    ))?;

    let args_b = (
        U256::from(2_500_000_000u128),
        U256::from(1_000_000_000_000_000_000u128), // 1e18
    )
        .abi_encode();
    let router_b = deploy_contract(provider, bytecode_b, args_b, deployer).await?;
    info!("MockRouter B deployed at {:?}", router_b);

    // ── Wrap 201 ETH → WETH so we can seed RouterA ──
    // Deployer has ETH (Anvil funded) but WETH is an ERC20 that must be wrapped.
    // Call WETH.deposit{value: 201 ether}() — selector 0xd0e30db0.
    let wrap_amount_hex = format!("0x{:x}", 201u128 * 1_000_000_000_000_000_000u128);
    let _: serde_json::Value = provider
        .raw_request(
            "eth_sendTransaction".into(),
            serde_json::json!([{
                "from":  format!("{deployer:?}"),
                "to":    format!("{weth:?}"),
                "data":  "0xd0e30db0",
                "value": wrap_amount_hex,
                "gas":   "0x30000",
            }]),
        )
        .await
        .map_err(|e| anyhow!("WETH wrap failed: {}", e))?;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    info!("Wrapped 201 ETH → WETH");

    // ── Seed MockRouter A with WETH (so it can pay out WETH) ──
    // RouterA receives USDC and pays out WETH → needs WETH funded
    // Each arb uses 0.45 WETH; seed 200 WETH → ~440 arbs before depletion
    seed_router(provider, weth, router_a, U256::from(200u128) * U256::from(1_000_000_000_000_000_000u128)).await?;
    info!("Seeded RouterA with 200 WETH");

    // ── Seed MockRouter B with USDC (so it can pay out USDC) ──
    // RouterB receives WETH and pays out USDC → needs USDC funded
    // Each arb pays out 1125 USDC; seed 500,000 USDC → ~440 arbs before depletion
    seed_router(provider, usdc, router_b, U256::from(500_000_000_000u128)).await?; // 500,000 USDC (6 decimals)
    info!("Seeded RouterB with 500,000 USDC");

    Ok(MockRouterState {
        router_a: format!("{:?}", router_a),
        router_b: format!("{:?}", router_b),
        router_a_type: "v2".into(),
        router_b_type: "v2".into(),
    })
}

/// Transfer tokens from the signer to a router address.
async fn seed_router<P: Provider>(
    provider: &P,
    token: Address,
    router: Address,
    amount: U256,
) -> Result<()> {
    use crate::helpers::IERC20;
    IERC20::new(token, provider)
        .transfer(router, amount)
        .send()
        .await?
        .get_receipt()
        .await?;
    Ok(())
}
