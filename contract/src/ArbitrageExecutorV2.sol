// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @dev Concentrated-liquidity pool, called directly: Uniswap V3, PancakeSwap V3, Aerodrome
///      Slipstream (both factories) and Aero CL share this `swap` and pay through
///      `uniswapV3SwapCallback` (PancakeSwap: `pancakeV3SwapCallback`).
interface ICLPoolV2 {
    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data)
        external returns (int256 amount0, int256 amount1);
}

/// @dev Uniswap V2-style pair, called directly: Uniswap V2 forks, Aerodrome V2 and Aero V2.
///      Solidly-family pools (Aerodrome / Aero) also quote their own curve and fee.
interface IPairV2 {
    function getReserves() external view returns (uint256 reserve0, uint256 reserve1);
    function getAmountOut(uint256 amountIn, address tokenIn) external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

/// @dev Uniswap V4 PoolManager flash accounting (see ArbitrageExecutor v4).
interface IPoolManagerV4b {
    struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct SwapParams { bool zeroForOne; int256 amountSpecified; uint160 sqrtPriceLimitX96; }

    function unlock(bytes calldata data) external returns (bytes memory);
    function take(address currency, address to, uint256 amount) external;
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    function swap(PoolKey memory key, SwapParams memory params, bytes calldata hookData) external returns (int256);
}

interface IWETHV2 {
    function deposit() external payable;
    function withdraw(uint256 amount) external;
}

/**
 * @title ArbitrageExecutor v2 (lean)
 * @notice Borrow-only atomic arbitrage on pools called directly: 2-hop and triangular, the
 *         start amount taken from the Uniswap V4 PoolManager and repaid in the same tx. Each
 *         trade ends with at least `minProfit` more of the start token or reverts.
 *
 * Differences from v1 (ArbitrageExecutor.sol), all for gas:
 *   • no routers, no approvals: every leg is a pool (DirectCL, DirectPair) or a V4 pool;
 *   • the leg type travels in the top 4 bits of each leg's `fee` (6 DirectCL, 7 DirectPair,
 *     8 V4), so there is no allowlist to read; the low 20 bits are the DirectPair fee in bps
 *     (0 = the pool quotes itself). V4 PoolKeys stay registered (setV4Pools);
 *   • owner and PoolManager are immutable; no pause, no events, no profit counters;
 *   • flags live in transient storage; amounts between legs come from the pools' own
 *     return values / quotes instead of balance reads. Only the start token's balance is read,
 *     before and after the trade, and that check covers any leg that delivered less.
 * Safety does not depend on an allowlist: only the owner can trade, a callback pays only the
 * pool being swapped and at most that leg's input, and the final balance check reverts any
 * trade that lost value (a fake "pool" can only make the trade revert).
 * Fee-on-transfer tokens are not supported as a leg input (the next leg reverts short).
 */
contract ArbitrageExecutorV2 {
    using SafeERC20 for IERC20;

    /// @notice Interface version; the engine refuses to trade v2 calldata against anything else.
    uint8 public constant version = 2;
    /// @notice Kept for the engine's startup check: v2 has no pause (stop the engine instead).
    bool public constant paused = false;

    /// @notice The only account that can trade and withdraw. Fixed at deploy (redeploy to change).
    address public immutable owner;
    /// @notice Uniswap V4 PoolManager every trade borrows from.
    address public immutable poolManager;

    /// @dev Leg types (top 4 bits of a leg's fee) — the v1 RouterType ordinals.
    uint8 internal constant DIRECT_CL = 6;
    uint8 internal constant DIRECT_PAIR = 7;
    uint8 internal constant V4 = 8;
    uint256 internal constant KIND_SHIFT = 20;
    uint24 internal constant PARAM_MASK = 0xFFFFF;

    address internal constant WETH = 0x4200000000000000000000000000000000000006;
    uint160 internal constant MIN_SQRT_RATIO_PLUS_ONE = 4295128740;
    uint160 internal constant MAX_SQRT_RATIO_MINUS_ONE = 1461446703485210103287273052203988822378723970341;

    /// @dev Transient slots: inside a trade (unlock running), the pool a CL callback may come
    ///      from, and the most that callback may be paid.
    uint256 internal constant T_ACTIVE = 0;
    uint256 internal constant T_POOL = 1;
    uint256 internal constant T_MAX_IN = 2;

    /// @notice PoolKey of each registered V4 leg address (see setV4Pools).
    mapping(address => IPoolManagerV4b.PoolKey) public v4Pools;

    struct Trade {
        address[3] tokens;
        address[3] pools;
        uint24[3] fees;
        uint8 n;
        uint256 amountIn;
        uint256 minProfit;
    }

    // ─── Errors (names and arguments shared with v1, decoded by the engine) ───
    error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
    error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
    error ZeroAmount();
    error ZeroAddress();
    error TransferFailed();
    error InvalidPath();
    error UnsupportedRouterType(address router, uint8 rtype);
    error LegFailed(uint8 leg, address router, uint8 rtype, bytes reason);
    error NoOutput(uint8 leg, address router);
    error LegExhaustedGas(uint8 leg, address router, bytes reason);
    error UnexpectedCallback(address caller);
    error InvalidPairFee(uint8 leg, address pool, uint24 fee);
    error InvalidV4Key(address pool);
    error V4WrongPool(uint8 leg, address pool, address tokenIn, address tokenOut);
    error OwnableUnauthorizedAccount(address account);
    error Reentrant();

    constructor(address poolManager_) {
        if (poolManager_ == address(0)) revert ZeroAddress();
        owner = msg.sender;
        poolManager = poolManager_;
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert OwnableUnauthorizedAccount(msg.sender);
        _;
    }

    receive() external payable {}

    // ═══════════════════════════════════════════════════════════
    // Views kept for the engine's startup checks
    // ═══════════════════════════════════════════════════════════

    /// @notice True only for registered V4 pools: other legs need no registration in v2.
    function allowedRouters(address pool) external view returns (bool) {
        return v4Pools[pool].currency1 != address(0);
    }

    /// @notice 8 (V4) for registered V4 pools, else 0.
    function routerType(address pool) external view returns (uint8) {
        return v4Pools[pool].currency1 != address(0) ? V4 : 0;
    }

    /// @notice The leg address a V4 PoolKey is registered under (low 160 bits of its PoolId).
    function v4PoolAddress(IPoolManagerV4b.PoolKey memory key) public pure returns (address) {
        return address(uint160(uint256(keccak256(abi.encode(key)))));
    }

    // ═══════════════════════════════════════════════════════════
    // Admin
    // ═══════════════════════════════════════════════════════════

    /// @notice Register V4 pools (hookless, static fee, ordered, not ETH/WETH).
    function setV4Pools(IPoolManagerV4b.PoolKey[] calldata keys) external onlyOwner {
        for (uint256 i = 0; i < keys.length; i++) {
            IPoolManagerV4b.PoolKey calldata k = keys[i];
            address pool = v4PoolAddress(k);
            bool ethWeth = k.currency0 == address(0) && k.currency1 == WETH;
            if (k.hooks != address(0) || k.fee >= 1_000_000 || k.tickSpacing <= 0
                || k.currency0 >= k.currency1 || k.currency1 == address(0) || ethWeth) revert InvalidV4Key(pool);
            v4Pools[pool] = k;
        }
    }

    function withdrawToken(address token) external onlyOwner {
        uint256 balance = IERC20(token).balanceOf(address(this));
        if (balance == 0) revert ZeroAmount();
        IERC20(token).safeTransfer(owner, balance);
    }

    function withdrawETH() external onlyOwner {
        uint256 balance = address(this).balance;
        if (balance == 0) revert ZeroAmount();
        (bool ok, ) = payable(owner).call{value: balance}("");
        if (!ok) revert TransferFailed();
    }

    // ═══════════════════════════════════════════════════════════
    // Trades (same signatures as v1's *Borrowed entry points)
    // ═══════════════════════════════════════════════════════════

    /// @notice Buy `tokenOut` with `tokenIn` on poolA, sell it back on poolB, with `amountIn`
    ///         borrowed from the V4 PoolManager. `feeA` / `feeB`: leg type << 20 | param.
    function executeArbitrageBorrowed(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB,
        uint24 feeA,
        uint24 feeB,
        uint256 minProfit,
        uint256 deadline
    ) external onlyOwner {
        if (block.timestamp > deadline) revert DeadlineExpired(deadline, block.timestamp);
        if (amountIn == 0) revert ZeroAmount();
        if (tokenIn == address(0) || tokenOut == address(0)) revert ZeroAddress();
        if (tokenIn == tokenOut) revert InvalidPath();
        Trade memory t;
        t.n = 2;
        t.tokens[0] = tokenIn; t.tokens[1] = tokenOut;
        t.pools[0] = routerA; t.pools[1] = routerB;
        t.fees[0] = feeA; t.fees[1] = feeB;
        t.amountIn = amountIn; t.minProfit = minProfit;
        _run(t);
    }

    /// @notice A → B on poolAB, B → C on poolBC, C → A on poolCA, borrowed like the 2-hop.
    function executeTriangularArbitrageBorrowed(
        address tokenA,
        address tokenB,
        address tokenC,
        uint256 amountIn,
        address routerAB,
        address routerBC,
        address routerCA,
        uint24 feeAB,
        uint24 feeBC,
        uint24 feeCA,
        uint256 minProfit,
        uint256 deadline
    ) external onlyOwner {
        if (block.timestamp > deadline) revert DeadlineExpired(deadline, block.timestamp);
        if (amountIn == 0) revert ZeroAmount();
        if (tokenA == address(0) || tokenB == address(0) || tokenC == address(0)) revert ZeroAddress();
        if (tokenA == tokenB || tokenB == tokenC || tokenA == tokenC) revert InvalidPath();
        Trade memory t;
        t.n = 3;
        t.tokens[0] = tokenA; t.tokens[1] = tokenB; t.tokens[2] = tokenC;
        t.pools[0] = routerAB; t.pools[1] = routerBC; t.pools[2] = routerCA;
        t.fees[0] = feeAB; t.fees[1] = feeBC; t.fees[2] = feeCA;
        t.amountIn = amountIn; t.minProfit = minProfit;
        _run(t);
    }

    function _run(Trade memory t) internal {
        if (_tload(T_ACTIVE) != 0) revert Reentrant();
        _tstore(T_ACTIVE, 1);
        IPoolManagerV4b(poolManager).unlock(abi.encode(t));
        _tstore(T_ACTIVE, 0);
    }

    // ═══════════════════════════════════════════════════════════
    // Callbacks
    // ═══════════════════════════════════════════════════════════

    /// @notice PoolManager callback inside `unlock`: take the start amount, run the legs,
    ///         check the result, repay exactly what was taken.
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != poolManager || _tload(T_ACTIVE) == 0) revert UnexpectedCallback(msg.sender);
        Trade memory t = abi.decode(data, (Trade));
        address token = t.tokens[0];
        IPoolManagerV4b pm = IPoolManagerV4b(poolManager);

        uint256 balanceBefore = IERC20(token).balanceOf(address(this));
        pm.take(token, address(this), t.amountIn);
        _runLegs(t);
        uint256 balanceAfter = IERC20(token).balanceOf(address(this));
        // Same numbers as v1: net = balance after the legs minus the loan.
        uint256 net = balanceAfter > t.amountIn ? balanceAfter - t.amountIn : 0;
        if (net < balanceBefore + t.minProfit) revert NotProfitable(balanceBefore, net, t.minProfit);

        pm.sync(token);
        IERC20(token).safeTransfer(poolManager, t.amountIn);
        pm.settle();
        return "";
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _payPool(amount0Delta, amount1Delta, data);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _payPool(amount0Delta, amount1Delta, data);
    }

    /// @dev Only the pool being swapped on right now, and at most that leg's input.
    function _payPool(int256 amount0Delta, int256 amount1Delta, bytes calldata data) internal {
        address pool = address(uint160(_tload(T_POOL)));
        if (msg.sender != pool || pool == address(0)) revert UnexpectedCallback(msg.sender);
        uint256 owed = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        if (owed > _tload(T_MAX_IN)) revert UnexpectedCallback(msg.sender);
        IERC20(abi.decode(data, (address))).safeTransfer(msg.sender, owed);
    }

    // ═══════════════════════════════════════════════════════════
    // Legs
    // ═══════════════════════════════════════════════════════════

    function _runLegs(Trade memory t) internal {
        uint256 amount = t.amountIn;
        for (uint8 i = 0; i < t.n; i++) {
            address tokenIn = t.tokens[i];
            address tokenOut = t.tokens[(i + 1) % t.n];
            address pool = t.pools[i];
            uint8 kind = uint8(t.fees[i] >> KIND_SHIFT);
            uint8 leg = i + 1;
            if (kind == DIRECT_CL) {
                amount = _directCL(leg, pool, tokenIn, tokenOut, amount);
            } else if (kind == DIRECT_PAIR) {
                amount = _directPair(leg, pool, tokenIn, tokenOut, amount, t.fees[i] & PARAM_MASK);
            } else if (kind == V4) {
                amount = _v4Swap(leg, pool, tokenIn, tokenOut, amount);
            } else {
                revert UnsupportedRouterType(pool, kind);
            }
        }
    }

    /// @dev Exact-input swap on a CL pool (token0 < token1 on every supported factory); the
    ///      pool pulls its input through _payPool. Returns the output the pool reports.
    function _directCL(uint8 leg, address pool, address tokenIn, address tokenOut, uint256 amountIn)
        internal returns (uint256 out)
    {
        if (amountIn > uint256(type(int256).max)) revert ZeroAmount();
        bool zeroForOne = tokenIn < tokenOut;
        _tstore(T_POOL, uint256(uint160(pool)));
        _tstore(T_MAX_IN, amountIn);
        bytes memory ret = _call(leg, pool, DIRECT_CL, abi.encodeCall(ICLPoolV2.swap, (
            address(this), zeroForOne, int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE,
            abi.encode(tokenIn)
        )));
        _tstore(T_POOL, 0);
        (int256 a0, int256 a1) = abi.decode(ret, (int256, int256));
        int256 o = zeroForOne ? a1 : a0;
        if (o >= 0) revert NoOutput(leg, pool);
        out = uint256(-o);
    }

    /// @dev Pay a V2-style pair first, then ask for its output. `fee` 0: the pool's own
    ///      getAmountOut (Aerodrome / Aero); else the Uniswap V2 formula with `fee` bps.
    function _directPair(uint8 leg, address pair, address tokenIn, address tokenOut, uint256 amountIn, uint24 fee)
        internal returns (uint256 out)
    {
        if (fee >= 10_000) revert InvalidPairFee(leg, pair, fee);
        bool inIs0 = tokenIn < tokenOut;
        if (fee == 0) {
            out = IPairV2(pair).getAmountOut(amountIn, tokenIn);
        } else {
            (uint256 r0, uint256 r1) = IPairV2(pair).getReserves();
            (uint256 rIn, uint256 rOut) = inIs0 ? (r0, r1) : (r1, r0);
            uint256 inWithFee = amountIn * (10_000 - fee);
            out = (inWithFee * rOut) / (rIn * 10_000 + inWithFee);
        }
        if (out == 0) revert NoOutput(leg, pair);
        IERC20(tokenIn).safeTransfer(pair, amountIn);
        _call(leg, pair, DIRECT_PAIR, abi.encodeCall(IPairV2.swap, (
            inIs0 ? 0 : out, inIs0 ? out : 0, address(this), bytes("")
        )));
    }

    /// @dev Exact-input swap on a registered V4 pool inside the open unlock: swap, pay the
    ///      input (native ETH: unwrap WETH, settle with value), take the output (native: wrap).
    function _v4Swap(uint8 leg, address pool, address tokenIn, address tokenOut, uint256 amountIn)
        internal returns (uint256 out)
    {
        if (amountIn > uint256(uint128(type(int128).max))) revert ZeroAmount();
        IPoolManagerV4b.PoolKey memory key = v4Pools[pool];
        if (key.currency1 == address(0)) revert UnsupportedRouterType(pool, V4);
        bool native = key.currency0 == address(0);
        address cIn = native && tokenIn == WETH ? address(0) : tokenIn;
        address cOut = native && tokenOut == WETH ? address(0) : tokenOut;
        bool zeroForOne = cIn == key.currency0;
        if (!(zeroForOne ? cOut == key.currency1 : (cIn == key.currency1 && cOut == key.currency0))) {
            revert V4WrongPool(leg, pool, tokenIn, tokenOut);
        }
        IPoolManagerV4b pm = IPoolManagerV4b(poolManager);
        bytes memory data = abi.encodeCall(IPoolManagerV4b.swap, (key, IPoolManagerV4b.SwapParams({
            zeroForOne: zeroForOne,
            amountSpecified: -int256(amountIn),
            sqrtPriceLimitX96: zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE
        }), bytes("")));
        (bool ok, bytes memory ret) = poolManager.call(data);
        if (!ok) revert LegFailed(leg, pool, V4, ret);
        int256 delta = abi.decode(ret, (int256));
        int128 d0 = int128(delta >> 128);
        int128 d1 = int128(delta);
        (int128 inDelta, int128 outDelta) = zeroForOne ? (d0, d1) : (d1, d0);
        if (outDelta <= 0) revert NoOutput(leg, pool);

        // Pay what the swap used (all of amountIn unless the pool ran out of liquidity).
        uint256 paid = inDelta < 0 ? uint256(uint128(-inDelta)) : 0;
        if (paid > amountIn) revert UnexpectedCallback(pool);
        if (cIn == address(0)) {
            IWETHV2(WETH).withdraw(paid);
            pm.settle{value: paid}();
        } else {
            pm.sync(cIn);
            IERC20(cIn).safeTransfer(poolManager, paid);
            pm.settle();
        }
        out = uint256(uint128(outDelta));
        pm.take(cOut, address(this), out);
        if (cOut == address(0)) IWETHV2(WETH).deposit{value: out}();
    }

    /// @dev Call a pool; a revert becomes LegFailed (or LegExhaustedGas when it burned
    ///      nearly all its gas with a reason that can hide an out-of-gas — see v1 `_call`).
    function _call(uint8 leg, address pool, uint8 kind, bytes memory data) internal returns (bytes memory ret) {
        uint256 gasBefore = gasleft();
        bool ok;
        (ok, ret) = pool.call(data);
        if (!ok) {
            if (gasleft() < gasBefore / 8 && _mayHideOutOfGas(ret)) revert LegExhaustedGas(leg, pool, ret);
            revert LegFailed(leg, pool, kind, ret);
        }
    }

    function _mayHideOutOfGas(bytes memory reason) internal pure returns (bool) {
        if (reason.length == 0) return true;
        bytes32 h = keccak256(reason);
        return h == keccak256(abi.encodeWithSignature("Error(string)", "TF"))
            || h == keccak256(abi.encodeWithSignature("Error(string)", "STF"))
            || h == keccak256(abi.encodeWithSignature("Error(string)", "SA"));
    }

    function _tload(uint256 slot) internal view returns (uint256 v) {
        assembly { v := tload(slot) }
    }

    function _tstore(uint256 slot, uint256 v) internal {
        assembly { tstore(slot, v) }
    }
}
