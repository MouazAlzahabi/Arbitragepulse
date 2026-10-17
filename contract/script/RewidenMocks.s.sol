// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "./MockMarket.sol";

/**
 * @title RewidenMocks — restore the testnet mock spread after arbs have narrowed it
 *
 * Each executed arb buys mWETH on the cheap DEX (V2) and sells it on the dear one (V3),
 * pulling the prices together. This script trades both pools back to their targets
 * (MockMarket.V2_PRICE / V3_PRICE) using the deployer's mock tokens. Same token
 * addresses — no config change.
 *
 * Usage:
 *   MUSDC=0x… MWETH=0x… forge script script/RewidenMocks.s.sol --rpc-url base_sepolia --broadcast
 */

contract RewidenMocks is Script {
    uint256 constant TOLERANCE_BPS = 50; // final price must be within 0.5% of target

    function run() external {
        uint256 key = vm.envUint("PRIVATE_KEY");
        address me = vm.addr(key);
        address usdc = vm.envAddress("MUSDC");
        address weth = vm.envAddress("MWETH");
        bool wethIsToken0 = weth < usdc;

        address pair = IMockV2Factory(IMockV2Router(MockMarket.V2_ROUTER).factory()).getPair(usdc, weth);
        require(pair != address(0), "no V2 pair for MUSDC/MWETH - run DeployMocks.s.sol first");
        address pool = IMockV3Factory(MockMarket.V3_FACTORY).getPool(usdc, weth, MockMarket.V3_FEE);
        require(pool != address(0), "no V3 pool for MUSDC/MWETH - run DeployMocks.s.sol first");

        vm.startBroadcast(key);
        IERC20(usdc).approve(MockMarket.V2_ROUTER, type(uint256).max);
        IERC20(weth).approve(MockMarket.V2_ROUTER, type(uint256).max);
        IERC20(usdc).approve(MockMarket.V3_SWAP_ROUTER, type(uint256).max);
        IERC20(weth).approve(MockMarket.V3_SWAP_ROUTER, type(uint256).max);

        // ── V2: find the input whose swap lands the pool on V2_PRICE. The whole input
        //    (fee included) stays in the reserve while the output is priced on 99.7% of
        //    it, so solve with the pair's own formula instead of a k-constant shortcut.
        (uint256 rWeth, uint256 rUsdc) = _v2Reserves(pair, weth);
        require(rWeth > 0 && rUsdc > 0, "V2 pair has no liquidity - run DeployMocks.s.sol");
        address[] memory path = new address[](2);
        bool sellWeth = rUsdc * 1e18 / rWeth > MockMarket.V2_PRICE * 1e18; // mWETH too dear → sell it
        (path[0], path[1]) = sellWeth ? (weth, usdc) : (usdc, weth);
        uint256 amountIn = sellWeth
            ? _v2InputForPrice(rWeth, rUsdc, true)
            : _v2InputForPrice(rUsdc, rWeth, false);
        if (amountIn > 0) {
            require(IERC20(path[0]).balanceOf(me) >= amountIn, "deployer balance too low to reprice V2");
            IMockV2Router(MockMarket.V2_ROUTER).swapExactTokensForTokens(amountIn, 0, path, me, block.timestamp + 600);
        }

        // ── V3: offer the whole balance with a price limit — the pool stops exactly at the
        //    target and the router only takes what that needs.
        (uint160 sqrtNow,,,,,,) = IMockV3Pool(pool).slot0();
        uint160 sqrtTarget = MockMarket.sqrtPriceX96(MockMarket.V3_PRICE, wethIsToken0);
        if (sqrtNow != sqrtTarget) {
            // Price = token1/token0. Paying token0 in (zeroForOne) lowers it; paying token1
            // in raises it. Target below current → zeroForOne.
            bool zeroForOne = sqrtTarget < sqrtNow;
            address tIn = zeroForOne == wethIsToken0 ? weth : usdc;
            ISwapRouterV3(MockMarket.V3_SWAP_ROUTER).exactInputSingle(ISwapRouterV3.ExactInputSingleParams({
                tokenIn: tIn, tokenOut: tIn == weth ? usdc : weth, fee: MockMarket.V3_FEE, recipient: me,
                amountIn: IERC20(tIn).balanceOf(me), amountOutMinimum: 0, sqrtPriceLimitX96: sqrtTarget
            }));
        }
        vm.stopBroadcast();

        (rWeth, rUsdc) = _v2Reserves(pair, weth);
        uint256 v2PriceE6 = rUsdc * 1e6 / rWeth;
        (sqrtNow,,,,,,) = IMockV3Pool(pool).slot0();
        uint256 v3PriceE6 = MockMarket.priceE6(sqrtNow, wethIsToken0);
        console.log("V2 price mUSDC/mWETH (x1e6):", v2PriceE6);
        console.log("V3 price mUSDC/mWETH (x1e6):", v3PriceE6);
        require(_near(v2PriceE6, MockMarket.V2_PRICE * 1e6), "V2 did not reach its target price");
        require(_near(v3PriceE6, MockMarket.V3_PRICE * 1e6), "V3 did not reach its target - deployer balance too low");
    }

    /// @dev Smallest input (binary search) after which price = rUsdc'/rWeth' crosses
    ///      V2_PRICE. `wethIn`: selling mWETH lowers the price; else buying raises it.
    function _v2InputForPrice(uint256 rIn, uint256 rOut, bool wethIn) internal pure returns (uint256) {
        uint256 lo = 0;
        uint256 cap = rIn * 100; // ~10_000× price move — far beyond any narrowing by arbs
        uint256 hi = cap + 1;    // one past the cap: lo == cap + 1 means "never reached"
        for (uint256 i = 0; i < 128 && lo < hi; i++) {
            uint256 mid = (lo + hi) / 2;
            uint256 out = mid * 997 * rOut / (rIn * 1000 + mid * 997);
            (uint256 newWeth, uint256 newUsdc) = wethIn ? (rIn + mid, rOut - out) : (rOut - out, rIn + mid);
            bool reached = wethIn
                ? newUsdc * 1e18 / newWeth <= MockMarket.V2_PRICE * 1e18
                : newUsdc * 1e18 / newWeth >= MockMarket.V2_PRICE * 1e18;
            if (reached) hi = mid; else lo = mid + 1;
        }
        // Fail before any swap when even `cap` doesn't reach the target.
        require(lo <= cap, "V2 target price out of search range - pool far off, redeploy mocks");
        return lo;
    }

    function _v2Reserves(address pair, address weth) internal view returns (uint256 rWeth, uint256 rUsdc) {
        (uint112 r0, uint112 r1,) = IMockV2Pair(pair).getReserves();
        (rWeth, rUsdc) = IMockV2Pair(pair).token0() == weth ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
    }

    function _near(uint256 got, uint256 want) internal pure returns (bool) {
        uint256 diff = got > want ? got - want : want - got;
        return diff * 10_000 <= want * TOLERANCE_BPS;
    }
}
