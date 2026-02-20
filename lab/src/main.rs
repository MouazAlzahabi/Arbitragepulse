mod helpers;
mod mock_router;
mod scenario;
mod setup;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use anyhow::{anyhow, Result};
use dotenvy::dotenv;
use tracing::info;
use tracing_subscriber::EnvFilter;

const ANVIL_RPC: &str = "http://127.0.0.1:8545";
const CONTRACT_DIR: &str = "contract";

// Default Anvil account 0 private key
const DEFAULT_PRIVATE_KEY: &str =
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(
            std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()),
        ))
        .init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "setup" => cmd_setup().await?,
        "scenario" => cmd_scenario().await?,
        "test-reverts" => cmd_test_reverts().await?,
        _ => {
            println!("ArbitragePulse Lab — Rust Edition");
            println!();
            println!("Usage: lab <command>");
            println!();
            println!("Commands:");
            println!("  setup         Deploy ArbitrageExecutor + MockRouters on local Anvil");
            println!("  scenario      Run full E2E arb scenario (requires setup first)");
            println!("  test-reverts  Run 4 revert test scenarios");
            println!();
            println!("Prerequisites:");
            println!("  1. Compile contracts: cd ../contract && forge build");
            println!("  2. Start Anvil: anvil --fork-url <OPTIMISM_RPC> --chain-id 10 --block-time 2");
        }
    }

    Ok(())
}

async fn build_provider() -> Result<impl Provider + Clone> {
    let private_key = std::env::var("PRIVATE_KEY")
        .unwrap_or_else(|_| DEFAULT_PRIVATE_KEY.to_string());

    let signer: PrivateKeySigner = private_key.parse()?;
    let wallet = EthereumWallet::from(signer);

    let url = ANVIL_RPC.parse()
        .map_err(|e| anyhow!("Cannot parse Anvil URL {}: {}", ANVIL_RPC, e))?;

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(url);

    // Verify connection
    let chain_id = provider.get_chain_id().await
        .map_err(|e| anyhow!("Cannot connect to Anvil at {}: {}", ANVIL_RPC, e))?;
    info!("Connected to chain {} at {}", chain_id, ANVIL_RPC);

    Ok(provider)
}

fn deployer_address() -> Address {
    let private_key = std::env::var("PRIVATE_KEY")
        .unwrap_or_else(|_| DEFAULT_PRIVATE_KEY.to_string());

    let signer: PrivateKeySigner = private_key.parse().expect("Invalid private key");
    signer.address()
}

async fn cmd_setup() -> Result<()> {
    let provider = build_provider().await?;
    let deployer = deployer_address();
    info!("Deployer: {:?}", deployer);

    setup::run_setup(&provider, deployer, CONTRACT_DIR, ANVIL_RPC).await?;
    Ok(())
}

async fn cmd_scenario() -> Result<()> {
    let provider = build_provider().await?;

    // Load state from file (created by setup)
    let state = load_state()?;
    scenario::run_scenario(&provider, &state).await?;
    Ok(())
}

async fn cmd_test_reverts() -> Result<()> {
    let provider = build_provider().await?;
    let state = load_state()?;
    scenario::run_test_reverts(&provider, &state).await?;
    Ok(())
}

fn load_state() -> Result<setup::LabState> {
    let content = std::fs::read_to_string(".lab-state.json")
        .map_err(|_| anyhow!("No .lab-state.json found. Run `lab setup` first."))?;
    serde_json::from_str(&content).map_err(|e| anyhow!("Invalid .lab-state.json: {}", e))
}
