// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/utils/math/Math.sol";
// Swap interfaces (IUniswapV2Router02, ISwapRouterV3) come from the executor, so the
// scripts and the contract encode the same calldata.
import "../src/ArbitrageExecutor.sol";

// Extra Uniswap interfaces shared by DeployMocks.s.sol and RewidenMocks.s.sol.

interface IMockV2Router is IUniswapV2Router02 {
    function factory() external view returns (address);
    function addLiquidity(
        address tokenA, address tokenB, uint256 amountADesired, uint256 amountBDesired,
        uint256 amountAMin, uint256 amountBMin, address to, uint256 deadline
    ) external returns (uint256 amountA, uint256 amountB, uint256 liquidity);
}

interface IMockV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address);
}

interface IMockV2Pair {
    function getReserves() external view returns (uint112, uint112, uint32);
    function token0() external view returns (address);
}

interface IMockV3Factory {
    function getPool(address, address, uint24) external view returns (address);
}

interface IMockV3Pool {
    function slot0() external view returns (uint160 sqrtPriceX96, int24, uint16, uint16, uint16, uint8, bool);
}

interface IMockPositionManager {
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

/// @dev Shared constants + price math for the Base Sepolia mock market
///      (DeployMocks.s.sol seeds it, RewidenMocks.s.sol restores the spread).
library MockMarket {
    // Uniswap deployments on Base Sepolia (verified on-chain 2026-10-16).
    address internal constant V2_ROUTER = 0x1689E7B1F10000AE47eBfE339a4f69dECd19F602;
    address internal constant V3_SWAP_ROUTER = 0x94cC0AaC535CCDB3C01d6787D6413C739ae12bc4; // SwapRouter02
    address internal constant V3_POSITION_MANAGER = 0x27F971cb582BF9E50F397e4d29a5C7A34f11faA2;
    address internal constant V3_FACTORY = 0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24;
    uint24 internal constant V3_FEE = 3000;
    int24 internal constant TICK_SPACING = 60;

    uint256 internal constant V2_PRICE = 2500; // mUSDC per mWETH
    uint256 internal constant V3_PRICE = 2575; // +3% — under the engine's 5% phantom cap
    uint256 internal constant LIQ_WETH = 100e18; // per DEX

    /// @dev sqrtPriceX96 for `price` (mUSDC per mWETH, both 18 decimals) given which token
    ///      sorts first. Uniswap price = token1 per token0.
    function sqrtPriceX96(uint256 price, bool wethIsToken0) internal pure returns (uint160) {
        (uint256 amt0, uint256 amt1) = wethIsToken0 ? (uint256(1e18), price * 1e18) : (price * 1e18, uint256(1e18));
        // sqrt(amt1/amt0) * 2^96 = sqrt(amt1 * 2^96 / amt0) * 2^48 — stays within 256 bits.
        return uint160(Math.sqrt((amt1 << 96) / amt0) << 48);
    }

    /// @dev Inverse of sqrtPriceX96, scaled by 1e6 (for tests and sanity checks).
    function priceE6(uint160 sqrtP, bool wethIsToken0) internal pure returns (uint256) {
        uint256 sq = uint256(sqrtP) * uint256(sqrtP); // token1/token0 × 2^192
        return wethIsToken0 ? Math.mulDiv(sq, 1e6, 1 << 192) : Math.mulDiv(1 << 192, 1e6, sq);
    }
}
