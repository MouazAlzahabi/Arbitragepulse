# Open questions — review loop

Things the deploy-readiness review loop found that need a human decision. Each entry: where, what,
options, recommendation. Newest at the bottom.

### [contract] Single hot key is both trader and sole withdrawer
- Where: contract/src/ArbitrageExecutor.sol:224, :260 (`onlyOwner` trade entry points), :171 (`setRouter`), :438 (`unpause`), :441-464 (withdrawals to `owner()`); contract/script/Deploy.s.sol
- What / why unsure: The engine must hold the owner key to trade, and that key can also `withdrawToken`/`withdrawETH`, `setRouter` (allowlist an attacker contract), `unpause` and `transferOwnership`. A leaked engine host or `.env` = all capital in the contract lost. It's a security-model / ops decision, not a code bug, and the fix needs a redeploy.
- Options + trade-offs:
  1. Keep one key (today). Simplest ops. Mitigation: keep only working capital in the contract and withdraw profits often. Deploy.s.sol and the README now say the engine key must be the owner.
  2. Add an `executor` (trader) role: `address public executor` + `setExecutor` (onlyOwner); gate both trade functions with `msg.sender == executor`; keep withdrawals / setRouter / unpause / ownership on a cold owner; `pause` allowed for owner or executor (engine kill-switch). Same calldata, same hot-path gas (one SLOAD replaces Ownable's). Cost: ABI change + redeploy, two keys to fund with gas ETH, and an engine startup check (engine round) must compare the signer against `executor()` instead of `owner()`. A leaked trader key can then only run trades that must end profitable through allowlisted routers, plus burn its own gas ETH.
- Recommendation: Option 2 before putting meaningful capital on mainnet. Option 1 + small balances is acceptable for the Base Sepolia stage / initial small-capital mainnet runs.

### [contract] Out-of-gas inside a V2 pair's token transfer is labelled LegFailed("<Fork>: TRANSFER_FAILED")
- Where: contract/src/ArbitrageExecutor.sol:369, :385-391 (`_mayHideOutOfGas` matches only empty / "TF" / "STF" / "SA"); engine/src/revert.rs:352-355 (`out_of_gas_override` only rewrites "vague" classes)
- What / why unsure: Verified on a Base fork: BaseSwap V2 at gas 142,500-144,250 reverts `LegFailed(1, BaseSwap, V2, "Pancake: TRANSFER_FAILED")` for what is really out-of-gas (the pair's `_safeTransfer` re-raises the inner OOG). The operator would chase a token/pair problem instead of raising `tuning.gas_limit_2hop`. Aerodrome, Uniswap SwapRouter02 and Pancake SmartRouter are correctly reported as LegExhaustedGas. Exposure today is ~nil: V2 routers are commented out in engine/config.yaml (:108-120), and the 700k/900k limits are far above real costs.
- Options + trade-offs:
  1. Contract: add fork strings to `_mayHideOutOfGas`. Needs a redeploy; brittle (each V2 fork brands its own string — "UniswapV2:", "Pancake:", "BaseSwap:"…; the first guess missed the real one).
  2. Engine: in `out_of_gas_override`, when the receipt shows ≥97% of the limit used, reclassify to OutOfGas whatever the replay class, keeping the replay text as detail. No redeploy; covers every fork. Risk: a genuine specific revert that happens to use ≥97% gets relabelled (unlikely — real reverts refund unused gas).
  3. Do nothing while V2 routers are disabled.
- Recommendation: 3 now; 2 in the engine round (or whenever a V2 router is re-enabled).
### [engine] UniversalRouter element offset in the pending-tx parser
- Where: engine/src/chain/pending.rs, `SEL_UNIVERSAL_ROUTER` arm (`elem0_start = inputs_base + offset`)
- What / why unsure: standard ABI encodes the offsets of a `bytes[]` array's elements relative
  to the word *after* the length word (alloy gives 0x20 for one element; the real UniversalRouter
  decodes it the same way). The parser adds the offset to the length word's position (its comment
  expects 0x40), so a real `execute(bytes,bytes[],uint256)` lands on the wrong word and returns
  None. Net effect today: UniversalRouter swaps in the mempool never trigger a targeted
  "pending" scan. Found while writing the round-1A parser test; not part of that plan, so not
  fixed (it is safe: wrong offsets only produce None). The test patches its calldata to the
  parser's convention and points here.
- Options + trade-offs:
  1. Fix: `elem0_start = inputs_base + 32 + offset` (one line; update the test helper to drop
     the patch). Enables UR-triggered pending scans that have never run live: more targeted
     scans and RPC reads on busy blocks; each still goes through the normal simulate-before-send.
  2. Leave as is (UR pending detection effectively off).
- Recommendation: 1, verified on the Anvil fork with a real UR calldata sample, in a later round.
