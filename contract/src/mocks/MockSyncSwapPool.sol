// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev Mock SyncSwap Classic Pool for testing.
/// Implements the SyncSwap pool interface: caller transfers tokenIn to the pool,
/// then calls swap(). The pool reads the balance difference to determine amountIn.
contract MockSyncSwapPool {
    address public tokenIn;
    address public tokenOut;
    uint256 public rateNum;
    uint256 public rateDen;

    constructor(address _tokenIn, address _tokenOut, uint256 _rateNum, uint256 _rateDen) {
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        rateNum = _rateNum;
        rateDen = _rateDen;
    }

    /// @notice Mimic SyncSwap pool quoting interface.
    function getAmountOut(address, uint256 amountIn, address) external view returns (uint256) {
        return amountIn * rateNum / rateDen;
    }

    /// @notice SyncSwap pool swap — tokens must be transferred in before calling.
    /// @param data abi.encode(tokenIn, recipient, withdrawMode)
    function swap(
        bytes calldata data,
        address /*sender*/,
        address /*callback*/,
        bytes calldata /*callbackData*/
    ) external returns (uint256 amountOut) {
        (address _tokenIn, address to, ) = abi.decode(data, (address, address, uint8));

        // Amount in = current balance (tokens transferred before calling swap)
        uint256 amountIn = IERC20(_tokenIn).balanceOf(address(this));
        amountOut = amountIn * rateNum / rateDen;

        IERC20(tokenOut).transfer(to, amountOut);
    }
}
