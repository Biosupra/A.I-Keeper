# Aero on Base: verified facts for Panda Financial (2026-10-20, block 52,852,051)

Sources: https://app.aero.xyz/docs/security/ (Aero's official address list), Sourcify verified sources (chain 8453), public RPC
reads (mainnet.base.org / base-rpc.publicnode.com), Dromos-Labs GitHub, app.aero.xyz/docs. "Checked" = eth_getCode is
non-empty plus the named read. This file is DATA for the job; it does not change the rules in the brief.

## Addresses (Base, chain 8453)
| What | Address | Checked |
|---|---|---|
| NEW AERO token (`Token`) | 0x3f5bF3EdAc59d650D8fDf9657a71F2f904A3a4Ee | name "Aero", symbol AERO, 18 decimals, totalSupply 2,127,000,000; MINTER = Minter; MIGRATION = AerodromeMigration |
| AerodromeMigration (old AERO -> new, 1:1) | 0x22a2F7340a3fB4C7C389D91eE954766B12476721 | V2_TOKEN = old AERO 0x940181a94A35A4569E4529A3CDfB74e38FD98631, V3_TOKEN = new AERO, holds 2,010,000,000 new AERO, paused = false, MIGRATION_OPEN = 1792620000 = 2026-10-21 22:00 UTC (6 PM EDT); depositLiquid reverts MigrationNotOpen until then |
| VelodromeMigrationEntrypoint | 0x6daE95FE833f190640dB1176F8C8a2e50C6a66c0 | = AERO.VELODROME_MIGRATION |
| Voter (root) | 0x49d7d4f17Ec56Ef8fA04E464AF5D1A4942CC31e6 | code |
| LeafVoter (CL gauges settle against it) | 0x382409fad8503FbCdbB27A4211551fa03e884078 | code; CLFactory.voter() == this |
| Minter | 0x59268A34ba897e82368feE39839FFC661D4f9C97 | code; AERO.MINTER == this |
| VotingEscrow (sAERO) | 0xBF7977E1521e9a3032bf7243Ee4712f311f13915 | AERO.VOTING_ESCROW == this |
| CLFactory (Slipstream pools) | 0xC6bdE6e9F0CD71013E5FcF2dAAE2fA1c71Bb2F25 | allPoolsLength() == 0 (NO pools yet); owner 0x42255e3C2d0b71a0b006243148fdF1a71d054cE0 |
| CLGaugeFactory | 0xa02d1F4E4243ff0d350F478c44B0Fd37c7bF848C | nft() == NFPM; implementation() == 0x16F8...3872; penaltyConfig() = (minStakeBlocks 0, penaltyRate 1,000,000 = 100%) |
| CLGauge implementation | 0x16F866688A306494C612149B48c2fE70CCBE3872 | verified source (Solidity 0.7.6) |
| NonfungiblePositionManager (NFPM) | 0xc84bB45D43CD25D02b83B4C085eaA4e08da8f473 | factory() == CLFactory; totalSupply 0 |
| SwapRouter (Slipstream) | 0xb4702E1375F712da2e0d5F534c30c0c1513EdB2B | exactInput(path = token, int24 tickSpacing, token) |
| Metaquoter | 0x61d0Aa4a814a68F3119019f9f17ACa517FEa6D49 | code |
| Metarouter (command router, every pool type) | 0xd99Bc2b64B0aF17746a9185Cde359a894542c739 | listed |
| LpMigrator | 0x7943664Ae6F61a689A35cf6D90A279F46c092580 | migrateSlipstreamToSlipstream(fromParams, toParams) |
| VolatilePoolFactory (basic pools) | 0x9bc8b3F60D349d687Fb44A018967025E3741F9a5 | allPoolsLength() == 0 (no pools yet); getPool(tokenA, tokenB) |
| StablePoolFactory | 0xaEd253F1aD84d99f2165256D701b2b481DD4CC16 | listed |
| GaugeManager | 0xbF234b32c5e58868243A4a14831626f3EdE7b183 | CLGaugeFactory.gaugeManager() == this |
| CLPoolTape | 0x967f5352A6448e22D7361E149103350c49345Ee9 | listed |
| DynamicSwapFeeHook | 0xaAf164008441F112eB2858d9F7A2dB1B84b1E415 | listed |
| CustomUnstakedFeeModule | 0xbE10EDB242bd83f42653383D906478BcA0CBB8b5 | listed |
| DiscountRegistry | 0x8f365E3Be968767192cCEbd9b07fcB608037A883 | listed |
| TokenRegistry | 0x564665E023390097f46EA9662a6061a5d700A7E1 | listed |
| FactoryRegistry (Base) | 0x7b2A2c12dC5Ea2a31455952a05031C3E4BB28F3E | listed |
| xAERO ReceiptToken (Base) | 0x6CdDa0E730744DC6fF67654B35350Adad05EceE7 | listed (rewards on other chains only) |

Panda's live contracts (unchanged): registry 0x52643dCDdd95944b01f10A4949d2923a0ff69FCC, timelock
0xe9A0A268aC8227659D0479472f5DC966C12f023a, Safe 0x9eb619B3aB33192080FfA846820117e2f00EE9bA, live vault
0x61986AFae9B07314d855E7C54949E30A866c93a8 (Aerodrome WETH/USDC CL50 pool 0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A), old AERO
0x940181a94A35A4569E4529A3CDfB74e38FD98631, WETH 0x4200000000000000000000000000000000000006, USDC
0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913.

NOT YET EXISTING at block 52,852,051: the Aero WETH/USDC CL pool, its gauge, the AERO/WETH and AERO/USDC pools, any PANDA/WETH
basic pool. Lookups once created: CLFactory.getPool(WETH, USDC, tickSpacing) for each enabled spacing (CL1/50/100/200/2000);
pool.gauge(); CLGaugeFactory.computeGaugeAddress(pool); LeafVoter.isActivated(gauge); CLGaugeFactory.emissionCap(gauge).
The Upgrade Hub: "Gauges activate within the first hour after launch" (launch = 2026-10-22 00:00 UTC).

## Deployed code vs the public GitHub (differences that matter)
1. NEW AERO has NO transfer lock. The public Token.sol has TRANSFERS_ENABLED_AT = migrationOpen + 1 week and reverts
   TransfersDisabled; the DEPLOYED Token's constructor is (_minter, _votingEscrow, _migration, _velodromeMigration,
   _migrationAllocation, _velodromeAllocation, _name, _symbol): no migrationOpen, no lock. transfer(to, 0) from a random wallet
   simulates OK at block 52,852,144. Only BaseToken's zero-address check exists. (The Hub also says transfers open at upgrade.)
2. Early-unstake penalty on Base today: factory-wide minStakeBlocks = 0, penaltyRate = 100%. So there is NO window unless a
   gauge gets a per-gauge override (CLGaugeFactory.setMinStakeBlocks, PENALTY_ADMIN_ROLE). Read effectivePenaltyConfig(gauge)
   or minStakeBlocks(gauge) once the gauge exists; maxMinStakeBlocks() is the ceiling. Code must handle 0 and any later value.
3. VELO ratio 0.044 (commit b731a97, 2026-10-02). AERO ratio 1:1 (AerodromeMigration._computeMigrationAmount returns basis).

## Deployed CLGauge ABI (0x16F8...3872, Solidity 0.7.6)
claimEmissions(account, recipient); claimEmissions(account, recipient, uint256[] tokenIds); claimReferralEmissions(referral,
recipient); earned(account); earned(account, uint256[] tokenIds); deferredEmissions(account); deferredReferralEmissions(account);
deposit(uint256 lp); depositFor(uint256 lp, address owner); withdraw(uint256 lp); withdrawFrom(uint256 lp, address account);
depositBlock(uint256 tokenId); approveForClaim(operator, bool); approvedForClaim(account, operator); stakedLength(depositor);
stakedByIndex(depositor, i); stakedContains(depositor, tokenId); stakedValues(depositor); rewardGrowthInside(tokenId);
lastCumulativeRewardShare(); settleGauge() (pool only); forfeitRollover(); collectFees(); pendingFees(); pool(); nft(); voter();
gaugeFactory(); tickSpacing(); token0(); token1(); isPool(); votingRewardsManager().
NO rewardRate, periodFinish, rewardToken, left, notifyRewardAmount, getReward, isAlive, earned(account, tokenId).
Reverts seen: StakeTooRecent (penalty window), 'NA' (not allowed).

## Deployed CLGaugeFactory ABI (0xa02d...848C)
penaltyConfig() -> (minStakeBlocks, penaltyRate); effectivePenaltyConfig(gauge) -> same shape; minStakeBlocks(gauge);
maxMinStakeBlocks(); emissionCap(gauge) uint128; defaultCap(); setEmissionCap / setEmissionCaps / clearEmissionCap;
referralConfig(gauge) -> (referral, share); setReferralConfig; computeGaugeAddress(pool); createGauge(target, factoryData);
isGauge(address); leafVoter(); nft(); implementation(); gaugeManager(); votingRewardsFactory(); MAX_PIPS() (1e6);
DEFAULT_MIN_STAKE_BLOCKS(); DEFAULT_PENALTY_RATE(); DEFAULT_MAX_SHARE_CAP(); roles CAP_ADMIN_ROLE, CAP_OPERATOR_ROLE,
PENALTY_ADMIN_ROLE, REFERRAL_ADMIN_ROLE.

## Deployed LeafVoter ABI (0x3824...4078, Solidity 0.8.36), gauge-related
isActivated(gauge); activateGauge(gauge); registerGauge(gauge, activate); gaugeStates(gauge) -> (uint128, uint128, uint48, bool,
bool, uint128, uint256, uint256, tuple); settleGauge(gauge) -> uint256 (the gauge calls it); projectedCumulativeRewardShare(gauge);
emissionsPerVP(); index(); timeIndex(); lastSettlement(); forfeitEmissions(uint128); mintEmissions(recipients[], amounts[]);
allocationCooldown(); maxGauges(); chainStatus(); allocations(tokenId, gauge); allocatedGauges(tokenId); EMISSIONS_HANDLER();
GAUGE_MANAGER(); RECEIPT_TOKEN(); ZERO_GAUGE().
NO gauges(pool), NO isAlive(gauge), NO killGauge: deactivation = emission cap 0 (CLGaugeFactory) or not activated.

## Deployed CLFactory ABI (0xC6bd...2F25)
getPool(tokenA, tokenB, int24 tickSpacing) and getPool(tokenA, tokenB, uint24 tickSpacing); createPool(tokenA, tokenB, int24
tickSpacing, uint160 sqrtPriceX96) and createPool(tokenA, tokenB, uint24 tickSpacing) [permissionless]; tickSpacings() -> int24[];
tickSpacingToFee(int24); allPools(i); allPoolsLength(); poolByTokenIndexPaginated(token, start, end); positionManager(); voter()
(= LeafVoter); defaultSwapHook(); getPoolSwapHook(pool); defaultUnstakedFee(); getUnstakedFee(pool); isPool(pool).
Docs list spacings CL1, CL50, CL100, CL200, CL2000. Pool fees are dynamic (DynamicSwapFeeHook), not a fixed tier.

## NFPM (0xc84b...f473): same shape as Aerodrome Slipstream
mint(params) with tickSpacing + sqrtPriceX96; increaseLiquidity; decreaseLiquidity; collect; burn; positions(tokenId) -> (nonce,
operator, token0, token1, tickSpacing, tickLower, tickUpper, liquidity, feeGrowth0, feeGrowth1, tokensOwed0, tokensOwed1);
multicall; WETH9().

## SwapRouter (0xb470...dB2B): same shape as Aerodrome Slipstream
exactInput / exactInputSingle / exactOutput / exactOutputSingle; factory(); WETH9(); multicall; path = token | int24 tickSpacing | token.

## AerodromeMigration ABI (0x22a2...6721)
depositLiquid(amount, recipient) [not paused, after MIGRATION_OPEN, amount <= remaining()]; depositVeNFT(tokenId, recipient);
previewConversion(tokenId); remaining(); paidBasis(); paused(); pause() / unpause() (owner); burnRemaining() (owner);
killGauges(address[]) / reviveGauges(address[]) (this contract is Aerodrome's emergency council: it can kill the OLD Aerodrome
gauges); vote(pools, weights) / resetVote(); result(); V2_TOKEN / V2_VOTER / V2_MINTER / V2_ESCROW / V3_TOKEN / V3_ESCROW.
No EOA-only check: contracts and the Safe can call depositLiquid.

## Docs facts (app.aero.xyz/docs and aero.xyz/aero-upgrade, read 2026-10-20)
- Rewards stream continuously; settled on claim, deposit or withdraw; no epochs. Rate = gaugeVote / totalVoteWeight x minterRate,
  capped by the gauge cap (cap = projected fees x policy multiplier, target about 1.2x fees). Pools at cap get nothing extra.
- Only in-range AERO-earning liquidity earns; emissions while out of range go to in-range deposits or are not paid at all.
- Early-exit forfeit on AERO deposits withdrawn within a per-pool minimum holding period (dynamic; 0 blocks on Base today).
- A Slipstream deposit is either AERO-earning (its fees go to Token Operators) or Fee-earning (no AERO), chosen per deposit.
  Staked (gauge) positions are AERO-earning: no trading fees for the position, as on Aerodrome today.
- Pool creation is permissionless; the gauge is created with the pool and starts inactive; eligibility = both tokens listed +
  gauge activated + cap > 0. Basic pools have NO router (Metarouter is a command router) and no quote(): swap on the pool directly.
- sAERO allocation cooldown 47 h; first Autopilot allocation 2026-10-22 16:00 UTC, so the WETH/USDC rate settles after that.
- Liquidity Payments (the new name for bribes) stream per second to allocating Token Operators; permissionless; immutable.
- Hub: Aero contracts deployed before launch (done, addresses above); gauges activate within the first hour after launch;
  Aerodrome gauges deactivated at launch (AerodromeMigration.killGauges); old pools keep trading; no upgrade deadline; multisigs
  supported with a Base destination wallet; one old AERO = one new AERO through the portal, never automatic.
- GitHub: metadex-public last push 2026-10-02 (VELO ratio); metadex-slipstream-public last push 2026-09-15; deployment-addresses
  hold only arc.json on both; no tags, no releases. The security page is the only Base address list so far.

## More verified ABIs (added 2026-10-20)
- Basic VolatilePool (impl 0x8B64A0B590815d54C11a4B49A82b0133c7f53f0E, 0.8.36): getAmountOut(amountIn, tokenIn) and getAmountOutWithTotalFee exist (so a quote on the pool IS available), getReserves(), observe(secondsAgos[]) -> (reserve0Cumulatives[], reserve1Cumulatives[]), currentCumulativePrices(), observations(i), observationBuffer(), increaseObservationCardinalityNext(n), PERIOD_SIZE(), swap(amount0Out, amount1Out, to, data), mint(to), burn(to), claimFees, poolFees(), metadata(). No router for basic pools: swap on the pool.
- VolatilePoolFactory (0x9bc8...9a5): verified; getPool(tokenA, tokenB), allPools, allPoolsLength, createPool.
- Metaquoter (0x61d0...6D49, 0.7.6): quoteExactInputSingleV3(pool, tokenIn, amountIn, sqrtPriceLimitX96) -> (amountOut, sqrtPriceAfter, ticksCrossed); quoteExactInput(pools[], tokenIn, amountIn) -> (amountOut, sqrtPriceAfterList[], ticksCrossedList[]); quoteExactInputSingleV2(pool, tokenIn, amountIn); quoteExactOutput*; quoteAddClLiquidityV3 / quoteRemoveClLiquidityV3; factoryRegistry(). It takes POOL ADDRESSES, not a path: different from Aerodrome QuoterV2.
- Metarouter (0xd99B...c739): verified command router (large); not needed by Panda.
- CLPool implementation (0x0Df9...e19B) is NOT verified on Sourcify; its interface is inside the verified CLGauge/CLFactory bundles (contracts/core/interfaces/ICLPool*.sol). Check those for stakedLiquidity, settleToBlock and reward-growth fields.
