use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use anyhow::{anyhow, Result};
use serde_json::Value;

// ─── ERC20 minimal ABI ────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
    }
}

// ─── Artifact reading ─────────────────────────────────────────────────────────

/// Read bytecode from a Foundry artifact JSON file.
pub fn read_bytecode(artifact_path: &str) -> Result<Bytes> {
    let content = std::fs::read_to_string(artifact_path)
        .map_err(|e| anyhow!("Cannot read artifact '{}': {}", artifact_path, e))?;

    let artifact: Value = serde_json::from_str(&content)
        .map_err(|e| anyhow!("Invalid JSON in '{}': {}", artifact_path, e))?;

    let bytecode = artifact["bytecode"]["object"]
        .as_str()
        .ok_or_else(|| anyhow!("No bytecode.object in '{}'", artifact_path))?;

    let hex = bytecode.strip_prefix("0x").unwrap_or(bytecode);
    let bytes = hex::decode(hex)
        .map_err(|e| anyhow!("Invalid hex bytecode in '{}': {}", artifact_path, e))?;

    Ok(Bytes::from(bytes))
}

// ─── Contract deployment ──────────────────────────────────────────────────────

/// Deploy a contract with pre-encoded bytecode + constructor args.
///
/// Uses a raw `eth_sendTransaction` RPC call (bypassing the wallet filler) because
/// alloy 1.7's WalletFiller rejects create transactions that have no `to` field.
/// This works on Anvil where all default accounts are pre-unlocked.
pub async fn deploy_contract<P: Provider>(
    provider: &P,
    bytecode: Bytes,
    constructor_args: Vec<u8>,
    from: Address,
) -> Result<Address> {
    let mut deploy_data = bytecode.to_vec();
    deploy_data.extend_from_slice(&constructor_args);
    let data_hex = format!("0x{}", hex::encode(&deploy_data));

    // Raw eth_sendTransaction — Anvil signs on its side for unlocked accounts.
    let tx_hash: serde_json::Value = provider
        .raw_request(
            "eth_sendTransaction".into(),
            serde_json::json!([{ "from": format!("{from:?}"), "data": data_hex, "gas": "0x4C4B40" }]),
        )
        .await
        .map_err(|e| anyhow!("Deploy send failed: {}", e))?;

    let hash: B256 = tx_hash
        .as_str()
        .ok_or_else(|| anyhow!("eth_sendTransaction returned non-string tx hash"))?
        .parse()
        .map_err(|e| anyhow!("Cannot parse tx hash: {}", e))?;

    // Poll for receipt — Anvil mines every 2s with --block-time
    let receipt = {
        let mut attempts = 0u32;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            match provider.get_transaction_receipt(hash).await {
                Ok(Some(r)) => break r,
                Ok(None) => {
                    attempts += 1;
                    if attempts > 20 {
                        return Err(anyhow!("Timed out waiting for deploy receipt {:?}", hash));
                    }
                }
                Err(e) => return Err(anyhow!("Deploy receipt fetch failed: {}", e)),
            }
        }
    };

    receipt
        .contract_address
        .ok_or_else(|| anyhow!("No contract address in deploy receipt"))
}

// ─── Token helpers ────────────────────────────────────────────────────────────

/// Get ERC20 balance for an account.
pub async fn token_balance<P: Provider>(
    provider: &P,
    token: Address,
    account: Address,
) -> Result<U256> {
    let result = IERC20::new(token, provider)
        .balanceOf(account)
        .call()
        .await?;
    Ok(result)
}

/// Transfer tokens using impersonation (Anvil only).
/// Impersonates `from` to call transfer(to, amount).
#[allow(dead_code)]
pub async fn impersonate_transfer<P: Provider>(
    provider: &P,
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
) -> Result<()> {
    use alloy::sol_types::SolCall;

    // Encode transfer(to, amount)
    let calldata = IERC20::transferCall { to, amount }.abi_encode();

    // Use eth_sendTransaction with from field (Anvil impersonation mode)
    let tx = TransactionRequest::default()
        .from(from)
        .to(token)
        .input(Bytes::from(calldata).into());

    provider
        .send_transaction(tx)
        .await
        .map_err(|e| anyhow!("Impersonate transfer failed: {}", e))?
        .get_receipt()
        .await
        .map_err(|e| anyhow!("Transfer receipt failed: {}", e))?;

    Ok(())
}

/// Approve a spender from the default signer.
#[allow(dead_code)]
pub async fn approve_token<P: Provider>(
    provider: &P,
    token: Address,
    spender: Address,
    amount: U256,
) -> Result<()> {
    IERC20::new(token, provider)
        .approve(spender, amount)
        .send()
        .await?
        .get_receipt()
        .await?;
    Ok(())
}

/// ETH transfer (Anvil — fund an account).
#[allow(dead_code)]
pub async fn fund_eth<P: Provider>(
    provider: &P,
    to: Address,
    amount_eth: u64,
) -> Result<()> {
    let amount = U256::from(amount_eth) * U256::from(1_000_000_000_000_000_000u128);
    let tx = TransactionRequest::default()
        .to(to)
        .value(amount);

    provider
        .send_transaction(tx)
        .await?
        .get_receipt()
        .await?;
    Ok(())
}
