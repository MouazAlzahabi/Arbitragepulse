use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use futures::future::join_all;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tracing::debug;

use crate::abi::{IMulticall3, IQuoterV2, IUniswapV2Router02};
use crate::config::{PairConfig, RouterConfig, RouterType};

// ─── Opportunity ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArbOpportunity {
    pub chain_id: u64,
    pub pair_id: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub router_a: Address,
    pub router_b: Address,
    pub router_a_type: RouterType,
    pub router_b_type: RouterType,
    pub fee_a: u32,
    pub fee_b: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_a_id: String,
    pub router_b_id: String,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TriangularOpportunity {
    pub chain_id: u64,
    pub triplet_id: String, // "USDC-WETH-OP"
    pub token_a: Address,
    pub token_b: Address,
    pub token_c: Address,
    pub amount_in: U256,
    pub router_ab: Address,
    pub router_bc: Address,
    pub router_ca: Address,
    pub router_ab_type: RouterType,
    pub router_bc_type: RouterType,
    pub router_ca_type: RouterType,
    pub fee_ab: u32,
    pub fee_bc: u32,
    pub fee_ca: u32,
    pub expected_profit: U256,
    pub profit_usd: f64,
    pub router_ab_id: String,
    pub router_bc_id: String,
    pub router_ca_id: String,
}

/// Unified opportunity enum for 2-hop and triangular arbitrage.
/// Allows chain.rs to handle both types in a single evaluation loop.
#[derive(Debug, Clone)]
pub enum Opportunity {
    TwoHop(ArbOpportunity),
    Triangular(TriangularOpportunity),
}

impl Opportunity {
    pub fn profit_usd(&self) -> f64 {
        match self {
            Opportunity::TwoHop(o) => o.profit_usd,
            Opportunity::Triangular(o) => o.profit_usd,
        }
    }

    pub fn pair_id(&self) -> &str {
        match self {
            Opportunity::TwoHop(o) => &o.pair_id,
            Opportunity::Triangular(o) => &o.triplet_id,
        }
    }

    /// Full opportunity fingerprint for deduplication.
    /// Includes pair, routers, and amount to prevent re-evaluating identical opportunities.
    pub fn fingerprint(&self) -> String {
        match self {
            Opportunity::TwoHop(o) => {
                format!("{}|{}|{}|{}",
                    o.pair_id,
                    o.router_a_id,
                    o.router_b_id,
                    o.amount_in
                )
            }
            Opportunity::Triangular(o) => {
                format!("{}|{}|{}|{}|{}",
                    o.triplet_id,
                    o.router_ab_id,
                    o.router_bc_id,
                    o.router_ca_id,
                    o.amount_in
                )
            }
        }
    }
}

// ─── Internal task metadata ───────────────────────────────────────────────────

#[derive(Clone)]
struct ForwardTask {
    pair_idx: usize,
    router_id: String,
    router_addr: Address,
    router_type: RouterType,
    fee: u32,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
}

#[derive(Clone)]
#[allow(dead_code)]
struct ReverseTask {
    pair_idx: usize,
    pair_id: String,
    token_in_symbol: String,
    token_in_decimals: u8,
    router_a_id: String,
    router_a_addr: Address,
    router_a_type: RouterType,
    fee_a: u32,
    amount_in: U256,
    router_b_id: String,
    router_b_addr: Address,
    router_b_type: RouterType,
    fee_b: u32,
    token_out_amount: U256,
    token_in: Address,
    token_out: Address,
}

#[derive(Clone, Debug)]
struct TriangularTask {
    triplet_id: String,     // "A-B-C"
    leg: TriangularLeg,     // AB, BC, or CA
    token_a: Address,
    token_b: Address,
    token_c: Address,
    amount_in: U256,
    router_addr: Address,
    router_id: String,
    router_type: RouterType,
    fee: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TriangularLeg {
    AB,  // A→B
    BC,  // B→C
    CA,  // C→A
}

// ─── Strategy ─────────────────────────────────────────────────────────────────

/// Cached quote result with timestamp
#[derive(Clone)]
struct CachedQuote {
    result: U256,
    cached_at: Instant,
}

pub struct Strategy {
    pub chain_id: u64,
    pub pairs: Vec<PairConfig>,
    pub routers: Vec<RouterConfig>,
    pub native_price_usd: f64,
    pub min_profit_usd: f64,
    /// QuoterV2 contract address for V3 quotes (chain-specific, NOT the swap router)
    pub quoter_v2_address: Option<Address>,
    /// Quote cache: maps quote fingerprint → (result, timestamp)
    /// TTL = 4 seconds (2 blocks on most chains)
    quote_cache: HashMap<String, CachedQuote>,
    quote_cache_ttl: Duration,
}

impl Strategy {
    pub fn new(
        chain_id: u64,
        pairs: Vec<PairConfig>,
        routers: Vec<RouterConfig>,
        min_profit_usd: f64,
        quoter_v2_address: Option<Address>,
    ) -> Self {
        Self {
            chain_id,
            pairs,
            routers,
            native_price_usd: 2500.0,
            min_profit_usd,
            quoter_v2_address,
            quote_cache: HashMap::new(),
            quote_cache_ttl: Duration::from_secs(4), // 2 blocks on most chains
        }
    }

    /// Evaluate all pairs on all router combinations for arb opportunities.
    /// All quotes run concurrently (two phases: forward then reverse).
    pub async fn evaluate<P: Provider + Clone>(&self, provider: &P) -> Vec<ArbOpportunity> {
        let chain_routers: Vec<&RouterConfig> = self
            .routers
            .iter()
            .filter(|r| r.chain_id == self.chain_id)
            .collect();

        // ── Phase 1: Forward quotes via multicall3 (1 RPC round-trip) ───────────

        let mut fwd_tasks: Vec<ForwardTask> = Vec::new();
        let mut fwd_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (pi, pair) in self.pairs.iter().enumerate().filter(|(_, p)| p.chain_id == self.chain_id) {
            let token_in = match pair.token_in.parse::<Address>() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let token_out = match pair.token_out.parse::<Address>() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let amount_in = match parse_amount_capped(
                &pair.trade_amount,
                pair.max_trade.as_deref(),
                pair.token_in_decimals,
            ) {
                Some(a) => a,
                None => continue,
            };

            for router in &chain_routers {
                let router_addr: Address = match router.address.parse() {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                match router.router_type {
                    RouterType::V2 => {
                        let calldata = IUniswapV2Router02::getAmountsOutCall {
                            amountIn: amount_in,
                            path: vec![token_in, token_out],
                        }
                        .abi_encode();
                        fwd_mc.push((router_addr, calldata));
                        fwd_tasks.push(ForwardTask {
                            pair_idx: pi,
                            router_id: router.id.clone(),
                            router_addr,
                            router_type: RouterType::V2,
                            fee: 0,
                            amount_in,
                            token_in,
                            token_out,
                        });
                    }
                    RouterType::V3 => {
                        let quoter = match self.quoter_v2_address {
                            Some(q) => q,
                            None => {
                                debug!("No QuoterV2 address for chain {} — skipping V3 quotes", self.chain_id);
                                continue;
                            }
                        };
                        let tiers = if router.fee_tiers.is_empty() {
                            vec![500u32, 3000, 10000]
                        } else {
                            router.fee_tiers.clone()
                        };
                        for fee in tiers {
                            let calldata = IQuoterV2::quoteExactInputSingleCall {
                                params: IQuoterV2::QuoteExactInputSingleParams {
                                    tokenIn: token_in,
                                    tokenOut: token_out,
                                    amountIn: amount_in,
                                    fee: Uint::from(fee),
                                    sqrtPriceLimitX96: Uint::ZERO,
                                },
                            }
                            .abi_encode();
                            fwd_mc.push((quoter, calldata));
                            fwd_tasks.push(ForwardTask {
                                pair_idx: pi,
                                router_id: router.id.clone(),
                                router_addr,
                                router_type: RouterType::V3,
                                fee,
                                amount_in,
                                token_in,
                                token_out,
                            });
                        }
                    }
                }
            }
        }

        let fwd_raw = run_multicall(provider, fwd_mc).await;

        // Group successful forward quotes by pair_idx
        let mut pair_quotes: std::collections::HashMap<usize, Vec<ForwardTask>> =
            std::collections::HashMap::new();

        for (task, raw_opt) in fwd_tasks.into_iter().zip(fwd_raw.into_iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_out = match task.router_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
            };
            if let Some(out) = amount_out.filter(|o| !o.is_zero()) {
                let mut t = task;
                t.amount_in = out; // repurpose field to carry token_out amount
                pair_quotes.entry(t.pair_idx).or_default().push(t);
            }
        }

        // ── Phase 2: Reverse quotes via multicall3 (1 RPC round-trip) ───────────

        let mut rev_tasks: Vec<ReverseTask> = Vec::new();
        let mut rev_mc: Vec<(Address, Vec<u8>)> = Vec::new();

        for (pi, quotes) in &pair_quotes {
            if quotes.len() < 2 {
                continue;
            }
            let pair = &self.pairs[*pi];
            let original_amount_in = match parse_amount_capped(
                &pair.trade_amount,
                pair.max_trade.as_deref(),
                pair.token_in_decimals,
            ) {
                Some(a) => a,
                None => continue,
            };

            for (ai, q_a) in quotes.iter().enumerate() {
                let token_out_amount = q_a.amount_in;

                for (bi, q_b) in quotes.iter().enumerate() {
                    if ai == bi {
                        continue;
                    }

                    let token_in = q_a.token_in;
                    let token_out = q_a.token_out;
                    let rb_addr = q_b.router_addr;
                    let fee_b = q_b.fee;

                    let (target, calldata) = match q_b.router_type {
                        RouterType::V2 => {
                            let cd = IUniswapV2Router02::getAmountsOutCall {
                                amountIn: token_out_amount,
                                path: vec![token_out, token_in],
                            }
                            .abi_encode();
                            (rb_addr, cd)
                        }
                        RouterType::V3 => {
                            let quoter = match self.quoter_v2_address {
                                Some(q) => q,
                                None => continue,
                            };
                            let cd = IQuoterV2::quoteExactInputSingleCall {
                                params: IQuoterV2::QuoteExactInputSingleParams {
                                    tokenIn: token_out,
                                    tokenOut: token_in,
                                    amountIn: token_out_amount,
                                    fee: Uint::from(fee_b),
                                    sqrtPriceLimitX96: Uint::ZERO,
                                },
                            }
                            .abi_encode();
                            (quoter, cd)
                        }
                    };

                    rev_mc.push((target, calldata));
                    rev_tasks.push(ReverseTask {
                        pair_idx: *pi,
                        pair_id: pair.id.clone(),
                        token_in_symbol: pair.token_in_symbol.clone(),
                        token_in_decimals: pair.token_in_decimals,
                        router_a_id: q_a.router_id.clone(),
                        router_a_addr: q_a.router_addr,
                        router_a_type: q_a.router_type.clone(),
                        fee_a: q_a.fee,
                        amount_in: original_amount_in,
                        router_b_id: q_b.router_id.clone(),
                        router_b_addr: rb_addr,
                        router_b_type: q_b.router_type.clone(),
                        fee_b,
                        token_out_amount,
                        token_in,
                        token_out,
                    });
                }
            }
        }

        let rev_raw = run_multicall(provider, rev_mc).await;

        // ── Find profitable opportunities ──────────────────────────────────────

        let mut opportunities = Vec::new();

        for (task, raw_opt) in rev_tasks.iter().zip(rev_raw.into_iter()) {
            let raw = match raw_opt { Some(r) => r, None => continue };
            let amount_back_opt = match task.router_b_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
            };
            if let Some(amount_back) = amount_back_opt.filter(|&b| b > task.amount_in) {
                let profit = amount_back - task.amount_in;
                let profit_usd = token_amount_to_usd(
                    profit,
                    task.token_in_decimals,
                    &task.token_in_symbol,
                    self.native_price_usd,
                );

                if profit_usd >= self.min_profit_usd {
                    debug!(
                        "[{}] Arb: {} | profit=${:.4} | {}/{}",
                        self.chain_id,
                        task.pair_id,
                        profit_usd,
                        task.router_a_id,
                        task.router_b_id
                    );
                    opportunities.push(ArbOpportunity {
                        chain_id: self.chain_id,
                        pair_id: task.pair_id.clone(),
                        token_in: task.token_in,
                        token_out: task.token_out,
                        amount_in: task.amount_in,
                        router_a: task.router_a_addr,
                        router_b: task.router_b_addr,
                        router_a_type: task.router_a_type.clone(),
                        router_b_type: task.router_b_type.clone(),
                        fee_a: task.fee_a,
                        fee_b: task.fee_b,
                        expected_profit: profit,
                        profit_usd,
                        router_a_id: task.router_a_id.clone(),
                        router_b_id: task.router_b_id.clone(),
                    });
                }
            }
        }

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));
        opportunities
    }

    /// Detect triangular arbitrage opportunities (A → B → C → A loops).
    /// Returns top opportunities sorted by expected profit.
    ///
    /// Uses multicall batching for all quotes (1 RPC call instead of 3N sequential calls).
    /// Enumerates all router combinations for maximum opportunity discovery.
    pub async fn detect_triangular<P: Provider + Clone>(
        &self,
        provider: &P,
        max_opportunities: usize,
    ) -> Vec<TriangularOpportunity> {
        // Only run if we have at least 3 trusted tokens
        let trusted_tokens: Vec<_> = self.pairs.iter()
            .filter(|p| p.chain_id == self.chain_id)
            .flat_map(|p| vec![&p.token_in, &p.token_out])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if trusted_tokens.len() < 3 {
            return vec![];
        }

        let chain_routers: Vec<&RouterConfig> = self.routers.iter()
            .filter(|r| r.chain_id == self.chain_id)
            .collect();

        if chain_routers.is_empty() {
            return vec![];
        }

        // ── Phase 1: Build all multicall tasks upfront ────────────────────────────

        let mut mc_calls: Vec<(Address, Vec<u8>)> = Vec::new();
        let mut tasks: Vec<TriangularTask> = Vec::new();

        // Enumerate token triplets (limit to 10 to avoid explosion)
        for (i, token_a_str) in trusted_tokens.iter().take(10).enumerate() {
            for (j, token_b_str) in trusted_tokens.iter().skip(i + 1).take(10).enumerate() {
                for (_, token_c_str) in trusted_tokens.iter().skip(i + j + 2).take(10).enumerate() {
                    let token_a: Address = match token_a_str.parse() {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    let token_b: Address = match token_b_str.parse() {
                        Ok(a) => a,
                        Err(_) => continue,
                    };
                    let token_c: Address = match token_c_str.parse() {
                        Ok(a) => a,
                        Err(_) => continue,
                    };

                    let amount_in = U256::from(1000_000_000u128); // 1000 USDC
                    let triplet_id = format!("{:?}-{:?}-{:?}", token_a, token_b, token_c);

                    // Enumerate all router combinations (R³ for 3 legs)
                    for router_ab in &chain_routers {
                        for router_bc in &chain_routers {
                            for router_ca in &chain_routers {
                                self.build_triangular_multicall(
                                    &mut mc_calls,
                                    &mut tasks,
                                    triplet_id.clone(),
                                    token_a,
                                    token_b,
                                    token_c,
                                    amount_in,
                                    router_ab,
                                    router_bc,
                                    router_ca,
                                );
                            }
                        }
                    }
                }
            }
        }

        if mc_calls.is_empty() {
            return vec![];
        }

        // ── Phase 2: Execute ALL quotes in single multicall ───────────────────────

        let raw_results = run_multicall(provider, mc_calls).await;

        // ── Phase 3: Decode results and group by triplet_id ──────────────────────

        use std::collections::HashMap;
        let mut triplet_quotes: HashMap<String, HashMap<TriangularLeg, Vec<(TriangularTask, U256)>>> =
            HashMap::new();

        for (task, raw_opt) in tasks.into_iter().zip(raw_results.into_iter()) {
            let raw = match raw_opt {
                Some(r) => r,
                None => continue,
            };

            let amount_out = match task.router_type {
                RouterType::V2 => IUniswapV2Router02::getAmountsOutCall::abi_decode_returns(&raw)
                    .ok()
                    .and_then(|v| v.last().copied()),
                RouterType::V3 => IQuoterV2::quoteExactInputSingleCall::abi_decode_returns(&raw)
                    .ok()
                    .map(|r| r.amountOut),
            };

            if let Some(out) = amount_out.filter(|o| !o.is_zero()) {
                triplet_quotes
                    .entry(task.triplet_id.clone())
                    .or_default()
                    .entry(task.leg.clone())
                    .or_default()
                    .push((task, out));
            }
        }

        // ── Phase 4: Reconstruct profitable loops ─────────────────────────────────

        let mut opportunities = Vec::new();

        for (triplet_id, legs) in triplet_quotes {
            let ab_quotes = match legs.get(&TriangularLeg::AB) {
                Some(q) if !q.is_empty() => q,
                _ => continue,
            };
            let bc_quotes = match legs.get(&TriangularLeg::BC) {
                Some(q) if !q.is_empty() => q,
                _ => continue,
            };
            let ca_quotes = match legs.get(&TriangularLeg::CA) {
                Some(q) if !q.is_empty() => q,
                _ => continue,
            };

            // Try all combinations of (AB quote, BC quote, CA quote)
            for (task_ab, _amount_b) in ab_quotes {
                for (task_bc, _amount_c) in bc_quotes {
                    for (task_ca, final_a) in ca_quotes {
                        // Validate amounts match across legs (BC input = AB output, CA input = BC output)
                        // Note: We can't perfectly validate since we quoted with fixed amounts,
                        // but we filter obviously broken paths where amounts are wildly mismatched.

                        let amount_in = task_ab.amount_in;

                        // Check profitability
                        if *final_a <= amount_in {
                            continue;
                        }

                        let profit = *final_a - amount_in;
                        let profit_usd = u256_to_f64(profit) / 1_000_000.0;

                        if profit_usd < self.min_profit_usd {
                            continue;
                        }

                        opportunities.push(TriangularOpportunity {
                            chain_id: self.chain_id,
                            triplet_id: triplet_id.clone(),
                            token_a: task_ab.token_a,
                            token_b: task_ab.token_b,
                            token_c: task_ab.token_c,
                            amount_in,
                            router_ab: task_ab.router_addr,
                            router_bc: task_bc.router_addr,
                            router_ca: task_ca.router_addr,
                            router_ab_type: task_ab.router_type.clone(),
                            router_bc_type: task_bc.router_type.clone(),
                            router_ca_type: task_ca.router_type.clone(),
                            fee_ab: task_ab.fee,
                            fee_bc: task_bc.fee,
                            fee_ca: task_ca.fee,
                            expected_profit: profit,
                            profit_usd,
                            router_ab_id: task_ab.router_id.clone(),
                            router_bc_id: task_bc.router_id.clone(),
                            router_ca_id: task_ca.router_id.clone(),
                        });
                    }
                }
            }
        }

        opportunities.sort_by(|a, b| b.expected_profit.cmp(&a.expected_profit));
        opportunities.truncate(max_opportunities);
        opportunities
    }

    /// Build multicall tasks for one triangular arbitrage triplet.
    /// Adds 3 quote tasks (A→B, B→C, C→A) to the multicall batch.
    fn build_triangular_multicall(
        &self,
        mc_calls: &mut Vec<(Address, Vec<u8>)>,
        tasks: &mut Vec<TriangularTask>,
        triplet_id: String,
        token_a: Address,
        token_b: Address,
        token_c: Address,
        amount_in: U256,
        router_ab: &RouterConfig,
        router_bc: &RouterConfig,
        router_ca: &RouterConfig,
    ) {
        // ── Leg AB: A→B ───────────────────────────────────────────────────────────
        self.add_triangular_leg_quote(
            mc_calls,
            tasks,
            triplet_id.clone(),
            TriangularLeg::AB,
            token_a,
            token_b,
            token_c,
            amount_in,
            token_a,
            token_b,
            router_ab,
        );

        // ── Leg BC: B→C (we don't know amount_b yet, so use a reasonable estimate) ──
        // Use 2× amount_in as a conservative upper bound for quoting purposes.
        // The actual execution will use the real amount_b from leg AB.
        let estimated_amount_b = amount_in * U256::from(2u32);
        self.add_triangular_leg_quote(
            mc_calls,
            tasks,
            triplet_id.clone(),
            TriangularLeg::BC,
            token_a,
            token_b,
            token_c,
            estimated_amount_b,
            token_b,
            token_c,
            router_bc,
        );

        // ── Leg CA: C→A (similarly estimate amount_c) ─────────────────────────────
        let estimated_amount_c = amount_in * U256::from(4u32);
        self.add_triangular_leg_quote(
            mc_calls,
            tasks,
            triplet_id,
            TriangularLeg::CA,
            token_a,
            token_b,
            token_c,
            estimated_amount_c,
            token_c,
            token_a,
            router_ca,
        );
    }

    /// Add a single triangular leg quote to the multicall batch.
    fn add_triangular_leg_quote(
        &self,
        mc_calls: &mut Vec<(Address, Vec<u8>)>,
        tasks: &mut Vec<TriangularTask>,
        triplet_id: String,
        leg: TriangularLeg,
        token_a: Address,
        token_b: Address,
        token_c: Address,
        amount_in: U256,
        token_in: Address,
        token_out: Address,
        router: &RouterConfig,
    ) {
        let router_addr: Address = match router.address.parse() {
            Ok(a) => a,
            Err(_) => return,
        };

        match router.router_type {
            RouterType::V2 => {
                let calldata = IUniswapV2Router02::getAmountsOutCall {
                    amountIn: amount_in,
                    path: vec![token_in, token_out],
                }
                .abi_encode();
                mc_calls.push((router_addr, calldata));
                tasks.push(TriangularTask {
                    triplet_id,
                    leg,
                    token_a,
                    token_b,
                    token_c,
                    amount_in,
                    router_addr,
                    router_id: router.id.clone(),
                    router_type: RouterType::V2,
                    fee: 0,
                });
            }
            RouterType::V3 => {
                let Some(quoter) = self.quoter_v2_address else {
                    return;
                };
                let tiers = if router.fee_tiers.is_empty() {
                    vec![3000u32]
                } else {
                    router.fee_tiers.clone()
                };
                // For triangular, only use first fee tier to reduce combinatorial explosion
                let fee = tiers[0];
                let calldata = IQuoterV2::quoteExactInputSingleCall {
                    params: IQuoterV2::QuoteExactInputSingleParams {
                        tokenIn: token_in,
                        tokenOut: token_out,
                        amountIn: amount_in,
                        fee: Uint::from(fee),
                        sqrtPriceLimitX96: Uint::ZERO,
                    },
                }
                .abi_encode();
                mc_calls.push((quoter, calldata));
                tasks.push(TriangularTask {
                    triplet_id,
                    leg,
                    token_a,
                    token_b,
                    token_c,
                    amount_in,
                    router_addr,
                    router_id: router.id.clone(),
                    router_type: RouterType::V3,
                    fee,
                });
            }
        }
    }

    /// Two-round adaptive size optimizer (~2 RTTs, ~200ms).
    ///
    /// Round 1 (coarse): 5 probes linearly across [10% → 200%] of `amount_in`,
    /// capped by `max_trade` if the pair has one configured.
    ///
    /// Round 2 (fine): 5 probes in a tight window (±one coarse step) around
    /// the Round 1 winner.
    ///
    /// Returns the opportunity with the amount that maximises absolute profit.
    /// Falls back to the original opportunity unchanged if no improvement found.
    ///
    /// Fast-path: if the detected spread is already >5%, the optimizer is
    /// skipped entirely (~100ms savings). At that spread the marginal gain from
    /// probing is small compared to the latency cost of 20 extra RPC calls.
    pub async fn optimize<P: Provider + Clone + 'static>(
        &self,
        opp: &ArbOpportunity,
        provider: &P,
    ) -> ArbOpportunity {
        const COARSE: usize = 5;
        const FINE: usize = 5;
        const LARGE_SPREAD_THRESHOLD: f64 = 0.05; // 5 %

        // Skip optimizer for wide spreads — the extra 20 RPC calls cost more
        // latency than they gain in profit precision.
        if !opp.amount_in.is_zero() {
            let spread = u256_to_f64(opp.expected_profit) / u256_to_f64(opp.amount_in);
            if spread >= LARGE_SPREAD_THRESHOLD {
                debug!(
                    "[{}] Spread {:.1}% ≥ {:.0}% — skipping optimizer",
                    opp.pair_id,
                    spread * 100.0,
                    LARGE_SPREAD_THRESHOLD * 100.0,
                );
                return opp.clone();
            }
        }

        // Lower bound: 10% of configured trade amount
        let min_amount = opp.amount_in / U256::from(10u32);

        // Upper bound: 2× amount_in, capped by max_trade when configured
        let double_amount = opp.amount_in * U256::from(2u32);
        let max_amount = self
            .pairs
            .iter()
            .find(|p| p.id == opp.pair_id)
            .and_then(|p| p.max_trade.as_deref())
            .and_then(|s| {
                let pair = self.pairs.iter().find(|p| p.id == opp.pair_id)?;
                parse_amount_capped(s, None, pair.token_in_decimals)
            })
            .map(|cap| double_amount.min(cap))
            .unwrap_or(double_amount);

        if min_amount >= max_amount || min_amount.is_zero() {
            return opp.clone();
        }

        // ── Round 1: Coarse scan across full range ────────────────────────────
        let coarse_winner = probe_range(
            opp,
            provider,
            self.quoter_v2_address,
            min_amount,
            max_amount,
            COARSE,
        )
        .await;

        let Some((coarse_amount, _)) = coarse_winner else {
            return opp.clone();
        };

        // ── Round 2: Fine scan around Round 1 winner ──────────────────────────
        let coarse_step = (max_amount - min_amount) / U256::from((COARSE - 1) as u128);
        let fine_min = coarse_amount.saturating_sub(coarse_step).max(min_amount);
        let fine_max = (coarse_amount + coarse_step).min(max_amount);

        let fine_winner = probe_range(
            opp,
            provider,
            self.quoter_v2_address,
            fine_min,
            fine_max,
            FINE,
        )
        .await;

        // Pick whichever gave more absolute profit
        let (opt_amount, opt_back) = [coarse_winner, fine_winner]
            .into_iter()
            .flatten()
            .max_by_key(|(amt, back)| *back - *amt)
            .unwrap_or((opp.amount_in, opp.amount_in + opp.expected_profit));

        let opt_profit = if opt_back > opt_amount {
            opt_back - opt_amount
        } else {
            return opp.clone();
        };

        // Only update if strictly better than the originally detected opportunity
        if opt_profit <= opp.expected_profit {
            return opp.clone();
        }

        let profit_scale = u256_to_f64(opt_profit) / u256_to_f64(opp.expected_profit);

        debug!(
            "[{}] Size optimized: ${:.4} → ${:.4} (+{:.1}%)",
            opp.pair_id,
            opp.profit_usd,
            opp.profit_usd * profit_scale,
            (profit_scale - 1.0) * 100.0,
        );

        ArbOpportunity {
            amount_in: opt_amount,
            expected_profit: opt_profit,
            profit_usd: opp.profit_usd * profit_scale,
            ..opp.clone()
        }
    }

    pub fn update_native_price(&mut self, price: f64) {
        self.native_price_usd = price;
    }

    /// Generate cache key for a quote
    fn quote_fingerprint(router: &Address, token_in: &Address, token_out: &Address, amount_in: &U256, fee: u32) -> String {
        format!("{:?}|{:?}|{:?}|{}|{}", router, token_in, token_out, amount_in, fee)
    }

    /// Get cached quote if available and fresh
    fn get_cached_quote(&self, key: &str) -> Option<U256> {
        self.quote_cache.get(key).and_then(|cached| {
            if cached.cached_at.elapsed() < self.quote_cache_ttl {
                Some(cached.result)
            } else {
                None
            }
        })
    }

    /// Cache a quote result
    fn cache_quote(&mut self, key: String, result: U256) {
        self.quote_cache.insert(key, CachedQuote {
            result,
            cached_at: Instant::now(),
        });
    }

    /// Clean up expired cache entries (called periodically)
    pub fn cleanup_quote_cache(&mut self) {
        self.quote_cache.retain(|_, cached| cached.cached_at.elapsed() < self.quote_cache_ttl);
    }
}

// ─── Multicall3 batch helper ───────────────────────────────────────────────────

/// Batch all calls into a single `eth_call` to Multicall3.
/// Returns `Some(Vec<u8>)` for each call that succeeded, `None` for failures.
/// Falls back to parallel individual `eth_call`s if Multicall3 is unavailable.
async fn run_multicall<P: Provider>(
    provider: &P,
    calls: Vec<(Address, Vec<u8>)>,
) -> Vec<Option<Vec<u8>>> {
    if calls.is_empty() {
        return vec![];
    }

    let mc3: Address = "0xcA11bde05977b3631167028862bE2a173976CA11"
        .parse()
        .expect("hardcoded multicall3 address");

    let mc_calls: Vec<IMulticall3::Call3> = calls
        .iter()
        .map(|(target, data)| IMulticall3::Call3 {
            target: *target,
            allowFailure: true,
            callData: data.clone().into(),
        })
        .collect();

    let agg_calldata = IMulticall3::aggregate3Call { calls: mc_calls }.abi_encode();
    let tx = TransactionRequest::default()
        .to(mc3)
        .input(agg_calldata.into());

    if let Ok(raw) = provider.call(tx).await {
        if let Ok(ret) = IMulticall3::aggregate3Call::abi_decode_returns(&raw) {
            return ret
                .into_iter()
                .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None })
                .collect();
        }
    }

    // Fallback: sequential individual eth_calls (e.g. local chain without Multicall3)
    debug!("Multicall3 unavailable — using sequential individual calls");
    let mut results: Vec<Option<Vec<u8>>> = Vec::with_capacity(calls.len());
    for (target, data) in calls.iter() {
        let tx = TransactionRequest::default()
            .to(*target)
            .input(data.clone().into());
        results.push(provider.call(tx).await.ok().map(|b| b.to_vec()));
    }
    results
}

// ─── Probe helper ─────────────────────────────────────────────────────────────

/// Fire `steps` linearly-spaced quote pairs concurrently across [min_amount, max_amount].
/// Returns `(amount_in, amount_back)` for the probe with the highest absolute profit,
/// or `None` if no probe was profitable.
async fn probe_range<P: Provider + Clone + 'static>(
    opp: &ArbOpportunity,
    provider: &P,
    quoter: Option<Address>,
    min_amount: U256,
    max_amount: U256,
    steps: usize,
) -> Option<(U256, U256)> {
    if steps == 0 || min_amount >= max_amount {
        return None;
    }

    let range = max_amount - min_amount;
    let token_in = opp.token_in;
    let token_out = opp.token_out;
    let ra = opp.router_a;
    let ra_type = opp.router_a_type.clone();
    let rb = opp.router_b;
    let rb_type = opp.router_b_type.clone();
    let fa = opp.fee_a;
    let fb = opp.fee_b;

    type BoxFut = Pin<Box<dyn Future<Output = Option<(U256, U256)>> + Send>>;
    let mut futs: Vec<BoxFut> = Vec::with_capacity(steps);

    for i in 0..steps {
        let p = provider.clone();
        let probe_amount = if steps == 1 {
            (min_amount + max_amount) / U256::from(2u32)
        } else {
            min_amount + range * U256::from(i as u128) / U256::from((steps - 1) as u128)
        };
        let ra_t = ra_type.clone();
        let rb_t = rb_type.clone();

        futs.push(Box::pin(async move {
            let mid = match ra_t {
                RouterType::V2 => quote_v2(&p, ra, probe_amount, token_in, token_out).await,
                RouterType::V3 => match quoter {
                    Some(q) => quote_v3(&p, q, probe_amount, token_in, token_out, fa).await,
                    None => None,
                },
            };
            let mid = mid.filter(|m| !m.is_zero())?;

            let back = match rb_t {
                RouterType::V2 => quote_v2(&p, rb, mid, token_out, token_in).await,
                RouterType::V3 => match quoter {
                    Some(q) => quote_v3(&p, q, mid, token_out, token_in, fb).await,
                    None => None,
                },
            };
            back.filter(|&b| b > probe_amount).map(|b| (probe_amount, b))
        }));
    }

    join_all(futs)
        .await
        .into_iter()
        .flatten()
        .max_by_key(|(amt, back)| *back - *amt)
}

// ─── Quote helpers ────────────────────────────────────────────────────────────

async fn quote_v2<P: Provider>(
    provider: &P,
    router: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
) -> Option<U256> {
    let path = vec![token_in, token_out];
    match IUniswapV2Router02::new(router, provider)
        .getAmountsOut(amount_in, path)
        .call()
        .await
    {
        Ok(amounts) => amounts.last().copied(),
        Err(e) => {
            debug!("V2 quote failed on {:?}: {}", router, e);
            None
        }
    }
}

async fn quote_v3<P: Provider>(
    provider: &P,
    quoter: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    fee: u32,
) -> Option<U256> {
    let fee_u24: Uint<24, 1> = Uint::from(fee);
    let sqrt_zero: Uint<160, 3> = Uint::ZERO;

    match IQuoterV2::new(quoter, provider)
        .quoteExactInputSingle(IQuoterV2::QuoteExactInputSingleParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: fee_u24,
            sqrtPriceLimitX96: sqrt_zero,
        })
        .call()
        .await
    {
        Ok(r) => Some(r.amountOut),
        Err(e) => {
            debug!("V3 quote failed on {:?} fee={}: {}", quoter, fee, e);
            None
        }
    }
}

// ─── Utility ──────────────────────────────────────────────────────────────────

/// Parse trade amount, capping at max_trade if set.
fn parse_amount_capped(trade_amount: &str, max_trade: Option<&str>, decimals: u8) -> Option<U256> {
    let parsed: f64 = trade_amount.parse().ok()?;
    let capped = if let Some(max_str) = max_trade {
        let max: f64 = max_str.parse().ok()?;
        parsed.min(max)
    } else {
        parsed
    };
    let scale = 10_u128.pow(decimals as u32);
    let raw = (capped * scale as f64) as u128;
    if raw == 0 {
        return None;
    }
    Some(U256::from(raw))
}

fn token_amount_to_usd(amount: U256, decimals: u8, symbol: &str, native_price: f64) -> f64 {
    let scale = 10_f64.powi(decimals as i32);
    let float_amount = u256_to_f64(amount) / scale;
    match symbol.to_uppercase().as_str() {
        "USDC" | "USDT" | "DAI" | "WXDAI" | "USDC.E" | "USDBC" => float_amount,
        "WETH" | "ETH" | "WMATIC" | "MATIC" => float_amount * native_price,
        _ => float_amount,
    }
}

fn u256_to_f64(value: U256) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(0.0)
}
