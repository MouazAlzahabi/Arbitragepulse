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

// ─── Solidly (ve(3,3)) Router ────────────────────────────────────────────────
// Used by Lynex, Nile Exchange, and other Solidly forks on Linea.
// fee field encoding (reuses uint24): 0 = volatile (vAMM: xy=k), 1 = stable (sAMM: x³y+y³x=k)

sol! {
    #[sol(rpc)]
    interface ISolidlyRouter {
        struct Route {
            address from;
            address to;
            bool stable;
        }
        function getAmountsOut(uint256 amountIn, Route[] memory routes)
            external view returns (uint256[] memory amounts);
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] calldata routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
    }
}

// ─── Aerodrome Router (Base) ──────────────────────────────────────────────────
// Aerodrome V2 uses Route{from, to, stable, factory} (4-field).
// factory=address(0) → router falls back to its internal default factory.
// fee=0 → volatile (xy=k), fee≠0 → stable (x³y+y³x=k).

sol! {
    #[sol(rpc)]
    interface IAerodromeRouter {
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }
        function getAmountsOut(uint256 amountIn, Route[] memory routes)
            external view returns (uint256[] memory amounts);
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] calldata routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] memory amounts);
    }
}

// ─── SyncSwap Pool (per-pool quoting) ────────────────────────────────────────
// SyncSwap pools are queried directly (not via router).
// Pool address obtained from factory.getPool(tokenA, tokenB).

sol! {
    #[sol(rpc)]
    interface ISyncSwapPool {
        function getAmountOut(
            address tokenIn,
            uint256 amountIn,
            address sender
        ) external view returns (uint256 amountOut);
        // data = abi.encode(tokenIn, recipient, withdrawMode)
        function swap(
            bytes calldata data,
            address sender,
            address callback,
            bytes calldata callbackData
        ) external returns (uint256 amountOut);
    }
}

// ─── SyncSwap Classic/Stable Pool Factory ────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface ISyncSwapClassicPoolFactory {
        function getPool(
            address tokenA,
            address tokenB
        ) external view returns (address pool);
    }
}

// ─── V2 / Solidly Pair events ─────────────────────────────────────────────────

sol! {
    event PairSwapV2(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );

    /// Emitted by every V2 and Solidly-volatile pair on every swap.
    /// reserve0/reserve1 are the NEW balances after the swap.
    /// NOTE: Name MUST be "Sync" (not "PairSyncV2") — alloy computes SIGNATURE_HASH
    /// from the event name, and on-chain pairs emit Sync(uint112,uint112).
    event Sync(uint112 reserve0, uint112 reserve1);

}

// ─── Pool discovery (startup, one-time) ───────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IRouterWithFactory {
        function factory() external view returns (address);
    }

    #[sol(rpc)]
    interface IUniswapV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }

    #[sol(rpc)]
    interface ISolidlyFactory {
        /// stable=false → volatile (xy=k), stable=true → stable (x³y+y³x=k)
        function getPair(address tokenA, address tokenB, bool stable) external view returns (address pair);
    }

    #[sol(rpc)]
    interface IUniswapV2Pair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function getReserves() external view returns (
            uint112 reserve0,
            uint112 reserve1,
            uint32 blockTimestampLast
        );
    }
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

// ─── V3 Pool (for swap event subscriptions + local state seeding) ─────────────

sol! {
    /// NOTE: Name MUST be "Swap" (not "PoolSwapV3") — alloy computes SIGNATURE_HASH
    /// from the event name, and on-chain V3 pools emit Swap(...).
    event Swap(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );
}

// ─── Uniswap V3 Pool Factory ──────────────────────────────────────────────────
// Used at startup to discover pool addresses for each (tokenA, tokenB, fee) tuple.
// Factory addresses differ per chain and are configured in config.yaml per-router.

sol! {
    #[sol(rpc)]
    interface IUniswapV3Factory {
        function getPool(
            address tokenA,
            address tokenB,
            uint24 fee
        ) external view returns (address pool);
    }
}

// ─── Uniswap V3 Pool (state seeding + event subscriptions) ───────────────────
// Used at startup to read initial sqrtPriceX96 and liquidity via slot0() + liquidity().
// After startup, both fields are kept fresh from Swap events (zero RPC).

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}
