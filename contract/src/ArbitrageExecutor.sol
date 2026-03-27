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

    function getAmountsOut(
        uint256 amountIn,
        address[] calldata path
    ) external view returns (uint256[] memory amounts);

    function WETH() external pure returns (address);
}

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
        external returns (uint256 amountOut);
}

interface ISolidlyRouter {
    struct Route {
        address from;
        address to;
        bool stable;
    }
    function getAmountsOut(uint256 amountIn, Route[] calldata routes)
        external view returns (uint256[] memory amounts);
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        Route[] calldata routes,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts);
}

// Aerodrome V2 (Base) — uses 4-field Route including factory address.
// factory=address(0) → router uses its internal default factory.
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

// SyncSwap — pools are called directly (transfer-then-swap, no approve pattern)
interface ISyncSwapPool {
    // data = abi.encode(tokenIn, recipient, withdrawMode)
    // withdrawMode: 1 = ERC20 transfer, 2 = native unwrap
    function swap(
        bytes calldata data,
        address sender,
        address callback,
        bytes calldata callbackData
    ) external returns (uint256 amountOut);
}

interface ISyncSwapFactory {
    function getPool(address tokenA, address tokenB) external view returns (address pool);
}

/**
 * @title ArbitrageExecutor v2
 * @notice Atomic cross-DEX arbitrage executor with V2 and V3 support.
 *
 * Improvements over v1:
 *   1. Caller-supplied deadline — stale txns expire instead of being held
 *   2. minProfit parameter — ensures profit covers gas, not just 1 wei
 *   3. ReentrancyGuard — protects against malicious token callbacks
 *   4. Exact allowance hygiene — approve exact, reset to 0 after each swap
 *   5. Ownable2Step — typo-proof ownership transfer
 *   6. batchExecute — multiple arbs in one transaction
 *   7. On-chain profit accumulator — track total profits without parsing logs
 *   8. Pausable — freeze execution without withdrawing
 *   9. V3 support — dispatch to Uniswap V3 exactInputSingle when routerType is V3
 */
contract ArbitrageExecutor is Ownable2Step, ReentrancyGuard, Pausable {
    using SafeERC20 for IERC20;

    // ─── Router Types ─────────────────────────────────────────

    enum RouterType { V2, V3, Solidly, SyncSwap, Aerodrome }

    // ─── State ────────────────────────────────────────────────

    /// @notice Total accumulated profit per token (improvement #7)
    mapping(address => uint256) public totalProfit;

    /// @notice Total number of successful trades
    uint256 public totalTrades;

    /// @notice Routers approved to be used in arbitrage calls
    mapping(address => bool) public allowedRouters;

    /// @notice Router protocol type (V2 or V3) — defaults to V2 if not set
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

    event BatchExecuted(uint256 attempted, uint256 succeeded, uint256 totalBatchProfit);
    event WithdrawETH(address indexed to, uint256 amount);
    event WithdrawToken(address indexed token, address indexed to, uint256 amount);
    event RouterApproved(address indexed router, bool approved);
    event RouterTypeSet(address indexed router, RouterType rtype);
    event ApprovalRevoked(address indexed token, address indexed spender);

    // ─── Errors ───────────────────────────────────────────────

    error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
    error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
    error ZeroAmount();
    error ZeroAddress();
    error TransferFailed();
    error EmptyBatch();
    error RouterNotAllowed(address router);
    error InsufficientOutput(uint256 got, uint256 min);
    error PoolNotFound(address factory);

    // ─── Constructor ──────────────────────────────────────────

    /// @dev Ownable2Step (improvement #5): deployer is initial owner,
    ///      transfer requires new owner to call acceptOwnership()
    constructor() Ownable(msg.sender) {}

    // ─── Router Allowlist ─────────────────────────────────────

    /**
     * @notice Approve or revoke a router for use in arbitrage.
     *         Defaults to V2 router type.
     */
    function setAllowedRouter(address router, bool approved) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        allowedRouters[router] = approved;
        emit RouterApproved(router, approved);
    }

    /**
     * @notice Set the protocol type for an already-approved router.
     *         Call setAllowedRouter first, then setRouterType if not V2.
     *         RouterType: 0=V2, 1=V3, 2=Solidly
     */
    function setRouterType(address router, RouterType rtype) external onlyOwner {
        if (router == address(0)) revert ZeroAddress();
        routerType[router] = rtype;
        emit RouterTypeSet(router, rtype);
    }

    // ─── Receive ETH ──────────────────────────────────────────

    receive() external payable {}

    // ─── Modifiers ────────────────────────────────────────────

    /// @dev Improvement #1: reject transactions past their deadline
    modifier checkDeadline(uint256 deadline) {
        if (block.timestamp > deadline) {
            revert DeadlineExpired(deadline, block.timestamp);
        }
        _;
    }

    // ═══════════════════════════════════════════════════════════
    // CORE: Single Arbitrage Execution
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Execute an atomic arbitrage: buy tokenOut on routerA,
     *         sell tokenOut on routerB, profit in tokenIn.
     *
     * Works with V2, V3, and Solidly routers — dispatch is automatic based on
     * the routerType mapping set via setRouterType().
     *
     * @param tokenIn    Base token we start and end with
     * @param tokenOut   Intermediate token to flip
     * @param amountIn   How much tokenIn to spend
     * @param routerA    DEX to buy on (cheaper)
     * @param routerB    DEX to sell on (more expensive)
     * @param feeA       Fee encoding for routerA:
     *                     V2: ignored (pass 0)
     *                     V3: fee tier (500/3000/10000)
     *                     Solidly: pool type (0=volatile, 1=stable)
     * @param feeB       Fee encoding for routerB (same convention as feeA)
     * @param minProfit  Minimum profit required in tokenIn units (#2)
     * @param deadline   Unix timestamp — revert if tx lands after this (#1)
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
    )
        external
        onlyOwner
        whenNotPaused        // #8
        nonReentrant         // #3
        checkDeadline(deadline) // #1
    {
        if (amountIn == 0) revert ZeroAmount();
        if (tokenIn == address(0) || tokenOut == address(0)) revert ZeroAddress();
        if (routerA == address(0) || routerB == address(0)) revert ZeroAddress();
        if (!allowedRouters[routerA]) revert RouterNotAllowed(routerA);
        if (!allowedRouters[routerB]) revert RouterNotAllowed(routerB);

        uint256 profit = _executeSwapPair(
            tokenIn, tokenOut, amountIn, routerA, routerB, feeA, feeB, minProfit, deadline
        );

        // Accumulate profit (#7)
        totalProfit[tokenIn] += profit;
        totalTrades++;

        emit ArbitrageExecuted(tokenIn, tokenOut, routerA, routerB, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // CORE: Multi-Hop Arbitrage (V2 only)
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Execute arbitrage with custom multi-hop paths (V2 only).
     *         e.g., USDC → WETH → TOKEN on routerA, TOKEN → USDC on routerB
     */
    function executeArbitrageMultiHop(
        address tokenIn,
        uint256 amountIn,
        address routerA,
        address routerB,
        address[] calldata pathA,
        address[] calldata pathB,
        uint256 minProfit,
        uint256 deadline
    )
        external
        onlyOwner
        whenNotPaused
        nonReentrant
        checkDeadline(deadline)
    {
        if (amountIn == 0) revert ZeroAmount();
        if (routerA == address(0) || routerB == address(0)) revert ZeroAddress();
        if (!allowedRouters[routerA]) revert RouterNotAllowed(routerA);
        if (!allowedRouters[routerB]) revert RouterNotAllowed(routerB);
        require(pathA.length >= 2 && pathB.length >= 2, "Invalid path length");
        require(pathA[0] == tokenIn, "pathA must start with tokenIn");
        require(pathB[pathB.length - 1] == tokenIn, "pathB must end with tokenIn");

        uint256 balanceBefore = IERC20(tokenIn).balanceOf(address(this));

        // Buy leg
        _approveExact(tokenIn, routerA, amountIn);
        uint256[] memory amountsA = IUniswapV2Router02(routerA)
            .swapExactTokensForTokens(amountIn, 0, pathA, address(this), deadline);

        // Sell leg — require at least amountIn + minProfit back for MEV protection
        address tokenMid = pathA[pathA.length - 1];
        uint256 midAmount = amountsA[amountsA.length - 1];
        _approveExact(tokenMid, routerB, midAmount);
        IUniswapV2Router02(routerB)
            .swapExactTokensForTokens(midAmount, amountIn + minProfit, pathB, address(this), deadline);

        // Cleanup allowances (#4)
        _resetAllowance(tokenIn, routerA);
        _resetAllowance(tokenMid, routerB);

        // Profitability check with minProfit (#2)
        uint256 balanceAfter = IERC20(tokenIn).balanceOf(address(this));
        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }

        uint256 profit;
        unchecked { profit = balanceAfter - balanceBefore; } // balanceAfter >= balanceBefore + minProfit checked above
        totalProfit[tokenIn] += profit;
        totalTrades++;

        emit ArbitrageExecuted(tokenIn, tokenMid, routerA, routerB, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // CORE: Triangular Arbitrage (3-token loop)
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Execute triangular arbitrage: A → B → C → A loop.
     *         Example: USDC → WETH → OP → USDC
     *
     * All three legs can use V2 or V3 routers (dispatch is automatic).
     *
     * @param tokenA     Base token we start and end with
     * @param tokenB     First intermediate token
     * @param tokenC     Second intermediate token
     * @param amountIn   How much tokenA to start with
     * @param routerAB   Router for A→B leg
     * @param routerBC   Router for B→C leg
     * @param routerCA   Router for C→A leg
     * @param feeAB      V3 fee tier for A→B (0 for V2)
     * @param feeBC      V3 fee tier for B→C (0 for V2)
     * @param feeCA      V3 fee tier for C→A (0 for V2)
     * @param minProfit  Minimum profit required in tokenA units
     * @param deadline   Unix timestamp — revert if tx lands after this
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
    )
        external
        onlyOwner
        whenNotPaused
        nonReentrant
        checkDeadline(deadline)
    {
        if (amountIn == 0) revert ZeroAmount();
        if (tokenA == address(0) || tokenB == address(0) || tokenC == address(0)) {
            revert ZeroAddress();
        }
        if (!allowedRouters[routerAB]) revert RouterNotAllowed(routerAB);
        if (!allowedRouters[routerBC]) revert RouterNotAllowed(routerBC);
        if (!allowedRouters[routerCA]) revert RouterNotAllowed(routerCA);

        uint256 balanceBefore = IERC20(tokenA).balanceOf(address(this));

        // ── Leg 1: A → B ──
        uint256 amountB;
        if (routerType[routerAB] == RouterType.V3) {
            amountB = _executeSwapV3(tokenA, tokenB, amountIn, routerAB, feeAB, 0, deadline);
        } else if (routerType[routerAB] == RouterType.Solidly) {
            amountB = _executeSwapSolidly(tokenA, tokenB, amountIn, routerAB, feeAB, 0, deadline);
        } else if (routerType[routerAB] == RouterType.SyncSwap) {
            amountB = _executeSwapSyncSwap(tokenA, tokenB, amountIn, routerAB, 0, deadline);
        } else if (routerType[routerAB] == RouterType.Aerodrome) {
            amountB = _executeSwapAerodrome(tokenA, tokenB, amountIn, routerAB, feeAB, 0, deadline);
        } else {
            amountB = _executeSwapV2(tokenA, tokenB, amountIn, routerAB, 0, deadline);
        }

        // ── Leg 2: B → C ──
        uint256 amountC;
        if (routerType[routerBC] == RouterType.V3) {
            amountC = _executeSwapV3(tokenB, tokenC, amountB, routerBC, feeBC, 0, deadline);
        } else if (routerType[routerBC] == RouterType.Solidly) {
            amountC = _executeSwapSolidly(tokenB, tokenC, amountB, routerBC, feeBC, 0, deadline);
        } else if (routerType[routerBC] == RouterType.SyncSwap) {
            amountC = _executeSwapSyncSwap(tokenB, tokenC, amountB, routerBC, 0, deadline);
        } else if (routerType[routerBC] == RouterType.Aerodrome) {
            amountC = _executeSwapAerodrome(tokenB, tokenC, amountB, routerBC, feeBC, 0, deadline);
        } else {
            amountC = _executeSwapV2(tokenB, tokenC, amountB, routerBC, 0, deadline);
        }

        // ── Leg 3: C → A ──
        // Require at least amountIn + minProfit on the final leg for router-level MEV protection
        if (routerType[routerCA] == RouterType.V3) {
            _executeSwapV3(tokenC, tokenA, amountC, routerCA, feeCA, amountIn + minProfit, deadline);
        } else if (routerType[routerCA] == RouterType.Solidly) {
            _executeSwapSolidly(tokenC, tokenA, amountC, routerCA, feeCA, amountIn + minProfit, deadline);
        } else if (routerType[routerCA] == RouterType.SyncSwap) {
            _executeSwapSyncSwap(tokenC, tokenA, amountC, routerCA, amountIn + minProfit, deadline);
        } else if (routerType[routerCA] == RouterType.Aerodrome) {
            _executeSwapAerodrome(tokenC, tokenA, amountC, routerCA, feeCA, amountIn + minProfit, deadline);
        } else {
            _executeSwapV2(tokenC, tokenA, amountC, routerCA, amountIn + minProfit, deadline);
        }

        // ── Profitability gate ──
        uint256 balanceAfter = IERC20(tokenA).balanceOf(address(this));
        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }

        uint256 profit;
        unchecked { profit = balanceAfter - balanceBefore; } // balanceAfter >= balanceBefore + minProfit checked above
        totalProfit[tokenA] += profit;
        totalTrades++;

        // Emit event with tokenB as "tokenOut" for dashboard compatibility
        emit ArbitrageExecuted(tokenA, tokenB, routerAB, routerCA, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // CORE: Batch Execution (improvement #6)
    // ═══════════════════════════════════════════════════════════

    struct ArbParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        address routerA;
        address routerB;
        uint24 feeA;     // V3 fee tier for routerA (0 for V2)
        uint24 feeB;     // V3 fee tier for routerB (0 for V2)
        uint256 minProfit;
    }

    /**
     * @notice Execute multiple arbs in one transaction.
     *         Individual arbs that fail are skipped (not reverted),
     *         so one bad trade doesn't kill the profitable ones.
     *
     * @param arbs      Array of arbitrage parameters
     * @param deadline  Shared deadline for all arbs in this batch
     * @return results  Profit for each arb (0 = skipped/failed)
     */
    function batchExecute(
        ArbParams[] calldata arbs,
        uint256 deadline
    )
        external
        onlyOwner
        whenNotPaused
        nonReentrant
        checkDeadline(deadline)
        returns (uint256[] memory results)
    {
        if (arbs.length == 0) revert EmptyBatch();

        results = new uint256[](arbs.length);
        uint256 succeeded = 0;
        uint256 batchProfit = 0;

        for (uint256 i = 0; i < arbs.length; i++) {
            ArbParams calldata a = arbs[i];

            // Skip invalid params
            if (a.amountIn == 0 || a.tokenIn == address(0) || a.tokenOut == address(0)) {
                continue;
            }

            // Skip unapproved routers
            if (!allowedRouters[a.routerA] || !allowedRouters[a.routerB]) {
                continue;
            }

            // Skip if insufficient balance
            if (IERC20(a.tokenIn).balanceOf(address(this)) < a.amountIn) {
                continue;
            }

            // Try-catch: failures are skipped, not fatal
            try this._executeSingle(a, deadline) returns (uint256 profit) {
                results[i] = profit;
                totalProfit[a.tokenIn] += profit;
                batchProfit += profit;
                succeeded++;

                emit ArbitrageExecuted(
                    a.tokenIn, a.tokenOut, a.routerA, a.routerB, a.amountIn, profit
                );
            } catch {
                results[i] = 0;
            }
        }

        totalTrades += succeeded;
        emit BatchExecuted(arbs.length, succeeded, batchProfit);
    }

    /**
     * @dev External wrapper for try-catch in batchExecute.
     *      MUST only be callable by the contract itself.
     */
    function _executeSingle(
        ArbParams calldata a,
        uint256 deadline
    ) external returns (uint256 profit) {
        if (msg.sender != address(this)) revert RouterNotAllowed(msg.sender);
        if (!allowedRouters[a.routerA]) revert RouterNotAllowed(a.routerA);
        if (!allowedRouters[a.routerB]) revert RouterNotAllowed(a.routerB);

        profit = _executeSwapPair(
            a.tokenIn, a.tokenOut, a.amountIn,
            a.routerA, a.routerB, a.feeA, a.feeB, a.minProfit, deadline
        );
    }

    // ═══════════════════════════════════════════════════════════
    // INTERNAL: Swap Logic (V2 + V3 dispatch)
    // ═══════════════════════════════════════════════════════════

    function _executeSwapPair(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB,
        uint24 feeA,
        uint24 feeB,
        uint256 minProfit,
        uint256 deadline
    ) internal returns (uint256 profit) {
        uint256 balanceBefore = IERC20(tokenIn).balanceOf(address(this));

        // ── Leg 1: Buy tokenOut on routerA ──
        uint256 tokenOutReceived;
        if (routerType[routerA] == RouterType.V3) {
            tokenOutReceived = _executeSwapV3(tokenIn, tokenOut, amountIn, routerA, feeA, 0, deadline);
        } else if (routerType[routerA] == RouterType.Solidly) {
            tokenOutReceived = _executeSwapSolidly(tokenIn, tokenOut, amountIn, routerA, feeA, 0, deadline);
        } else if (routerType[routerA] == RouterType.SyncSwap) {
            tokenOutReceived = _executeSwapSyncSwap(tokenIn, tokenOut, amountIn, routerA, 0, deadline);
        } else if (routerType[routerA] == RouterType.Aerodrome) {
            tokenOutReceived = _executeSwapAerodrome(tokenIn, tokenOut, amountIn, routerA, feeA, 0, deadline);
        } else {
            tokenOutReceived = _executeSwapV2(tokenIn, tokenOut, amountIn, routerA, 0, deadline);
        }

        // ── Leg 2: Sell tokenOut on routerB ──
        // amountOutMin = amountIn + minProfit: router-level MEV protection.
        if (routerType[routerB] == RouterType.V3) {
            _executeSwapV3(tokenOut, tokenIn, tokenOutReceived, routerB, feeB, amountIn + minProfit, deadline);
        } else if (routerType[routerB] == RouterType.Solidly) {
            _executeSwapSolidly(tokenOut, tokenIn, tokenOutReceived, routerB, feeB, amountIn + minProfit, deadline);
        } else if (routerType[routerB] == RouterType.SyncSwap) {
            _executeSwapSyncSwap(tokenOut, tokenIn, tokenOutReceived, routerB, amountIn + minProfit, deadline);
        } else if (routerType[routerB] == RouterType.Aerodrome) {
            _executeSwapAerodrome(tokenOut, tokenIn, tokenOutReceived, routerB, feeB, amountIn + minProfit, deadline);
        } else {
            _executeSwapV2(tokenOut, tokenIn, tokenOutReceived, routerB, amountIn + minProfit, deadline);
        }

        // ── Profitability gate ── (#2)
        uint256 balanceAfter = IERC20(tokenIn).balanceOf(address(this));

        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }

        unchecked { profit = balanceAfter - balanceBefore; } // balanceAfter >= balanceBefore + minProfit checked above
    }

    function _executeSwapV2(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address router,
        uint256 amountOutMin,
        uint256 deadline
    ) internal returns (uint256 amountOut) {
        _approveExact(tokenIn, router, amountIn);  // #4

        address[] memory path = new address[](2);
        path[0] = tokenIn;
        path[1] = tokenOut;

        uint256[] memory amounts = IUniswapV2Router02(router)
            .swapExactTokensForTokens(amountIn, amountOutMin, path, address(this), deadline);

        amountOut = amounts[amounts.length - 1];
        _resetAllowance(tokenIn, router);  // #4
    }

    function _executeSwapV3(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address router,
        uint24 fee,
        uint256 amountOutMin,
        uint256 deadline
    ) internal returns (uint256 amountOut) {
        _approveExact(tokenIn, router, amountIn);  // #4

        amountOut = ISwapRouterV3(router).exactInputSingle(
            ISwapRouterV3.ExactInputSingleParams({
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                fee: fee,
                recipient: address(this),
                amountIn: amountIn,
                amountOutMinimum: amountOutMin,
                sqrtPriceLimitX96: 0
            })
        );

        _resetAllowance(tokenIn, router);  // #4
    }

    function _executeSwapSolidly(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address router,
        uint24 fee,
        uint256 amountOutMin,
        uint256 deadline
    ) internal returns (uint256 amountOut) {
        _approveExact(tokenIn, router, amountIn);  // #4

        // fee=0 → volatile pool (xy=k), fee≠0 → stable pool (x³y+y³x=k)
        bool stable = (fee != 0);

        ISolidlyRouter.Route[] memory routes = new ISolidlyRouter.Route[](1);
        routes[0] = ISolidlyRouter.Route({ from: tokenIn, to: tokenOut, stable: stable });

        uint256[] memory amounts = ISolidlyRouter(router)
            .swapExactTokensForTokens(amountIn, amountOutMin, routes, address(this), deadline);

        amountOut = amounts[amounts.length - 1];
        _resetAllowance(tokenIn, router);  // #4
    }

    function _executeSwapAerodrome(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address router,
        uint24 fee,
        uint256 amountOutMin,
        uint256 deadline
    ) internal returns (uint256 amountOut) {
        _approveExact(tokenIn, router, amountIn);

        // fee=0 → volatile pool (xy=k), fee≠0 → stable pool (x³y+y³x=k)
        // factory=address(0) → Aerodrome router uses its internal default factory
        bool stable = (fee != 0);
        IAerodromeRouter.Route[] memory routes = new IAerodromeRouter.Route[](1);
        routes[0] = IAerodromeRouter.Route({ from: tokenIn, to: tokenOut, stable: stable, factory: address(0) });

        uint256[] memory amounts = IAerodromeRouter(router)
            .swapExactTokensForTokens(amountIn, amountOutMin, routes, address(this), deadline);

        amountOut = amounts[amounts.length - 1];
        _resetAllowance(tokenIn, router);
    }

    function _executeSwapSyncSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address factory,      // routerA/routerB = SyncSwap pool factory address
        uint256 amountOutMin,
        uint256 /*deadline*/  // deadline enforced by outer checkDeadline modifier
    ) internal returns (uint256 amountOut) {
        address pool = ISyncSwapFactory(factory).getPool(tokenIn, tokenOut);
        if (pool == address(0)) revert PoolNotFound(factory);

        // SyncSwap pools: transfer tokens in, then call swap (no approve needed)
        IERC20(tokenIn).safeTransfer(pool, amountIn);

        // withdrawMode = 1 → ERC20 transfer to recipient
        bytes memory swapData = abi.encode(tokenIn, address(this), uint8(1));
        amountOut = ISyncSwapPool(pool).swap(swapData, address(this), address(0), "");

        if (amountOut < amountOutMin) revert InsufficientOutput(amountOut, amountOutMin);
    }

    // ═══════════════════════════════════════════════════════════
    // INTERNAL: Allowance Helpers (improvement #4)
    // ═══════════════════════════════════════════════════════════

    /**
     * @dev Approve exact amount. Uses forceApprove which handles tokens
     *      that require allowance = 0 before setting new value (e.g., USDT).
     */
    function _approveExact(address token, address spender, uint256 amount) internal {
        IERC20(token).forceApprove(spender, amount);
    }

    /**
     * @dev Reset allowance to 0 after swap. Prevents dangling allowances.
     */
    function _resetAllowance(address token, address spender) internal {
        IERC20(token).forceApprove(spender, 0);
    }

    // ═══════════════════════════════════════════════════════════
    // VIEW: Estimation & Stats
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Simulate a V2 arb off-chain (no gas cost).
     */
    function estimateArbitrage(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB
    ) external view returns (uint256 profit, uint256 amountOut) {
        address[] memory pathBuy = new address[](2);
        pathBuy[0] = tokenIn;
        pathBuy[1] = tokenOut;

        address[] memory pathSell = new address[](2);
        pathSell[0] = tokenOut;
        pathSell[1] = tokenIn;

        try IUniswapV2Router02(routerA).getAmountsOut(amountIn, pathBuy) returns (
            uint256[] memory amountsBuy
        ) {
            uint256 tokenOutAmount = amountsBuy[amountsBuy.length - 1];

            try IUniswapV2Router02(routerB).getAmountsOut(tokenOutAmount, pathSell) returns (
                uint256[] memory amountsSell
            ) {
                amountOut = amountsSell[amountsSell.length - 1];
                if (amountOut > amountIn) {
                    profit = amountOut - amountIn;
                }
            } catch {
                profit = 0;
                amountOut = 0;
            }
        } catch {
            profit = 0;
            amountOut = 0;
        }
    }

    /**
     * @notice Get accumulated stats for a token (#7).
     */
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
    // PAUSE (improvement #8)
    // ═══════════════════════════════════════════════════════════

    /// @notice Freeze all trade execution. Withdrawals still work.
    function pause() external onlyOwner {
        _pause();
    }

    /// @notice Resume trade execution.
    function unpause() external onlyOwner {
        _unpause();
    }

    // ═══════════════════════════════════════════════════════════
    // FUND MANAGEMENT
    // ═══════════════════════════════════════════════════════════

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

    /// @notice Withdraw specific amount of an ERC20. Always works (even when paused).
    function withdrawTokenAmount(address _token, uint256 _amount) external onlyOwner {
        if (_token == address(0)) revert ZeroAddress();
        if (_amount == 0) revert ZeroAmount();

        IERC20(_token).safeTransfer(owner(), _amount);
        emit WithdrawToken(_token, owner(), _amount);
    }

    // ═══════════════════════════════════════════════════════════
    // EMERGENCY
    // ═══════════════════════════════════════════════════════════

    /// @notice Zero out allowance for a token on a spender.
    function revokeApproval(address _token, address _spender) external onlyOwner {
        IERC20(_token).forceApprove(_spender, 0);
        emit ApprovalRevoked(_token, _spender);
    }
}
