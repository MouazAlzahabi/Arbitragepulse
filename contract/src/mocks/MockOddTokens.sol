// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev Burns 1% of every transfer (fee-on-transfer token).
contract MockFeeOnTransferERC20 is ERC20 {
    constructor(uint256 supply) ERC20("Fee Token", "FEE") { _mint(msg.sender, supply); }

    function _update(address from, address to, uint256 value) internal override {
        if (from != address(0) && to != address(0)) {
            uint256 fee = value / 100;
            super._update(from, address(0), fee);
            value -= fee;
        }
        super._update(from, to, value);
    }
}

/// @dev Reverts on approve(spender, 0) — like BNB's original token.
contract MockNoZeroApproveERC20 is ERC20 {
    constructor(uint256 supply) ERC20("No Zero Approve", "NZA") { _mint(msg.sender, supply); }

    function approve(address spender, uint256 value) public override returns (bool) {
        require(value > 0, "zero approve");
        return super.approve(spender, value);
    }
}

/// @dev V2-style router that reports twice the output it actually sends.
contract MockLyingRouter {
    function swapExactTokensForTokens(
        uint256 amountIn, uint256, address[] calldata path, address to, uint256
    ) external returns (uint256[] memory amounts) {
        IERC20(path[0]).transferFrom(msg.sender, address(this), amountIn);
        uint256 out = amountIn * 2;               // real rate 2:1
        IERC20(path[1]).transfer(to, out);
        amounts = new uint256[](2);
        amounts[0] = amountIn;
        amounts[1] = out * 2;                     // misreported
    }
}

/// @dev Router that "succeeds" but sends nothing.
contract MockNoOpRouter {
    function swapExactTokensForTokens(
        uint256 amountIn, uint256, address[] calldata path, address, uint256
    ) external returns (uint256[] memory amounts) {
        IERC20(path[0]).transferFrom(msg.sender, address(this), amountIn);
        amounts = new uint256[](2);
        amounts[0] = amountIn;
        amounts[1] = 1;
    }
}

/// @dev Matches any selector, succeeds, returns no data and moves no tokens
///      (e.g. a contract mistaken for a router).
contract MockSilentRouter {
    fallback() external {}
}

/// @dev V2-style router that pulls only half the approved input (leaves an allowance).
contract MockPartialRouter {
    function swapExactTokensForTokens(
        uint256 amountIn, uint256, address[] calldata path, address to, uint256
    ) external returns (uint256[] memory amounts) {
        IERC20(path[0]).transferFrom(msg.sender, address(this), amountIn / 2);
        IERC20(path[1]).transfer(to, amountIn * 2);
        amounts = new uint256[](2);
        amounts[0] = amountIn / 2;
        amounts[1] = amountIn * 2;
    }
}

/// @dev Token whose balances can be cut by anyone (models a negative rebase).
contract MockSlashableERC20 is ERC20 {
    constructor(uint256 supply) ERC20("Slashable", "SLS") { _mint(msg.sender, supply); }

    function slash(address holder, uint256 amount) external { _burn(holder, amount); }
}

/// @dev V2-style router that takes the input, pays nothing, and slashes the caller's
///      existing tokenOut balance — the leg's output delta is negative.
contract MockSlashingRouter {
    function swapExactTokensForTokens(
        uint256 amountIn, uint256, address[] calldata path, address, uint256
    ) external returns (uint256[] memory amounts) {
        IERC20(path[0]).transferFrom(msg.sender, address(this), amountIn);
        MockSlashableERC20(path[1]).slash(msg.sender, 1 ether);
        amounts = new uint256[](2);
    }
}
