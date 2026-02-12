// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/**
 * @dev Mock Uniswap V2 Router for testing.
 *      Simulates swaps with a fixed exchange rate.
 *
 *      rate = rateNumerator / rateDenominator
 *      e.g., 2/1 = 2x output, 6/10 = 0.6x output
 */
contract MockRouter {
    uint256 public rateNumerator;
    uint256 public rateDenominator;

    constructor(uint256 _rateNumerator, uint256 _rateDenominator) {
        rateNumerator = _rateNumerator;
        rateDenominator = _rateDenominator;
    }

    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 /* amountOutMin */,
        address[] calldata path,
        address to,
        uint256 /* deadline */
    ) external returns (uint256[] memory amounts) {
        require(path.length >= 2, "Invalid path");

        address tokenIn = path[0];
        address tokenOut = path[path.length - 1];

        // Pull tokenIn from caller
        IERC20(tokenIn).transferFrom(msg.sender, address(this), amountIn);

        // Calculate output
        uint256 amountOut = (amountIn * rateNumerator) / rateDenominator;

        // Send tokenOut to recipient
        IERC20(tokenOut).transfer(to, amountOut);

        // Return amounts array (matches Uniswap interface)
        amounts = new uint256[](path.length);
        amounts[0] = amountIn;
        amounts[path.length - 1] = amountOut;
    }

    function getAmountsOut(
        uint256 amountIn,
        address[] calldata path
    ) external view returns (uint256[] memory amounts) {
        amounts = new uint256[](path.length);
        amounts[0] = amountIn;
        amounts[path.length - 1] = (amountIn * rateNumerator) / rateDenominator;
    }
}
