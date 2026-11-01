// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/ArbitrageExecutor.sol";
import "../src/mocks/MockERC20.sol";
import "../src/mocks/MockDirectVenues.sol";

/**
 * @title V4 legs (router type 8) on the mock PoolManager
 *
 * Rates (amountIn 100 TKA): V4 buy pool 1 TKA → 2 TKB, sell CL pool 1 TKB → 0.6 TKA (+20 TKA).
 * Native pool ETH/TKA: 1 TKA → 0.001 ETH; CL pool WETH/TKA: 1 WETH → 1200 TKA (+20 %).
 */
contract V4LegsTest is Test {
    address constant WETH = 0x4200000000000000000000000000000000000006;

    ArbitrageExecutor public executor;
    MockPoolManager public pm;
    MockERC20 public tokenA;
    MockERC20 public tokenB;
    MockERC20 public tokenC;
    MockCLPool public clSell;

    address public attacker;
    uint256 public deadline;
    uint256 constant AMT = 100 ether;

    function setUp() public {
        attacker = makeAddr("attacker");
        deadline = block.timestamp + 300;
        vm.etch(WETH, address(new MockWETH()).code);
        pm = new MockPoolManager();
        executor = new ArbitrageExecutor(address(pm));
        tokenA = new MockERC20("Token A", "TKA", 10_000_000 ether);
        tokenB = new MockERC20("Token B", "TKB", 10_000_000 ether);
        tokenC = new MockERC20("Token C", "TKC", 10_000_000 ether);

        clSell = new MockCLPool(address(tokenA), address(tokenB), 6, 10);
        tokenA.transfer(address(clSell), 100_000 ether);
        tokenB.transfer(address(clSell), 100_000 ether);
        executor.setRouter(address(clSell), ArbitrageExecutor.RouterType.DirectCL);

        tokenA.transfer(address(pm), 1_000_000 ether);
        tokenB.transfer(address(pm), 1_000_000 ether);
        vm.deal(address(pm), 1_000 ether);
    }

    function _key(address a, address b, uint24 fee, int24 ts) internal pure returns (IPoolManagerV4.PoolKey memory k) {
        (address c0, address c1) = a < b ? (a, b) : (b, a);
        k = IPoolManagerV4.PoolKey({ currency0: c0, currency1: c1, fee: fee, tickSpacing: ts, hooks: address(0) });
    }

    /// Register `k` on the executor and give it a rate on the mock PoolManager.
    function _pool(IPoolManagerV4.PoolKey memory k, uint256 num, uint256 den) internal returns (address pool) {
        IPoolManagerV4.PoolKey[] memory keys = new IPoolManagerV4.PoolKey[](1);
        keys[0] = k;
        executor.setV4Pools(keys);
        pm.setRate(MockPoolManager.PoolKey(k.currency0, k.currency1, k.fee, k.tickSpacing, k.hooks), num, den);
        pool = executor.v4PoolAddress(k);
    }

    /// Rate for swapping in `tokenIn` on a pool whose rate is set for currency0 → currency1.
    function _abPool(uint256 num, uint256 den) internal returns (address) {
        // TKA → TKB at num/den; the mock rate is per pool, so TKB → TKA would also use num/den.
        return _pool(_key(address(tokenA), address(tokenB), 500, 10), num, den);
    }

    function _wethCL(uint256 num, uint256 den) internal returns (MockCLPool cl) {
        cl = new MockCLPool(WETH, address(tokenA), num, den);
        vm.deal(address(this), 100 ether);
        MockWETH(payable(WETH)).deposit{value: 100 ether}();
        MockWETH(payable(WETH)).transfer(address(cl), 100 ether);
        tokenA.transfer(address(cl), 100_000 ether);
        executor.setRouter(address(cl), ArbitrageExecutor.RouterType.DirectCL);
    }

    // ── Pool address and registration ──────────────────────────────

    function test_PoolAddress_IsLow160OfPoolId() public view {
        IPoolManagerV4.PoolKey memory k = _key(address(0), address(tokenA), 500, 10);
        bytes32 id = keccak256(abi.encode(k.currency0, k.currency1, k.fee, k.tickSpacing, k.hooks));
        assertEq(executor.v4PoolAddress(k), address(uint160(uint256(id))));
    }

    function test_SetV4Pools_Registers() public {
        IPoolManagerV4.PoolKey memory k = _key(address(tokenA), address(tokenB), 3000, 60);
        address pool = _pool(k, 1, 1);
        assertTrue(executor.allowedRouters(pool));
        assertEq(uint8(executor.routerType(pool)), uint8(ArbitrageExecutor.RouterType.V4));
        (address c0,, uint24 fee, int24 ts, address hooks) = executor.v4Pools(pool);
        assertEq(c0, k.currency0);
        assertEq(fee, 3000);
        assertEq(ts, 60);
        assertEq(hooks, address(0));
    }

    function _expectBadKey(IPoolManagerV4.PoolKey memory k) internal {
        IPoolManagerV4.PoolKey[] memory keys = new IPoolManagerV4.PoolKey[](1);
        keys[0] = k;
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.InvalidV4Key.selector, executor.v4PoolAddress(k)));
        executor.setV4Pools(keys);
    }

    function test_SetV4Pools_RejectsUnsupportedKeys() public {
        IPoolManagerV4.PoolKey memory k = _key(address(tokenA), address(tokenB), 500, 10);
        k.hooks = address(0xBEEF);
        _expectBadKey(k); // hooked
        k = _key(address(tokenA), address(tokenB), 0x800000, 10);
        _expectBadKey(k); // dynamic fee
        k = _key(address(tokenA), address(tokenB), 500, 0);
        _expectBadKey(k); // no tick spacing
        k = _key(address(tokenA), address(tokenB), 500, 10);
        (k.currency0, k.currency1) = (k.currency1, k.currency0);
        _expectBadKey(k); // unordered
        _expectBadKey(_key(address(0), WETH, 500, 10)); // ETH/WETH
    }

    function test_SetV4Pools_OnlyOwner() public {
        IPoolManagerV4.PoolKey[] memory keys = new IPoolManagerV4.PoolKey[](1);
        keys[0] = _key(address(tokenA), address(tokenB), 500, 10);
        vm.prank(attacker);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, attacker));
        executor.setV4Pools(keys);
    }

    function test_SetV4Pools_NeedsPoolManager() public {
        ArbitrageExecutor plain = new ArbitrageExecutor(address(0));
        IPoolManagerV4.PoolKey[] memory keys = new IPoolManagerV4.PoolKey[](1);
        keys[0] = _key(address(tokenA), address(tokenB), 500, 10);
        vm.expectRevert(ArbitrageExecutor.BorrowUnavailable.selector);
        plain.setV4Pools(keys);
    }

    function test_SetRouter_CannotRegisterV4() public {
        vm.expectRevert(abi.encodeWithSelector(
            ArbitrageExecutor.UnsupportedRouterType.selector, address(clSell), ArbitrageExecutor.RouterType.V4));
        executor.setRouter(address(clSell), ArbitrageExecutor.RouterType.V4);
    }

    // ── Trades ─────────────────────────────────────────────────────

    function test_V4_Borrowed_TwoHop_ERC20() public {
        address v4 = _abPool(2, 1);
        uint256 pmA = tokenA.balanceOf(address(pm));
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, v4, address(clSell), 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether, "profit");
        assertEq(tokenB.balanceOf(address(executor)), 0, "nothing left mid-route");
        // PoolManager: +100 TKA in (swap) +100 TKA repaid −100 TKA lent = +100 TKA net.
        assertEq(tokenA.balanceOf(address(pm)), pmA + AMT);
        assertEq(pm.debt(address(tokenA)), 0);
        assertEq(pm.debt(address(tokenB)), 0);
    }

    function test_V4_NativeOutput_WrapsToWETH() public {
        // TKA → ETH on V4 (1 TKA = 0.001 ETH), WETH → TKA on CL at 1200: +20 %.
        address v4 = _pool(_key(address(0), address(tokenA), 500, 10), 1, 1000);
        MockCLPool cl = _wethCL(1200, 1);
        executor.executeArbitrageBorrowed(address(tokenA), WETH, AMT, v4, address(cl), 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
        assertEq(MockWETH(payable(WETH)).balanceOf(address(executor)), 0);
        assertEq(address(executor).balance, 0, "no ETH left");
        assertEq(pm.debt(address(0)), 0);
    }

    function test_V4_NativeInput_UnwrapsWETH() public {
        // TKA → WETH on CL (1 TKA = 0.001 WETH), ETH → TKA on V4 at 1200: +20 %.
        MockCLPool cl = _wethCL(1, 1000);
        address v4 = _pool(_key(address(0), address(tokenA), 500, 10), 1200, 1);
        executor.executeArbitrageBorrowed(address(tokenA), WETH, AMT, address(cl), v4, 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
        assertEq(address(executor).balance, 0);
        assertEq(pm.debt(address(0)), 0);
    }

    function test_V4_Triangular() public {
        // TKA → TKB (V4, ×2), TKB → TKC (V4, ×1), TKC → TKA (CL, ×0.6): +20 %.
        address ab = _abPool(2, 1);
        address bc = _pool(_key(address(tokenB), address(tokenC), 500, 10), 1, 1);
        tokenC.transfer(address(pm), 1_000_000 ether);
        MockCLPool ca = new MockCLPool(address(tokenC), address(tokenA), 6, 10);
        tokenA.transfer(address(ca), 100_000 ether);
        tokenC.transfer(address(ca), 100_000 ether);
        executor.setRouter(address(ca), ArbitrageExecutor.RouterType.DirectCL);
        executor.executeTriangularArbitrageBorrowed(address(tokenA), address(tokenB), address(tokenC), AMT,
            ab, bc, address(ca), 0, 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 20 ether);
    }

    function test_V4_PartialFill_PaysOnlyWhatWasUsed() public {
        // The pool uses half the input: 50 TKA → 100 TKB → 60 TKA, 50 TKA unspent: +10 TKA.
        address v4 = _abPool(2, 1);
        pm.setInputUse(1, 2);
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, v4, address(clSell), 0, 0, 1 ether, deadline);
        assertEq(tokenA.balanceOf(address(executor)), 10 ether);
        assertEq(pm.debt(address(tokenA)), 0);
    }

    function test_V4_Loss_Reverts() public {
        address v4 = _abPool(1, 1); // 100 TKA → 100 TKB → 60 TKA
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.NotProfitable.selector, 0, 0, 1 ether));
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, v4, address(clSell), 0, 0, 1 ether, deadline);
    }

    function test_V4_Funded_Reverts() public {
        address v4 = _abPool(2, 1);
        tokenA.transfer(address(executor), 1_000 ether);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.V4NeedsBorrow.selector, 1, v4));
        executor.executeArbitrage(address(tokenA), address(tokenB), AMT, v4, address(clSell), 0, 0, 1 ether, deadline);
    }

    function test_V4_WrongPool_Reverts() public {
        address ac = _pool(_key(address(tokenA), address(tokenC), 500, 10), 2, 1);
        vm.expectRevert(abi.encodeWithSelector(
            ArbitrageExecutor.V4WrongPool.selector, 1, ac, address(tokenA), address(tokenB)));
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, ac, address(clSell), 0, 0, 1 ether, deadline);
    }

    function test_V4_Unregistered_Reverts() public {
        address rogue = executor.v4PoolAddress(_key(address(tokenA), address(tokenB), 100, 1));
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, rogue));
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, rogue, address(clSell), 0, 0, 1 ether, deadline);
    }

    function test_V4_Revoked_Reverts() public {
        address v4 = _abPool(2, 1);
        executor.revokeRouter(v4);
        vm.expectRevert(abi.encodeWithSelector(ArbitrageExecutor.RouterNotAllowed.selector, v4));
        executor.executeArbitrageBorrowed(address(tokenA), address(tokenB), AMT, v4, address(clSell), 0, 0, 1 ether, deadline);
    }
}
