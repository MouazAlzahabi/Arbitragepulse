// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/**
 * @dev Mock Solidly (ve(3,3)) Router for testing.
 *      Implements getAmountsOut and swapExactTokensForTokens with
 *      Solidly-style Route[] interface and a fixed exchange rate.
 *
 *      rate = rateNumerator / rateDenominator
 *      e.g., 2/1 = 2x output, 6/10 = 0.6x output
 */
contract MockSolidlyRouter {
    uint256 public rateNumerator;
    uint256 public rateDenominator;

    struct Route {
        address from;
        address to;
        bool stable;
    }

    constructor(uint256 _rateNumerator, uint256 _rateDenominator) {
        rateNumerator = _rateNumerator;
        rateDenominator = _rateDenominator;
    }

    function getAmountsOut(
        uint256 amountIn,
        Route[] calldata routes
    ) external view returns (uint256[] memory amounts) {
        amounts = new uint256[](routes.length + 1);
        amounts[0] = amountIn;
        for (uint256 i = 0; i < routes.length; i++) {
            amounts[i + 1] = (amounts[i] * rateNumerator) / rateDenominator;
        }
    }

    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        Route[] calldata routes,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts) {
        require(block.timestamp <= deadline, "Deadline expired");
        require(routes.length > 0, "Empty routes");

        // Calculate output using fixed rate
        amounts = new uint256[](routes.length + 1);
        amounts[0] = amountIn;
        for (uint256 i = 0; i < routes.length; i++) {
            amounts[i + 1] = (amounts[i] * rateNumerator) / rateDenominator;
        }
        uint256 amountOut = amounts[routes.length];
        require(amountOut >= amountOutMin, "Insufficient output amount");

        // Pull input token from caller
        IERC20(routes[0].from).transferFrom(msg.sender, address(this), amountIn);

        // Send output token to recipient
        IERC20(routes[routes.length - 1].to).transfer(to, amountOut);
    }
}
