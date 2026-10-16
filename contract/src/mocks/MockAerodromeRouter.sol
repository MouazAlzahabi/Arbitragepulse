// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/**
 * @dev Mock Aerodrome V2 router (4-field Route). Fixed rate per pool type, so tests can
 *      check that `stable` is passed through: volatile and stable rates differ.
 */
contract MockAerodromeRouter {
    struct Route { address from; address to; bool stable; address factory; }

    uint256 public volNum; uint256 public volDen;
    uint256 public stableNum; uint256 public stableDen;
    bool public lastStable;
    address public lastFactory;

    constructor(uint256 _volNum, uint256 _volDen, uint256 _stableNum, uint256 _stableDen) {
        volNum = _volNum; volDen = _volDen; stableNum = _stableNum; stableDen = _stableDen;
    }

    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        Route[] calldata routes,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory amounts) {
        require(deadline >= block.timestamp, "Router: EXPIRED");
        require(routes.length == 1, "one route");
        Route calldata r = routes[0];
        lastStable = r.stable;
        lastFactory = r.factory;
        IERC20(r.from).transferFrom(msg.sender, address(this), amountIn);
        uint256 out = r.stable ? amountIn * stableNum / stableDen : amountIn * volNum / volDen;
        require(out >= amountOutMin, "Router: INSUFFICIENT_OUTPUT_AMOUNT");
        IERC20(r.to).transfer(to, out);
        amounts = new uint256[](2);
        amounts[0] = amountIn;
        amounts[1] = out;
    }
}
