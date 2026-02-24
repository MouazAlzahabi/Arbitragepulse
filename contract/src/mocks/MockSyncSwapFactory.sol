// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @dev Mock SyncSwap Pool Factory for testing.
contract MockSyncSwapFactory {
    mapping(address => mapping(address => address)) private _pools;

    /// @notice Register a pool for a specific direction (tokenIn → tokenOut).
    /// Call twice to register both directions, using the same or different pools.
    function setPool(address tokenIn, address tokenOut, address pool) external {
        _pools[tokenIn][tokenOut] = pool;
    }

    /// @notice Returns the pool address for a given token pair.
    function getPool(address tokenA, address tokenB) external view returns (address) {
        return _pools[tokenA][tokenB];
    }
}
