// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/**
 * @title MockSlipstreamRouter — Aerodrome Slipstream SwapRouter (8-field exactInputSingle
 *        with int24 tickSpacing and a deadline). Swaps at a fixed rate; records what the
 *        executor encoded.
 */
contract MockSlipstreamRouter {
    uint256 public rateNumerator;
    uint256 public rateDenominator;
    int24 public lastTickSpacing;
    uint256 public lastDeadline;

    constructor(uint256 _rateNumerator, uint256 _rateDenominator) {
        rateNumerator = _rateNumerator;
        rateDenominator = _rateDenominator;
    }

    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        int24 tickSpacing;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    function exactInputSingle(ExactInputSingleParams calldata params) external returns (uint256 amountOut) {
        require(block.timestamp <= params.deadline, "Transaction too old");
        lastTickSpacing = params.tickSpacing;
        lastDeadline = params.deadline;
        IERC20(params.tokenIn).transferFrom(msg.sender, address(this), params.amountIn);
        amountOut = (params.amountIn * rateNumerator) / rateDenominator;
        require(amountOut >= params.amountOutMinimum, "Too little received");
        IERC20(params.tokenOut).transfer(params.recipient, amountOut);
    }
}
