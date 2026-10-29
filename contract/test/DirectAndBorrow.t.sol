// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockDirectVenues.sol";

/**
 * @title DirectCL / DirectPair router types and V4 borrowing
 *
 * Rates (amountIn 100 TKA): buy pool 1 TKA → 2 TKB; sell pool 1 TKB → 0.6 TKA (+20 TKA);
 * losing sell pool 1 TKB → 0.4 TKA (−20 TKA).
 */
contract DirectAndBorrowTest is Test {
    ArbitrageExecutor public executor;
    MockPoolManager public pm;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockERC20 public tokenC;
    MockCLPool public clBuy;
    MockCLPool public clSell;
    MockCLPool public clLoss;
    MockPair public pairSell;

    address public attacker;
    uint256 public deadline;
    uint256 constant AMT = 100 ether;

    event Borrowed(address indexed token, uint256 amount);

    function setUp() public {
        attacker = makeAddr("attacker");
        deadline = block.timestamp + 300;
        pm = new MockPoolManager();
        executor = new ArbitrageExecutor(address(pm));
        tokenA = new MockERC20("Token A", "TKA", 10_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 10_000_000 ether);
        tokenC = new MockERC20("Token C", "TKC", 10_000_000 ether);

        clBuy = new MockCLPool(address(tokenA), address(tokenB), 2, 1);
        clSell = new MockCLPool(address(tokenA), address(tokenB), 6, 10);
        clLoss = new MockCLPool(address(tokenA), address(tokenB), 4, 10);
        pairSell = new MockPair(address(tokenA), address(tokenB), 6, 10);
        _fund(address(clBuy)); _fund(address(clSell)); _fund(address(clLoss)); _fund(address(pairSell));
        pairSell.sync();
        tokenA.transfer(address(pm), 1_000_000 ether);

        executor.setRouter(address(clBuy), ArbitrageExecutor.RouterType.DirectCL);
        executor.setRouter(address(clSell), ArbitrageExecutor.RouterType.DirectCL);
        executor.setRouter(address(clLoss), ArbitrageExecutor.RouterType.DirectCL);
        executor.setRouter(address(pairSell), ArbitrageExecutor.RouterType.DirectPair);
    }

    function _fund(address venue) internal {
        tokenA.transfer(venue, 100_000 ether);
        tokenB.transfer(venue, 100_000 ether);
        tokenC.transfer(venue, 100_000 ether);
    }

    function _arb(address rA, address rB, uint24 fB) internal {
        executor.executeArbitrage(address(tokenA), address(tokenB), AMT, rA, rB, 0, fB, 1 ether, deadline);
    }

    function _arbBorrowed(address rA, address rB, uint24 fB) internal {
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, rA, rB, 0, fB, 1 ether, deadline);
    }

    // ── DirectCL / DirectPair, funded ──────────────────────────────

    function test_DirectCL_Funded_Profitable() public {
        tokenA.transfer(address(executor), 1_000 ether);
        _arb(address(clBuy), address(clSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether);
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    function test_DirectCL_PancakeCallback() public {
        clBuy.setPancake(true);
        tokenA.transfer(address(executor), 1_000 ether);
        _arb(address(clBuy), address(clSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether);
    }

    function test_DirectPair_PoolQuote_Profitable() public {
        tokenA.transfer(address(executor), 1_000 ether);
        _arb(address(clBuy), address(pairSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether);
    }

    function test_DirectPair_UniV2Formula() public {
        // A fair pair: 100k TKA / 100k TKB; sell 200 TKB at 0.3 %.
        MockPair fair = new MockPair(address(tokenA), address(tokenB), 0, 1);
        tokenA.transfer(address(fair), 100_000 ether);
        tokenB.transfer(address(fair), 100_000 ether);
        fair.sync();
        executor.setRouter(address(fair), ArbitrageExecutor.RouterType.DirectPair);
        tokenA.transfer(address(executor), 1_000 ether);
        uint256 inWithFee = 200 ether * 9_970;
        uint256 expected = inWithFee * 100_000 ether / (100_000 ether * 10_000 + inWithFee);
        executor.executeArbitrage(address(tokenA), address(tokenB), AMT, address(clBuy), address(fair), 0, 30, 0, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 900 ether + expected);
    }

    function test_DirectPair_FeeTooHigh_Reverts() public {
        tokenA.transfer(address(executor), 1_000 ether);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.InvalidPairFee.selector, 2, address(pairSell), 10_000));
        _arb(address(clBuy), address(pairSell), 10_000);
    }

    // ── Borrowing ──────────────────────────────────────────────────

    function test_Borrowed_NeedsNoBalance_RepaysExactly() public {
        uint256 pmBefore = tokenA.balanceOf(address(pm));
        assertEq(tokenA.balanceOf(address(executor)), 0);
        vm.expectEmit(true, false, false, true, address(executor));
        emit Borrowed(address(tokenA), AMT);
        _arbBorrowed(address(clBuy), address(clSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether, "profit stays");
        assertEq(tokenA.balanceOf(address(pm)), pmBefore, "loan repaid exactly");
        assertEq(pm.debt(address(tokenA)), 0);
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    function test_Borrowed_KeepsExistingBalance() public {
        tokenA.transfer(address(executor), 50 ether);
        _arbBorrowed(address(clBuy), address(pairSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 70 ether);
    }

    function test_Borrowed_Loss_Reverts_NothingMoves() public {
        uint256 pmBefore = tokenA.balanceOf(address(pm));
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NotProfitable.selector, 0, 0, 1 ether));
        _arbBorrowed(address(clBuy), address(clLoss), 0);
        assertEq(tokenA.balanceOf(address(pm)), pmBefore);
        assertEq(tokenA.balanceOf(address(executor)), 0);
    }

    function test_Borrowed_Triangular_DirectLegs() public {
        MockCLPool bc = new MockCLPool(address(tokenB), address(tokenC), 1, 1);
        MockCLPool ca = new MockCLPool(address(tokenC), address(tokenA), 6, 10);
        _fund(address(bc)); _fund(address(ca));
        executor.setRouter(address(bc), ArbitrageExecutor.RouterType.DirectCL);
        executor.setRouter(address(ca), ArbitrageExecutor.RouterType.DirectCL);
        executor.executeTriangularArbitrageBorrowed(address(tokenA), address(tokenB), address(tokenC), AMT,
            address(clBuy), address(bc), address(ca), 0, 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    function test_Borrowed_Unavailable_WithoutPoolManager() public {
        ArbitrageExecutor plain = new ArbitrageExecutor(address(0));
        plain.setRouter(address(clBuy), ArbitrageExecutor.RouterType.DirectCL);
        plain.setRouter(address(clSell), ArbitrageExecutor.RouterType.DirectCL);
        vm.expectRevert(ArbitrageExecutor.BorrowUnavailable.selector);
        plain.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, address(clBuy), address(clSell), 0, 0, 0, deadline);
    }

    function test_Borrowed_OnlyOwner() public {
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, attacker));
        _arbBorrowed(address(clBuy), address(clSell), 0);
    }

    // ── Callback abuse ─────────────────────────────────────────────

    function test_SwapCallback_FromStranger_Reverts() public {
        tokenA.transfer(address(executor), 1_000 ether);
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnexpectedCallback.selector, attacker));
        executor.uniswapV3SwapCallback(int256(1_000 ether), 0, abi.encode(address(tokenA)));
        vm.prank(address(clBuy)); // an allowlisted pool, but not mid-swap
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnexpectedCallback.selector, address(clBuy)));
        executor.pancakeV3SwapCallback(int256(1_000 ether), 0, abi.encode(address(tokenA)));
    }

    function test_SwapCallback_Overcharge_Reverts() public {
        tokenA.transfer(address(executor), 1_000 ether);
        clBuy.setOvercharge(1);
        vm.expectRevert(); // LegFailed(1, clBuy, DirectCL, UnexpectedCallback(clBuy))
        _arb(address(clBuy), address(clSell), 0);
        assertEq(tokenA.balanceOf(address(executor)), 1_000 ether);
    }

    function test_UnlockCallback_NotPoolManager_Reverts() public {
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnexpectedCallback.selector, attacker));
        executor.unlockCallback("");
    }

    function test_UnlockCallback_PoolManagerWhenIdle_Reverts() public {
        // A PoolManager unlock opened by someone else must never run (or fund) a trade.
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnexpectedCallback.selector, address(pm)));
        pm.poke(address(executor), "");
    }

    function test_DirectPool_NotAllowlisted_Reverts() public {
        MockCLPool rogue = new MockCLPool(address(tokenA), address(tokenB), 2, 1);
        tokenA.transfer(address(executor), 1_000 ether);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, address(rogue)));
        _arb(address(rogue), address(clSell), 0);
    }

    // ── setRouters ─────────────────────────────────────────────────

    function test_SetRouters_Batch() public {
        address[] memory pools = new address[](2);
        pools[0] = address(new MockCLPool(address(tokenA), address(tokenB), 1, 1));
        pools[1] = address(new MockCLPool(address(tokenA), address(tokenC), 1, 1));
        executor.setRouters(pools, ArbitrageExecutor.RouterType.DirectCL);
        assertTrue(executor.allowedRouters(pools[0]) && executor.allowedRouters(pools[1]));
        assertEq(uint8(executor.routerType(pools[1])), uint8(ArbitrageExecutor.RouterType.DirectCL));
        pools[1] = attacker; // EOA
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NotAContract.selector, attacker));
        executor.setRouters(pools, ArbitrageExecutor.RouterType.DirectCL);
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, attacker));
        executor.setRouters(pools, ArbitrageExecutor.RouterType.DirectCL);
    }
}
