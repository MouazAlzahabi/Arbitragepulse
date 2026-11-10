// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/ArbitrageExecutorV2.sol";

/**
 * @title Deploy ArbitrageExecutorV2 (and register its V4 pools)
 *
 * The deployer (PRIVATE_KEY) becomes the immutable owner: use the engine's key.
 * V4_KEYS (optional): abi.encode(PoolKey[]) as hex — e.g. the keys registered on the previous
 * executor — registered in batches of 50 with setV4Pools.
 *
 *   PRIVATE_KEY=... V4_KEYS=0x... forge script script/DeployV2.s.sol --rpc-url <base rpc> --broadcast --slow
 */
contract DeployExecutorV2 is Script {
    address constant POOL_MANAGER = 0x498581fF718922c3f8e6A244956aF099B2652b2b; // Base

    function run() external {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        require(block.chainid == 8453, "Base mainnet only");
        bytes memory raw = vm.envOr("V4_KEYS", bytes(""));
        IPoolManagerV4b.PoolKey[] memory keys = raw.length == 0
            ? new IPoolManagerV4b.PoolKey[](0)
            : abi.decode(raw, (IPoolManagerV4b.PoolKey[]));

        console.log("Deployer / owner:", vm.addr(deployerKey));
        vm.startBroadcast(deployerKey);
        ArbitrageExecutorV2 executor = new ArbitrageExecutorV2(POOL_MANAGER);
        for (uint256 i = 0; i < keys.length; i += 50) {
            uint256 n = keys.length - i < 50 ? keys.length - i : 50;
            IPoolManagerV4b.PoolKey[] memory batch = new IPoolManagerV4b.PoolKey[](n);
            for (uint256 j = 0; j < n; j++) batch[j] = keys[i + j];
            executor.setV4Pools(batch);
        }
        vm.stopBroadcast();

        console.log("ArbitrageExecutorV2 deployed to:", address(executor));
        console.log("V4 pools registered:", keys.length);
        console.log("Engine: chain.contract_address =", address(executor), "and chain.executor_v2: true");
    }
}
