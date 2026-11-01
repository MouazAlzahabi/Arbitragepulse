// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";

/**
 * @title Fork test on real Base pools: DirectCL on Uniswap V3 / PancakeSwap V3 /
 *        Aero CL / SushiSwap V3, DirectPair on Uniswap V2 / Aerodrome V2 / Aero V2, V4 legs
 *        on the native ETH/USDC pool, and a borrow from the real Uniswap V4 PoolManager.
 *
 * Runs only with BASE_FORK_RPC set (an Alchemy *test* key or a public endpoint):
 *   BASE_FORK_RPC=https://... forge test --match-contract ForkVenues -vv
 *
 * Each case first opens a price gap (dumps USDC into one pool for WETH), then trades
 * USDC → WETH on another pool and back on the dumped one.
 */
contract ForkVenuesTest is Test {
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b;

    address constant UNI_V3 = 0xd0b53D9277642d899DF5C87A3966A349A798F224;    // USDC/WETH 0.05 %
    address constant PANCAKE_V3 = 0xB775272E537cc670C65DC852908aD47015244EaF; // USDC/WETH 0.05 %
    address constant AERO_CL = 0x4A7af46bC8aBc00048c6c7920a1488DAF568366c;   // USDC/WETH ts50
    address constant UNI_V2 = 0x88A43bbDF9D098eEC7bCEda4e2494615dfD9bB9C;
    address constant AERODROME_V2 = 0xcDAC0d6c6C59727a65F871236188350531885C43; // volatile
    address constant AERO_V2 = 0x800EB395B577B234C5065c8E0cCb0F3A1401B2c2;      // volatile
    address constant SUSHI_V3_FACTORY = 0xc35DADB65012eC5796536bD9864eD8773aBc74C4;

    ArbitrageExecutor executor;
    bool forked;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_FORK_RPC", string(""));
        if (bytes(rpc).length == 0) return;
        vm.createSelectFork(rpc);
        forked = true;
        executor = new ArbitrageExecutor(POOL_MANAGER);
        address[] memory cl = new address[](3);
        (cl[0], cl[1], cl[2]) = (UNI_V3, PANCAKE_V3, AERO_CL);
        executor.setRouters(cl, ArbitrageExecutor.RouterType.DirectCL);
        address[] memory pairs = new address[](3);
        (pairs[0], pairs[1], pairs[2]) = (UNI_V2, AERODROME_V2, AERO_V2);
        executor.setRouters(pairs, ArbitrageExecutor.RouterType.DirectPair);
        IPoolManagerV4.PoolKey[] memory keys = new IPoolManagerV4.PoolKey[](1);
        keys[0] = _ethUsdcV4();
        executor.setV4Pools(keys);
    }

    /// Hookless native ETH/USDC 0.05 % V4 pool (L ≈ 4.5e16 on 2026-11-01).
    function _ethUsdcV4() internal pure returns (IPoolManagerV4.PoolKey memory) {
        return IPoolManagerV4.PoolKey({ currency0: address(0), currency1: USDC, fee: 500, tickSpacing: 10, hooks: address(0) });
    }

    /// Dump `weth` into a V2-style pair for USDC: WETH there becomes cheap.
    function _dumpWethIntoPair(address pair, uint256 weth) internal {
        deal(WETH, address(this), weth);
        bool wethIs0 = IPair(pair).token0() == WETH;
        uint256 out = pair == UNI_V2 ? _uniV2Out(pair, weth, wethIs0) : IPair(pair).getAmountOut(weth, WETH);
        IERC20(WETH).transfer(pair, weth);
        IPair(pair).swap(wethIs0 ? 0 : out, wethIs0 ? out : 0, address(this), "");
    }

    /// Borrowed USDC → WETH → USDC with a V4 leg: the PoolManager's own accounting must
    /// close (unlock reverts otherwise); returns the USDC profit.
    function _borrowedArbV4(address buy, address sell, uint24 buyFee, uint24 sellFee, uint256 amountIn)
        internal returns (uint256 profit)
    {
        uint256 before = IERC20(USDC).balanceOf(address(executor));
        executor.executeArbitrageBorrowed(USDC, WETH, amountIn, buy, sell, buyFee, sellFee, 1, block.timestamp + 60);
        profit = IERC20(USDC).balanceOf(address(executor)) - before;
        assertGt(profit, 0);
        assertEq(IERC20(WETH).balanceOf(address(executor)), 0, "no WETH left");
        assertEq(address(executor).balance, 0, "no ETH left");
    }

    /// Dump `usdc` into a V2-style pair for WETH: WETH there becomes expensive.
    function _dumpIntoPair(address pair, uint256 usdc) internal {
        deal(USDC, address(this), usdc);
        bool usdcIs0 = IPair(pair).token0() == USDC;
        uint256 out = pair == UNI_V2 ? _uniV2Out(pair, usdc, usdcIs0) : IPair(pair).getAmountOut(usdc, USDC);
        IERC20(USDC).transfer(pair, usdc);
        IPair(pair).swap(usdcIs0 ? 0 : out, usdcIs0 ? out : 0, address(this), "");
    }

    function _uniV2Out(address pair, uint256 amountIn, bool inIs0) internal view returns (uint256) {
        (uint256 r0, uint256 r1) = IPair(pair).getReserves();
        (uint256 rIn, uint256 rOut) = inIs0 ? (r0, r1) : (r1, r0);
        return amountIn * 997 * rOut / (rIn * 1000 + amountIn * 997);
    }

    function _borrowedArb(address buy, address sell, uint24 sellFee, uint256 amountIn) internal returns (uint256 profit) {
        uint256 pmBefore = IERC20(USDC).balanceOf(POOL_MANAGER);
        uint256 before = IERC20(USDC).balanceOf(address(executor));
        executor.executeArbitrageBorrowed(USDC, WETH, amountIn, buy, sell, 0, sellFee, 1, block.timestamp + 60);
        assertEq(IERC20(USDC).balanceOf(POOL_MANAGER), pmBefore, "V4 loan repaid exactly");
        profit = IERC20(USDC).balanceOf(address(executor)) - before;
        assertGt(profit, 0);
    }

    function test_Fork_Borrowed_UniV3_to_AerodromeV2() public {
        if (!forked) return;
        _dumpIntoPair(AERODROME_V2, 300_000e6);
        uint256 p = _borrowedArb(UNI_V3, AERODROME_V2, 0, 5_000e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }

    function test_Fork_Borrowed_AeroCL_to_AeroV2() public {
        if (!forked) return;
        _dumpIntoPair(AERO_V2, 300_000e6);
        uint256 p = _borrowedArb(AERO_CL, AERO_V2, 0, 5_000e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }

    function test_Fork_Borrowed_Pancake_to_UniV2() public {
        if (!forked) return;
        _dumpIntoPair(UNI_V2, 200_000e6);
        uint256 p = _borrowedArb(PANCAKE_V3, UNI_V2, 30, 3_000e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }

    /// No gap: a round trip loses the fees and must revert with nothing moved.
    function test_Fork_Borrowed_NoGap_Reverts() public {
        if (!forked) return;
        vm.expectRevert();
        executor.executeArbitrageBorrowed(USDC, WETH, 5_000e6, UNI_V3, AERO_CL, 0, 0, 1, block.timestamp + 60);
    }

    /// V4 leg with native ETH output (wrapped to WETH), sold on a dumped Aerodrome V2 pair.
    function test_Fork_Borrowed_V4Native_to_AerodromeV2() public {
        if (!forked) return;
        _dumpIntoPair(AERODROME_V2, 300_000e6);
        address v4 = executor.v4PoolAddress(_ethUsdcV4());
        uint256 p = _borrowedArbV4(v4, AERODROME_V2, 0, 0, 5_000e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }

    /// V4 leg with native ETH input (WETH unwrapped): buy cheap WETH on a dumped Uniswap V2
    /// pair, sell it on V4.
    function test_Fork_Borrowed_UniV2_to_V4Native() public {
        if (!forked) return;
        _dumpWethIntoPair(UNI_V2, 100 ether);
        address v4 = executor.v4PoolAddress(_ethUsdcV4());
        uint256 p = _borrowedArbV4(UNI_V2, v4, 30, 0, 3_000e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }

    /// V4 legs outside a borrow can't run (the PoolManager is locked).
    function test_Fork_Funded_V4_Reverts() public {
        if (!forked) return;
        deal(USDC, address(executor), 5_000e6);
        address v4 = executor.v4PoolAddress(_ethUsdcV4());
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.V4NeedsBorrow.selector, 1, v4));
        executor.executeArbitrage(USDC, WETH, 5_000e6, v4, AERODROME_V2, 0, 0, 1, block.timestamp + 60);
    }

    /// SushiSwap V3 pools pay back through uniswapV3SwapCallback (DirectCL).
    function test_Fork_Borrowed_SushiV3_to_AerodromeV2() public {
        if (!forked) return;
        (bool ok, bytes memory ret) = SUSHI_V3_FACTORY.staticcall(
            abi.encodeWithSignature("getPool(address,address,uint24)", USDC, WETH, uint24(500)));
        address sushi = ok ? abi.decode(ret, (address)) : address(0);
        assertTrue(sushi != address(0), "Sushi USDC/WETH 0.05% pool");
        address[] memory cl = new address[](1);
        cl[0] = sushi;
        executor.setRouters(cl, ArbitrageExecutor.RouterType.DirectCL);
        _dumpIntoPair(AERODROME_V2, 300_000e6);
        uint256 p = _borrowedArb(sushi, AERODROME_V2, 0, 500e6);
        emit log_named_decimal_uint("profit USDC", p, 6);
    }
}
