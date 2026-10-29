// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "@openzeppelin/contracts/token/ERC20/IERC20.sol";

interface IV3Callback {
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

interface IUnlockCallback {
    function unlockCallback(bytes calldata data) external returns (bytes memory);
}

/**
 * @dev Mock concentrated-liquidity pool: sends `amountIn * num / den` of the other token
 *      first, then asks the caller for its input through the swap callback (like Uniswap
 *      V3), and checks it was paid. `overcharge` asks for more than the input (an
 *      attacker pool); `pancake` uses PancakeSwap's callback name.
 */
contract MockCLPool {
    address public token0;
    address public token1;
    uint256 public num;
    uint256 public den;
    uint256 public overcharge;
    bool public pancake;

    constructor(address a, address b, uint256 _num, uint256 _den) {
        (token0, token1) = a < b ? (a, b) : (b, a);
        num = _num;
        den = _den;
    }

    function setOvercharge(uint256 extra) external { overcharge = extra; }
    function setPancake(bool p) external { pancake = p; }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata data)
        external returns (int256 amount0, int256 amount1)
    {
        require(amountSpecified > 0, "exact input only");
        uint256 amountIn = uint256(amountSpecified);
        uint256 out = amountIn * num / den;
        (address tIn, address tOut) = zeroForOne ? (token0, token1) : (token1, token0);
        IERC20(tOut).transfer(recipient, out);
        uint256 ask = amountIn + overcharge;
        (amount0, amount1) = zeroForOne ? (int256(ask), -int256(out)) : (-int256(out), int256(ask));
        uint256 before = IERC20(tIn).balanceOf(address(this));
        if (pancake) IV3Callback(msg.sender).pancakeV3SwapCallback(amount0, amount1, data);
        else IV3Callback(msg.sender).uniswapV3SwapCallback(amount0, amount1, data);
        require(IERC20(tIn).balanceOf(address(this)) >= before + ask, "IIA");
    }
}

/**
 * @dev Mock V2-style pair: reserves for the Uniswap V2 formula, a fixed-rate getAmountOut
 *      (Solidly-style), and a swap that only pays out if the input already arrived and
 *      x·y does not drop below what the formula allows (checked against `minIn`).
 */
contract MockPair {
    address public token0;
    address public token1;
    uint256 public num;
    uint256 public den;
    uint256 public r0;
    uint256 public r1;

    constructor(address a, address b, uint256 _num, uint256 _den) {
        (token0, token1) = a < b ? (a, b) : (b, a);
        num = _num;
        den = _den;
    }

    function sync() external {
        r0 = IERC20(token0).balanceOf(address(this));
        r1 = IERC20(token1).balanceOf(address(this));
    }

    function getReserves() external view returns (uint256, uint256, uint256) { return (r0, r1, block.timestamp); }

    function getAmountOut(uint256 amountIn, address) external view returns (uint256) { return amountIn * num / den; }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        uint256 in0 = IERC20(token0).balanceOf(address(this)) - r0;
        uint256 in1 = IERC20(token1).balanceOf(address(this)) - r1;
        require(in0 > 0 || in1 > 0, "INSUFFICIENT_INPUT");
        if (amount0Out > 0) IERC20(token0).transfer(to, amount0Out);
        if (amount1Out > 0) IERC20(token1).transfer(to, amount1Out);
        r0 = IERC20(token0).balanceOf(address(this));
        r1 = IERC20(token1).balanceOf(address(this));
    }
}

/**
 * @dev Mock Uniswap V4 PoolManager flash accounting: `take` adds a debt, `sync` + transfer +
 *      `settle` pays it, and `unlock` reverts unless every debt is cleared. No fee.
 */
contract MockPoolManager {
    mapping(address => int256) public debt;
    address[] internal touched;
    address internal synced;
    uint256 internal syncedBalance;
    bool internal unlocked;

    function unlock(bytes calldata data) external returns (bytes memory ret) {
        require(!unlocked, "AlreadyUnlocked");
        unlocked = true;
        ret = IUnlockCallback(msg.sender).unlockCallback(data);
        for (uint256 i = 0; i < touched.length; i++) require(debt[touched[i]] == 0, "CurrencyNotSettled");
        delete touched;
        unlocked = false;
    }

    function take(address currency, address to, uint256 amount) external {
        require(unlocked, "ManagerLocked");
        debt[currency] += int256(amount);
        touched.push(currency);
        IERC20(currency).transfer(to, amount);
    }

    function sync(address currency) external {
        synced = currency;
        syncedBalance = IERC20(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        require(unlocked, "ManagerLocked");
        paid = IERC20(synced).balanceOf(address(this)) - syncedBalance;
        debt[synced] -= int256(paid);
    }

    /// Lets a test call any contract's unlockCallback as the PoolManager outside an unlock.
    function poke(address target, bytes calldata data) external {
        IUnlockCallback(target).unlockCallback(data);
    }
}
