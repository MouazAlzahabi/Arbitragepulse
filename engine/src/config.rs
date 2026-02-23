use anyhow::{anyhow, Result};
use dotenvy::dotenv;
use serde::Deserialize;

// ─── Raw YAML config structures ───────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ChainConfig {
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    pub ws_rpc: String,
    pub http_rpc: String,
    pub native_currency: String,
    pub wrapped_native: String,
    pub block_time_ms: u64,
    pub contract_address: String,
    pub min_native_balance: f64,
    pub min_profit_usd: f64,
    pub deadline_seconds: u64,
    /// Optional QuoterV2 address for V3 quoting (chain-specific)
    #[serde(default)]
    pub quoter_v2_address: Option<String>,
    /// Minimum swap amount to trigger opportunity evaluation (filters dust swaps).
    /// Default: 1e17 (0.1 ETH or 100k USDC). Set to 0 to disable filtering.
    #[serde(default = "default_min_swap_amount")]
    pub min_swap_amount_filter: u128,
    /// Fallback WS endpoints tried in order if the primary ws_rpc fails.
    /// Leave empty to use only the primary.
    #[serde(default)]
    pub ws_rpc_fallbacks: Vec<String>,
}

fn default_min_swap_amount() -> u128 {
    100_000_000_000_000_000 // 1e17 = 0.1 ETH or 100k USDC (6 decimals)
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RouterType {
    V2,
    V3,
}

impl Default for RouterType {
    fn default() -> Self {
        RouterType::V2
    }
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct RouterConfig {
    pub id: String,
    pub name: String,
    pub chain_id: u64,
    pub address: String,
    #[serde(rename = "type", default)]
    pub router_type: RouterType,
    #[serde(default)]
    pub fee_bps: u32,
    /// V3 fee tiers to try (500, 3000, 10000)
    #[serde(default)]
    pub fee_tiers: Vec<u32>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct PairConfig {
    pub id: String,
    pub chain_id: u64,
    pub token_in: String,
    pub token_out: String,
    pub token_in_symbol: String,
    pub token_out_symbol: String,
    pub token_in_decimals: u8,
    pub token_out_decimals: u8,
    pub trade_amount: String,
    pub min_swap_size: String,
    /// Optional cap: if set, actual trade size = min(trade_amount, max_trade)
    #[serde(default)]
    pub max_trade: Option<String>,
    #[serde(default)]
    pub watch_pools: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AppConfigRaw {
    chains: Vec<ChainConfig>,
    routers: Vec<RouterConfig>,
    #[serde(default)]
    pairs: Vec<PairConfig>,
}

// ─── Parsed config (validated) ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub chains: Vec<ChainConfig>,
    pub routers: Vec<RouterConfig>,
    pub pairs: Vec<PairConfig>,
}

// ─── Env ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Env {
    pub private_key: String,
    pub port: u16,
    pub log_level: String,
    pub api_key: String,
    pub config_path: String,
}

pub fn load_env() -> Result<Env> {
    let _ = dotenv(); // OK if .env missing

    let private_key = std::env::var("PRIVATE_KEY")
        .map_err(|_| anyhow!("PRIVATE_KEY not set in .env"))?;

    if private_key.contains("YOUR_") || private_key.is_empty() {
        return Err(anyhow!("PRIVATE_KEY not configured"));
    }

    Ok(Env {
        private_key,
        port: std::env::var("PORT")
            .unwrap_or_else(|_| "3000".into())
            .parse()
            .unwrap_or(3000),
        log_level: std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()),
        api_key: std::env::var("API_KEY").unwrap_or_default(),
        config_path: std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.yaml".into()),
    })
}

/// Replace `${VAR_NAME}` placeholders in YAML content with env var values.
fn substitute_env_vars(content: String) -> String {
    let re = regex::Regex::new(r"\$\{([A-Z0-9_]+)\}").expect("valid regex");
    re.replace_all(&content, |caps: &regex::Captures| {
        std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
    })
    .into_owned()
}

pub fn load_config(path: &str) -> Result<AppConfig> {
    let raw_content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("Cannot read config file '{}': {}", path, e))?;

    let content = substitute_env_vars(raw_content);

    let raw: AppConfigRaw = serde_yaml::from_str(&content)
        .map_err(|e| anyhow!("Invalid config YAML: {}", e))?;

    // Validate contract addresses
    for chain in &raw.chains {
        if chain.enabled && chain.contract_address.contains("YOUR_") {
            return Err(anyhow!(
                "Chain '{}' is enabled but contract_address is not set",
                chain.name
            ));
        }
    }

    Ok(AppConfig {
        chains: raw.chains,
        routers: raw.routers,
        pairs: raw.pairs,
    })
}
