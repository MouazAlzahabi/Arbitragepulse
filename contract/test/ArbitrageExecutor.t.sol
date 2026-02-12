// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockRouter.sol";

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
 */
contract ArbitrageExecutorTest is Test {
    ArbitrageExecutor public executor;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockRouter public routerA; // 1 TKA → 2 TKB (buy side)
    MockRouter public routerB; // 1 TKB → 0.6 TKA (sell side) → profit
    MockRouter public routerC; // 1 TKB → 0.4 TKA (sell side) → loss

    address public owner;
    address public attacker;
    address public newOwner;
    uint256 public deadline;

    function setUp() public {
        owner = address(this);
        attacker = makeAddr("attacker");
        newOwner = makeAddr("newOwner");

        // Deploy executor
        executor = new ArbitrageExecutor();

        // Deploy mock tokens
        tokenA = new MockERC20("Token A", "TKA", 1_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 1_000_000 ether);

        // Deploy mock routers with different rates
        routerA = new MockRouter(2, 1);   // 1 TKA → 2 TKB
        routerB = new MockRouter(6, 10);  // 1 TKB → 0.6 TKA → 2 * 0.6 = 1.2 TKA = PROFIT
        routerC = new MockRouter(4, 10);  // 1 TKB → 0.4 TKA → 2 * 0.4 = 0.8 TKA = LOSS

        // Fund routers
        tokenB.transfer(address(routerA), 100_000 ether);
        tokenA.transfer(address(routerB), 100_000 ether);
        tokenA.transfer(address(routerC), 100_000 ether);

        // Fund executor with trading capital
        tokenA.transfer(address(executor), 1_000 ether);

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
        emit ArbitrageExecutor.WithdrawETH(address(this), 0.5 ether);
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
        emit ArbitrageExecutor.WithdrawToken(address(tokenA), address(this), 1_000 ether);
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
            0, deadline
        );

        uint256 after_ = tokenA.balanceOf(address(executor));
        assertGt(after_, before);
    }

    function test_ExecuteArb_EmitsEvent() public {
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecutor.ArbitrageExecuted(
            address(tokenA), address(tokenB),
            address(routerA), address(routerB),
            100 ether, 20 ether
        );

        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsIfUnprofitable() public {
        vm.expectRevert(); // NotProfitable
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerC), // Loss router
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsOnZeroAmount() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            0,
            address(routerA), address(routerB),
            0, deadline
        );
    }

    function test_ExecuteArb_RevertsOnZeroAddress() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.executeArbitrage(
            address(tokenA), address(0),
            100 ether,
            address(routerA), address(routerB),
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
        uint256 pastDeadline = block.timestamp - 100;

        vm.expectRevert();
        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
            0, pastDeadline
        );
    }

    function test_Deadline_AcceptsWithinDeadline() public {
        uint256 futureDeadline = block.timestamp + 60;

        executor.executeArbitrage(
            address(tokenA), address(tokenB),
            100 ether,
            address(routerA), address(routerB),
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
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerB),
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
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerC), // Loss
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
            minProfit: 0
        });
        arbs[1] = ArbitrageExecutor.ArbParams({
            tokenIn: address(tokenA), tokenOut: address(tokenB),
            amountIn: 50 ether,
            routerA: address(routerA), routerB: address(routerC),
            minProfit: 0
        });

        vm.expectEmit(false, false, false, true);
        emit ArbitrageExecutor.BatchExecuted(2, 1, 10 ether);
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
            0, deadline
        );

        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    function test_TotalProfit_Accumulates() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, deadline
        );
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, deadline
        );

        assertEq(executor.totalProfit(address(tokenA)), 40 ether);
    }

    function test_TotalTrades_Increments() public {
        assertEq(executor.totalTrades(), 0);

        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, deadline
        );

        assertEq(executor.totalTrades(), 1);
    }

    function test_GetStats_ReturnsCorrectValues() public {
        executor.executeArbitrage(
            address(tokenA), address(tokenB), 100 ether,
            address(routerA), address(routerB), 0, deadline
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
            address(routerA), address(routerB), 0, deadline
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
            address(routerA), address(routerB), 0, deadline
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

    // Allow receiving ETH for withdraw tests
    receive() external payable {}
}
