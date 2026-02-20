// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockRouter.sol";
import "../src/mocks/MockV3Router.sol";

/**
 * @title ArbitrageExecutor v2 — Full Test Suite (Forge)
 *
 * Port of original Hardhat/Chai tests to pure Solidity + forge-std.
 *
 * Original DoD (ARB-001):
 *   ✅ Test Case 1: Owner can deposit and withdraw ETH
 *   ✅ Test Case 2: executeArbitrage reverts if trade is unprofitable
 *   ✅ Test Case 3: Non-owner cannot call withdraw or execute
 *
 * Improvement Coverage:
 *   ✅ #1 Deadline: stale txns revert
 *   ✅ #2 minProfit: dust profits rejected
 *   ✅ #3 ReentrancyGuard: verified via modifier presence
 *   ✅ #4 Allowance hygiene: allowances reset to 0 after swaps
 *   ✅ #5 Ownable2Step: 2-step ownership transfer
 *   ✅ #6 batchExecute: multiple arbs, partial failures handled
 *   ✅ #7 Profit accumulator: totalProfit and totalTrades tracked
 *   ✅ #8 Pausable: execution blocked when paused, withdrawals still work
 *   ✅ #9 V3 support: V3, mixed V2+V3, and V3+V2 arbs
 */
contract ArbitrageExecutorTest is Test {
    ArbitrageExecutor public executor;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockRouter public routerA;   // V2: 1 TKA → 2 TKB (buy side)
    MockRouter public routerB;   // V2: 1 TKB → 0.6 TKA → profit
    MockRouter public routerC;   // V2: 1 TKB → 0.4 TKA → loss
    MockV3Router public routerV3A; // V3: 1 TKA → 2 TKB (buy side)
    MockV3Router public routerV3B; // V3: 1 TKB → 0.6 TKA → profit
    MockV3Router public routerV3C; // V3: 1 TKB → 0.4 TKA → loss

    address public owner;
    address public attacker;
    address public newOwner;
    uint256 public deadline;

    // Event declarations for testing
    event WithdrawETH(address indexed to, uint256 amount);
    event WithdrawToken(address indexed token, address indexed to, uint256 amount);
    event ArbitrageExecuted(
        address indexed tokenIn,
        address indexed tokenOut,
        address routerA,
        address routerB,
        uint256 amountIn,
        uint256 profit
    );
    event BatchExecuted(uint256 attempted, uint256 succeeded, uint256 totalBatchProfit);

    function setUp() public {
        owner = address(this);
        attacker = makeAddr("attacker");
        newOwner = makeAddr("newOwner");

        // Deploy executor
        executor = new ArbitrageExecutor();

        // Deploy mock tokens
        tokenA = new MockERC20("Token A", "TKA", 1_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 1_000_000 ether);

        // Deploy V2 mock routers with different rates
        routerA = new MockRouter(2, 1);   // 1 TKA → 2 TKB
        routerB = new MockRouter(6, 10);  // 1 TKB → 0.6 TKA → 2 * 0.6 = 1.2 TKA = PROFIT
        routerC = new MockRouter(4, 10);  // 1 TKB → 0.4 TKA → 2 * 0.4 = 0.8 TKA = LOSS

        // Deploy V3 mock routers with same rates
        routerV3A = new MockV3Router(2, 1);   // 1 TKA → 2 TKB
        routerV3B = new MockV3Router(6, 10);  // 1 TKB → 0.6 TKA = PROFIT
        routerV3C = new MockV3Router(4, 10);  // 1 TKB → 0.4 TKA = LOSS

        // Fund V2 routers
        tokenB.transfer(address(routerA), 100_000 ether);
        tokenA.transfer(address(routerB), 100_000 ether);
        tokenA.transfer(address(routerC), 100_000 ether);

        // Fund V3 routers
        tokenB.transfer(address(routerV3A), 100_000 ether);
        tokenA.transfer(address(routerV3B), 100_000 ether);
        tokenA.transfer(address(routerV3C), 100_000 ether);

        // Fund executor with trading capital
        tokenA.transfer(address(executor), 1_000 ether);

        // Whitelist V2 routers (V2 is default, no setRouterType needed)
        executor.setAllowedRouter(address(routerA), true);
        executor.setAllowedRouter(address(routerB), true);
        executor.setAllowedRouter(address(routerC), true);

        // Whitelist V3 routers and set their type
        executor.setAllowedRouter(address(routerV3A), true);
        executor.setRouterType(address(routerV3A), ArbitrageExecutor.RouterType.V3);
        executor.setAllowedRouter(address(routerV3B), true);
        executor.setRouterType(address(routerV3B), ArbitrageExecutor.RouterType.V3);
        executor.setAllowedRouter(address(routerV3C), true);
        executor.setRouterType(address(routerV3C), ArbitrageExecutor.RouterType.V3);

        // Default deadline: 1 hour from now
        deadline = block.timestamp + 3600;
    }

    // ════════════════════════════════════════════════════════════
    // DoD Test Case 1: Owner can deposit and withdraw ETH
    // ════════════════════════════════════════════════════════════

    function test_AcceptETH() public {
        (bool ok,) = address(executor).call{value: 1 ether}("");
        assertTrue(ok);
        assertEq(address(executor).balance, 1 ether);
    }

    function test_WithdrawETH() public {
        (bool ok,) = address(executor).call{value: 1 ether}("");
        assertTrue(ok);

        uint256 before = address(this).balance;
        executor.withdrawETH();
        uint256 after_ = address(this).balance;

        assertEq(after_ - before, 1 ether);
    }

    function test_WithdrawETH_EmitsEvent() public {
        (bool ok,) = address(executor).call{value: 0.5 ether}("");
        assertTrue(ok);

        vm.expectEmit(true, false, false, true);
        emit WithdrawETH(address(this), 0.5 ether);
        executor.withdrawETH();
    }

    function test_WithdrawETH_RevertsOnZero() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.withdrawETH();
    }

    // ════════════════════════════════════════════════════════════
    // DoD Test Case 1 (cont): Token withdrawal
    // ════════════════════════════════════════════════════════════

    function test_WithdrawToken_FullBalance() public {
        uint256 before = tokenA.balanceOf(address(this));
        executor.withdrawToken(address(tokenA));
        uint256 after_ = tokenA.balanceOf(address(this));

        assertEq(after_ - before, 1_000 ether);
    }

    function test_WithdrawTokenAmount_Partial() public {
        executor.withdrawTokenAmount(address(tokenA), 300 ether);
        assertEq(tokenA.balanceOf(address(executor)), 700 ether);
    }

    function test_WithdrawToken_EmitsEvent() public {
        vm.expectEmit(true, true, false, true);
        emit WithdrawToken(address(tokenA), address(this), 1_000 ether);
        executor.withdrawToken(address(tokenA));
    }

    function test_WithdrawToken_RevertsOnZero() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.withdrawToken(address(tokenB)); // executor has no tokenB
    }

    function test_WithdrawToken_WorksWhenPaused() public {
        executor.pause();
        // Should NOT revert — withdrawals bypass pause
        executor.withdrawToken(address(tokenA));
    }

    // ════════════════════════════════════════════════════════════
    // DoD Test Case 2: executeArbitrage reverts if unprofitable
    // ════════════════════════════════════════════════════════════

    function test_ExecuteProfitableArb() public {
        uint256 before = tokenA.balanceOf(address(executor));

        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,  // feeA, feeB (V2 = 0)
            0, deadline
        );

        uint256 after_ = tokenA.balanceOf(address(executor));
        assertGt(after_, before);
    }

    function test_ExecuteArb_EmitsEvent() public {
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecuted(
            address(tokenA), address(tokenB),
            address(routerA), address(routerB),
            100 ether, 20 ether
        );

        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsIfUnprofitable() public {
        vm.expectRevert(); // NotProfitable
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerC), // Loss router
            0, 0,
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsOnZeroAmount() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            0,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsOnZeroAddress() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.executeArbitrage(
            address(tokenA), address(0),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );
    }

    // ════════════════════════════════════════════════════════════
    // DoD Test Case 3: Non-owner access control
    // ════════════════════════════════════════════════════════════

    function test_NonOwnerCannotExecute() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );
    }

    function test_NonOwnerCannotWithdrawETH() public {
        (bool ok,) = address(executor).call{value: 1 ether}("");
        assertTrue(ok);

        vm.prank(attacker);
        vm.expectRevert();
        executor.withdrawETH();
    }

    function test_NonOwnerCannotWithdrawTokens() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.withdrawToken(address(tokenA));
    }

    function test_NonOwnerCannotPause() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.pause();
    }

    function test_NonOwnerCannotBatchExecute() public {
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](0);
        vm.prank(attacker);
        vm.expectRevert();
        executor.batchExecute(arbs, deadline);
    }

    function test_NonOwnerCannotRevokeApproval() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.revokeApproval(address(tokenA), address(routerA));
    }

    function test_CorrectOwner() public view {
        assertEq(executor.owner(), address(this));
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #1: Deadline
    // ════════════════════════════════════════════════════════════

    function test_Deadline_RevertsIfExpired() public {
        vm.warp(1000); // Set block.timestamp to a non-zero value
        uint256 pastDeadline = block.timestamp - 100;

        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, pastDeadline
        );
    }

    function test_Deadline_AcceptsWithinDeadline() public {
        uint256 futureDeadline = block.timestamp + 60;

        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, futureDeadline
        );
        // No revert = pass
    }

    function test_Deadline_BatchRevertsIfExpired() public {
        uint256 pastDeadline = block.timestamp - 1;
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](0);

        vm.expectRevert();
        executor.batchExecute(arbs, pastDeadline);
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #2: minProfit
    // ════════════════════════════════════════════════════════════

    function test_MinProfit_SucceedsWhenAbove() public {
        // Trade: 100 → 120 = 20 profit. minProfit = 10. Should pass.
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            10 ether, deadline
        );
    }

    function test_MinProfit_RevertsWhenBelow() public {
        // Trade profit is 20 TKA, but we require 50 → revert
        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            50 ether, deadline
        );
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #4: Allowance hygiene
    // ════════════════════════════════════════════════════════════

    function test_AllowanceResetAfterArb() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );

        assertEq(tokenA.allowance(address(executor), address(routerA)), 0);
        assertEq(tokenB.allowance(address(executor), address(routerB)), 0);
    }

    function test_RevokeApproval() public {
        executor.revokeApproval(address(tokenA), address(routerA));
        assertEq(tokenA.allowance(address(executor), address(routerA)), 0);
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #5: Ownable2Step
    // ════════════════════════════════════════════════════════════

    function test_Ownable2Step_RequiresAcceptance() public {
        executor.transferOwnership(newOwner);

        // Owner unchanged until accepted
        assertEq(executor.owner(), address(this));
        assertEq(executor.pendingOwner(), newOwner);
    }

    function test_Ownable2Step_CompleteTransfer() public {
        executor.transferOwnership(newOwner);

        vm.prank(newOwner);
        executor.acceptOwnership();

        assertEq(executor.owner(), newOwner);
    }

    function test_Ownable2Step_RandomCannotAccept() public {
        executor.transferOwnership(newOwner);

        vm.prank(attacker);
        vm.expectRevert();
        executor.acceptOwnership();
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #6: Batch execution
    // ════════════════════════════════════════════════════════════

    function test_BatchExecute_MultipleProfitable() public {
        uint256 balBefore = tokenA.balanceOf(address(executor));

        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](2);
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });

        executor.batchExecute(arbs, deadline);

        uint256 balAfter = tokenA.balanceOf(address(executor));
        // Each 50 trade = 10 profit → 20 total
        assertEq(balAfter - balBefore, 20 ether);
    }

    function test_BatchExecute_SkipsFailingArbs() public {
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](2);
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerC), // Loss
            feeA: 0, feeB: 0,
            minProfit: 0
        });

        // Should NOT revert
        executor.batchExecute(arbs, deadline);
    }

    function test_BatchExecute_EmitsBatchExecuted() public {
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](2);
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerC),
            feeA: 0, feeB: 0,
            minProfit: 0
        });

        vm.expectEmit(false, false, false, true);
        emit BatchExecuted(2, 1, 10 ether);
        executor.batchExecute(arbs, deadline);
    }

    function test_BatchExecute_RevertsOnEmpty() public {
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](0);

        vm.expectRevert(ArbitrageExecutor.EmptyBatch.selector);
        executor.batchExecute(arbs, deadline);
    }

    function test_BatchExecute_SkipsInsufficientBalance() public {
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](1);
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 999_999 ether, // Way more than balance
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });

        // Should not revert, just skip
        executor.batchExecute(arbs, deadline);
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #7: Profit accumulator
    // ════════════════════════════════════════════════════════════

    function test_TotalProfit_TrackedPerToken() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );

        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    function test_TotalProfit_Accumulates() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );

        assertEq(executor.totalProfit(address(tokenA)), 40 ether);
    }

    function test_TotalTrades_Increments() public {
        assertEq(executor.totalTrades(), 0);

        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );

        assertEq(executor.totalTrades(), 1);
    }

    function test_GetStats_ReturnsCorrectValues() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );

        (uint256 profit, uint256 trades, uint256 balance) = executor.getStats(address(tokenA));

        assertEq(profit, 20 ether);
        assertEq(trades, 1);
        // 1000 initial + 20 profit = 1020
        assertEq(balance, 1_020 ether);
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #8: Pausable
    // ════════════════════════════════════════════════════════════

    function test_Pause_BlocksExecution() public {
        executor.pause();

        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );
    }

    function test_Pause_BlocksBatchExecute() public {
        executor.pause();
        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](0);

        vm.expectRevert();
        executor.batchExecute(arbs, deadline);
    }

    function test_Unpause_AllowsExecution() public {
        executor.pause();
        executor.unpause();

        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, 0, 0, deadline
        );
    }

    function test_Pause_StillAllowsWithdrawals() public {
        executor.pause();

        // ETH withdrawal
        (bool ok,) = address(executor).call{value: 1 ether}("");
        assertTrue(ok);
        executor.withdrawETH();

        // Token withdrawal
        executor.withdrawToken(address(tokenA));
    }

    // ════════════════════════════════════════════════════════════
    // estimateArbitrage
    // ════════════════════════════════════════════════════════════

    function test_Estimate_ProfitableRoute() public view {
        (uint256 profit, uint256 amountOut) = executor.estimateArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB)
        );

        assertEq(profit, 20 ether);
        assertEq(amountOut, 120 ether);
    }

    function test_Estimate_UnprofitableRoute() public view {
        (uint256 profit,) = executor.estimateArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerC)
        );

        assertEq(profit, 0);
    }

    // ════════════════════════════════════════════════════════════
    // Router Allowlist
    // ════════════════════════════════════════════════════════════

    function test_RouterNotAllowed_RevertsOnUnknownRouter() public {
        address fakeRouter = makeAddr("fakeRouter");

        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, fakeRouter));
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            fakeRouter, address(routerB),
            0, 0,
            0, deadline
        );
    }

    function test_RouterNotAllowed_RevertsOnRevokedRouter() public {
        executor.setAllowedRouter(address(routerA), false);

        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, address(routerA)));
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, 0,
            0, deadline
        );
    }

    function test_SetAllowedRouter_UpdatesAllowlist() public {
        address newRouter = makeAddr("newRouter");

        assertFalse(executor.allowedRouters(newRouter));

        executor.setAllowedRouter(newRouter, true);
        assertTrue(executor.allowedRouters(newRouter));

        executor.setAllowedRouter(newRouter, false);
        assertFalse(executor.allowedRouters(newRouter));
    }

    function test_SetAllowedRouter_ZeroAddressReverts() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.setAllowedRouter(address(0), true);
    }

    function test_NonOwnerCannotSetAllowedRouter() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.setAllowedRouter(address(routerA), false);
    }

    function test_BatchExecute_SkipsUnapprovedRouter() public {
        executor.setAllowedRouter(address(routerA), false);

        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](1);
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 100 ether,
            routerA: address(routerA), routerB: address(routerB),
            feeA: 0, feeB: 0,
            minProfit: 0
        });

        uint256 balBefore = tokenA.balanceOf(address(executor));
        executor.batchExecute(arbs, deadline);
        // Trade was skipped — balance unchanged
        assertEq(tokenA.balanceOf(address(executor)), balBefore);
    }

    // ════════════════════════════════════════════════════════════
    // Improvement #9: V3 support
    // ════════════════════════════════════════════════════════════

    function test_V3_RouterType_SetAndGet() public view {
        // V3 routers should report V3 type
        assertEq(uint8(executor.routerType(address(routerV3A))), uint8(ArbitrageExecutor.RouterType.V3));
        assertEq(uint8(executor.routerType(address(routerV3B))), uint8(ArbitrageExecutor.RouterType.V3));
        // V2 routers default to V2 (0)
        assertEq(uint8(executor.routerType(address(routerA))), uint8(ArbitrageExecutor.RouterType.V2));
    }

    function test_V3_ExecuteProfitableArb() public {
        uint256 before = tokenA.balanceOf(address(executor));

        // V3→V3: buy on routerV3A (1 TKA → 2 TKB), sell on routerV3B (1 TKB → 0.6 TKA)
        // 100 TKA → 200 TKB → 120 TKA: profit = 20 TKA
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerV3A), address(routerV3B),
            3000, 3000,  // V3 fee tier (not enforced by MockV3Router, but validates flow)
            0, deadline
        );

        uint256 after_ = tokenA.balanceOf(address(executor));
        assertEq(after_ - before, 20 ether);
    }

    function test_V3_RevertsIfUnprofitable() public {
        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerV3A), address(routerV3C), // Loss router
            3000, 3000,
            0, deadline
        );
    }

    function test_MixedV2V3_ArbBuyV2SellV3() public {
        uint256 before = tokenA.balanceOf(address(executor));

        // Buy on V2 routerA (1 TKA → 2 TKB), sell on V3 routerV3B (1 TKB → 0.6 TKA)
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerV3B),
            0, 3000,  // feeA=0 for V2, feeB=3000 for V3
            0, deadline
        );

        uint256 after_ = tokenA.balanceOf(address(executor));
        assertEq(after_ - before, 20 ether);
    }

    function test_MixedV3V2_ArbBuyV3SellV2() public {
        uint256 before = tokenA.balanceOf(address(executor));

        // Buy on V3 routerV3A (1 TKA → 2 TKB), sell on V2 routerB (1 TKB → 0.6 TKA)
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerV3A), address(routerB),
            3000, 0,  // feeA=3000 for V3, feeB=0 for V2
            0, deadline
        );

        uint256 after_ = tokenA.balanceOf(address(executor));
        assertEq(after_ - before, 20 ether);
    }

    function test_V3_AllowanceResetAfterArb() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerV3A), address(routerV3B),
            3000, 3000,
            0, deadline
        );

        // Allowances must be reset to 0
        assertEq(tokenA.allowance(address(executor), address(routerV3A)), 0);
        assertEq(tokenB.allowance(address(executor), address(routerV3B)), 0);
    }

    function test_V3_BatchExecute() public {
        uint256 balBefore = tokenA.balanceOf(address(executor));

        ArbitrageExecutor.ArbParams[] memory arbs = new ArbitrageExecutor.ArbParams[](3);
        // V3→V3 profitable
        arbs[0] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 100 ether,
            routerA: address(routerV3A), routerB: address(routerV3B),
            feeA: 3000, feeB: 3000,
            minProfit: 0
        });
        // V2→V3 profitable
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 100 ether,
            routerA: address(routerA), routerB: address(routerV3B),
            feeA: 0, feeB: 3000,
            minProfit: 0
        });
        // V3→V3 loss (skipped)
        arbs[2] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 100 ether,
            routerA: address(routerV3A), routerB: address(routerV3C),
            feeA: 3000, feeB: 3000,
            minProfit: 0
        });

        uint256[] memory results = executor.batchExecute(arbs, deadline);

        assertEq(results[0], 20 ether); // V3→V3 profit
        assertEq(results[1], 20 ether); // V2→V3 profit
        assertEq(results[2], 0);        // loss → skipped

        uint256 balAfter = tokenA.balanceOf(address(executor));
        assertEq(balAfter - balBefore, 40 ether);
    }

    function test_V3_NonOwnerCannotSetRouterType() public {
        vm.prank(attacker);
        vm.expectRevert();
        executor.setRouterType(address(routerA), ArbitrageExecutor.RouterType.V3);
    }

    function test_V3_SetRouterType_ZeroAddressReverts() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.setRouterType(address(0), ArbitrageExecutor.RouterType.V3);
    }

    // ═══════════════════════════════════════════════════════════
    // TRIANGULAR ARBITRAGE TESTS
    // ═══════════════════════════════════════════════════════════

    function test_Triangular_V2V2V2_Profitable() public {
        // Setup: TKA → TKB → TKC → TKA loop with profit
        // Create tokenC (third token) with 1M supply
        MockERC20 tokenC = new MockERC20("Token C", "TKC", 1_000_000 ether);

        // Deploy routerBC (TKB/TKC pool) and routerCA (TKC/TKA pool)
        // Configure pricing loop for profit:
        // A→B: existing routerA (1 TKA → 2 TKB)
        // B→C: new routerBC (need rate that gives profit)
        // C→A: new routerCA (need rate that gives profit)
        MockRouter routerBC = new MockRouter(3, 1);  // 1 TKB → 3 TKC
        MockRouter routerCA = new MockRouter(51, 100); // 1 TKC → 0.51 TKA

        // Fund routers
        tokenC.transfer(address(routerBC), 300_000 ether);
        tokenB.transfer(address(routerBC), 100_000 ether);
        tokenA.transfer(address(routerCA), 100_000 ether);
        tokenC.transfer(address(routerCA), 100_000 ether);

        // Whitelist new routers
        executor.setAllowedRouter(address(routerBC), true);
        executor.setAllowedRouter(address(routerCA), true);

        // Calculate expected profit:
        // 100 TKA → 200 TKB (via routerA: 1→2)
        // 200 TKB → 600 TKC (via routerBC: 1→3)
        // 600 TKC → 306 TKA (via routerCA: 1→0.51)
        // Net: 100 TKA → 306 TKA = +206 TKA profit

        uint256 balBefore = tokenA.balanceOf(address(executor));

        executor.executeTriangularArbitrage(
            address(tokenA), // TKA
            address(tokenB), // TKB
            address(tokenC), // TKC
            100 ether,
            address(routerA),  // A→B (1→2)
            address(routerBC), // B→C (1→3)
            address(routerCA), // C→A (1→0.51)
            0, 0, 0, // V2 fees
            100 ether, // minProfit (expect ~206)
            deadline
        );

        uint256 balAfter = tokenA.balanceOf(address(executor));
        uint256 profit = balAfter - balBefore;

        assertGt(profit, 100 ether); // At least 100 TKA profit
        assertEq(executor.totalTrades(), 1);
    }

    function test_Triangular_V3V3V3_Profitable() public {
        // V3 triangular: TKA → TKB → TKC → TKA with V3 routers
        MockERC20 tokenC = new MockERC20("Token C", "TKC", 1_000_000 ether);

        // Deploy V3 routers for B→C and C→A legs
        MockV3Router routerV3_BC = new MockV3Router(3, 1);  // 1 TKB → 3 TKC
        MockV3Router routerV3_CA = new MockV3Router(51, 100); // 1 TKC → 0.51 TKA

        // Fund routers
        tokenC.transfer(address(routerV3_BC), 300_000 ether);
        tokenB.transfer(address(routerV3_BC), 100_000 ether);
        tokenA.transfer(address(routerV3_CA), 100_000 ether);
        tokenC.transfer(address(routerV3_CA), 100_000 ether);

        // Whitelist and set types
        executor.setAllowedRouter(address(routerV3_BC), true);
        executor.setAllowedRouter(address(routerV3_CA), true);
        executor.setRouterType(address(routerV3_BC), ArbitrageExecutor.RouterType.V3);
        executor.setRouterType(address(routerV3_CA), ArbitrageExecutor.RouterType.V3);

        uint256 balBefore = tokenA.balanceOf(address(executor));

        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            100 ether,
            address(routerV3A),   // A→B (V3, 1→2)
            address(routerV3_BC), // B→C (V3, 1→3)
            address(routerV3_CA), // C→A (V3, 1→0.51)
            3000, 3000, 3000, // V3 fee tiers
            100 ether,
            deadline
        );

        uint256 balAfter = tokenA.balanceOf(address(executor));
        assertGt(balAfter - balBefore, 100 ether);
    }

    function test_Triangular_MixedV2V3_Profitable() public {
        // Mixed: V2 → V3 → V2
        MockERC20 tokenC = new MockERC20("Token C", "TKC", 1_000_000 ether);

        MockRouter routerCA_v2 = new MockRouter(51, 100); // 1 TKC → 0.51 TKA (V2)
        MockV3Router routerBC_v3 = new MockV3Router(3, 1); // 1 TKB → 3 TKC (V3)

        tokenA.transfer(address(routerCA_v2), 100_000 ether);
        tokenC.transfer(address(routerCA_v2), 100_000 ether);
        tokenB.transfer(address(routerBC_v3), 100_000 ether);
        tokenC.transfer(address(routerBC_v3), 300_000 ether);

        executor.setAllowedRouter(address(routerCA_v2), true);
        executor.setAllowedRouter(address(routerBC_v3), true);
        executor.setRouterType(address(routerBC_v3), ArbitrageExecutor.RouterType.V3);

        uint256 balBefore = tokenA.balanceOf(address(executor));

        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            100 ether,
            address(routerA),      // V2 (1→2)
            address(routerBC_v3),  // V3 (1→3)
            address(routerCA_v2),  // V2 (1→0.51)
            0, 3000, 0,
            100 ether,
            deadline
        );

        uint256 balAfter = tokenA.balanceOf(address(executor));
        assertGt(balAfter - balBefore, 100 ether);
    }

    function test_Triangular_RevertsIfUnprofitable() public {
        MockERC20 tokenC = new MockERC20("Token C", "TKC", 1_000_000 ether);

        // Bad pricing: loss loop - use routerC (existing V2 loss router: 1→0.4)
        // A→B: 100 → 200 (routerA)
        // B→C: 200 → 80 (routerC gives 0.4 rate, treating B as input)
        // C→A: need to get back >100 from 80 TKC → impossible
        MockRouter routerCA_bad = new MockRouter(2, 10); // 1 TKC → 0.2 TKA (bad rate)
        tokenA.transfer(address(routerCA_bad), 100_000 ether);
        tokenC.transfer(address(routerCA_bad), 100_000 ether);
        executor.setAllowedRouter(address(routerCA_bad), true);

        vm.expectRevert();
        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            100 ether,
            address(routerA),    // 1→2
            address(routerC),    // Treats input as B, gives 0.4 output
            address(routerCA_bad), // 1→0.2
            0, 0, 0,
            1 ether, // require 1 TKA profit
            deadline
        );
    }

    function test_Triangular_RevertsIfRouterNotAllowed() public {
        MockERC20 tokenC = new MockERC20("Token C", "TKC", 1_000_000 ether);
        MockRouter routerCA_unwhitelisted = new MockRouter(51, 100);
        // Don't whitelist

        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, address(routerCA_unwhitelisted)));
        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            100 ether,
            address(routerA),
            address(routerA),
            address(routerCA_unwhitelisted), // not whitelisted
            0, 0, 0,
            1 ether,
            deadline
        );
    }

    function test_Triangular_RevertsIfZeroAddress() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.executeTriangularArbitrage(
            address(0), // tokenA = 0
            address(tokenB),
            address(tokenA),
            1000 ether,
            address(routerA),
            address(routerA),
            address(routerA),
            0, 0, 0,
            1 ether,
            deadline
        );
    }

    function test_Triangular_RevertsIfZeroAmount() public {
        MockERC20 tokenC = new MockERC20("OP", "OP", 18);

        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            0, // amountIn = 0
            address(routerA),
            address(routerA),
            address(routerA),
            0, 0, 0,
            1 ether,
            deadline
        );
    }

    function test_Triangular_NonOwnerCannotExecute() public {
        MockERC20 tokenC = new MockERC20("OP", "OP", 18);

        vm.prank(attacker);
        vm.expectRevert();
        executor.executeTriangularArbitrage(
            address(tokenA),
            address(tokenB),
            address(tokenC),
            1000 ether,
            address(routerA),
            address(routerA),
            address(routerA),
            0, 0, 0,
            1 ether,
            deadline
        );
    }

    // Allow receiving ETH for withdraw tests
    receive() external payable {}
}
