// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "@openzeppelin/contracts/utils/math/Math.sol";
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

        // ── V2: with k fixed, price P ⇔ rWeth' = sqrt(k / P). The router keeps 0.3% of the
        //    input, so send delta × 1000/997 to land the effective input on the target.
        (uint256 rWeth, uint256 rUsdc) = _v2Reserves(pair, weth);
        require(rWeth > 0 && rUsdc > 0, "V2 pair has no liquidity - run DeployMocks.s.sol");
        uint256 targetWeth = Math.sqrt(rWeth * rUsdc / MockMarket.V2_PRICE);
        require(targetWeth > 0, "V2 pair reserves too small to reprice");
        address[] memory path = new address[](2);
        uint256 amountIn;
        if (targetWeth > rWeth) {
            // V2 mWETH too expensive → sell mWETH into it
            (path[0], path[1]) = (weth, usdc);
            amountIn = (targetWeth - rWeth) * 1000 / 997;
        } else if (targetWeth < rWeth) {
            // V2 mWETH too cheap → buy mWETH with mUSDC
            (path[0], path[1]) = (usdc, weth);
            amountIn = (rWeth * rUsdc / targetWeth - rUsdc) * 1000 / 997;
        }
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

    function _v2Reserves(address pair, address weth) internal view returns (uint256 rWeth, uint256 rUsdc) {
        (uint112 r0, uint112 r1,) = IMockV2Pair(pair).getReserves();
        (rWeth, rUsdc) = IMockV2Pair(pair).token0() == weth ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
    }

    function _near(uint256 got, uint256 want) internal pure returns (bool) {
        uint256 diff = got > want ? got - want : want - got;
        return diff * 10_000 <= want * TOLERANCE_BPS;
    }
}
