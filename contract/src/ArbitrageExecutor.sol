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

/**
 * @title ArbitrageExecutor v2
 * @notice Atomic cross-DEX arbitrage executor with enhanced safety.
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
 */
contract ArbitrageExecutor is Ownable2Step, ReentrancyGuard, Pausable {
    using SafeERC20 for IERC20;

    // ─── State ────────────────────────────────────────────────

    /// @notice Total accumulated profit per token (improvement #7)
    mapping(address => uint256) public totalProfit;

    /// @notice Total number of successful trades
    uint256 public totalTrades;

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

    // ─── Errors ───────────────────────────────────────────────

    error NotProfitable(uint256 balanceBefore, uint256 balanceAfter, uint256 minProfit);
    error DeadlineExpired(uint256 deadline, uint256 currentTimestamp);
    error ZeroAmount();
    error ZeroAddress();
    error TransferFailed();
    error EmptyBatch();

    // ─── Constructor ──────────────────────────────────────────

    /// @dev Ownable2Step (improvement #5): deployer is initial owner,
    ///      transfer requires new owner to call acceptOwnership()
    constructor() Ownable(msg.sender) {}

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
     * @param tokenIn    Base token we start and end with
     * @param tokenOut   Intermediate token to flip
     * @param amountIn   How much tokenIn to spend
     * @param routerA    DEX to buy on (cheaper)
     * @param routerB    DEX to sell on (more expensive)
     * @param minProfit  Minimum profit required in tokenIn units (#2)
     *                   Set this >= estimated gas cost to avoid dust profits
     * @param deadline   Unix timestamp — revert if tx lands after this (#1)
     */
    function executeArbitrage(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB,
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

        uint256 profit = _executeSwapPair(
            tokenIn, tokenOut, amountIn, routerA, routerB, minProfit, deadline
        );

        // Accumulate profit (#7)
        totalProfit[tokenIn] += profit;
        totalTrades++;

        emit ArbitrageExecuted(tokenIn, tokenOut, routerA, routerB, amountIn, profit);
    }

    // ═══════════════════════════════════════════════════════════
    // CORE: Multi-Hop Arbitrage
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Execute arbitrage with custom multi-hop paths.
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
        require(pathA.length >= 2 && pathB.length >= 2, "Invalid path length");
        require(pathA[0] == tokenIn, "pathA must start with tokenIn");
        require(pathB[pathB.length - 1] == tokenIn, "pathB must end with tokenIn");

        uint256 balanceBefore = IERC20(tokenIn).balanceOf(address(this));

        // Buy leg
        _approveExact(tokenIn, routerA, amountIn);
        uint256[] memory amountsA = IUniswapV2Router02(routerA)
            .swapExactTokensForTokens(amountIn, 0, pathA, address(this), deadline);

        // Sell leg
        address tokenMid = pathA[pathA.length - 1];
        uint256 midAmount = amountsA[amountsA.length - 1];
        _approveExact(tokenMid, routerB, midAmount);
        IUniswapV2Router02(routerB)
            .swapExactTokensForTokens(midAmount, 0, pathB, address(this), deadline);

        // Cleanup allowances (#4)
        _resetAllowance(tokenIn, routerA);
        _resetAllowance(tokenMid, routerB);

        // Profitability check with minProfit (#2)
        uint256 balanceAfter = IERC20(tokenIn).balanceOf(address(this));
        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }

        uint256 profit = balanceAfter - balanceBefore;
        totalProfit[tokenIn] += profit;
        totalTrades++;

        emit ArbitrageExecuted(tokenIn, tokenMid, routerA, routerB, amountIn, profit);
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
        require(msg.sender == address(this), "Internal only");

        profit = _executeSwapPair(
            a.tokenIn, a.tokenOut, a.amountIn,
            a.routerA, a.routerB, a.minProfit, deadline
        );
    }

    // ═══════════════════════════════════════════════════════════
    // INTERNAL: Swap Logic
    // ═══════════════════════════════════════════════════════════

    function _executeSwapPair(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address routerA,
        address routerB,
        uint256 minProfit,
        uint256 deadline
    ) internal returns (uint256 profit) {
        uint256 balanceBefore = IERC20(tokenIn).balanceOf(address(this));

        // ── Leg 1: Buy tokenOut on routerA ──
        _approveExact(tokenIn, routerA, amountIn);  // #4

        address[] memory pathBuy = new address[](2);
        pathBuy[0] = tokenIn;
        pathBuy[1] = tokenOut;

        uint256[] memory amountsBuy = IUniswapV2Router02(routerA)
            .swapExactTokensForTokens(amountIn, 0, pathBuy, address(this), deadline);

        uint256 tokenOutReceived = amountsBuy[amountsBuy.length - 1];

        // ── Leg 2: Sell tokenOut on routerB ──
        _approveExact(tokenOut, routerB, tokenOutReceived);  // #4

        address[] memory pathSell = new address[](2);
        pathSell[0] = tokenOut;
        pathSell[1] = tokenIn;

        IUniswapV2Router02(routerB)
            .swapExactTokensForTokens(tokenOutReceived, 0, pathSell, address(this), deadline);

        // ── Cleanup allowances ── (#4)
        _resetAllowance(tokenIn, routerA);
        _resetAllowance(tokenOut, routerB);

        // ── Profitability gate ── (#2)
        uint256 balanceAfter = IERC20(tokenIn).balanceOf(address(this));

        if (balanceAfter < balanceBefore + minProfit) {
            revert NotProfitable(balanceBefore, balanceAfter, minProfit);
        }

        profit = balanceAfter - balanceBefore;
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
        if (IERC20(token).allowance(address(this), spender) > 0) {
            IERC20(token).forceApprove(spender, 0);
        }
    }

    // ═══════════════════════════════════════════════════════════
    // VIEW: Estimation & Stats
    // ═══════════════════════════════════════════════════════════

    /**
     * @notice Simulate an arb off-chain (no gas cost).
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
    function withdrawETH() external onlyOwner {
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
    }
}
