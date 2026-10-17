// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/access/Ownable2Step.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import "@openzeppelin/contracts/utils/Pausable.sol";

interface IUniswapV2Router02 {
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

/// @dev Uniswap SwapRouter02 / PancakeSwap V3 SmartRouter (7-field, no deadline in struct).
interface ISwapRouterV3 {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams calldata params)
        external payable returns (uint256 amountOut);
}

/// @dev Aerodrome V2 (Base) — 4-field Route; factory=address(0) → router's default factory.
interface IAerodromeRouter {
    struct Route {
        address from;
        address to;
        bool stable;
        address factory;
    }
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        Route[] calldata routes,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

/**
 * @title ArbitrageExecutor v3
 * @notice Atomic cross-DEX arbitrage: 2-hop (buy on A, sell on B) and triangular
 *         (A → B → C → A). Each trade either ends with at least `minProfit` more of the
 *         start token or reverts — the contract never keeps a loss.
 *
 * Failure reporting — every revert says what went wrong and where:
 *   • a router call that reverts is re-raised as LegFailed(leg, router, type, reason),
 *     with the router's own revert data in `reason` (e.g. "Too little received");
 *   • missing funds → InsufficientBalance; a leg that delivers nothing → NoOutput;
 *   • misconfiguration (unknown / retired router type, V3 fee 0, repeated tokens) has
 *     its own error, checked before any swap.
 *
 * Amounts between legs are measured as balance deltas, not taken from router return
 * values, so a router that misreports its output cannot make the next leg over-spend.
 * Fee-on-transfer tokens are NOT supported as a leg input: real V2/Aerodrome pools
 * revert ("K") and V3 pools revert ("IIA") when the input arrives short.
 * Allowances are exact and cleared after each leg if anything is left.
 */
contract ArbitrageExecutor is Ownable2Step, ReentrancyGuard, Pausable {
    using SafeERC20 for IERC20;

    // ─── Router Types ─────────────────────────────────────────
    /// @dev Ordinals are part of the off-chain interface (setRouter takes a uint8):
    ///      0 = V2, 1 = V3, 4 = Aerodrome. 2 and 3 (Solidly v1, SyncSwap) are retired —
    ///      no Base deployment — and rejected; they keep their slots so 4 stays Aerodrome.
    enum RouterType { V2, V3, RetiredSolidly, RetiredSyncSwap, Aerodrome }

    // ─── State ────────────────────────────────────────────────

    /// @notice Total accumulated profit per token
    mapping(address => uint256) public totalProfit;

    /// @notice Total number of successful trades
    uint256 public totalTrades;

    /// @notice Routers approved to be used in arbitrage calls
    mapping(address => bool) public allowedRouters;

    /// @notice Router protocol type (set together with the allowlist by setRouter)
    mapping(address => RouterType) public routerType;

    // ─── Events ───────────────────────────────────────────────

    event ArbitrageExecuted(
        address indexed tokenIn,
        address indexed tokenOut,
        address routerA,
        address routerB,
        uint256 amountIn,
        uint256 profit
    );
    event TriangularArbitrageExecuted(
        address indexed tokenA,
        address indexed tokenB,
        address indexed tokenC,
        address routerAB,
        address routerBC,
        address routerCA,
        uint256 amountIn,
        uint256 profit
    );
    event WithdrawETH(address indexed to, uint256 amount);
    event WithdrawToken(address indexed token, address indexed to, uint256 amount);
    event RouterApproved(address indexed router, bool approved);
    event RouterTypeSet(address indexed router, RouterType rtype);
    event ApprovalRevoked(address indexed token, address indexed spender);
    /// @notice A leftover allowance could not be cleared (token rejects approve(0) and
    ///         approve(1)); `revokeApproval` it once the token allows.
    event AllowanceNotCleared(address indexed token, address indexed spender, uint256 remaining);

    // ─── Errors ───────────────────────────────────────────────

    /// @notice The trade completed but ended below `balanceBefore + minProfit`.
    error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
    error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
    error ZeroAmount();
    error ZeroAddress();
    error TransferFailed();
    error RouterNotAllowed(address router);
    /// @notice The router's configured type has no swap path (retired type).
    error UnsupportedRouterType(address router, RouterType rtype);
    /// @notice The contract holds less of `token` than the trade needs.
    error InsufficientBalance(address token, uint256 balance, uint256 needed);
    /// @notice The same token appears twice in the route.
    error InvalidPath();
    /// @notice A V3 leg's fee is not a pool fee tier (hundredths of a bip, 100..999999 —
    ///         e.g. 30 passed as bps instead of 3000).
    error InvalidV3Fee(uint8 leg, address router, uint24 fee);
    /// @notice A router call reverted. `reason` is the router's own revert data.
    error LegFailed(uint8 leg, address router, RouterType rtype, bytes reason);
    /// @notice A leg succeeded but the contract received none of its output token.
    error NoOutput(uint8 leg, address router);
    /// @notice setRouter was given an address with no code (EOA or typo).
    error NotAContract(address router);
    /// @notice A router call used (nearly) all the gas forwarded to it: out of gas — raise
    ///         the tx gas limit — or, if it persists at higher limits, an invalid opcode /
    ///         failed assert inside the router or pool (the EVM can't tell the two apart).
    ///         `reason` is the router's own revert data: empty, or Uniswap TransferHelper's
    ///         "TF"/"STF"/"SA", which is how it re-raises an inner out-of-gas.
    error LegExhaustedGas(uint8 leg, address router, bytes reason);
    /// @notice Ownership can be transferred but not renounced: every withdrawal is onlyOwner.
    error RenounceDisabled();

    // ─── Constructor ──────────────────────────────────────────

    /// @dev Ownable2Step: deployer is initial owner; transfer needs acceptOwnership().
    constructor() Ownable(msg.sender) {}

    /// @dev Renouncing would lock every token in the contract (all withdrawals are onlyOwner).
    function renounceOwnership() public view override onlyOwner {
        revert RenounceDisabled();
    }

    // ─── Router configuration ─────────────────────────────────

    /// @notice Allow a router with its type: 0 = V2, 1 = V3 (SwapRouter02), 4 = Aerodrome
    ///         (2 and 3 are retired). The only way to allow a router, so none is ever
    ///         allowed with a forgotten type. Call again to change the type.
    function setRouter(address router, RouterType rtype) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        if (router.code.length == 0) revert NotAContract(router);
        if (!_isSupported(rtype)) revert UnsupportedRouterType(router, rtype);
        routerType[router] = rtype;
        allowedRouters[router] = true;
        emit RouterTypeSet(router, rtype);
        emit RouterApproved(router, true);
    }

    /// @notice Stop using a router.
    function revokeRouter(address router) external onlyOwner {
        allowedRouters[router] = false;
        emit RouterApproved(router, false);
    }

    function _isSupported(RouterType rtype) internal pure returns (bool) {
        return rtype == RouterType.V2 || rtype == RouterType.V3 || rtype == RouterType.Aerodrome;
    }

    // ─── Receive ETH ──────────────────────────────────────────

    receive() external payable {}

    modifier checkDeadline(uint256 deadline) {
        if (block.timestamp > deadline) revert DeadlineExpired(deadline, block.timestamp);
        _;
    }

    // ═══════════════════════════════════════════════════════════
    // 2-hop arbitrage
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Buy `tokenOut` with `tokenIn` on routerA, sell it back on routerB.
     * @param feeA/feeB  V2: ignored. V3: fee tier (100/500/2500/3000/10000).
     *                   Aerodrome: 0 = volatile pool, non-zero = stable pool.
     * @param minProfit  Minimum gain in tokenIn. Enforced by the contract on the final
     *                   balance (NotProfitable), not by the routers: they get a zero
     *                   min-out, so slippage reads the same on every DEX (PancakeSwap's
     *                   router reverts without a reason). The tx is atomic, so this is
     *                   as safe as router-level protection.
     */
    function executeArbitrage(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB,
        uint24 feeA,
        uint24 feeB,
        uint256 minProfit,
        uint256 deadline
    ) external onlyOwner whenNotPaused nonReentrant checkDeadline(deadline) {
        if (amountIn == 0) revert ZeroAmount();
        if (tokenIn == address(0) || tokenOut == address(0)) revert ZeroAddress();
        if (tokenIn == tokenOut) revert InvalidPath();
        RouterType typeA = _checkRouter(routerA);
        RouterType typeB = _checkRouter(routerB);
        uint256 balanceBefore = _checkBalance(tokenIn, amountIn);

        (uint256 mid,) = _swap(1, routerA, typeA, tokenIn, tokenOut, amountIn, feeA, deadline);
        (, uint256 balanceAfter) = _swap(2, routerB, typeB, tokenOut, tokenIn, mid, feeB, deadline);

        uint256 profit = _settle(tokenIn, balanceBefore, balanceAfter, minProfit);
        emit ArbitrageExecuted(tokenIn, tokenOut, routerA, routerB, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // Triangular arbitrage (A → B → C → A)
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice A → B on routerAB, B → C on routerBC, C → A on routerCA.
     *         Fees follow the same convention as executeArbitrage.
     */
    function executeTriangularArbitrage(
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
    ) external onlyOwner whenNotPaused nonReentrant checkDeadline(deadline) {
        if (amountIn == 0) revert ZeroAmount();
        if (tokenA == address(0) || tokenB == address(0) || tokenC == address(0)) revert ZeroAddress();
        if (tokenA == tokenB || tokenB == tokenC || tokenA == tokenC) revert InvalidPath();
        RouterType typeAB = _checkRouter(routerAB);
        RouterType typeBC = _checkRouter(routerBC);
        RouterType typeCA = _checkRouter(routerCA);
        uint256 balanceBefore = _checkBalance(tokenA, amountIn);

        (uint256 amountB,) = _swap(1, routerAB, typeAB, tokenA, tokenB, amountIn, feeAB, deadline);
        (uint256 amountC,) = _swap(2, routerBC, typeBC, tokenB, tokenC, amountB, feeBC, deadline);
        (, uint256 balanceAfter) = _swap(3, routerCA, typeCA, tokenC, tokenA, amountC, feeCA, deadline);

        uint256 profit = _settle(tokenA, balanceBefore, balanceAfter, minProfit);
        emit TriangularArbitrageExecuted(tokenA, tokenB, tokenC, routerAB, routerBC, routerCA, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // Internal
    // ═══════════════════════════════════════════════════════════

    /// @dev setRouter is the only way in and rejects retired types, so an allowed
    ///      router's type is always supported.
    function _checkRouter(address router) internal view returns (RouterType) {
        if (!allowedRouters[router]) revert RouterNotAllowed(router);
        return routerType[router];
    }

    /// @dev Returns the current balance (the "before" for the profit check).
    function _checkBalance(address token, uint256 needed) internal view returns (uint256 balance) {
        balance = IERC20(token).balanceOf(address(this));
        if (balance < needed) revert InsufficientBalance(token, balance, needed);
    }

    /// @dev `balanceAfter` is the final leg's output balance (the start token), read by
    ///      _swap — passed in so a trade doesn't pay for the same balanceOf twice.
    function _settle(address token, uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit)
        internal returns (uint256 profit)
    {
        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }
        unchecked { profit = balanceAfter - balanceBefore; }
        totalProfit[token] += profit;
        totalTrades++;
    }

    /**
     * @dev One swap leg on any supported router type. Returns the amount of `tokenOut`
     *      actually received (balance delta) and the resulting `tokenOut` balance (used by
     *      the final leg's caller as the settle balance). Router reverts become LegFailed.
     *      Min-out is 0: profit is enforced once, in _settle.
     */
    function _swap(
        uint8 leg,
        address router,
        RouterType rtype,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint24 fee,
        uint256 deadline
    ) internal returns (uint256 received, uint256 outAfter) {
        // Range check, not a tier list: tiers differ per DEX (Pancake 2500) and can be
        // added by governance; this catches 0 and bps-style values like 30.
        if (rtype == RouterType.V3 && (fee < 100 || fee >= 1_000_000)) revert InvalidV3Fee(leg, router, fee);

        uint256 outBefore = IERC20(tokenOut).balanceOf(address(this));
        IERC20(tokenIn).forceApprove(router, amountIn);

        // Low-level call: the router's return value is never decoded (we measure the
        // balance delta), so a router returning nothing or garbage can't revert here
        // without a LegFailed/NoOutput that names the leg.
        bytes memory data;
        if (rtype == RouterType.V3) {
            data = abi.encodeCall(ISwapRouterV3.exactInputSingle, (ISwapRouterV3.ExactInputSingleParams({
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                fee: fee,
                recipient: address(this),
                amountIn: amountIn,
                amountOutMinimum: 0,
                sqrtPriceLimitX96: 0
            })));
        } else if (rtype == RouterType.Aerodrome) {
            IAerodromeRouter.Route[] memory routes = new IAerodromeRouter.Route[](1);
            routes[0] = IAerodromeRouter.Route({ from: tokenIn, to: tokenOut, stable: fee != 0, factory: address(0) });
            data = abi.encodeCall(IAerodromeRouter.swapExactTokensForTokens,
                (amountIn, 0, routes, address(this), deadline));
        } else {
            // V2 (allowed routers only ever have a supported type)
            address[] memory path = new address[](2);
            path[0] = tokenIn;
            path[1] = tokenOut;
            data = abi.encodeCall(IUniswapV2Router02.swapExactTokensForTokens,
                (amountIn, 0, path, address(this), deadline));
        }
        uint256 gasBefore = gasleft();
        (bool ok, bytes memory reason) = router.call(data);
        if (!ok) {
            // EIP-150: every call level keeps 1/64 of its gas, so a router that burned all it
            // was given (directly or several calls deep) leaves only a few percent here, while
            // a missing pool / wrong type reverts early with most of it left. Applied only to
            // reasons that can hide an out-of-gas: none at all, or Uniswap TransferHelper's
            // "TF"/"STF"/"SA" re-raise of a failed inner call. A specific reason (K(),
            // InsufficientLiquidity, …) is the router's real answer even when it came late
            // in an expensive swap, and stays LegFailed. Out of gas and an INVALID opcode both
            // burn everything — hence the error's name. At very tight limits even this revert
            // can't be paid for: the tx then has no data and the engine reads the receipt.
            if (gasleft() < gasBefore / 8 && _mayHideOutOfGas(reason)) {
                revert LegExhaustedGas(leg, router, reason);
            }
            revert LegFailed(leg, router, rtype, reason);
        }

        _clearAllowance(tokenIn, router);
        outAfter = IERC20(tokenOut).balanceOf(address(this));
        // `<=` rather than a checked subtraction: a balance that shrank (rebasing token,
        // router pulling tokenOut) must still name the leg, not panic.
        if (outAfter <= outBefore) revert NoOutput(leg, router);
        unchecked { received = outAfter - outBefore; }
    }

    /// @dev True for revert data an out-of-gas inner call can turn into: empty, or Uniswap
    ///      TransferHelper's Error("TF" | "STF" | "SA"). Failure path only — gas is no concern.
    function _mayHideOutOfGas(bytes memory reason) internal pure returns (bool) {
        if (reason.length == 0) return true;
        bytes32 h = keccak256(reason);
        return h == keccak256(abi.encodeWithSignature("Error(string)", "TF"))
            || h == keccak256(abi.encodeWithSignature("Error(string)", "STF"))
            || h == keccak256(abi.encodeWithSignature("Error(string)", "SA"));
    }

    /// @dev Routers normally pull exactly the approved amount; only write when they didn't.
    ///      Best effort: a token that rejects approve(0) must not abort a finished trade,
    ///      so it falls back to a 1-wei allowance (to a whitelisted router).
    function _clearAllowance(address token, address spender) internal {
        if (IERC20(token).allowance(address(this), spender) == 0 || _tryApprove(token, spender, 0)) return;
        if (!_tryApprove(token, spender, 1)) {
            emit AllowanceNotCleared(token, spender, IERC20(token).allowance(address(this), spender));
        }
    }

    /// @dev approve() that reports failure instead of reverting: false for a revert, a
    ///      `false` / malformed return value, or an address with no code (a call there
    ///      "succeeds" with no data). The code check is warm (~100 gas) on the trade path.
    function _tryApprove(address token, address spender, uint256 value) internal returns (bool) {
        if (token.code.length == 0) return false;
        (bool ok, bytes memory ret) = token.call(abi.encodeCall(IERC20.approve, (spender, value)));
        if (!ok) return false;
        if (ret.length == 0) return true; // USDT-style: no return value
        // Read the word instead of abi.decode(bool), which reverts on anything but 0/1.
        return ret.length >= 32 && abi.decode(ret, (uint256)) == 1;
    }

    // ═══════════════════════════════════════════════════════════
    // Views
    // ═══════════════════════════════════════════════════════════

    /// @notice Accumulated stats for a token.
    function getStats(address token) external view returns (
        uint256 _totalProfit,
        uint256 _totalTrades,
        uint256 _contractBalance
    ) {
        _totalProfit = totalProfit[token];
        _totalTrades = totalTrades;
        _contractBalance = IERC20(token).balanceOf(address(this));
    }

    // ═══════════════════════════════════════════════════════════
    // Pause / funds / emergency
    // ═══════════════════════════════════════════════════════════

    /// @notice Freeze all trade execution. Withdrawals still work.
    function pause() external onlyOwner { _pause(); }

    /// @notice Resume trade execution.
    function unpause() external onlyOwner { _unpause(); }

    /// @notice Withdraw all native ETH. Always works (even when paused).
    function withdrawETH() external onlyOwner nonReentrant {
        uint256 balance = address(this).balance;
        if (balance == 0) revert ZeroAmount();
        (bool success, ) = payable(owner()).call{value: balance}("");
        if (!success) revert TransferFailed();
        emit WithdrawETH(owner(), balance);
    }

    /// @notice Withdraw full balance of an ERC20. Always works (even when paused).
    function withdrawToken(address _token) external onlyOwner {
        if (_token == address(0)) revert ZeroAddress();
        uint256 balance = IERC20(_token).balanceOf(address(this));
        if (balance == 0) revert ZeroAmount();
        IERC20(_token).safeTransfer(owner(), balance);
        emit WithdrawToken(_token, owner(), balance);
    }

    /// @notice Withdraw a specific amount of an ERC20. Always works (even when paused).
    function withdrawTokenAmount(address _token, uint256 _amount) external onlyOwner {
        if (_token == address(0)) revert ZeroAddress();
        if (_amount == 0) revert ZeroAmount();
        IERC20(_token).safeTransfer(owner(), _amount);
        emit WithdrawToken(_token, owner(), _amount);
    }

    /// @notice Zero out an allowance this contract granted (1 wei for tokens that reject
    ///         approve(0)).
    function revokeApproval(address _token, address _spender) external onlyOwner {
        if (!_tryApprove(_token, _spender, 0) && !_tryApprove(_token, _spender, 1)) {
            revert SafeERC20.SafeERC20FailedOperation(_token);
        }
        emit ApprovalRevoked(_token, _spender);
    }
}
