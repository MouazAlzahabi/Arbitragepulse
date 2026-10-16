// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../script/MockMarket.sol";

/// @dev The testnet mock market's price math, for both token orders.
contract MockMarketTest is Test {
    function test_SqrtPriceRoundTrips_BothOrders() public pure {
        for (uint256 p = 1; p <= 100_000; p *= 7) {
            for (uint256 o = 0; o < 2; o++) {
                bool wethIsToken0 = o == 0;
                uint256 back = MockMarket.priceE6(MockMarket.sqrtPriceX96(p, wethIsToken0), wethIsToken0);
                assertApproxEqRel(back, p * 1e6, 1e12, "price must round-trip within 1e-6");
            }
        }
    }

    function test_TargetSpreadUnderPhantomCap() public pure {
        // Engine rejects > 5% (tuning.phantom_cap_bps = 500).
        assertLt((MockMarket.V3_PRICE - MockMarket.V2_PRICE) * 10_000 / MockMarket.V2_PRICE, 500);
    }
}
