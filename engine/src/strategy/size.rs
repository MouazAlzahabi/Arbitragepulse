//! Trade sizing from cached pool state, with no RPC.
//!
//! A chain of constant-product legs is itself a Möbius map `out(x) = a·x / (b + c·x)`.
//! Composing one more leg `(r_in, r_out, γ)`:
//!   a' = γ·r_out·a,  b' = r_in·b,  c' = r_in·c + γ·a.
//! Profit `out(x) − x` peaks where out'(x) = a·b / (b + c·x)² = 1, i.e.
//!   x* = (√(a·b) − b) / c,
//! and is positive only when the marginal rate a/b > 1. V3 legs use their in-range virtual
//! reserves, exact up to the tick cap, so x* is also held under every leg's `max_in`.

use crate::pool_cache::LegCurve;

/// The route `legs` (in order) as Möbius coefficients `(a, b, c)` of `out(x) = a·x/(b + c·x)`,
/// normalised to b = 1 after every leg so 3-leg products of 1e25-scale reserves stay finite.
fn compose(legs: &[LegCurve]) -> (f64, f64, f64) {
    let (mut a, mut b, mut c) = (1.0_f64, 1.0_f64, 0.0_f64);
    for l in legs {
        let (na, nb, nc) = (l.gamma * l.r_out * a, l.r_in * b, l.r_in * c + l.gamma * a);
        (a, b, c) = (na / nb, 1.0, nc / nb);
    }
    (a, b, c)
}

/// Profit-maximising input for a route of xy=k legs, clamped so no leg is fed more than its
/// `max_in`. None when the route has no edge at the margin (a/b ≤ 1), or its inputs are
/// degenerate (zero/non-finite reserves).
pub(crate) fn optimal_input(legs: &[LegCurve]) -> Option<f64> {
    if legs.is_empty()
        || legs.iter().any(|l| !(l.r_in > 0.0 && l.r_out > 0.0 && l.gamma > 0.0 && l.r_in.is_finite() && l.r_out.is_finite()))
    {
        return None;
    }
    let (a, b, c) = compose(legs);
    if !(a > b) || !(c > 0.0) {
        return None;
    }
    let mut x = ((a * b).sqrt() - b) / c;
    // Leg k's input is the output of legs 0..k: invert that prefix map at the leg's cap.
    for k in 0..legs.len() {
        let cap = legs[k].max_in;
        if !cap.is_finite() {
            continue;
        }
        let (pa, pb, pc) = compose(&legs[..k]);
        let max_x = if k == 0 {
            cap
        } else if pa > cap * pc {
            cap * pb / (pa - cap * pc)
        } else {
            f64::INFINITY // the prefix can never deliver `cap`
        };
        x = x.min(max_x);
    }
    (x.is_finite() && x > 0.0).then_some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(r_in: f64, r_out: f64, fee: f64) -> LegCurve {
        LegCurve { r_in, r_out, gamma: 1.0 - fee, max_in: f64::INFINITY }
    }

    fn out(legs: &[LegCurve], x: f64) -> f64 {
        legs.iter().fold(x, |v, l| l.gamma * v * l.r_out / (l.r_in + l.gamma * v))
    }

    #[test]
    fn two_hop_matches_closed_form_and_beats_neighbours() {
        // Pool A sells X cheap (1 USDC → 1.01 X), pool B buys it at 1.0: a 1% gap.
        let legs = [leg(1e12, 1.01e12, 0.0005), leg(1e12, 1e12, 0.0005)];
        let x = optimal_input(&legs).unwrap();
        let p = |x: f64| out(&legs, x) - x;
        assert!(p(x) > 0.0);
        assert!(p(x) >= p(x * 0.99) && p(x) >= p(x * 1.01), "x*={x} is not the profit peak");
        // Textbook two-pool formula: x* = (√(γ1γ2 R1i R1o R2i R2o) − R1i R2i) / (γ1 (R2i + γ2 R1o)).
        let (g1, g2) = (legs[0].gamma, legs[1].gamma);
        let want = ((g1 * g2 * 1e12 * 1.01e12 * 1e12 * 1e12).sqrt() - 1e24) / (g1 * (1e12 + g2 * 1.01e12));
        assert!((x - want).abs() / want < 1e-9);
    }

    #[test]
    fn no_edge_means_no_size() {
        // Same price, fees on both sides: the round trip loses at any size.
        assert_eq!(optimal_input(&[leg(1e12, 1e12, 0.0005), leg(1e12, 1e12, 0.0005)]), None);
        assert_eq!(optimal_input(&[]), None);
        assert_eq!(optimal_input(&[leg(0.0, 1e12, 0.0), leg(1e12, 1e12, 0.0)]), None);
    }

    #[test]
    fn three_legs_peak_and_scale_free() {
        // USDC(6) → WETH(18) → cbBTC(8) → USDC(6), raw units, a 0.6% loop gap.
        let legs = [leg(3e13, 1e22, 0.0005), leg(1e22, 2.9e10, 0.0005), leg(2.9e10, 3.03e13, 0.0001)];
        let x = optimal_input(&legs).unwrap();
        let p = |x: f64| out(&legs, x) - x;
        assert!(p(x) > 0.0 && p(x) >= p(x * 0.98) && p(x) >= p(x * 1.02));
    }

    #[test]
    fn caps_hold_every_leg_within_its_limit() {
        let mut legs = [leg(1e12, 1.05e12, 0.0005), leg(1e12, 1e12, 0.0005)];
        let free = optimal_input(&legs).unwrap();
        legs[1].max_in = free * 0.3; // the second leg's tick cap binds
        let x = optimal_input(&legs).unwrap();
        assert!(x < free);
        let into_b = out(&legs[..1], x);
        assert!((into_b - legs[1].max_in).abs() / legs[1].max_in < 1e-9);
        legs[0].max_in = x * 0.5; // now the first leg's cap binds harder
        assert!((optimal_input(&legs).unwrap() - x * 0.5).abs() < 1e-3);
    }
}
