// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/ArbitrageExecutor.sol";

/**
 * @title Deploy ArbitrageExecutor
 *
 * Usage (rpc/etherscan aliases from foundry.toml; verification needs ETHERSCAN_API_KEY):
 *   # Deploy to Anvil (local fork)
 *   forge script script/Deploy.s.sol --rpc-url local --broadcast
 *
 *   # Deploy to Base Sepolia (testing stage)
 *   forge script script/Deploy.s.sol --rpc-url base_sepolia --broadcast --verify
 *
 *   # Deploy to Base
 *   forge script script/Deploy.s.sol --rpc-url base --broadcast --verify
 *
 *   # Dry-run (simulate only, no broadcast)
 *   forge script script/Deploy.s.sol --rpc-url base
 */
contract DeployExecutor is Script {
    function run() external {
        uint256 deployerKey = vm.envUint("PRIVATE_KEY");
        address deployer = vm.addr(deployerKey);

        console.log("Chain id:", block.chainid);
        console.log("Deployer:", deployer);
        console.log("Balance:", deployer.balance);

        vm.startBroadcast(deployerKey);

        ArbitrageExecutor executor = new ArbitrageExecutor();

        vm.stopBroadcast();

        string memory chain = block.chainid == 8453 ? "base" : block.chainid == 84532 ? "base-sepolia" : "<chain>";

        console.log("");
        console.log("======================================");
        console.log("ArbitrageExecutor deployed to:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("======================================");
        console.log("");
        console.log("Next steps:");
        console.log("  1. Verify (if --verify was not passed):");
        console.log(string.concat(
            "       forge verify-contract ", vm.toString(address(executor)),
            " src/ArbitrageExecutor.sol:ArbitrageExecutor --chain ", chain, " --watch"
        ));
        console.log("  2. Whitelist each router with its type (0 = V2, 1 = V3 SwapRouter02, 4 = Aerodrome):");
        console.log(string.concat(
            "       cast send ", vm.toString(address(executor)), " 'setRouter(address,uint8)' <router> <type>"
        ));
        console.log("  3. Fund:    send the pair's start token (e.g. USDC) to", address(executor));
        console.log("  4. Engine:  chain.contract_address =", address(executor));
        console.log("     Trades are onlyOwner: the engine's PRIVATE_KEY must be this owner:", executor.owner());
        console.log("     The same key can withdraw everything - keep only working capital in the contract.");
    }
}
