// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/ArbitrageExecutorV2.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockDirectVenues.sol";

/**
 * @title ArbitrageExecutorV2 on the mock venues
 *
 * Rates (amountIn 100 TKA): buy pool 1 TKA → 2 TKB; sell pool 1 TKB → 0.6 TKA (+20 TKA);
 * losing sell pool 1 TKB → 0.4 TKA (−20 TKA). Every trade borrows from the mock PoolManager.
 */
contract ExecutorV2Test is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;
    uint24 constant CL = 6 << 20;
    uint24 constant PAIR = 7 << 20;
    uint24 constant V4 = 8 << 20;

    ArbitrageExecutorV2 public executor;
    MockPoolManager public pm;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockERC20 public tokenC;
    MockCLPool public clBuy;
    MockCLPool public clSell;
    MockCLPool public clLoss;
    MockPair public pairSell;
    MockCLPool public clBC;
    MockCLPool public clCA;

    address public attacker;
    uint256 public deadline;
    uint256 constant AMT = 100 ether;

    function setUp() public {
        attacker = makeAddr("attacker");
        deadline = block.timestamp + 300;
        vm.etch(WETH, address(new MockWETH()).code);
        pm = new MockPoolManager();
        executor = new ArbitrageExecutorV2(address(pm));
        tokenA = new MockERC20("Token A", "TKA", 10_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 10_000_000 ether);
        tokenC = new MockERC20("Token C", "TKC", 10_000_000 ether);

        clBuy = new MockCLPool(address(tokenA), address(tokenB), 2, 1);
        clSell = new MockCLPool(address(tokenA), address(tokenB), 6, 10);
        clLoss = new MockCLPool(address(tokenA), address(tokenB), 4, 10);
        pairSell = new MockPair(address(tokenA), address(tokenB), 6, 10);
        clBC = new MockCLPool(address(tokenB), address(tokenC), 1, 1);
        clCA = new MockCLPool(address(tokenC), address(tokenA), 6, 10);
        _fund(address(clBuy)); _fund(address(clSell)); _fund(address(clLoss)); _fund(address(pairSell));
        _fund(address(clBC)); _fund(address(clCA));
        pairSell.sync();
        tokenA.transfer(address(pm), 1_000_000 ether);
        tokenB.transfer(address(pm), 1_000_000 ether);
    }

    function _fund(address venue) internal {
        tokenA.transfer(venue, 100_000 ether);
        tokenB.transfer(venue, 100_000 ether);
        tokenC.transfer(venue, 100_000 ether);
    }

    function _two(address a, address b, uint24 fa, uint24 fb, uint256 minProfit) internal {
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, a, b, fa, fb, minProfit, deadline);
    }

    // ── Interface ───────────────────────────────────────────────────

    function test_Interface_VersionOwnerPausedPoolManager() public view {
        assertEq(executor.version(), 2);
        assertEq(executor.owner(), address(this));
        assertFalse(executor.paused());
        assertEq(executor.poolManager(), address(pm));
    }

    function test_Constructor_RejectsZeroPoolManager() public {
        vm.expectRevert(ArbitrageExecutorV2.ZeroAddress.selector);
        new ArbitrageExecutorV2(address(0));
    }

    // ── Profitable trades ───────────────────────────────────────────

    function test_TwoHop_CLThenCL_KeepsProfitAndRepays() public {
        _two(address(clBuy), address(clSell), CL, CL, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether, "profit stays in the executor");
        assertEq(pm.debt(address(tokenA)), 0, "loan repaid");
    }

    function test_TwoHop_CLThenSelfQuotingPair() public {
        _two(address(clBuy), address(pairSell), CL, PAIR, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    function test_TwoHop_CLThenV2FormulaPair() public {
        // Uniswap V2 formula on 100k/100k reserves, 30 bps: 200 TKB in → ~199.0 TKA out.
        MockPair v2 = new MockPair(address(tokenA), address(tokenB), 1, 1);
        _fund(address(v2));
        v2.sync();
        _two(address(clBuy), address(v2), CL, PAIR | 30, 1 ether);
        uint256 inWithFee = 200 ether * 9970;
        uint256 expected = inWithFee * 100_000 ether / (100_000 ether * 10_000 + inWithFee);
        assertEq(tokenA.balanceOf(address(executor)), expected - AMT);
    }

    function test_Triangle_CLCLCL() public {
        // 100 A → 200 B → 200 C → 120 A.
        executor.executeTriangularArbitrageBorrowed(
            address(tokenA), address(tokenB), address(tokenC), AMT,
            address(clBuy), address(clBC), address(clCA), CL, CL, CL, 1 ether, deadline
        );
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    function test_PancakeCallback_Accepted() public {
        clSell.setPancake(true);
        _two(address(clBuy), address(clSell), CL, CL, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    function test_V4Leg_BuyOnV4SellOnCL() public {
        (address c0, address c1) = address(tokenA) < address(tokenB)
            ? (address(tokenA), address(tokenB)) : (address(tokenB), address(tokenA));
        IPoolManagerV4b.PoolKey[] memory keys = new IPoolManagerV4b.PoolKey[](1);
        keys[0] = IPoolManagerV4b.PoolKey(c0, c1, 500, 10, address(0));
        executor.setV4Pools(keys);
        pm.setRate(MockPoolManager.PoolKey(c0, c1, 500, 10, address(0)), 2, 1);
        address pool = executor.v4PoolAddress(keys[0]);
        assertTrue(executor.allowedRouters(pool));
        assertEq(executor.routerType(pool), 8);
        _two(pool, address(clSell), V4, CL, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    // ── Reverts ─────────────────────────────────────────────────────

    function test_Loss_RevertsNotProfitableWithNet() public {
        // 100 A → 200 B → 80 A: net after repaying the 100 loan is 0 (a loss).
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.NotProfitable.selector, 0, 0, 1));
        _two(address(clBuy), address(clLoss), CL, CL, 1);
    }

    function test_BelowMinProfit_Reverts() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.NotProfitable.selector, 0, 20 ether, 21 ether));
        _two(address(clBuy), address(clSell), CL, CL, 21 ether);
    }

    function test_NonOwner_Reverts() public {
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.OwnableUnauthorizedAccount.selector, attacker));
        _two(address(clBuy), address(clSell), CL, CL, 1);
    }

    function test_UnknownLegType_Reverts() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.UnsupportedRouterType.selector, address(clBuy), uint8(1)));
        _two(address(clBuy), address(clSell), 1 << 20, CL, 1);
    }

    function test_ZeroFee_IsNotALegType() public {
        // A v1-style fee (no type bits) must not be traded.
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.UnsupportedRouterType.selector, address(clBuy), uint8(0)));
        _two(address(clBuy), address(clSell), 0, CL, 1);
    }

    function test_UnregisteredV4Pool_Reverts() public {
        address fake = makeAddr("v4");
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.UnsupportedRouterType.selector, fake, uint8(8)));
        _two(fake, address(clSell), V4, CL, 1);
    }

    function test_OverchargingPool_Reverts() public {
        clSell.setOvercharge(1);
        vm.expectRevert(); // LegFailed wrapping UnexpectedCallback
        _two(address(clBuy), address(clSell), CL, CL, 1);
    }

    function test_CallbackFromStranger_Reverts() public {
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.UnexpectedCallback.selector, attacker));
        executor.uniswapV3SwapCallback(1 ether, 0, abi.encode(address(tokenA)));
    }

    function test_UnlockCallbackOutsideTrade_Reverts() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.UnexpectedCallback.selector, address(pm)));
        pm.poke(address(executor), "");
    }

    function test_Deadline_Reverts() public {
        vm.warp(deadline + 1);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.DeadlineExpired.selector, deadline, deadline + 1));
        _two(address(clBuy), address(clSell), CL, CL, 1);
    }

    function test_SameToken_Reverts() public {
        vm.expectRevert(ArbitrageExecutorV2.InvalidPath.selector);
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenA), AMT, address(clBuy), address(clSell), CL, CL, 1, deadline);
    }

    function test_PairFeeOutOfRange_Reverts() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.InvalidPairFee.selector, uint8(2), address(pairSell), uint24(10_000)));
        _two(address(clBuy), address(pairSell), CL, PAIR | 10_000, 1);
    }

    function test_SetV4Pools_RejectsHookedKey() public {
        IPoolManagerV4b.PoolKey[] memory keys = new IPoolManagerV4b.PoolKey[](1);
        keys[0] = IPoolManagerV4b.PoolKey(address(tokenA) < address(tokenB) ? address(tokenA) : address(tokenB),
            address(tokenA) < address(tokenB) ? address(tokenB) : address(tokenA), 500, 10, address(1));
        vm.expectRevert();
        executor.setV4Pools(keys);
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.OwnableUnauthorizedAccount.selector, attacker));
        executor.setV4Pools(new IPoolManagerV4b.PoolKey[](0));
    }

    // ── Funds ───────────────────────────────────────────────────────

    function test_WithdrawToken_GoesToOwnerOnly() public {
        _two(address(clBuy), address(clSell), CL, CL, 1 ether);
        uint256 before = tokenA.balanceOf(address(this));
        executor.withdrawToken(address(tokenA));
        assertEq(tokenA.balanceOf(address(this)) - before, 20 ether);
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutorV2.OwnableUnauthorizedAccount.selector, attacker));
        executor.withdrawToken(address(tokenA));
    }

    // ── Gas vs v1 (same mock trades) ────────────────────────────────

    function test_Gas_V2BelowV1() public {
        ArbitrageExecutor v1 = new ArbitrageExecutor(address(pm));
        v1.setRouter(address(clBuy), ArbitrageExecutor.RouterType.DirectCL);
        v1.setRouter(address(clSell), ArbitrageExecutor.RouterType.DirectCL);
        v1.setRouter(address(clBC), ArbitrageExecutor.RouterType.DirectCL);
        v1.setRouter(address(clCA), ArbitrageExecutor.RouterType.DirectCL);

        uint256 g = gasleft();
        v1.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, address(clBuy), address(clSell), 0, 0, 1, deadline);
        uint256 v1Two = g - gasleft();
        g = gasleft();
        _two(address(clBuy), address(clSell), CL, CL, 1);
        uint256 v2Two = g - gasleft();

        g = gasleft();
        v1.executeTriangularArbitrageBorrowed(address(tokenA), address(tokenB), address(tokenC), AMT,
            address(clBuy), address(clBC), address(clCA), 0, 0, 0, 1, deadline);
        uint256 v1Tri = g - gasleft();
        g = gasleft();
        executor.executeTriangularArbitrageBorrowed(address(tokenA), address(tokenB), address(tokenC), AMT,
            address(clBuy), address(clBC), address(clCA), CL, CL, CL, 1, deadline);
        uint256 v2Tri = g - gasleft();

        emit log_named_uint("v1 2-hop (direct CL, borrowed)", v1Two);
        emit log_named_uint("v2 2-hop", v2Two);
        emit log_named_uint("v1 triangle", v1Tri);
        emit log_named_uint("v2 triangle", v2Tri);
        assertLt(v2Two, v1Two);
        assertLt(v2Tri, v1Tri);
    }
}
