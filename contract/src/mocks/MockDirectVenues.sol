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
 *      Currency address(0) is native ETH (`take` sends ETH, `settle` with value pays it).
 *      `swap` trades at a fixed rate per PoolId (`setRate`): exact input only.
 */
contract MockPoolManager {
    struct PoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
    struct SwapParams { bool zeroForOne; int256 amountSpecified; uint160 sqrtPriceLimitX96; }

    mapping(address => int256) public debt;
    address[] internal touched;
    address internal synced;
    uint256 internal syncedBalance;
    bool internal unlocked;
    mapping(bytes32 => uint256) public rateNum;
    mapping(bytes32 => uint256) public rateDen;
    /// Input actually used per swap, as a fraction (1/1 = all): models a pool running dry.
    uint256 public useNum = 1;
    uint256 public useDen = 1;

    receive() external payable {}

    function setRate(PoolKey calldata key, uint256 num, uint256 den) external {
        bytes32 id = keccak256(abi.encode(key));
        rateNum[id] = num;
        rateDen[id] = den;
    }

    function setInputUse(uint256 num, uint256 den) external { useNum = num; useDen = den; }

    function swap(PoolKey calldata key, SwapParams calldata params, bytes calldata) external returns (int256) {
        require(unlocked, "ManagerLocked");
        require(params.amountSpecified < 0, "exact input only");
        bytes32 id = keccak256(abi.encode(key));
        require(rateDen[id] != 0, "PoolNotInitialized");
        uint256 amountIn = uint256(-params.amountSpecified) * useNum / useDen;
        uint256 out = amountIn * rateNum[id] / rateDen[id];
        (address cIn, address cOut) = params.zeroForOne ? (key.currency0, key.currency1) : (key.currency1, key.currency0);
        debt[cIn] += int256(amountIn);
        debt[cOut] -= int256(out);
        touched.push(cIn);
        touched.push(cOut);
        (int128 d0, int128 d1) = params.zeroForOne
            ? (-int128(int256(amountIn)), int128(int256(out)))
            : (int128(int256(out)), -int128(int256(amountIn)));
        return (int256(d0) << 128) | int256(uint256(uint128(d1)));
    }

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
        if (currency == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            require(ok, "ETH transfer");
        } else {
            IERC20(currency).transfer(to, amount);
        }
    }

    function sync(address currency) external {
        synced = currency;
        syncedBalance = IERC20(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        require(unlocked, "ManagerLocked");
        if (msg.value > 0) {
            debt[address(0)] -= int256(msg.value);
            return msg.value;
        }
        paid = IERC20(synced).balanceOf(address(this)) - syncedBalance;
        debt[synced] -= int256(paid);
    }

    /// Lets a test call any contract's unlockCallback as the PoolManager outside an unlock.
    function poke(address target, bytes calldata data) external {
        IUnlockCallback(target).unlockCallback(data);
    }
}

/// @dev Minimal WETH9 (deposit / withdraw / ERC20), etched at the OP Stack predeploy in tests.
contract MockWETH {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    receive() external payable { deposit(); }

    function deposit() public payable { balanceOf[msg.sender] += msg.value; }

    function withdraw(uint256 amount) external {
        balanceOf[msg.sender] -= amount;
        (bool ok,) = msg.sender.call{value: amount}("");
        require(ok, "ETH transfer");
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        if (from != msg.sender) allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}
