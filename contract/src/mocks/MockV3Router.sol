// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/**
 * @dev Mock Uniswap V3 Router for testing.
 *      Implements exactInputSingle with a fixed exchange rate.
 *
 *      rate = rateNumerator / rateDenominator
 *      e.g., 2/1 = 2x output, 6/10 = 0.6x output
 */
contract MockV3Router {
    uint256 public rateNumerator;
    uint256 public rateDenominator;
    /// @dev Last call's fee / price limit, so tests can assert what the executor encoded.
    uint24 public lastFee;
    uint160 public lastSqrtPriceLimitX96;

    constructor(uint256 _rateNumerator, uint256 _rateDenominator) {
        rateNumerator = _rateNumerator;
        rateDenominator = _rateDenominator;
    }

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
        external
        returns (uint256 amountOut)
    {
        lastFee = params.fee;
        lastSqrtPriceLimitX96 = params.sqrtPriceLimitX96;

        // Pull tokenIn from caller
        IERC20(params.tokenIn).transferFrom(msg.sender, address(this), params.amountIn);

        // Calculate output
        amountOut = (params.amountIn * rateNumerator) / rateDenominator;

        require(amountOut >= params.amountOutMinimum, "Too little received");

        // Send tokenOut to recipient
        IERC20(params.tokenOut).transfer(params.recipient, amountOut);
    }
}
