// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "@openzeppelin/contracts/utils/math/Math.sol";
import "../src/mocks/MockERC20.sol";

/**
 * @title DeployMocks — guaranteed arbitrage spread for the Base Sepolia testing stage
 *
 * Deploys two mock tokens (mUSDC, mWETH; 18 decimals) and seeds them on two real DEXes
 * at prices ~3% apart, so the engine sees a genuine cross-DEX spread through its normal
 * pool discovery (factory.getPair / factory.getPool) — no special mock routers needed:
 *   • Uniswap V2:            1 mWETH = 2500 mUSDC
 *   • Uniswap V3 (0.3% fee): 1 mWETH = 2575 mUSDC (full-range liquidity)
 * 3% stays under the engine's 5% phantom cap (tuning.phantom_cap_bps).
 *
 * The deployer keeps the remaining tokens; send some mUSDC to the executor to trade.
 *
 * Usage (Base Sepolia, or an Anvil fork of it):
 *   forge script script/DeployMocks.s.sol --rpc-url $BASE_SEPOLIA_RPC --broadcast
 * Then paste the printed addresses into engine/config-testnet.yaml.
 */

interface IUniswapV2Router02Mini {
    function factory() external view returns (address);
    function addLiquidity(
        address tokenA, address tokenB, uint256 amountADesired, uint256 amountBDesired,
        uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline
    ) external returns (uint256 amountA, uint256 amountB, uint256 liquidity);
}

interface IUniswapV2FactoryMini {
    function getPair(address tokenA, address tokenB) external view returns (address);
}

interface INonfungiblePositionManagerMini {
    struct MintParams {
        address token0; address token1; uint24 fee; int24 tickLower; int24 tickUpper;
        uint256 amount0Desired; uint256 amount1Desired; uint256 amount0Min; uint256 amount1Min;
        address recipient; uint256 deadline;
    }
    function createAndInitializePoolIfNecessary(address token0, address token1, uint24 fee, uint160 sqrtPriceX96)
        external payable returns (address pool);
    function mint(MintParams calldata params)
        external payable returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1);
}

contract DeployMocks is Script {
    // Uniswap deployments on Base Sepolia (verified on-chain 2026-10-16).
    address constant V2_ROUTER = 0x1689E7B1F10000AE47eBfE339a4f69dECd19F602;
    address constant V3_POSITION_MANAGER = 0x27F971cb582BF9E50F397e4d29a5C7A34f11faA2;
    uint24 constant V3_FEE = 3000;
    int24 constant TICK_SPACING = 60;

    uint256 constant V2_PRICE = 2500; // mUSDC per mWETH
    uint256 constant V3_PRICE = 2575; // +3%
    uint256 constant LIQ_WETH = 100e18; // per DEX

    function run() external {
        uint256 key = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(key);
        vm.startBroadcast(key);

        MockERC20 usdc = new MockERC20("Mock USDC", "mUSDC", 10_000_000e18);
        MockERC20 weth = new MockERC20("Mock WETH", "mWETH", 10_000e18);

        // ── Uniswap V2 pool at V2_PRICE ──
        usdc.approve(V2_ROUTER, type(uint256).max);
        weth.approve(V2_ROUTER, type(uint256).max);
        IUniswapV2Router02Mini(V2_ROUTER).addLiquidity(
            address(weth), address(usdc), LIQ_WETH, LIQ_WETH * V2_PRICE, 0, 0, deployer, block.timestamp + 600
        );
        address v2Pair = IUniswapV2FactoryMini(IUniswapV2Router02Mini(V2_ROUTER).factory())
            .getPair(address(weth), address(usdc));

        // ── Uniswap V3 pool at V3_PRICE, full range ──
        (address t0, address t1) = address(usdc) < address(weth)
            ? (address(usdc), address(weth))
            : (address(weth), address(usdc));
        // price = token1 per token0; sqrtPriceX96 = sqrt(price) * 2^96, computed as
        // sqrt(amount1 * 2^96 / amount0) * 2^48 to stay within 256 bits.
        (uint256 amt0, uint256 amt1) = t0 == address(weth)
            ? (uint256(1e18), V3_PRICE * 1e18)
            : (V3_PRICE * 1e18, uint256(1e18));
        uint160 sqrtPriceX96 = uint160(Math.sqrt((amt1 << 96) / amt0) << 48);
        INonfungiblePositionManagerMini npm = INonfungiblePositionManagerMini(V3_POSITION_MANAGER);
        address v3Pool = npm.createAndInitializePoolIfNecessary(t0, t1, V3_FEE, sqrtPriceX96);
        usdc.approve(V3_POSITION_MANAGER, type(uint256).max);
        weth.approve(V3_POSITION_MANAGER, type(uint256).max);
        int24 maxTick = 887272 / TICK_SPACING * TICK_SPACING;
        (uint256 d0, uint256 d1) = t0 == address(weth)
            ? (LIQ_WETH, LIQ_WETH * V3_PRICE)
            : (LIQ_WETH * V3_PRICE, LIQ_WETH);
        npm.mint(INonfungiblePositionManagerMini.MintParams({
            token0: t0, token1: t1, fee: V3_FEE, tickLower: -maxTick, tickUpper: maxTick,
            amount0Desired: d0, amount1Desired: d1, amount0Min: 0, amount1Min: 0,
            recipient: deployer, deadline: block.timestamp + 600
        }));

        vm.stopBroadcast();

        console.log("======================================");
        console.log("mUSDC (token_in):  ", address(usdc));
        console.log("mWETH (token_out): ", address(weth));
        console.log("Uniswap V2 pair:   ", v2Pair, " (1 mWETH = 2500 mUSDC)");
        console.log("Uniswap V3 pool:   ", v3Pool, " (1 mWETH = 2575 mUSDC, fee 0.3%)");
        console.log("======================================");
        console.log("Next: paste mUSDC/mWETH into engine/config-testnet.yaml (mock pair),");
        console.log("      send mUSDC to the executor, and whitelist both routers on it.");
    }
}
