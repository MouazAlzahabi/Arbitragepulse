use anyhow::{anyhow, Result};
use dotenvy::dotenv;
use serde::Deserialize;

// ─── Raw YAML config structures ───────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
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
    /// Max number of Multicall3 chunks fired concurrently per scan.
    /// Free Alchemy plan (330 CU/s): use 3. Growth plan (3000 CU/s): use 10–12.
    /// Each chunk = 1 eth_call (26 CU). Default: 3.
    #[serde(default = "default_rpc_concurrency")]
    pub rpc_concurrency: usize,
    /// How often to scan for opportunities (milliseconds).
    /// Decoupled from block_time_ms — set lower to scan more frequently.
    /// Default: 1000ms (60 scans/min). Must be ≤ block_time_ms for meaningful effect.
    #[serde(default = "default_scan_interval_ms")]
    pub scan_interval_ms: u64,
    /// Reserve change threshold (basis points) to classify a V2/Solidly Sync event as "large".
    /// A large swap bypasses the burst guard and clears cooldowns for affected tokens.
    /// Default: 200 (2% reserve shift). Set to 0 to disable.
    #[serde(default = "default_large_swap_threshold_bps")]
    pub large_swap_threshold_bps: u32,
    /// sqrtPriceX96 change threshold (basis points) for V3 Swap events.
    /// 50 = 0.5% sqrtP change (~1% price impact). Set to 0 to disable.
    #[serde(default = "default_large_v3_threshold_bps")]
    pub large_v3_threshold_bps: u32,
    /// Extra HTTP RPC URLs to broadcast signed transactions to simultaneously.
    /// The same signed tx bytes are sent to all endpoints concurrently; the first
    /// success is used. Reduces submission latency by racing multiple paths to the
    /// sequencer. Leave empty to use only the primary http_rpc.
    #[serde(default)]
    pub submission_rpcs: Vec<String>,
    /// Skip QuoterV2 verification (phases 1.5/1.5b/1.5c) and submit immediately
    /// after Phase 1 local spot quotes. Saves ~80-160ms per scan — enough to land
    /// in the same block as the detected opportunity on FCFS chains (Base).
    /// Tradeoff: some txs revert ("Too little received") when local quotes are
    /// optimistic. The contract's min_profit floor is the only safety net.
    /// Set to false to restore full QuoterV2 verification. Default: false.
    #[serde(default)]
    pub optimistic_submission: bool,
}

fn default_rpc_concurrency() -> usize {
    3
}

fn default_scan_interval_ms() -> u64 {
    1000
}

fn default_min_swap_amount() -> u128 {
    100_000_000_000_000_000 // 1e17 = 0.1 ETH or 100k USDC (6 decimals)
}

fn default_large_swap_threshold_bps() -> u32 {
    200 // 2% reserve shift
}

fn default_large_v3_threshold_bps() -> u32 {
    50 // 0.5% sqrtP change (~1% price impact)
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RouterType {
    V2,
    V3,
    /// Solidly / ve(3,3) fork (Lynex, Nile, Velodrome-style).
    /// Uses Route[]{from, to, stable} interface.
    /// fee field encodes pool type: 0=volatile, 1=stable.
    Solidly,
    /// SyncSwap: per-pool quoting via pool.getAmountOut(tokenIn, amountIn, sender).
    /// The router `address` in config points to the Pool Factory.
    /// Quoting only — execution requires major contract changes (not yet supported).
    SyncSwap,
    /// Aerodrome V2 (Base): uses Route{from, to, stable, factory} (4-field struct).
    /// Same pool discovery and local quoting as Solidly, different execution interface.
    /// fee=0 → volatile (xy=k), fee=1 → stable (x³y+y³x=k).
    /// factory=address(0) in Route → Aerodrome uses its own default factory.
    Aerodrome,
}

impl Default for RouterType {
    fn default() -> Self {
        RouterType::V2
    }
}

#[derive(Debug, Deserialize, Clone)]
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
    /// Per-router QuoterV2 address for V3 quotes.
    /// If set, overrides the chain-level quoter_v2_address for this specific router.
    /// Required when multiple V3 protocols on the same chain each have their own QuoterV2
    /// (e.g., PancakeSwap V3 vs Uniswap V3 on Linea).
    #[serde(default)]
    pub quoter_address: Option<String>,
    /// V3 pool factory address. Required for V3 pool discovery at startup.
    /// V3 swap routers don't expose factory() on-chain, so this must be configured.
    #[serde(default)]
    pub factory_address: Option<String>,
    /// Fee (basis points) for Solidly stable pools (x³y+xy³=k).
    /// If not set, falls back to fee_bps. Aerodrome stable pools use ~1 bps (0.01%).
    #[serde(default)]
    pub stable_fee_bps: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
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
    /// Per-pair profit floor in USD. Overrides chain-level min_profit_usd when set.
    /// Use to raise the bar on competitive pairs (e.g. USDC/WETH) while keeping it
    /// low for less-contested pairs (ZORA, VIRTUAL) that share the same chain config.
    #[serde(default)]
    pub min_profit_usd: Option<f64>,
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
    /// Whether to start in dry-run mode (detect-only, no tx submission).
    /// Set DRY_RUN=false in .env to start live. Default: true (safe default).
    pub dry_run: bool,
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
        dry_run: std::env::var("DRY_RUN")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true), // safe default: start dry unless explicitly set to false
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
