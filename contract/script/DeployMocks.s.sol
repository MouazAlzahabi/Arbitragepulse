// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/mocks/MockERC20.sol";
import "./MockMarket.sol";

/**
 * @title DeployMocks — guaranteed arbitrage spread for the Base Sepolia testing stage
 *
 * Deploys mUSDC and mWETH (18 decimals) and seeds them on the REAL Uniswap V2 and V3
 * deployments at prices MockMarket.V2_PRICE / V3_PRICE apart, so the engine finds them
 * through its normal pool discovery. Executed arbs narrow the gap over time; restore it
 * with RewidenMocks.s.sol (same tokens, no config change).
 *
 * Usage:
 *   forge script script/DeployMocks.s.sol --rpc-url base_sepolia --broadcast
 */
contract DeployMocks is Script {
    function run() external {
        uint256 key = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(key);
        vm.startBroadcast(key);

        MockERC20 usdc = new MockERC20("Mock USDC", "mUSDC", 10_000_000e18);
        MockERC20 weth = new MockERC20("Mock WETH", "mWETH", 10_000e18);

        // ── Uniswap V2 pair at V2_PRICE ──
        uint256 v2Usdc = MockMarket.LIQ_WETH * MockMarket.V2_PRICE;
        usdc.approve(MockMarket.V2_ROUTER, type(uint256).max);
        weth.approve(MockMarket.V2_ROUTER, type(uint256).max);
        // Exact minimums: a pair that already exists at another price fails here.
        IMockV2Router(MockMarket.V2_ROUTER).addLiquidity(
            address(weth), address(usdc), MockMarket.LIQ_WETH, v2Usdc,
            MockMarket.LIQ_WETH, v2Usdc, deployer, block.timestamp + 600
        );
        address v2Pair = IMockV2Factory(IMockV2Router(MockMarket.V2_ROUTER).factory())
            .getPair(address(weth), address(usdc));

        // ── Uniswap V3 pool at V3_PRICE, full range ──
        bool wethIsToken0 = address(weth) < address(usdc);
        (address t0, address t1) = wethIsToken0 ? (address(weth), address(usdc)) : (address(usdc), address(weth));
        IMockPositionManager npm = IMockPositionManager(MockMarket.V3_POSITION_MANAGER);
        address v3Pool = npm.createAndInitializePoolIfNecessary(
            t0, t1, MockMarket.V3_FEE, MockMarket.sqrtPriceX96(MockMarket.V3_PRICE, wethIsToken0)
        );
        // Fail loudly if the pool already existed at another price.
        (uint160 sqrtP,,,,,,) = IMockV3Pool(v3Pool).slot0();
        uint256 v3PriceE6 = MockMarket.priceE6(sqrtP, wethIsToken0);
        require(
            v3PriceE6 > MockMarket.V3_PRICE * 1e6 * 999 / 1000 && v3PriceE6 < MockMarket.V3_PRICE * 1e6 * 1001 / 1000,
            "V3 pool exists at a different price"
        );

        usdc.approve(MockMarket.V3_POSITION_MANAGER, type(uint256).max);
        weth.approve(MockMarket.V3_POSITION_MANAGER, type(uint256).max);
        int24 maxTick = 887272 / MockMarket.TICK_SPACING * MockMarket.TICK_SPACING;
        uint256 v3Usdc = MockMarket.LIQ_WETH * MockMarket.V3_PRICE;
        (uint256 d0, uint256 d1) = wethIsToken0 ? (MockMarket.LIQ_WETH, v3Usdc) : (v3Usdc, MockMarket.LIQ_WETH);
        npm.mint(IMockPositionManager.MintParams({
            token0: t0, token1: t1, fee: MockMarket.V3_FEE, tickLower: -maxTick, tickUpper: maxTick,
            amount0Desired: d0, amount1Desired: d1, amount0Min: d0 * 99 / 100, amount1Min: d1 * 99 / 100,
            recipient: deployer, deadline: block.timestamp + 600
        }));

        vm.stopBroadcast();

        console.log("======================================");
        console.log("mUSDC (token_in): ", address(usdc));
        console.log("mWETH (token_out):", address(weth));
        console.log("Uniswap V2 pair:  ", v2Pair);
        console.log("  price mUSDC/mWETH:", MockMarket.V2_PRICE);
        console.log("Uniswap V3 pool:  ", v3Pool);
        console.log("  price mUSDC/mWETH:", v3PriceE6 / 1e6, " fee (ppm):", uint256(MockMarket.V3_FEE));
        console.log("======================================");
        console.log("Whitelist on the executor (setRouter(router, type)):");
        console.log("  Uniswap V2 Router02, type 0:", MockMarket.V2_ROUTER);
        console.log("  Uniswap V3 SwapRouter02, type 1:", MockMarket.V3_SWAP_ROUTER);
        console.log("Then send mUSDC to the executor and paste mUSDC/mWETH into");
        console.log("engine/config-testnet.yaml (base-sepolia-mock-usdc-weth).");
        console.log("Restore the spread later with: MUSDC=... MWETH=... forge script script/RewidenMocks.s.sol");
    }
}
