use alloy::primitives::{Address, Uint, U256};
use alloy::providers::Provider;
use futures::future::join_all;
use std::future::Future;
use std::pin::Pin;
use tracing::debug;

use crate::abi::{IAerodromeRouter, IQuoterV2, ISolidlyRouter, IUniswapV2Router02};
use crate::config::RouterType;
use crate::types::ArbOpportunity;
use crate::util::u256_to_f64;
use super::{Strategy, parse_amount_capped};

impl Strategy {
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
        max_balance: Option<U256>,
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

        // Upper bound: use max_trade if configured, otherwise trade_amount.
        // Previously used double_amount.min(cap) which capped at 2× the tick-capped
        // amount_in ($6 → $12), blocking the optimizer from probing up to $100.
        let double_amount = opp.amount_in * U256::from(2u32);
        let max_amount = self
            .pairs
            .iter()
            .find(|p| p.id == opp.pair_id)
            .and_then(|p| {
                let cap_str = p.max_trade.as_deref().unwrap_or(&p.trade_amount);
                parse_amount_capped(cap_str, None, p.token_in_decimals)
            })
            .unwrap_or(double_amount);

        // Also cap by the contract's known token_in balance (from periodic refresh).
        // Prevents the optimizer from probing amounts the contract cannot cover.
        let max_amount = if let Some(bal) = max_balance {
            if bal.is_zero() {
                // Contract has no balance — cannot execute, skip optimizer
                return opp.clone();
            }
            max_amount.min(bal)
        } else {
            max_amount
        };

        if min_amount >= max_amount || min_amount.is_zero() {
            return opp.clone();
        }

        // ── Resolve per-router QuoterV2 addresses ────────────────────────────
        // RouterConfig.quoter_address overrides the chain-level quoter_v2_address.
        // PancakeV3 and UniswapV3 have different QuoterV2 contracts on Linea.
        let quoter_a: Option<Address> = self.routers.iter()
            .find(|r| r.id == opp.router_a_id)
            .and_then(|r| r.quoter_address.as_deref())
            .and_then(|s| s.parse().ok())
            .or(self.quoter_v2_address);
        let quoter_b: Option<Address> = self.routers.iter()
            .find(|r| r.id == opp.router_b_id)
            .and_then(|r| r.quoter_address.as_deref())
            .and_then(|s| s.parse().ok())
            .or(self.quoter_v2_address);

        // ── Round 1: Coarse scan across full range ────────────────────────────
        let coarse_winner = probe_range(
            opp,
            provider,
            quoter_a,
            quoter_b,
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
            quoter_a,
            quoter_b,
            fine_min,
            fine_max,
            FINE,
        )
        .await;

        // Pick whichever gave more absolute profit
        let (opt_amount, opt_back_raw) = [coarse_winner, fine_winner]
            .into_iter()
            .flatten()
            .max_by_key(|(amt, back)| *back - *amt)
            .unwrap_or((opp.amount_in, opp.amount_in + opp.expected_profit));

        // Apply 0.08% slippage buffer to opt_back: the optimizer uses live QuoterV2/RPC quotes
        // at probe time, but the tx executes ~60-130ms later. In that window, V3 and Aerodrome
        // pools can move (~0.02-0.04% per 2s block). Without this buffer, the optimizer strips
        // the buffer that evaluate.rs applied, allowing thin-margin upgrades that fail on-chain.
        let opt_back = (opt_back_raw * U256::from(9992)) / U256::from(10000);

        let opt_profit = if opt_back > opt_amount {
            opt_back - opt_amount
        } else {
            return opp.clone();
        };

        // Only update if strictly better than the originally detected (already-buffered) profit.
        // This comparison is valid: opp.expected_profit was computed with a buffer in evaluate.rs;
        // opt_profit is now also buffered — same units, safe to compare.
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
}

// ─── Probe helper ─────────────────────────────────────────────────────────────

/// Fire `steps` linearly-spaced quote pairs concurrently across [min_amount, max_amount].
/// Returns `(amount_in, amount_back)` for the probe with the highest absolute profit,
/// or `None` if no probe was profitable.
async fn probe_range<P: Provider + Clone + 'static>(
    opp: &ArbOpportunity,
    provider: &P,
    quoter_a: Option<Address>, // QuoterV2 for router A (leg 1 — token_in → token_out)
    quoter_b: Option<Address>, // QuoterV2 for router B (leg 2 — token_out → token_in)
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
                RouterType::V3 => match quoter_a {
                    Some(q) => quote_v3(&p, q, probe_amount, token_in, token_out, fa).await,
                    None => None,
                },
                RouterType::Solidly => {
                    quote_solidly(&p, ra, probe_amount, token_in, token_out, fa).await
                }
                RouterType::Aerodrome => {
                    quote_aerodrome(&p, ra, probe_amount, token_in, token_out, fa).await
                }
                RouterType::SyncSwap => None, // pool address not available in probe_range
            };
            let mid = mid.filter(|m| !m.is_zero())?;

            let back = match rb_t {
                RouterType::V2 => quote_v2(&p, rb, mid, token_out, token_in).await,
                RouterType::V3 => match quoter_b {
                    Some(q) => quote_v3(&p, q, mid, token_out, token_in, fb).await,
                    None => None,
                },
                RouterType::Solidly => {
                    quote_solidly(&p, rb, mid, token_out, token_in, fb).await
                }
                RouterType::Aerodrome => {
                    quote_aerodrome(&p, rb, mid, token_out, token_in, fb).await
                }
                RouterType::SyncSwap => None,
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
    let fee_u24: alloy::primitives::Uint<24, 1> = Uint::from(fee);
    let sqrt_zero: alloy::primitives::Uint<160, 3> = Uint::ZERO;

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

async fn quote_solidly<P: Provider>(
    provider: &P,
    router: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    fee: u32, // 0=volatile, 1=stable
) -> Option<U256> {
    let stable = fee != 0;
    match ISolidlyRouter::new(router, provider)
        .getAmountsOut(
            amount_in,
            vec![ISolidlyRouter::Route {
                from: token_in,
                to: token_out,
                stable,
            }],
        )
        .call()
        .await
    {
        Ok(amounts) => amounts.last().copied(),
        Err(e) => {
            debug!("Solidly quote failed on {:?} stable={}: {}", router, stable, e);
            None
        }
    }
}

async fn quote_aerodrome<P: Provider>(
    provider: &P,
    router: Address,
    amount_in: U256,
    token_in: Address,
    token_out: Address,
    fee: u32, // 0=volatile, 1=stable
) -> Option<U256> {
    let stable = fee != 0;
    // factory=address(0) → Aerodrome router uses its internal default factory
    match IAerodromeRouter::new(router, provider)
        .getAmountsOut(
            amount_in,
            vec![IAerodromeRouter::Route {
                from: token_in,
                to: token_out,
                stable,
                factory: Address::ZERO,
            }],
        )
        .call()
        .await
    {
        Ok(amounts) => amounts.last().copied(),
        Err(e) => {
            debug!("Aerodrome quote failed on {:?} stable={}: {}", router, stable, e);
            None
        }
    }
}
