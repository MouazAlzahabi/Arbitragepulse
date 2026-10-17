// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockRouter.sol";
import "../src/mocks/MockV3Router.sol";
import "../src/mocks/MockAerodromeRouter.sol";
import "../src/mocks/MockOddTokens.sol";

/**
 * @title ArbitrageExecutor v3 — test suite
 *
 * Rates used throughout (amountIn 100 TKA unless noted):
 *   buy  routers *A:  1 TKA → 2 TKB              (100 TKA → 200 TKB)
 *   sell routers *B:  1 TKB → 0.6 TKA  → profit  (200 TKB → 120 TKA, +20)
 *   sell routers *C:  1 TKB → 0.4 TKA  → loss    (200 TKB →  80 TKA, −20)
 *   Aerodrome: volatile 1 TKB → 0.6 TKA (profit), stable 1 TKB → 0.4 TKA (loss)
 */
contract ArbitrageExecutorTest is Test {
    ArbitrageExecutor public executor;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockERC20 public tokenC;

    MockRouter public v2A;   // TKA→TKB 2/1
    MockRouter public v2B;   // TKB→TKA 6/10
    MockRouter public v2C;   // TKB→TKA 4/10
    MockV3Router public v3A;
    MockV3Router public v3B;
    MockV3Router public v3C;
    MockAerodromeRouter public aero; // TKB→TKA: volatile 6/10, stable 4/10

    address public owner;
    address public attacker;
    uint256 public deadline;

    uint256 constant AMT = 100 ether;
    uint24 constant FEE = 500;

    event ArbitrageExecuted(
        address indexed tokenIn, address indexed tokenOut, address routerA, address routerB,
        uint256 amountIn, uint256 profit
    );

    function setUp() public {
        owner = address(this);
        attacker = makeAddr("attacker");
        deadline = block.timestamp + 300;

        executor = new ArbitrageExecutor();
        tokenA = new MockERC20("Token A", "TKA", 10_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 10_000_000 ether);
        tokenC = new MockERC20("Token C", "TKC", 10_000_000 ether);

        v2A = new MockRouter(2, 1);
        v2B = new MockRouter(6, 10);
        v2C = new MockRouter(4, 10);
        v3A = new MockV3Router(2, 1);
        v3B = new MockV3Router(6, 10);
        v3C = new MockV3Router(4, 10);
        aero = new MockAerodromeRouter(6, 10, 4, 10);

        _fund(address(v2A)); _fund(address(v3A));
        _fund(address(v2B)); _fund(address(v2C));
        _fund(address(v3B)); _fund(address(v3C));
        _fund(address(aero));

        tokenA.transfer(address(executor), 1_000 ether);

        executor.setRouter(address(v2A), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(v2B), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(v2C), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(v3A), ArbitrageExecutor.RouterType.V3);
        executor.setRouter(address(v3B), ArbitrageExecutor.RouterType.V3);
        executor.setRouter(address(v3C), ArbitrageExecutor.RouterType.V3);
        executor.setRouter(address(aero), ArbitrageExecutor.RouterType.Aerodrome);
    }

    function _fund(address router) internal {
        tokenA.transfer(router, 100_000 ether);
        tokenB.transfer(router, 100_000 ether);
        tokenC.transfer(router, 100_000 ether);
    }

    function _arb(address rA, address rB, uint24 fA, uint24 fB, uint256 minProfit) internal {
        executor.executeArbitrage(address(tokenA), address(tokenB), AMT, rA, rB, fA, fB, minProfit, deadline);
    }

    function _legFailed(uint8 leg, address router, ArbitrageExecutor.RouterType t, bytes memory reason)
        internal pure returns (bytes memory)
    {
        return abi.encodeWithSelector(ArbitrageExecutor.LegFailed.selector, leg, router, t, reason);
    }

    function _notProfitable(uint256 afterBal, uint256 minProfit) internal pure returns (bytes memory) {
        return abi.encodeWithSelector(ArbitrageExecutor.NotProfitable.selector, 1_000 ether, afterBal, minProfit);
    }

    function _err(string memory m) internal pure returns (bytes memory) {
        return abi.encodeWithSignature("Error(string)", m);
    }

    // ════════════════════════════════════════════════════════════
    // 2-hop: happy paths per router type
    // ════════════════════════════════════════════════════════════

    function test_V2V2_Profitable() public {
        uint256 before = tokenA.balanceOf(address(executor));
        _arb(address(v2A), address(v2B), 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
        assertEq(executor.totalTrades(), 1);
    }

    function test_V3V3_Profitable() public {
        uint256 before = tokenA.balanceOf(address(executor));
        _arb(address(v3A), address(v3B), FEE, FEE, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
    }

    function test_MixedV2V3_BothDirections() public {
        _arb(address(v2A), address(v3B), 0, FEE, 1 ether);
        _arb(address(v3A), address(v2B), FEE, 0, 1 ether);
        assertEq(executor.totalProfit(address(tokenA)), 40 ether);
    }

    function test_Aerodrome_VolatileSell_Profitable() public {
        uint256 before = tokenA.balanceOf(address(executor));
        _arb(address(v2A), address(aero), 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
        assertFalse(aero.lastStable(), "fee 0 must select the volatile pool");
        assertEq(aero.lastFactory(), address(0), "default factory");
    }

    function test_Aerodrome_StableFlagFollowsFee() public {
        // fee != 0 → stable pool (4/10 here: 100 → 200 → 80, a 20 loss)
        vm.expectRevert(_notProfitable(980 ether, 1 ether));
        _arb(address(v2A), address(aero), 0, 1, 1 ether);
    }

    function test_Arb_EmitsEvent() public {
        vm.expectEmit(true, true, false, true);
        emit ArbitrageExecuted(address(tokenA), address(tokenB), address(v2A), address(v2B), AMT, 20 ether);
        _arb(address(v2A), address(v2B), 0, 0, 1 ether);
    }

    function test_HeldIntermediateToken_IsNotSold() public {
        // inventory of tokenB already in the executor must not leak into the trade: a leg spends
        // only what the previous leg delivered (balance delta), never the whole balance
        tokenB.transfer(address(executor), 50 ether);
        _arb(address(v2A), address(v2B), 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether, "profit is the real +20");
        assertEq(tokenB.balanceOf(address(executor)), 50 ether, "held tokenB untouched");
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    // ════════════════════════════════════════════════════════════
    // Readable failures
    // ════════════════════════════════════════════════════════════

    // Slippage reads the same on every router type: routers get min-out 0 and the
    // contract reports the exact shortfall (PancakeSwap's router reverts with no reason).
    function test_Loss_V2_ReportsNotProfitableWithBalances() public {
        vm.expectRevert(_notProfitable(980 ether, 1 ether)); // 100 → 200 → 80
        _arb(address(v2A), address(v2C), 0, 0, 1 ether);
    }

    function test_Loss_V3_ReportsNotProfitableWithBalances() public {
        vm.expectRevert(_notProfitable(980 ether, 1 ether));
        _arb(address(v3A), address(v3C), FEE, FEE, 1 ether);
    }

    function test_MinProfitAboveSpread_ReportsNotProfitable() public {
        // spread is +20; asking for 30 → after 1020, required +30
        vm.expectRevert(_notProfitable(1_020 ether, 30 ether));
        _arb(address(v2A), address(v2B), 0, 0, 30 ether);
    }

    function test_ProfitExactlyAtFloor_Succeeds() public {
        // spread is +20 and minProfit is 20: the gate is `>=`, so a trade that lands exactly on the floor executes
        _arb(address(v2A), address(v2B), 0, 0, 20 ether);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether);
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    function test_RouterRevert_ReportsLegAndRouterReason() public {
        // Router without liquidity: its token transfer fails → the router's own error surfaces.
        MockRouter dry = new MockRouter(6, 10);
        executor.setRouter(address(dry), ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(_legFailed(2, address(dry), ArbitrageExecutor.RouterType.V2,
            abi.encodeWithSignature("ERC20InsufficientBalance(address,uint256,uint256)", address(dry), 0, 120 ether)));
        _arb(address(v2A), address(dry), 0, 0, 1 ether);
    }

    function test_InsufficientBalance() public {
        vm.expectRevert(abi.encodeWithSelector(
            ArbitrageExecutor.InsufficientBalance.selector, address(tokenA), 1_000 ether, 5_000 ether));
        executor.executeArbitrage(address(tokenA), address(tokenB), 5_000 ether,
            address(v2A), address(v2B), 0, 0, 0, deadline);
    }

    function test_V3FeeZero_IsRejectedBeforeSwapping() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.InvalidV3Fee.selector, uint8(1), address(v3A), uint24(0)));
        _arb(address(v3A), address(v3B), 0, FEE, 1 ether);
    }

    function test_V3FeeInBps_IsRejected() public {
        // 30 = "0.3%" in bps; the V3 tier is 3000
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.InvalidV3Fee.selector, uint8(2), address(v3B), uint24(30)));
        _arb(address(v3A), address(v3B), FEE, 30, 1 ether);
    }

    bytes4 immutable LEG_EXHAUSTED = ArbitrageExecutor.LegExhaustedGas.selector;

    /// @dev Revert data of an executeArbitrage call through `router` as leg 1 at `gasLimit`.
    function _revertDataAt(address router, uint256 gasLimit) internal returns (bytes memory data) {
        (bool ok, bytes memory ret) = address(executor).call{gas: gasLimit}(abi.encodeCall(
            ArbitrageExecutor.executeArbitrage,
            (address(tokenA), address(tokenB), AMT, router, address(v2B), 0, 0, 0, deadline)));
        assertFalse(ok, "must revert");
        return ret;
    }

    function test_ExhaustedGas_AcrossLimits_NamedOrNoData() public {
        // Direct, nested and INVALID-opcode routers, from tight to generous limits: the leg
        // is named (LegExhaustedGas) whenever the executor can still pay for the revert;
        // otherwise the tx has no data at all (the engine reads that from the receipt) —
        // never a misleading LegFailed "no pool / wrong router type".
        address[3] memory routers = [
            address(new MockGasBurnerRouter()), address(new MockNestedOutOfGasRouter()),
            address(new MockInvalidOpcodeRouter())
        ];
        for (uint256 r = 0; r < 3; r++) {
            executor.setRouter(routers[r], ArbitrageExecutor.RouterType.V2);
            bool named;
            for (uint256 g = 120_000; g <= 2_000_000; g += 80_000) {
                bytes memory d = _revertDataAt(routers[r], g);
                if (d.length == 0) continue;
                assertEq(bytes4(d), LEG_EXHAUSTED, "only LegExhaustedGas or no data");
                named = true;
            }
            assertTrue(named, "named at generous limits");
            assertEq(_revertDataAt(routers[r], 2_000_000),
                abi.encodeWithSelector(LEG_EXHAUSTED, uint8(1), routers[r], bytes("")));
        }
    }

    function test_InnerOutOfGasReraisedAsString_IsStillExhaustedGas() public {
        // Like Uniswap's TransferHelper: an inner call runs out of gas, the router reverts
        // with a string. Gas tells the truth, the string doesn't.
        MockStringifyingOutOfGasRouter r = new MockStringifyingOutOfGasRouter();
        executor.setRouter(address(r), ArbitrageExecutor.RouterType.V2);
        assertEq(_revertDataAt(address(r), 2_000_000),
            abi.encodeWithSelector(LEG_EXHAUSTED, uint8(1), address(r), _err("STF")));
    }

    function test_LateRealRevert_StaysLegFailed() public {
        // Used most of its gas, but said why: the router's answer wins over the gas heuristic.
        MockLateRevertRouter late = new MockLateRevertRouter();
        executor.setRouter(address(late), ArbitrageExecutor.RouterType.V2);
        assertEq(_revertDataAt(address(late), 2_000_000), _legFailed(1, address(late),
            ArbitrageExecutor.RouterType.V2, abi.encodeWithSelector(MockLateRevertRouter.K.selector)));
    }

    function test_V3RouterTypedAsV2_FailsWithEmptyReason() public {
        // V3 router registered with the wrong type → it has no V2 function.
        MockV3Router misconfigured = new MockV3Router(2, 1);
        _fund(address(misconfigured));
        executor.setRouter(address(misconfigured), ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(_legFailed(1, address(misconfigured), ArbitrageExecutor.RouterType.V2, ""));
        _arb(address(misconfigured), address(v2B), 0, 0, 1 ether);
    }

    function test_NoOutput() public {
        MockNoOpRouter noop = new MockNoOpRouter();
        executor.setRouter(address(noop), ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NoOutput.selector, uint8(1), address(noop)));
        _arb(address(noop), address(v2B), 0, 0, 1 ether);
    }

    function test_SameTokenTwice_InvalidPath() public {
        vm.expectRevert(ArbitrageExecutor.InvalidPath.selector);
        executor.executeArbitrage(address(tokenA), address(tokenA), AMT, address(v2A), address(v2B), 0, 0, 0, deadline);
    }

    function test_Deadline() public {
        vm.warp(deadline + 1);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.DeadlineExpired.selector, deadline, deadline + 1));
        _arb(address(v2A), address(v2B), 0, 0, 0);
    }

    function test_ZeroAmount() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.executeArbitrage(address(tokenA), address(tokenB), 0, address(v2A), address(v2B), 0, 0, 0, deadline);
    }

    function test_ZeroAddressToken() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.executeArbitrage(address(0), address(tokenB), AMT, address(v2A), address(v2B), 0, 0, 0, deadline);
    }

    function test_UnknownRouter() public {
        address unknown = makeAddr("unknown");
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, unknown));
        _arb(unknown, address(v2B), 0, 0, 0);
    }

    function test_RevokedRouter() public {
        executor.revokeRouter(address(v2B));
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, address(v2B)));
        _arb(address(v2A), address(v2B), 0, 0, 0);
    }

    function test_RetiredRouterTypesRejected() public {
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnsupportedRouterType.selector,
            address(v2A), ArbitrageExecutor.RouterType.RetiredSolidly));
        executor.setRouter(address(v2A), ArbitrageExecutor.RouterType.RetiredSolidly);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.UnsupportedRouterType.selector,
            address(v2A), ArbitrageExecutor.RouterType.RetiredSyncSwap));
        executor.setRouter(address(v2A), ArbitrageExecutor.RouterType.RetiredSyncSwap);
    }

    function test_Paused() public {
        executor.pause();
        vm.expectRevert(Pausable.EnforcedPause.selector);
        _arb(address(v2A), address(v2B), 0, 0, 0);
        executor.unpause();
        _arb(address(v2A), address(v2B), 0, 0, 0);
    }

    // ════════════════════════════════════════════════════════════
    // Robustness: amounts measured, odd tokens
    // ════════════════════════════════════════════════════════════

    function test_RouterMisreportingOutput_UsesRealBalance() public {
        // Lying router sends 200 TKB but reports 400. The sell leg must sell 200.
        MockLyingRouter liar = new MockLyingRouter();
        _fund(address(liar));
        executor.setRouter(address(liar), ArbitrageExecutor.RouterType.V2);
        uint256 before = tokenA.balanceOf(address(executor));
        _arb(address(liar), address(v2B), 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
        assertEq(tokenB.balanceOf(address(executor)), 0, "nothing stranded");
    }

    function test_FeeOnTransferIntermediate_ForwardsActualAmount() public {
        // TKA → FEE → TKA. 1% transfer fee on FEE: the sell leg is given what actually
        // arrived, not the buy router's quote. (Real pools still reject a short input —
        // FoT tokens are unsupported; this pins the contract's own accounting.)
        MockFeeOnTransferERC20 fee = new MockFeeOnTransferERC20(10_000_000 ether);
        MockRouter buy = new MockRouter(2, 1);
        MockRouter sell = new MockRouter(6, 10);
        fee.transfer(address(buy), 100_000 ether);
        tokenA.transfer(address(sell), 100_000 ether);
        executor.setRouter(address(buy), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(sell), ArbitrageExecutor.RouterType.V2);
        uint256 before = tokenA.balanceOf(address(executor));
        executor.executeArbitrage(address(tokenA), address(fee), AMT, address(buy), address(sell), 0, 0, 1 ether, deadline);
        // 200 FEE sent → 198 received → 118.8 TKA back
        assertEq(tokenA.balanceOf(address(executor)), before - AMT + 118.8 ether);
        assertEq(fee.balanceOf(address(executor)), 0, "nothing stranded");
    }

    function test_NotProfitable_WhenFeeEatsTheMargin() public {
        // tokenIn is fee-on-transfer: the sell router sends 120, but the executor
        // receives 118.8 < 100 + 19 → NotProfitable (the contract counts what arrived).
        MockFeeOnTransferERC20 fee = new MockFeeOnTransferERC20(10_000_000 ether);
        MockRouter buy = new MockRouter(2, 1);
        MockRouter sell = new MockRouter(6, 10);
        tokenB.transfer(address(buy), 100_000 ether);
        fee.transfer(address(sell), 100_000 ether);
        fee.transfer(address(executor), 1_000 ether);   // executor receives 990
        executor.setRouter(address(buy), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(sell), ArbitrageExecutor.RouterType.V2);
        uint256 before = fee.balanceOf(address(executor));
        // buy router pulls 100 FEE but only 99 reach it — it still quotes on 100
        vm.expectRevert(abi.encodeWithSelector(
            ArbitrageExecutor.NotProfitable.selector, before, before - AMT + 118.8 ether, 19 ether));
        executor.executeArbitrage(address(fee), address(tokenB), AMT, address(buy), address(sell), 0, 0, 19 ether, deadline);
    }

    function test_TokenRejectingZeroApprove_StillTrades() public {
        // Routers pull the exact approved amount, so no approve(0) is ever sent.
        MockNoZeroApproveERC20 nza = new MockNoZeroApproveERC20(10_000_000 ether);
        MockRouter buy = new MockRouter(2, 1);
        MockRouter sell = new MockRouter(6, 10);
        tokenB.transfer(address(buy), 100_000 ether);
        nza.transfer(address(sell), 100_000 ether);
        nza.transfer(address(executor), 1_000 ether);
        executor.setRouter(address(buy), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(sell), ArbitrageExecutor.RouterType.V2);
        executor.executeArbitrage(address(nza), address(tokenB), AMT, address(buy), address(sell), 0, 0, 1 ether, deadline);
        assertEq(nza.balanceOf(address(executor)), 1_020 ether);
    }

    function test_LeftoverAllowance_TokenRejectingZeroApprove_DoesNotAbort() public {
        MockNoZeroApproveERC20 nza = new MockNoZeroApproveERC20(10_000_000 ether);
        MockPartialRouter partialBuy = new MockPartialRouter();
        MockRouter sell = new MockRouter(6, 10);
        tokenB.transfer(address(partialBuy), 100_000 ether);
        nza.transfer(address(sell), 100_000 ether);
        nza.transfer(address(executor), 1_000 ether);
        executor.setRouter(address(partialBuy), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(sell), ArbitrageExecutor.RouterType.V2);
        // pulls 50, sends 200 TKB → 120 NZA back
        executor.executeArbitrage(address(nza), address(tokenB), AMT, address(partialBuy), address(sell), 0, 0, 1 ether, deadline);
        assertEq(nza.balanceOf(address(executor)), 1_070 ether);
        assertEq(nza.allowance(address(executor), address(partialBuy)), 1, "approve(0) rejected -> 1 wei left");
    }

    event AllowanceNotCleared(address indexed token, address indexed spender, uint256 remaining);

    /// @dev Buy router pulls only half the input (leaves an allowance); sell router is 6/10.
    function _partialTrade(address token) internal returns (MockPartialRouter partialBuy, MockRouter sell) {
        partialBuy = new MockPartialRouter();
        sell = new MockRouter(6, 10);
        tokenB.transfer(address(partialBuy), 100_000 ether);
        IERC20(token).transfer(address(sell), 100_000 ether);
        IERC20(token).transfer(address(executor), 1_000 ether);
        executor.setRouter(address(partialBuy), ArbitrageExecutor.RouterType.V2);
        executor.setRouter(address(sell), ArbitrageExecutor.RouterType.V2);
    }

    function test_MalformedApproveReturn_OnClear_KeepsTrade() public {
        // Leftover allowance + approve(0) returning the word 2: treated as a failed clear
        // (falls back to 1 wei), never an abi.decode revert that throws the trade away.
        MockMalformedApproveERC20 mal = new MockMalformedApproveERC20(10_000_000 ether);
        (MockPartialRouter partialBuy, MockRouter sell) = _partialTrade(address(mal));
        executor.executeArbitrage(address(mal), address(tokenB), AMT, address(partialBuy), address(sell), 0, 0, 1 ether, deadline);
        assertEq(mal.balanceOf(address(executor)), 1_070 ether);
        assertEq(mal.allowance(address(executor), address(partialBuy)), 1);
    }

    function test_MalformedApproveReturn_RevokeApproval_FallsBack() public {
        MockMalformedApproveERC20 mal = new MockMalformedApproveERC20(1 ether);
        vm.prank(address(executor));
        mal.approve(address(v2A), 500);
        executor.revokeApproval(address(mal), address(v2A));
        assertEq(mal.allowance(address(executor), address(v2A)), 1);
    }

    function test_AlwaysMalformedApprove_FailsUpFrontAndOnRevoke() public {
        // approve returns 2 for every value: the leg's own approve fails cleanly (SafeERC20),
        // and revokeApproval reports failure instead of dying inside abi.decode.
        MockAlwaysMalformedApproveERC20 bad = new MockAlwaysMalformedApproveERC20(10_000_000 ether);
        (MockPartialRouter partialBuy, MockRouter sell) = _partialTrade(address(bad));
        vm.expectRevert(abi.encodeWithSelector(SafeERC20.SafeERC20FailedOperation.selector, address(bad)));
        executor.executeArbitrage(address(bad), address(tokenB), AMT, address(partialBuy), address(sell), 0, 0, 0, deadline);
        vm.expectRevert(abi.encodeWithSelector(SafeERC20.SafeERC20FailedOperation.selector, address(bad)));
        executor.revokeApproval(address(bad), address(v2A));
    }

    function test_UnclearableAllowance_EmitsEventAndKeepsTrade() public {
        MockStickyAllowanceERC20 stk = new MockStickyAllowanceERC20(10_000_000 ether);
        (MockPartialRouter partialBuy, MockRouter sell) = _partialTrade(address(stk));
        vm.expectEmit(true, true, false, true);
        emit AllowanceNotCleared(address(stk), address(partialBuy), 50 ether);
        executor.executeArbitrage(address(stk), address(tokenB), AMT, address(partialBuy), address(sell), 0, 0, 1 ether, deadline);
        assertEq(stk.balanceOf(address(executor)), 1_070 ether);
    }

    function test_RevokeApproval_NonContractToken_Reverts() public {
        address eoa = makeAddr("notAToken");
        vm.expectRevert(abi.encodeWithSelector(SafeERC20.SafeERC20FailedOperation.selector, eoa));
        executor.revokeApproval(eoa, address(v2A));
    }

    function test_RevokeApproval_TokenRejectingZeroApprove() public {
        MockNoZeroApproveERC20 nza = new MockNoZeroApproveERC20(1 ether);
        vm.prank(address(executor));
        nza.approve(address(v2A), 500);
        executor.revokeApproval(address(nza), address(v2A));
        assertEq(nza.allowance(address(executor), address(v2A)), 1);
    }

    function test_ShrinkingOutputBalance_ReportsNoOutputNotPanic() public {
        MockSlashableERC20 sls = new MockSlashableERC20(1_000 ether);
        sls.transfer(address(executor), 10 ether);   // executor already holds some tokenOut
        MockSlashingRouter slasher = new MockSlashingRouter();
        executor.setRouter(address(slasher), ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NoOutput.selector, uint8(1), address(slasher)));
        executor.executeArbitrage(address(tokenA), address(sls), AMT, address(slasher), address(v2B), 0, 0, 0, deadline);
    }

    function test_LeftoverAllowance_IsCleared() public {
        MockPartialRouter partialBuy = new MockPartialRouter();
        _fund(address(partialBuy));
        executor.setRouter(address(partialBuy), ArbitrageExecutor.RouterType.V2);
        _arb(address(partialBuy), address(v2B), 0, 0, 1 ether);
        assertEq(tokenA.allowance(address(executor), address(partialBuy)), 0);
    }

    function test_AllowancesClearedAfterTrade() public {
        _arb(address(v2A), address(v3B), 0, FEE, 1 ether);
        _arb(address(v3A), address(aero), FEE, 0, 1 ether);
        assertEq(tokenA.allowance(address(executor), address(v2A)), 0);
        assertEq(tokenB.allowance(address(executor), address(v3B)), 0);
        assertEq(tokenA.allowance(address(executor), address(v3A)), 0);
        assertEq(tokenB.allowance(address(executor), address(aero)), 0);
    }

    // ════════════════════════════════════════════════════════════
    // Triangular
    // ════════════════════════════════════════════════════════════

    // A→B 2/1 (v2A), B→C via new 1/1 routers, C→A 6/10 (v2B rates) → 100 → 200 → 200 → 120
    function _triRouters(ArbitrageExecutor.RouterType tBC) internal returns (address bc) {
        if (tBC == ArbitrageExecutor.RouterType.V3) {
            MockV3Router r = new MockV3Router(1, 1); _fund(address(r)); bc = address(r);
        } else if (tBC == ArbitrageExecutor.RouterType.Aerodrome) {
            MockAerodromeRouter r = new MockAerodromeRouter(1, 1, 1, 1); _fund(address(r)); bc = address(r);
        } else {
            MockRouter r = new MockRouter(1, 1); _fund(address(r)); bc = address(r);
        }
        executor.setRouter(bc, tBC);
    }

    function _tri(address ab, address bc, address ca, uint24 fab, uint24 fbc, uint24 fca, uint256 minProfit) internal {
        executor.executeTriangularArbitrage(address(tokenA), address(tokenB), address(tokenC), AMT,
            ab, bc, ca, fab, fbc, fca, minProfit, deadline);
    }

    function test_Triangular_V2_Profitable() public {
        address bc = _triRouters(ArbitrageExecutor.RouterType.V2);
        uint256 before = tokenA.balanceOf(address(executor));
        _tri(address(v2A), bc, address(v2B), 0, 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
        assertEq(tokenB.balanceOf(address(executor)) + tokenC.balanceOf(address(executor)), 0, "nothing stranded");
    }

    function test_Triangular_HeldIntermediateTokens_AreNotSold() public {
        address bc = _triRouters(ArbitrageExecutor.RouterType.V2);
        tokenB.transfer(address(executor), 50 ether);
        tokenC.transfer(address(executor), 70 ether);
        _tri(address(v2A), bc, address(v2B), 0, 0, 0, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), 1_020 ether, "profit is the real +20");
        assertEq(tokenB.balanceOf(address(executor)), 50 ether, "held tokenB untouched");
        assertEq(tokenC.balanceOf(address(executor)), 70 ether, "held tokenC untouched");
        assertEq(executor.totalProfit(address(tokenA)), 20 ether);
    }

    event TriangularArbitrageExecuted(
        address indexed tokenA, address indexed tokenB, address indexed tokenC,
        address routerAB, address routerBC, address routerCA, uint256 amountIn, uint256 profit
    );

    function test_Triangular_EventNamesAllThreeRouters() public {
        address bc = _triRouters(ArbitrageExecutor.RouterType.V2);
        vm.expectEmit(true, true, true, true);
        emit TriangularArbitrageExecuted(address(tokenA), address(tokenB), address(tokenC),
            address(v2A), bc, address(v2B), AMT, 20 ether);
        _tri(address(v2A), bc, address(v2B), 0, 0, 0, 1 ether);
    }

    function test_Triangular_AllTypes() public {
        address bc = _triRouters(ArbitrageExecutor.RouterType.Aerodrome);
        uint256 before = tokenA.balanceOf(address(executor));
        _tri(address(v3A), bc, address(v3B), FEE, 0, FEE, 1 ether);
        assertEq(tokenA.balanceOf(address(executor)), before + 20 ether);
    }

    function test_V3FeeReachesRouterPerLeg() public {
        // 2-hop: distinct tiers per leg, each must reach its own router unchanged
        _arb(address(v3A), address(v3B), 500, 3000, 1 ether);
        assertEq(v3A.lastFee(), 500, "2-hop leg 1 fee");
        assertEq(v3B.lastFee(), 3000, "2-hop leg 2 fee");
        assertEq(v3A.lastSqrtPriceLimitX96(), 0, "no price limit");
        assertEq(v3B.lastSqrtPriceLimitX96(), 0, "no price limit");

        // triangular: 100 -> 200 (v3A 2/1) -> 200 (bc 1/1) -> 120 (v3B 6/10), three distinct tiers
        address bc = _triRouters(ArbitrageExecutor.RouterType.V3);
        _tri(address(v3A), bc, address(v3B), 100, 2500, 10000, 1 ether);
        assertEq(v3A.lastFee(), 100, "tri leg AB fee");
        assertEq(MockV3Router(bc).lastFee(), 2500, "tri leg BC fee");
        assertEq(v3B.lastFee(), 10000, "tri leg CA fee");
    }

    function test_Triangular_LossReportsNotProfitable() public {
        address bc = _triRouters(ArbitrageExecutor.RouterType.V3);
        vm.expectRevert(_notProfitable(980 ether, 1 ether)); // 100 → 200 → 200 → 80
        _tri(address(v2A), bc, address(v3C), 0, FEE, FEE, 1 ether);
    }

    function test_Triangular_Leg2Failure_NamesLeg2AndRouterBC() public {
        // unfunded 1/1 V2 router as BC: 100 TKA -> 200 TKB, then BC can't pay 200 TKC
        MockRouter dry = new MockRouter(1, 1);
        executor.setRouter(address(dry), ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(_legFailed(2, address(dry), ArbitrageExecutor.RouterType.V2,
            abi.encodeWithSignature("ERC20InsufficientBalance(address,uint256,uint256)", address(dry), 0, 200 ether)));
        _tri(address(v2A), address(dry), address(v2B), 0, 0, 0, 1 ether);
    }

    function test_Triangular_Leg3Failure_NamesLeg3AndRouterCA() public {
        // legs 1-2 run, then CA (V3) rejects a bps-style fee: the error must carry leg 3
        address bc = _triRouters(ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.InvalidV3Fee.selector, uint8(3), address(v3B), uint24(30)));
        _tri(address(v2A), bc, address(v3B), 0, 0, 30, 1 ether);
    }

    function test_Triangular_RepeatedToken() public {
        vm.expectRevert(ArbitrageExecutor.InvalidPath.selector);
        executor.executeTriangularArbitrage(address(tokenA), address(tokenB), address(tokenA), AMT,
            address(v2A), address(v2A), address(v2B), 0, 0, 0, 0, deadline);
    }

    function test_Triangular_RouterNotAllowed() public {
        address unknown = makeAddr("unknown");
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, unknown));
        _tri(address(v2A), unknown, address(v2B), 0, 0, 0, 0);
    }

    function test_Triangular_ZeroAmountAndAddress() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.executeTriangularArbitrage(address(tokenA), address(tokenB), address(tokenC), 0,
            address(v2A), address(v2A), address(v2B), 0, 0, 0, 0, deadline);
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.executeTriangularArbitrage(address(tokenA), address(0), address(tokenC), AMT,
            address(v2A), address(v2A), address(v2B), 0, 0, 0, 0, deadline);
    }

    // ════════════════════════════════════════════════════════════
    // Router configuration
    // ════════════════════════════════════════════════════════════

    function test_RouterReturningNoData_ReportsNoOutput() public {
        // Success with empty return data must not fail in the executor's own decoder.
        MockSilentRouter silent = new MockSilentRouter();
        executor.setRouter(address(silent), ArbitrageExecutor.RouterType.V3);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NoOutput.selector, uint8(1), address(silent)));
        _arb(address(silent), address(v2B), FEE, 0, 1 ether);
    }

    function test_SetRouter_RejectsNonContract() public {
        address eoa = makeAddr("eoa");
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NotAContract.selector, eoa));
        executor.setRouter(eoa, ArbitrageExecutor.RouterType.V2);
    }

    function test_SetRouter_RetypesAndRevokeRouter() public {
        executor.setRouter(address(v2A), ArbitrageExecutor.RouterType.V3);
        assertEq(uint8(executor.routerType(address(v2A))), 1);
        executor.revokeRouter(address(v2A));
        assertFalse(executor.allowedRouters(address(v2A)));
    }

    function test_SetRouter_AllowsAndTypes() public {
        address r = address(new MockAerodromeRouter(1, 1, 1, 1));
        executor.setRouter(r, ArbitrageExecutor.RouterType.Aerodrome);
        assertTrue(executor.allowedRouters(r));
        assertEq(uint8(executor.routerType(r)), 4, "Aerodrome must stay ordinal 4");
        assertEq(uint8(ArbitrageExecutor.RouterType.V2), 0);
        assertEq(uint8(ArbitrageExecutor.RouterType.V3), 1);
    }

    function test_SetRouter_ZeroAddress() public {
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.setRouter(address(0), ArbitrageExecutor.RouterType.V2);
    }

    // ════════════════════════════════════════════════════════════
    // Access control
    // ════════════════════════════════════════════════════════════

    function test_NonOwnerCannotTradeOrConfigure() public {
        bytes memory unauthorized = abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, attacker);
        vm.startPrank(attacker);
        vm.expectRevert(unauthorized);
        _arb(address(v2A), address(v2B), 0, 0, 0);
        vm.expectRevert(unauthorized);
        executor.executeTriangularArbitrage(address(tokenA), address(tokenB), address(tokenC), AMT,
            address(v2A), address(v2A), address(v2B), 0, 0, 0, 0, deadline);
        vm.expectRevert(unauthorized);
        executor.setRouter(attacker, ArbitrageExecutor.RouterType.V2);
        vm.expectRevert(unauthorized);
        executor.revokeRouter(address(v2A));
        vm.expectRevert(unauthorized);
        executor.pause();
        vm.expectRevert(unauthorized);
        executor.withdrawToken(address(tokenA));
        vm.expectRevert(unauthorized);
        executor.withdrawETH();
        vm.expectRevert(unauthorized);
        executor.revokeApproval(address(tokenA), address(v2A));
        vm.stopPrank();
    }

    function test_Ownable2Step() public {
        address next = makeAddr("next");
        executor.transferOwnership(next);
        assertEq(executor.owner(), owner, "pending until accepted");
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, attacker));
        executor.acceptOwnership();
        vm.prank(next);
        executor.acceptOwnership();
        assertEq(executor.owner(), next);
    }

    // ════════════════════════════════════════════════════════════
    // Funds
    // ════════════════════════════════════════════════════════════

    function test_RenounceOwnership_Disabled() public {
        vm.expectRevert(ArbitrageExecutor.RenounceDisabled.selector);
        executor.renounceOwnership();
        assertEq(executor.owner(), owner);
    }

    event WithdrawToken(address indexed token, address indexed to, uint256 amount);

    function test_WithdrawTokenAmount_PaysOwnerAndEmits() public {
        uint256 before = tokenA.balanceOf(owner);
        vm.expectEmit(true, true, false, true);
        emit WithdrawToken(address(tokenA), owner, 250 ether);
        executor.withdrawTokenAmount(address(tokenA), 250 ether);
        assertEq(tokenA.balanceOf(owner), before + 250 ether);
        assertEq(tokenA.balanceOf(address(executor)), 750 ether);
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.withdrawTokenAmount(address(tokenA), 0);
        vm.expectRevert(ArbitrageExecutor.ZeroAddress.selector);
        executor.withdrawTokenAmount(address(0), 1);
    }

    function test_TotalProfit_AccumulatesAcrossTrades() public {
        _arb(address(v2A), address(v2B), 0, 0, 1 ether);
        _arb(address(v3A), address(v3B), FEE, FEE, 1 ether);
        address bc = _triRouters(ArbitrageExecutor.RouterType.V2);
        _tri(address(v2A), bc, address(v2B), 0, 0, 0, 1 ether);
        assertEq(executor.totalProfit(address(tokenA)), 60 ether);
        assertEq(executor.totalTrades(), 3);
    }

    function test_WithdrawETH() public {
        vm.deal(address(executor), 1 ether);
        uint256 before = owner.balance;
        executor.withdrawETH();
        assertEq(owner.balance, before + 1 ether);
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.withdrawETH();
    }

    function test_WithdrawTokens_WorkWhenPaused() public {
        executor.pause();
        executor.withdrawTokenAmount(address(tokenA), 100 ether);
        assertEq(tokenA.balanceOf(address(executor)), 900 ether);
        executor.withdrawToken(address(tokenA));
        assertEq(tokenA.balanceOf(address(executor)), 0);
        vm.expectRevert(ArbitrageExecutor.ZeroAmount.selector);
        executor.withdrawToken(address(tokenA));
    }

    function test_RevokeApproval() public {
        vm.prank(address(executor));
        tokenA.approve(address(v2A), 123);
        executor.revokeApproval(address(tokenA), address(v2A));
        assertEq(tokenA.allowance(address(executor), address(v2A)), 0);
    }

    function test_GetStats() public {
        _arb(address(v2A), address(v2B), 0, 0, 1 ether);
        (uint256 p, uint256 n, uint256 bal) = executor.getStats(address(tokenA));
        assertEq(p, 20 ether);
        assertEq(n, 1);
        assertEq(bal, 1_020 ether);
    }

    receive() external payable {}
}
