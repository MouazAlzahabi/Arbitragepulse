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

// ─── Aerodrome Slipstream (concentrated liquidity) ───────────────────────────
// Uniswap-V3-style pools keyed by int24 tickSpacing instead of a fee tier. Same Swap /
// Mint / Burn events as Uniswap V3; slot0 has 6 fields (no feeProtocol); the swap fee is
// per pool (fee()). The quoter returns QuoterV2's 4 values, so decode_v3_quote serves both.

sol! {
    interface ISlipstreamQuoter {
        struct QuoteExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            int24 tickSpacing;
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

    interface ISlipstreamFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    }

    interface ISlipstreamPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
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

        /// Allow a router with its type (0 = V2, 1 = V3, 4 = Aerodrome); the only way to allow one.
        function setRouter(address router, uint8 rtype) external;
        function revokeRouter(address router) external;
        function allowedRouters(address router) external view returns (bool);
        function routerType(address router) external view returns (uint8);
        function totalProfit(address token) external view returns (uint256);
        function totalTrades() external view returns (uint256);
        function getStats(address token) external view returns (
            uint256 totalProfit,
            uint256 totalTrades,
            uint256 contractBalance
        );
        function owner() external view returns (address);
        function paused() external view returns (bool);
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

// ─── V2 / Aerodrome Pair events ─────────────────────────────────────────────────

sol! {
    event PairSwapV2(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );

    /// Emitted by every V2 and Aerodrome-volatile pair on every swap.
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
    interface IAerodromeFactory {
        /// Aerodrome V2 factory uses getPool() not getPair()
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
        /// Effective fee of `pool` in bps (/10_000): custom per-pool fee, else stableFee/volatileFee.
        function getFee(address pool, bool _stable) external view returns (uint256);
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
    /// Uniswap V3 Swap event (7 non-indexed fields).
    /// topic0 = keccak256("Swap(address,address,int256,int256,uint160,uint128,int24)")
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

// ─── PancakeSwap V3 pool (9-field Swap event) ─────────────────────────────────
// PancakeV3Pool emits Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128).
// topic0 = 0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83
// Wrapped in an interface so alloy derives SIGNATURE_HASH from the on-chain name "Swap",
// not from any Rust alias — matching the correct on-chain topic0.

sol! {
    interface IPancakeV3Pool {
        /// PancakeSwap V3 Swap event — 9 fields (adds protocolFeesToken0/1).
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint128 protocolFeesToken0,
            uint128 protocolFeesToken1
        );
    }
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

        /// Liquidity added to [tickLower, tickUpper). Same signature on PancakeSwap V3
        /// and Aerodrome Slipstream. topic0 = 0x7a53080b…
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        /// Liquidity removed from [tickLower, tickUpper). topic0 = 0x0c396cd9…
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
    }
}
