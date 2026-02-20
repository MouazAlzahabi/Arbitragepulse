use alloy::sol;

// ─── Uniswap V2 Router ────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IUniswapV2Router02 {
        function getAmountsOut(
            uint256 amountIn,
            address[] calldata path
        ) external view returns (uint256[] memory amounts);

        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            address[] calldata path,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
    }
}

// ─── Uniswap V3 SwapRouter ────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface ISwapRouterV3 {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params)
            external returns (uint256 amountOut);
    }
}

// ─── Uniswap V3 QuoterV2 ─────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IQuoterV2 {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
            external
            returns (
                uint256 amountOut,
                uint160 sqrtPriceX96After,
                uint32 initializedTicksCrossed,
                uint256 gasEstimate
            );
    }
}

// ─── ArbitrageExecutor ────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract ArbitrageExecutor {
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
        ) external;

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
        ) external;

        function setAllowedRouter(address router, bool approved) external;
        function setRouterType(address router, uint8 rtype) external;
        function allowedRouters(address router) external view returns (bool);
        function routerType(address router) external view returns (uint8);
        function totalProfit(address token) external view returns (uint256);
        function totalTrades() external view returns (uint256);
        function getStats(address token) external view returns (
            uint256 totalProfit,
            uint256 totalTrades,
            uint256 contractBalance
        );
        function pause() external;
        function unpause() external;
        function withdrawToken(address token) external;
    }
}

// ─── ERC20 ────────────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

// ─── V2 Pair (for swap event subscriptions) ───────────────────────────────────

sol! {
    event PairSwapV2(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );
}

// ─── Multicall3 (batch quoting) ───────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct McResult {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (McResult[] memory returnData);
    }
}

// ─── V3 Pool (for swap event subscriptions) ───────────────────────────────────

sol! {
    event PoolSwapV3(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );
}
