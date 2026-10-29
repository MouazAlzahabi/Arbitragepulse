use anyhow::{anyhow, Result};
use dotenvy::dotenv;
use serde::Deserialize;

// ─── Raw YAML config structures ───────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    pub id: u64,
    pub name: String,
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
    /// Flashblocks-aware WS endpoint (Base: `wss://mainnet-preconf.base.org`). When set,
    /// the engine also subscribes to `pendingLogs` there: pool events from 200 ms
    /// pre-confirmed sub-blocks, ~1.2 s before the sealed block (measured). Read-only, no
    /// key, no wallet. Unset = off.
    #[serde(default)]
    pub preconf_ws_rpc: Option<String>,
    /// Flashblocks-aware HTTP endpoint. When set, QuoterV2/Multicall3 reads, the optimizer,
    /// pre-flight and dry-run simulation call it at block tag `pending`, pricing the same
    /// pre-confirmed state the `preconf_ws_rpc` logs moved the pool cache to. Unset =
    /// `http_rpc` / the WS provider at `latest`.
    #[serde(default)]
    pub preconf_http_rpc: Option<String>,
    /// Fallback WS endpoints tried in order if the primary ws_rpc fails.
    /// Leave empty to use only the primary.
    #[serde(default)]
    pub ws_rpc_fallbacks: Vec<String>,
    /// Token addresses whose pools are watched only on the keyless `preconf_ws_rpc` feed,
    /// not on the primary (metered) ws_rpc log subscription: any pool with one of these on
    /// either side. Keeps added tokens off the paid event budget. Ignored (all pools on the
    /// primary feed, with a warning) when `preconf_ws_rpc` is unset.
    #[serde(default)]
    pub preconf_only_tokens: Vec<String>,
    /// Trade with the start amount borrowed from the Uniswap V4 PoolManager and repaid in the
    /// same transaction (executor *Borrowed entry points: no fee, no interest). Trades no
    /// longer need the contract's own balance: the balance guards and "unfunded" warnings
    /// are off and dry-run needs no assumed balance. Needs an executor deployed with the
    /// PoolManager (contract v4).
    #[serde(default)]
    pub borrow: bool,
    /// Total Multicall3 chunks in flight on the quote endpoint per chain, shared by all
    /// batches joined concurrently (Phase 1.5/1.5b/1.5c, prefetch, triangular verify).
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
    /// Tradeoff: some txs revert (NotProfitable) when local quotes are
    /// optimistic. The contract's min_profit floor is the only safety net.
    /// Set to false to restore full QuoterV2 verification. Default: false.
    #[serde(default)]
    pub optimistic_submission: bool,
    /// Run `eth_call` with the assembled tx immediately before `eth_sendRawTransaction`.
    /// On simulated revert, skips broadcast (saves gas) and logs JSON-RPC revert data /
    /// decoded `Error(string)` when the node returns it. Adds one RPC round-trip
    /// (~100–250ms); default **false**. Enable briefly on Base while debugging when
    /// explorers show only «Fail» and off-chain simulation tools misbehave.
    #[serde(default)]
    pub preflight_eth_call: bool,
    /// When false, skip the two-round adaptive size optimizer and trade at the configured
    /// trade_amount directly. Saves ~80-100ms per opportunity on FCFS chains (Base).
    /// Set to true to re-enable. Default: true (optimizer on).
    #[serde(default = "default_true")]
    pub optimize_size: bool,
    /// EIP-1559 maxPriorityFeePerGas (tip) in Wei added on top of baseFee.
    /// Higher values land earlier in the block when sequencers order by effective gas price.
    /// Base L2 competing bots use ~15_000_000 Wei (15 Mwei = 0.015 gwei).
    /// Default: 1_000_000 Wei (1 Mwei = 0.001 gwei) — conservative, good for non-competitive chains.
    /// Set to 15_000_000 on Base to reach position 1-5 vs 50-200.
    #[serde(default = "default_priority_fee_wei")]
    pub priority_fee_wei: u128,
    /// Enable pending transaction monitoring for same-block execution.
    /// Subscribes to the mempool and fires an arb TX when a large swap is detected
    /// before it confirms, matching the trigger TX's tip so both land in the same block.
    /// Requires a WS endpoint that exposes the mempool (Alchemy, QuickNode).
    /// Default: false.
    #[serde(default)]
    pub pending_tx_monitoring: bool,
    /// When true, only submit TXs triggered by the pending monitor (which uses dynamic
    /// tip matching for same-block landing). Block-scan, poll-tick, and swap-event
    /// triggered scans will detect opportunities and log them but skip TX submission.
    /// Prevents position-500+ failures from block-scan submissions on FCFS chains (Base).
    /// Scans still run (pool state stays fresh). Default: false.
    #[serde(default)]
    pub pending_only_submission: bool,
    /// Poll `eth_getLogs` on an interval as a fallback when WS `eth_subscribe(logs)` is
    /// unavailable or unreliable on the provider. On Base with Alchemy WS,
    /// leave **false** — WS updates are faster and avoid duplicate RPC + cache writes.
    #[serde(default)]
    pub poll_logs_fallback: bool,
    /// Minimum profit in USD required to submit a triangular arb. Default: same as min_profit_usd.
    /// Routes with a V3 leg are QuoterV2-verified first (cached sqrtPriceX96 overestimates
    /// by ~0.30%), so this compares against verified profit. With `optimistic_submission`
    /// they're submitted unverified on spot quotes, after deducting the ~0.30% overestimate.
    #[serde(default)]
    pub min_triangular_profit_usd: Option<f64>,
}

fn default_rpc_concurrency() -> usize {
    3
}

fn default_scan_interval_ms() -> u64 {
    1000
}

fn default_true() -> bool { true }

fn default_priority_fee_wei() -> u128 {
    1_000_000 // 1 Mwei = 0.001 gwei — safe default for non-competitive chains
}

fn default_large_swap_threshold_bps() -> u32 {
    200 // 2% reserve shift
}

fn default_large_v3_threshold_bps() -> u32 {
    50 // 0.5% sqrtP change (~1% price impact)
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RouterType {
    V2,
    V3,
    /// Aerodrome V2 (Base) — Solidly-family ve(3,3) DEX with Route{from, to, stable, factory}
    /// (4-field struct). Volatile (xy=k) and stable (x³y+y³x=k) pools.
    /// fee=0 → volatile, fee=1 → stable.
    /// factory=address(0) in Route → Aerodrome uses its own default factory.
    Aerodrome,
}

impl Default for RouterType {
    fn default() -> Self {
        RouterType::V2
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    pub id: String,
    pub name: String,
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
    /// Required when multiple V3 protocols on the chain each have their own QuoterV2
    /// (e.g., PancakeSwap V3 vs Uniswap V3).
    #[serde(default)]
    pub quoter_address: Option<String>,
    /// V3 pool factory address. Required for V3 pool discovery at startup.
    /// V3 swap routers don't expose factory() on-chain, so this must be configured.
    #[serde(default)]
    pub factory_address: Option<String>,
    /// Fee (basis points) for Solidly stable pools (x³y+xy³=k).
    /// If not set, falls back to fee_bps. Aerodrome's default stable fee is 5 bps (0.05%).
    /// For Aerodrome this (and fee_bps) is only the fallback: discovery reads each pool's
    /// fee from factory.getFee(pool, stable).
    #[serde(default)]
    pub stable_fee_bps: Option<u32>,
    /// Aerodrome Slipstream (with `type: "v3"`): concentrated-liquidity pools keyed by tick
    /// spacing. `fee_tiers` then lists tick spacings (1, 10, 50, 100, 200, 2000), which travel
    /// in every `fee` field (pool key, quote, calldata); each pool's swap fee is read at
    /// discovery. Needs the Slipstream factory and quoter, and the executor allowlisting the
    /// router as type 5.
    #[serde(default)]
    pub slipstream: bool,
    /// Trade on this venue's pools directly instead of through `address` (executor router
    /// types 6 DirectCL for `type: "v3"`, 7 DirectPair for `"v2"` / `"aerodrome"`). Every
    /// pool the engine uses must then be allowlisted on the executor (setRouters); the
    /// startup check lists the missing ones. `address` only identifies the venue.
    #[serde(default)]
    pub direct: bool,
    /// Aero V2 (with `type: "aerodrome"` and `direct: true`): volatile pools come from
    /// `factory_address` and stable pools from this factory, both via `getPool(a, b)`; each
    /// pool's fee is its factory fee module's `customFee(pool)` (0 = the factory default,
    /// `fee_bps` / `stable_fee_bps`).
    #[serde(default)]
    pub stable_factory_address: Option<String>,
    /// `quoter_address` is Aero's Metaquoter — `quoteExactInput(address[] pools, tokenIn,
    /// amountIn)` — instead of a QuoterV2 (needed for Aero CL pools, `slipstream: true`).
    #[serde(default)]
    pub metaquoter: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    pub id: String,
    pub token_in: String,
    pub token_out: String,
    pub token_in_symbol: String,
    pub token_out_symbol: String,
    pub token_in_decimals: u8,
    pub token_out_decimals: u8,
    pub trade_amount: String,
    /// Optional cap: if set, actual trade size = min(trade_amount, max_trade)
    #[serde(default)]
    pub max_trade: Option<String>,
    /// Per-pair profit floor in USD. Overrides chain-level min_profit_usd when set.
    /// Use to raise the bar on competitive pairs (e.g. USDC/WETH) while keeping it
    /// low for less-contested pairs (ZORA, VIRTUAL) that share the same chain config.
    #[serde(default)]
    pub min_profit_usd: Option<f64>,
}

/// One config file = one chain = one engine instance. To run another chain, deploy
/// another instance with its own config file, `.env`, `PORT` and database.
#[derive(Debug, Deserialize)]
struct AppConfigRaw {
    chain: Option<ChainConfig>,
    routers: Vec<RouterConfig>,
    #[serde(default)]
    pairs: Vec<PairConfig>,
    #[serde(default)]
    tuning: Tuning,
}

// ─── Parsed config (validated) ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub chain: ChainConfig,
    pub routers: Vec<RouterConfig>,
    pub pairs: Vec<PairConfig>,
    pub tuning: Tuning,
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
    /// SQLite trade database. One per instance (DB_PATH, default `trades.db`).
    pub db_path: String,
    /// API_INSECURE=1: allow an empty API_KEY (no HTTP auth). Local development only.
    pub api_insecure: bool,
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
        api_insecure: std::env::var("API_INSECURE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        config_path: std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.yaml".into()),
        dry_run: std::env::var("DRY_RUN")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true), // safe default: start dry unless explicitly set to false
        db_path: std::env::var("DB_PATH").unwrap_or_else(|_| "trades.db".into()),
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

    parse_config(&content)
}

/// Parse and validate config YAML (after env substitution).
pub fn parse_config(content: &str) -> Result<AppConfig> {
    // Old multi-chain files: say so before any field-level error (e.g. a removed
    // router type) would hide the real reason.
    let doc: serde_yaml::Value = serde_yaml::from_str(content)
        .map_err(|e| anyhow!("Invalid config YAML: {}", e))?;
    // Keys removed from `tuning:` — say what replaced them (serde would only say "unknown field").
    if let Some(t) = doc.get("tuning") {
        if t.get("router_min_out_slack_bps").is_some() || t.get("sell_leg_minprofit_mul_bps").is_some() {
            return Err(anyhow!(
                "tuning.router_min_out_slack_bps / sell_leg_minprofit_mul_bps were merged into \
                 tuning.onchain_min_profit_bps = (10000 - slack) × mul / 10000 (old defaults 200 / 6200 → 6076)"
            ));
        }
    }
    if doc.get("chains").is_some() {
        return Err(anyhow!(
            "`chains:` list is no longer supported — one chain per instance. \
             Use a single `chain:` block, and run another instance for another chain."
        ));
    }
    let raw: AppConfigRaw = serde_yaml::from_value(doc)
        .map_err(|e| anyhow!("Invalid config YAML: {}", e))?;
    let chain = raw.chain.ok_or_else(|| anyhow!("config is missing the `chain:` block"))?;
    if chain.contract_address.contains("YOUR_") {
        return Err(anyhow!("Chain '{}': contract_address is not set", chain.name));
    }
    for r in &raw.routers {
        if r.slipstream && (r.router_type != RouterType::V3 || r.factory_address.is_none() || r.quoter_address.is_none()) {
            return Err(anyhow!(
                "router '{}': slipstream: true needs type \"v3\", factory_address (Slipstream CL factory) and quoter_address (Slipstream quoter)",
                r.id
            ));
        }
        if r.slipstream && r.fee_tiers.iter().any(|&t| t == 0 || t > 16_384) {
            return Err(anyhow!("router '{}': Slipstream fee_tiers are tick spacings, 1..=16384", r.id));
        }
        if r.metaquoter && !(r.router_type == RouterType::V3 && r.quoter_address.is_some()) {
            return Err(anyhow!("router '{}': metaquoter: true needs type \"v3\" and quoter_address (Aero Metaquoter)", r.id));
        }
        if r.stable_factory_address.is_some() && !(r.router_type == RouterType::Aerodrome && r.direct && r.factory_address.is_some()) {
            return Err(anyhow!(
                "router '{}': stable_factory_address (Aero V2) needs type \"aerodrome\", direct: true and factory_address (the volatile factory)",
                r.id
            ));
        }
        if let Some(f) = r.fee_tiers.iter().find(|&&f| f == 0 || f >= 1_000_000) {
            return Err(anyhow!(
                "router '{}': fee tier {} is out of range — V3 fee tiers are 1..=999999 (hundredths of a bip: 100, 500, 3000, 10000)",
                r.id, f
            ));
        }
    }
    if let Some(u) = chain.preconf_http_rpc.as_deref() {
        u.parse::<url::Url>()
            .map_err(|e| anyhow!("chain.preconf_http_rpc is not a URL ({}): {}", crate::util::redact_url(u), e))?;
    }
    if !(10..=600).contains(&chain.deadline_seconds) {
        return Err(anyhow!("chain.deadline_seconds must be 10..=600 (got {})", chain.deadline_seconds));
    }
    {
        use crate::strategy::{NATIVE_SYMBOLS, USD_STABLE_SYMBOLS};
        for p in &raw.pairs {
            let s = p.token_in_symbol.as_str();
            let stable = USD_STABLE_SYMBOLS.iter().any(|x| x.eq_ignore_ascii_case(s));
            let native = NATIVE_SYMBOLS.iter().any(|x| x.eq_ignore_ascii_case(s))
                && p.token_in.eq_ignore_ascii_case(&chain.wrapped_native);
            if !stable && !native {
                return Err(anyhow!(
                    "pair '{}': token_in_symbol '{}' has no USD price — token_in must be a USD stablecoin ({}) or the wrapped native token ({}) at chain.wrapped_native",
                    p.id, s, USD_STABLE_SYMBOLS.join(", "), NATIVE_SYMBOLS.join(", ")
                ));
            }
        }
    }
    raw.tuning.validate(chain.block_time_ms)?;

    Ok(AppConfig {
        chain,
        routers: raw.routers,
        pairs: raw.pairs,
        tuning: raw.tuning,
    })
}

/// An empty API_KEY leaves every route (dry-run toggle, restart, stats reset) open to anyone
/// who can reach PORT — refuse unless explicitly allowed for local development.
pub fn check_api_key(api_key: &str, insecure: bool) -> Result<()> {
    if api_key.trim().is_empty() && !insecure {
        Err(anyhow!("API_KEY is empty — refusing to start: the HTTP API (pause, restart, dry-run toggle, stats reset) would have no authentication. Set API_KEY=<secret> in the instance env (e.g. `openssl rand -hex 32`) and use the same value in the dashboard, or set API_INSECURE=1 for local development only."))
    } else {
        Ok(())
    }
}

/// Fail fast when the RPC serves a different chain than the config describes (e.g. a
/// mainnet config pointed at a testnet RPC, or the reverse).
pub fn check_chain_id(configured: u64, rpc_reported: u64, chain_name: &str) -> Result<()> {
    if configured == rpc_reported {
        Ok(())
    } else {
        Err(anyhow!(
            "RPC for '{}' reports chain id {} but the config says {} — refusing to start \
             (wrong RPC URL or wrong config file?)",
            chain_name, rpc_reported, configured
        ))
    }
}

// ─── Tuning ───────────────────────────────────────────────────────────────────
// Every timing and threshold the engine uses, in one place. All fields default to the
// values tuned on Base (2 s blocks); override any of them in the config's `tuning:`
// section. Fields marked "block-relative" default to a multiple of `block_time_ms`, so a
// chain with a different block interval gets sensible values without overriding them.
// Read once at startup (config hot-reload does not change tuning — restart instead).

#[derive(Debug, Deserialize, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Tuning {
    // ── Execution ──
    /// Gas limit for a 2-hop arb tx.
    pub gas_limit_2hop: u64,
    /// Gas limit for a triangular (3-hop) arb tx.
    pub gas_limit_triangular: u64,
    /// Extra gas for a borrowed trade (V4 unlock / take / sync / settle + repay transfer).
    pub gas_borrow_extra: u64,
    /// Extra gas per leg on a hooked venue (Aero CL, `metaquoter: true`): its swap costs
    /// ~250-290k and its hook reverts "IG" unless 250k gas is left when it runs.
    pub gas_per_hooked_leg: u64,
    /// On-chain `minProfit` as bps of the gas-cost floor (6076 = 60.76%). The contract
    /// reverts `NotProfitable` unless final balance ≥ starting balance + minProfit; < 10000 leaves room for
    /// same-block drift, with the off-chain net-profit check as the economic guard.
    pub onchain_min_profit_bps: u64,
    /// Margin (bps) on the last known gas price when a fetch fails.
    pub gas_fallback_margin_bps: u64,
    /// Max age of the last known gas price to reuse it (ms).
    pub gas_fallback_max_age_ms: u64,
    /// Priority fee as a share of each trade's expected gross profit (bps; 0 = off — the
    /// fixed `chain.priority_fee_wei` is used). Inside a 200 ms flashblock txs are ordered
    /// by priority fee, so a higher tip wins position — but it is paid even when the tx
    /// reverts, so every lost race costs more. Tune from `arb_lost_race_total`. Max 9000.
    pub tip_profit_share_bps: u64,
    /// Cap on the profit-based priority fee (wei per gas).
    pub tip_max_wei: u128,

    // ── Detection ──
    /// Haircut on quoted output before profit checks, in tenths of a bp (5 = 0.5 bp).
    pub quoter_haircut_dbps: u64,
    /// Local spread above which Phase 1.5 QuoterV2 verification runs (fraction).
    pub p15_gate_spread: f64,
    /// Minimum active-tick liquidity for a V3 pool to be tradeable.
    pub min_v3_liquidity: u128,
    /// Pools shallower than this (USD, both sides; V3 = in-range virtual reserves) are skipped
    /// by every local quote until they fill. They stay subscribed. 0 = off.
    pub min_pool_depth_usd: f64,
    /// How often the shallow-pool set is recomputed from the cache (ms, no RPC).
    pub pool_depth_refresh_ms: u64,
    /// Re-run pool discovery this often (ms) to pick up pools created after startup and
    /// resubscribe the log feeds. 0 = startup only.
    pub pool_rediscover_ms: u64,
    /// Re-read dynamic swap fees of Slipstream-type pools that traded since the last read (ms).
    /// 0 = off (fee read once at discovery).
    pub cl_fee_refresh_ms: u64,
    /// Re-read Aerodrome V2 pool fees (admin-set, rarely change) this often (ms). 0 = off.
    pub v2_fee_refresh_ms: u64,
    /// Cap on a V3 spot quote's input as a fraction of virtual reserves (bps).
    pub v3_spot_max_vr_fraction_bps: u64,
    /// Per-V3-leg spot overestimate deducted in optimistic mode (bps).
    pub v3_spot_overestimate_bps: u64,
    /// Profit above this share of input is treated as a phantom quote (bps; 500 = 5%).
    pub phantom_cap_bps: u64,
    /// Triangular V3 verification: max QuoterV2 rounds.
    pub tri_verify_max_rounds: usize,
    /// Triangular V3 verification: lower bracket point below the upper bound (bps).
    pub tri_verify_bracket_bps: u64,
    /// Triangular V3 verification: max gap to reuse a quoted point (bps).
    pub tri_verify_max_gap_bps: u64,
    /// Size optimizer: coarse and fine probe counts.
    pub optimizer_coarse_steps: usize,
    pub optimizer_fine_steps: usize,
    /// Size optimizer: spread above which the coarse range widens (fraction).
    pub optimizer_large_spread: f64,
    /// How long a volatile (V2 / Aerodrome-volatile) and a stable pool's reserves count as
    /// fresh without a Sync event (ms).
    pub volatile_pool_max_age_ms: u64,
    pub stable_pool_max_age_ms: u64,
    /// The pool log feed counts as live while any pool log arrived within this window. Then
    /// a quiet V2/Aerodrome pool is still quoted (its reserves are current: every Sync is
    /// applied) and re-checked on-chain before trading; the per-pool max ages above apply
    /// only when the feed is silent.
    pub feed_silence_ms: u64,
    /// V3 pools with no Swap event for this long count as stale in the heartbeat (ms).
    pub v3_fresh_window_ms: u64,

    // ── RPC ──
    /// Timeout for background reads: price, balances, gas, nonce, ping (ms).
    pub rpc_read_timeout_ms: u64,
    /// Timeout for the pre-flight / dry-run simulation (ms). Block-relative: ½ block.
    pub rpc_simulate_timeout_ms: Option<u64>,
    /// Timeout for eth_sendRawTransaction (ms).
    pub rpc_send_timeout_ms: u64,
    /// Calls per Multicall3 batch, and the timeout for one batch (ms).
    pub multicall_chunk_size: usize,
    pub multicall_chunk_timeout_ms: u64,
    /// Startup pool-discovery batch size and timeout (ms).
    pub discover_chunk_size: usize,
    pub discover_call_timeout_ms: u64,
    /// Pause between discovery / fee-refresh multicall chunks (ms): keeps background reads
    /// under a free endpoint's per-second cap. 0 = back to back.
    pub discover_chunk_gap_ms: u64,
    /// After a rate-limited quote chunk, hold every new quote chunk this long (ms) and retry
    /// the limited one once.
    pub quote_rate_limit_backoff_ms: u64,
    /// Max age of a cached 2-hop QuoterV2 result (ms). Block-relative: 3 blocks.
    pub quote_cache_max_age_ms: Option<u64>,
    pub quote_cache_max_pools: usize,
    /// Don't cache a QuoterV2 result while the pool's local state is younger than this
    /// (ms). Right after a pre-confirmed log moves the pool, `pending` on the quote RPC can
    /// still price the previous state (measured on mainnet.base.org from us-east-1: 19% of
    /// calls, all caught up within 100 ms); cached under the new state, that stale quote
    /// would be served for up to quote_cache_max_age. 0 = always cache.
    pub quote_cache_settle_ms: u64,
    /// RPC health: hold after an outage, cap for repeated relapses, relapse window (ms).
    pub health_hold_base_ms: u64,
    pub health_hold_max_ms: u64,
    pub health_relapse_window_ms: u64,
    /// RPC health: monitor probe interval (ms). Block-relative: 1 block.
    pub health_monitor_every_ms: Option<u64>,
    /// QuoterV2 health warnings: never-worked / broke-after-working thresholds (ms) and
    /// minimum failed batches.
    pub quoter_never_ok_warn_after_ms: u64,
    pub quoter_broken_warn_after_ms: u64,
    pub quoter_warn_min_batches: u32,
    /// Minimum interval between rate-limit reports (ms).
    pub rate_limit_log_every_ms: u64,
    /// Main-RPC latency ping interval (ms): one eth_blockNumber on the WS provider, feeding
    /// only the dashboard's rpc_latency_ms / rpc_latency_max_ms. Every request counts
    /// against the WS provider's quota (10 s was 8,640/day; 60 s is 1,440).
    pub rpc_latency_sample_ms: u64,

    // ── Scan loop ──
    /// Cooldowns after a failed attempt and after a send (s); after a gas-unprofitable skip
    /// the route stays live and only its opportunity/skip log is muted this long.
    pub cooldown_secs: u64,
    pub send_cooldown_secs: u64,
    pub gas_reject_cooldown_secs: u64,
    /// Consecutive non-RPC execution failures before the chain is paused.
    pub max_consecutive_failures: u32,
    /// Warn when one scan holds the loop this long (ms).
    pub slow_scan_warn_ms: u64,
    /// Heartbeat warns when no full scan ran this long (ms).
    pub heartbeat_stall_warn_ms: u64,
    /// Exit (code 1) when no new block header arrived for this long (ms): the WS backend is dead, and a restart walks the RPC fallback list.
    pub block_stall_exit_ms: u64,
    /// Native-price refresh + heartbeat, contract-balance refresh, config hot-reload (ms).
    pub price_refresh_ms: u64,
    pub balance_refresh_ms: u64,
    pub config_reload_ms: u64,
    /// Max backoff between WS (re)connect attempts for subscriptions (ms).
    pub ws_reconnect_max_backoff_ms: u64,

    /// Set from the chain's block_time_ms at install (not configurable here).
    #[serde(skip)]
    block_time_ms: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            gas_limit_2hop: 700_000,
            gas_limit_triangular: 900_000,
            gas_borrow_extra: 120_000,
            gas_per_hooked_leg: 350_000,
            onchain_min_profit_bps: 6076, // = the former 98% slack × 62% multiplier
            gas_fallback_margin_bps: 2_500,
            gas_fallback_max_age_ms: 120_000,
            tip_profit_share_bps: 0,
            tip_max_wei: 2_000_000_000, // 2 gwei — ~1000× Base's typical base fee
            quoter_haircut_dbps: 5,
            p15_gate_spread: 0.00005,
            min_v3_liquidity: 1_000_000_000,
            min_pool_depth_usd: 20_000.0,
            pool_depth_refresh_ms: 5_000,
            pool_rediscover_ms: 21_600_000,
            cl_fee_refresh_ms: 30_000,
            v2_fee_refresh_ms: 3_600_000,
            v3_spot_max_vr_fraction_bps: 200,
            v3_spot_overestimate_bps: 30,
            phantom_cap_bps: 500,
            tri_verify_max_rounds: 3,
            tri_verify_bracket_bps: 100,
            tri_verify_max_gap_bps: 10,
            optimizer_coarse_steps: 5,
            optimizer_fine_steps: 5,
            optimizer_large_spread: 0.05,
            volatile_pool_max_age_ms: 120_000,
            stable_pool_max_age_ms: 30_000,
            feed_silence_ms: 30_000,
            v3_fresh_window_ms: 30_000,
            rpc_read_timeout_ms: 5_000,
            rpc_simulate_timeout_ms: None,
            rpc_send_timeout_ms: 5_000,
            multicall_chunk_size: 10,
            multicall_chunk_timeout_ms: 3_000,
            discover_chunk_size: 20,
            discover_call_timeout_ms: 15_000,
            discover_chunk_gap_ms: 100,
            quote_rate_limit_backoff_ms: 250,
            quote_cache_max_age_ms: None,
            quote_cache_max_pools: 1024,
            quote_cache_settle_ms: 500,
            health_hold_base_ms: 10_000,
            health_hold_max_ms: 80_000,
            health_relapse_window_ms: 60_000,
            health_monitor_every_ms: None,
            quoter_never_ok_warn_after_ms: 30_000,
            quoter_broken_warn_after_ms: 300_000,
            quoter_warn_min_batches: 5,
            rate_limit_log_every_ms: 30_000,
            rpc_latency_sample_ms: 60_000,
            cooldown_secs: 15,
            send_cooldown_secs: 15,
            gas_reject_cooldown_secs: 60,
            max_consecutive_failures: 20,
            slow_scan_warn_ms: 10_000,
            heartbeat_stall_warn_ms: 30_000,
            block_stall_exit_ms: 300_000,
            price_refresh_ms: 60_000,
            balance_refresh_ms: 30_000,
            config_reload_ms: 30_000,
            ws_reconnect_max_backoff_ms: 30_000,
            block_time_ms: 2_000,
        }
    }
}

fn ms(v: u64) -> std::time::Duration {
    std::time::Duration::from_millis(v)
}

impl Tuning {
    fn validate(&self, block_time_ms: u64) -> Result<()> {
        if block_time_ms == 0 {
            return Err(anyhow!("chain.block_time_ms must be > 0"));
        }
        let positive: [(&str, u64); 21] = [
            ("balance_refresh_ms", self.balance_refresh_ms),
            ("volatile_pool_max_age_ms", self.volatile_pool_max_age_ms),
            ("stable_pool_max_age_ms", self.stable_pool_max_age_ms),
            ("feed_silence_ms", self.feed_silence_ms),
            ("price_refresh_ms", self.price_refresh_ms),
            ("config_reload_ms", self.config_reload_ms),
            ("gas_limit_2hop", self.gas_limit_2hop),
            ("gas_limit_triangular", self.gas_limit_triangular),
            ("rpc_read_timeout_ms", self.rpc_read_timeout_ms),
            ("rpc_send_timeout_ms", self.rpc_send_timeout_ms),
            ("multicall_chunk_size", self.multicall_chunk_size as u64),
            ("multicall_chunk_timeout_ms", self.multicall_chunk_timeout_ms),
            ("discover_chunk_size", self.discover_chunk_size as u64),
            ("discover_call_timeout_ms", self.discover_call_timeout_ms),
            ("health_hold_base_ms", self.health_hold_base_ms),
            ("phantom_cap_bps", self.phantom_cap_bps),
            ("tri_verify_max_rounds", self.tri_verify_max_rounds as u64),
            ("max_consecutive_failures", self.max_consecutive_failures as u64),
            ("block_stall_exit_ms", self.block_stall_exit_ms),
            ("rpc_latency_sample_ms", self.rpc_latency_sample_ms),
            ("pool_depth_refresh_ms", self.pool_depth_refresh_ms),
        ];
        if let Some((name, _)) = positive.iter().find(|(_, v)| *v == 0) {
            return Err(anyhow!("tuning.{} must be > 0", name));
        }
        if self.health_hold_max_ms < self.health_hold_base_ms {
            return Err(anyhow!("tuning.health_hold_max_ms must be >= health_hold_base_ms"));
        }
        if !(self.min_pool_depth_usd >= 0.0 && self.min_pool_depth_usd.is_finite()) {
            return Err(anyhow!("tuning.min_pool_depth_usd must be a finite number >= 0"));
        }
        if self.quoter_haircut_dbps > 1_000 {
            return Err(anyhow!("tuning.quoter_haircut_dbps is in tenths of a bp and must be <= 1000 (10 bp)"));
        }
        if self.tip_profit_share_bps > 9_000 {
            return Err(anyhow!("tuning.tip_profit_share_bps must be <= 9000 (a tip of the whole profit leaves nothing)"));
        }
        if self.tip_profit_share_bps > 0 && self.tip_max_wei == 0 {
            return Err(anyhow!("tuning.tip_max_wei must be > 0 when tip_profit_share_bps is set"));
        }
        if self.onchain_min_profit_bps > 10_000 {
            return Err(anyhow!("tuning.onchain_min_profit_bps must be <= 10000"));
        }
        Ok(())
    }

    fn with_block_time(mut self, block_time_ms: u64) -> Self {
        self.block_time_ms = block_time_ms;
        self
    }

    pub fn rpc_simulate_timeout(&self) -> std::time::Duration {
        ms(self.rpc_simulate_timeout_ms.unwrap_or(self.block_time_ms / 2))
    }
    pub fn quote_cache_max_age(&self) -> std::time::Duration {
        ms(self.quote_cache_max_age_ms.unwrap_or(self.block_time_ms * 3))
    }
    pub fn health_monitor_every(&self) -> std::time::Duration {
        ms(self.health_monitor_every_ms.unwrap_or(self.block_time_ms))
    }
    pub fn rpc_read_timeout(&self) -> std::time::Duration { ms(self.rpc_read_timeout_ms) }
    pub fn quote_cache_settle(&self) -> std::time::Duration { ms(self.quote_cache_settle_ms) }
    pub fn rpc_send_timeout(&self) -> std::time::Duration { ms(self.rpc_send_timeout_ms) }
    pub fn multicall_chunk_timeout(&self) -> std::time::Duration { ms(self.multicall_chunk_timeout_ms) }
    pub fn discover_call_timeout(&self) -> std::time::Duration { ms(self.discover_call_timeout_ms) }
    pub fn gas_fallback_max_age(&self) -> std::time::Duration { ms(self.gas_fallback_max_age_ms) }
    pub fn health_hold_base(&self) -> std::time::Duration { ms(self.health_hold_base_ms) }
    pub fn health_hold_max(&self) -> std::time::Duration { ms(self.health_hold_max_ms) }
    pub fn health_relapse_window(&self) -> std::time::Duration { ms(self.health_relapse_window_ms) }
    pub fn quoter_never_ok_warn_after(&self) -> std::time::Duration { ms(self.quoter_never_ok_warn_after_ms) }
    pub fn quoter_broken_warn_after(&self) -> std::time::Duration { ms(self.quoter_broken_warn_after_ms) }
    pub fn rate_limit_log_every(&self) -> std::time::Duration { ms(self.rate_limit_log_every_ms) }
    pub fn rpc_latency_sample(&self) -> std::time::Duration { ms(self.rpc_latency_sample_ms) }
    pub fn slow_scan_warn(&self) -> std::time::Duration { ms(self.slow_scan_warn_ms) }
    pub fn heartbeat_stall_warn(&self) -> std::time::Duration { ms(self.heartbeat_stall_warn_ms) }
    pub fn block_stall_exit(&self) -> std::time::Duration { ms(self.block_stall_exit_ms) }
    pub fn volatile_pool_max_age(&self) -> std::time::Duration { ms(self.volatile_pool_max_age_ms) }
    pub fn stable_pool_max_age(&self) -> std::time::Duration { ms(self.stable_pool_max_age_ms) }
    pub fn pool_depth_refresh(&self) -> std::time::Duration { ms(self.pool_depth_refresh_ms) }
    pub fn v3_fresh_window(&self) -> std::time::Duration { ms(self.v3_fresh_window_ms) }
    pub fn price_refresh(&self) -> std::time::Duration { ms(self.price_refresh_ms) }
    pub fn balance_refresh(&self) -> std::time::Duration { ms(self.balance_refresh_ms) }
    pub fn config_reload(&self) -> std::time::Duration { ms(self.config_reload_ms) }
    pub fn ws_reconnect_max_backoff(&self) -> std::time::Duration { ms(self.ws_reconnect_max_backoff_ms) }


    /// True if `profit` exceeds [`Tuning::phantom_cap_bps`] of `input` (a phantom quote).
    pub fn is_phantom(&self, profit: alloy::primitives::U256, input: alloy::primitives::U256) -> bool {
        use alloy::primitives::U256;
        profit.saturating_mul(U256::from(10_000u64)) > input.saturating_mul(U256::from(self.phantom_cap_bps))
    }
}

static TUNING: std::sync::OnceLock<Tuning> = std::sync::OnceLock::new();

/// Install the instance's tuning once at startup (one process = one chain).
pub fn install_tuning(tuning: Tuning, block_time_ms: u64) {
    let _ = TUNING.set(tuning.with_block_time(block_time_ms));
}

/// The instance's tuning (defaults if not installed, e.g. in unit tests).
pub fn tuning() -> &'static Tuning {
    TUNING.get_or_init(Tuning::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
chain:
  id: 8453
  name: "Base"
  ws_rpc: "ws://x"
  http_rpc: "http://x"
  native_currency: "ETH"
  wrapped_native: "0x4200000000000000000000000000000000000006"
  block_time_ms: 2000
  contract_address: "0x0000000000000000000000000000000000000001"
  min_native_balance: 0.01
  min_profit_usd: 0.1
  deadline_seconds: 60
routers: []
"#;

    #[test]
    fn empty_tuning_matches_previous_constants() {
        let cfg = parse_config(MIN).unwrap();
        let t = cfg.tuning.with_block_time(cfg.chain.block_time_ms);
        assert_eq!(t.gas_limit_2hop, 700_000);
        assert_eq!(t.rpc_simulate_timeout(), std::time::Duration::from_secs(1));
        assert_eq!(t.quote_cache_max_age(), std::time::Duration::from_secs(6));
        assert_eq!(t.health_monitor_every(), std::time::Duration::from_secs(2));
        assert_eq!(t.phantom_cap_bps, 500);
        assert_eq!(t.block_stall_exit_ms, 300_000);
        assert_eq!(t.rpc_latency_sample(), std::time::Duration::from_secs(60));
    }

    #[test]
    fn fee_tiers_validated() {
        let with_tiers = |tiers: &str| MIN.replace(
            "routers: []\n",
            &format!("routers:\n  - id: r\n    name: r\n    address: \"0x0000000000000000000000000000000000000002\"\n    type: v3\n    fee_tiers: {tiers}\n"),
        );
        let err = parse_config(&with_tiers("[30000000]")).unwrap_err().to_string();
        assert!(err.contains("fee tier 30000000"), "{err}");
        assert!(parse_config(&with_tiers("[0]")).is_err());
        parse_config(&with_tiers("[100, 500, 3000, 10000]")).unwrap();
    }

    #[test]
    fn onchain_min_profit_replaces_removed_knobs() {
        let err = parse_config(&format!("{MIN}tuning:\n  router_min_out_slack_bps: 200\n")).unwrap_err().to_string();
        assert!(err.contains("onchain_min_profit_bps") && err.contains("6076"), "{err}");
        let cfg = parse_config(&format!("{MIN}tuning:\n  onchain_min_profit_bps: 7000\n")).unwrap();
        assert_eq!(cfg.tuning.onchain_min_profit_bps, 7000);
        assert_eq!(Tuning::default().onchain_min_profit_bps, 6076, "old 98% × 62%");
    }

    #[test]
    fn block_relative_timings_follow_block_time() {
        let t = Tuning::default().with_block_time(12_000);
        assert_eq!(t.rpc_simulate_timeout(), std::time::Duration::from_secs(6));
        assert_eq!(t.quote_cache_max_age(), std::time::Duration::from_secs(36));
        let pinned = Tuning { rpc_simulate_timeout_ms: Some(800), ..Tuning::default() }.with_block_time(12_000);
        assert_eq!(pinned.rpc_simulate_timeout(), std::time::Duration::from_millis(800));
    }

    #[test]
    fn rejects_old_format_and_bad_values() {
        let old = "chains:\n  - id: 8453\n    name: \"Base\"\nrouters: []\n";
        assert!(parse_config(&old).unwrap_err().to_string().contains("one chain per instance"));
        let typo = format!("{}tuning:\n  gas_limit_2hopp: 1\n", MIN);
        assert!(parse_config(&typo).is_err(), "unknown tuning keys must be rejected");
        let zero = format!("{}tuning:\n  multicall_chunk_size: 0\n", MIN);
        assert!(parse_config(&zero).is_err());
        let stall = format!("{}tuning:\n  block_stall_exit_ms: 0\n", MIN);
        assert!(parse_config(&stall).is_err());
        let ping = format!("{}tuning:\n  rpc_latency_sample_ms: 0\n", MIN);
        assert!(parse_config(&ping).is_err(), "interval(0) panics");
        let prune = format!("{}tuning:\n  pool_prune_after_ms: 1\n", MIN);
        assert!(parse_config(&prune).is_err(), "pool_prune_after_ms was removed");
    }

    fn with_pair(sym: &str, token_in: &str) -> String {
        format!(
            "{MIN}pairs:\n  - id: p\n    token_in: \"{token_in}\"\n    token_out: \"0x0000000000000000000000000000000000000003\"\n    token_in_symbol: \"{sym}\"\n    token_out_symbol: \"X\"\n    token_in_decimals: 6\n    token_out_decimals: 18\n    trade_amount: \"1\"\n"
        )
    }

    #[test]
    fn token_in_must_have_usd_price() {
        let usdc = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
        let err = parse_config(&with_pair("ZORA", usdc)).unwrap_err().to_string();
        assert!(err.contains("has no USD price"), "{err}");
        parse_config(&with_pair("usdc", usdc)).unwrap();
        parse_config(&with_pair("WETH", "0x4200000000000000000000000000000000000006")).unwrap();
        assert!(parse_config(&with_pair("WETH", "0x0000000000000000000000000000000000000004")).is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        let chain_key = MIN.replace("  deadline_seconds: 60\n", "  deadline_seconds: 60\n  min_swap_amount_filter: 1\n");
        let err = parse_config(&chain_key).unwrap_err().to_string();
        assert!(err.contains("unknown field"), "{err}");
        let pair_key = format!("{}    routers: [\"x\"]\n", with_pair("USDC", "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"));
        let err = parse_config(&pair_key).unwrap_err().to_string();
        assert!(err.contains("routers"), "{err}");
    }

    #[test]
    fn token_amount_to_usd_case_insensitive() {
        use crate::strategy::token_amount_to_usd;
        use alloy::primitives::U256;
        assert_eq!(token_amount_to_usd(U256::from(2_000_000u64), 6, "usdc", 3000.0), 2.0);
        assert_eq!(token_amount_to_usd(U256::from(10u64).pow(U256::from(18)), 18, "weth", 3000.0), 3000.0);
    }

    #[test]
    fn deadline_seconds_validated() {
        let err = parse_config(&MIN.replace("deadline_seconds: 60", "deadline_seconds: 5")).unwrap_err().to_string();
        assert!(err.contains("deadline_seconds"), "{err}");
        parse_config(MIN).unwrap();
    }

    #[test]
    fn api_key_required() {
        assert!(check_api_key("", false).is_err());
        assert!(check_api_key("  ", false).is_err());
        assert!(check_api_key("", true).is_ok());
        assert!(check_api_key("k", false).is_ok());
    }

    #[test]
    fn chain_id_guard() {
        assert!(check_chain_id(8453, 8453, "Base").is_ok());
        assert!(check_chain_id(8453, 84532, "Base").is_err());
    }

    #[test]
    fn shipped_configs_parse() {
        for path in ["config.yaml", "config-testnet.yaml"] {
            let full = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path);
            let content = std::fs::read_to_string(&full).unwrap();
            // Placeholders (testnet contract not deployed yet) are allowed here.
            let content = content.replace("YOUR_", "0000");
            let cfg = parse_config(&content).unwrap_or_else(|e| panic!("{}: {}", path, e));
            assert!(!cfg.routers.is_empty(), "{} has no routers", path);
        }
    }

    #[test]
    fn phantom_cap() {
        use alloy::primitives::U256;
        let t = Tuning::default();
        assert!(!t.is_phantom(U256::from(50u64), U256::from(1000u64)));
        assert!(t.is_phantom(U256::from(51u64), U256::from(1000u64)));
    }
}
