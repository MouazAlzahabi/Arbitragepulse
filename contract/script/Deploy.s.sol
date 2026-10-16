// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/ArbitrageExecutor.sol";

/**
 * @title Deploy ArbitrageExecutor
 *
 * Usage:
 *   # Deploy to Anvil (local fork)
 *   forge script script/Deploy.s.sol --rpc-url http://127.0.0.1:8545 --broadcast
 *
 *   # Deploy to Base Sepolia (testing stage)
 *   forge script script/Deploy.s.sol --rpc-url $BASE_SEPOLIA_RPC --broadcast
 *
 *   # Deploy to Base
 *   forge script script/Deploy.s.sol --rpc-url $BASE_RPC --broadcast --verify
 *
 *   # Dry-run (simulate only, no broadcast)
 *   forge script script/Deploy.s.sol --rpc-url $BASE_RPC
 */
contract DeployExecutor is Script {
    function run() external {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(deployerKey);

        console.log("Deployer:", deployer);
        console.log("Balance:", deployer.balance);

        vm.startBroadcast(deployerKey);

        ArbitrageExecutor executor = new ArbitrageExecutor();

        vm.stopBroadcast();

        console.log("");
        console.log("======================================");
        console.log("ArbitrageExecutor deployed to:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("======================================");
        console.log("");
        console.log("Next steps:");
        console.log("  1. Verify:  forge verify-contract", address(executor), "ArbitrageExecutor --chain base");
        console.log("  2. Fund:    Send WETH/USDC to", address(executor));
        console.log("  3. Update:  CONTRACT_ADDRESS=", address(executor));
    }
}
