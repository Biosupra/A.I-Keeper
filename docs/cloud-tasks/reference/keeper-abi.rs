//! Contract bindings (ethers `abigen!`). Each binding lives in its own module so the generated
//! helper types (`FactoryCall`, `FactoryReturn`, ...) of different contracts cannot collide.

pub mod pool {
    use ethers::prelude::*;
    abigen!(
        IPool,
        r#"[
            function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, bool unlocked)
            function token0() external view returns (address)
            function token1() external view returns (address)
            function tickSpacing() external view returns (int24)
            function fee() external view returns (uint24)
            function stakedLiquidity() external view returns (uint128)
            function observe(uint32[] secondsAgos) external view returns (int56[] tickCumulatives, uint160[] secondsPerLiquidityCumulativeX128s)
        ]"#
    );
}

pub mod position_manager {
    use ethers::prelude::*;
    abigen!(
        IPositionManager,
        r#"[
            function positions(uint256 tokenId) external view returns (uint96 nonce, address operator, address token0, address token1, int24 tickSpacing, int24 tickLower, int24 tickUpper, uint128 liquidity, uint256 feeGrowthInside0LastX128, uint256 feeGrowthInside1LastX128, uint128 tokensOwed0, uint128 tokensOwed1)
            function factory() external view returns (address)
        ]"#
    );
}

pub mod gauge {
    use ethers::prelude::*;
    abigen!(
        IGauge,
        r#"[
            function earned(address account, uint256 tokenId) external view returns (uint256)
            function stakedValues(address depositor) external view returns (uint256[])
            function rewardRate() external view returns (uint256)
            function depositTimestamp(uint256 tokenId) external view returns (uint256)
            function gaugeFactory() external view returns (address)
        ]"#
    );
}

/// CLGaugeFactory: holds the minimum stake time per pool and the penalty (bps) applied to rewards
/// claimed or withdrawn before it has passed. Both are admin-adjustable at any time.
pub mod gauge_factory {
    use ethers::prelude::*;
    abigen!(
        IGaugeFactory,
        r#"[
            function minStakeTimes(address pool) external view returns (uint256)
            function penaltyRate() external view returns (uint256)
        ]"#
    );
}

/// Aero's CLGauge (the merged Aerodrome + Velodrome protocol, Base, implementation 0x16F8...3872, verified source
/// in docs/cloud-tasks/sourcify/gaugeimpl.json). Own interface text. Unlike Aerodrome's gauge it has no
/// `rewardRate`, `periodFinish`, `rewardToken` or `earned(address, uint256)`: `earned(account, tokenIds[])` returns
/// the SUM over the ids plus the account's `deferredEmissions`, emissions are settled through the LeafVoter, and
/// the early-unstake window is `depositBlock(tokenId) + CLGaugeFactory.minStakeBlocks(gauge)` in BLOCKS.
pub mod aero_gauge {
    use ethers::prelude::*;
    abigen!(
        IAeroGauge,
        r#"[
            function earned(address account, uint256[] tokenIds) external view returns (uint256)
            function depositBlock(uint256 tokenId) external view returns (uint256)
            function stakedValues(address depositor) external view returns (uint256[])
            function deferredEmissions(address account) external view returns (uint256)
            function lastCumulativeRewardShare() external view returns (uint256)
            function gaugeFactory() external view returns (address)
            function pool() external view returns (address)
            function nft() external view returns (address)
            function voter() external view returns (address)
        ]"#
    );
}

/// Aero's CLGaugeFactory (0xa02d...848C): the early-unstake penalty that applies to a gauge, in blocks and pips
/// (1e6 = 100%), the per-gauge emission cap (AERO wei per second) and the referral cut taken out of every claim.
/// `effectivePenaltyConfig` returns the factory-wide config with the gauge's own `minStakeBlocks` override applied;
/// `minStakeBlocks(gauge)` is the fallback read of the same number.
pub mod aero_gauge_factory {
    use ethers::prelude::*;
    abigen!(
        IAeroGaugeFactory,
        r#"[
            struct PenaltyConfig { uint256 minStakeBlocks; uint256 penaltyRate; }
            function effectivePenaltyConfig(address gauge) external view returns (PenaltyConfig)
            function minStakeBlocks(address gauge) external view returns (uint256)
            function emissionCap(address gauge) external view returns (uint128)
            function referralConfig(address gauge) external view returns (address referral, uint256 share)
            function isGauge(address gauge) external view returns (bool)
            function leafVoter() external view returns (address)
            function nft() external view returns (address)
        ]"#
    );
}

/// Aero's LeafVoter on Base (0x3824...4078, verified source in docs/cloud-tasks/sourcify/leafvoter.json): the
/// emissions a CL gauge settles against. `projectedCumulativeRewardShare(gauge)` is what `settleGauge` would credit
/// now (AERO wei, cumulative, only ever rising); `gaugeStates(gauge).point` x `emissionsPerVP` / 1e18 is the
/// instantaneous rate before the gauge's cap (see `aero::voter_gauge_rate_wei_per_s`).
pub mod leaf_voter {
    use ethers::prelude::*;
    abigen!(
        ILeafVoter,
        r#"[
            struct Point { int128 bias; int128 slope; uint48 ts; uint128 permanentStakeBalance; }
            function projectedCumulativeRewardShare(address gauge) external view returns (uint256)
            function emissionsPerVP() external view returns (uint256)
            function isActivated(address gauge) external view returns (bool)
            function gaugeStates(address gauge) external view returns (uint128 ceiling, uint128 claimed, uint48 lastSettlement, bool isRegistered, bool isActivated, uint128 surplus, uint256 lastIndex, uint256 lastTimeIndex, Point point)
        ]"#
    );
}

/// The Aero PandaVault's linked library (PandaVaultLib, called on its OWN address as a plain view): the first block
/// at which unstaking `tokenId` forfeits nothing (0 = no window). The keeper's fallback for the window when the
/// gauge factory read fails; `VAULT_LIB_ADDRESS` names the library (deployments/<network>.json `vaultLib`).
pub mod vault_lib {
    use ethers::prelude::*;
    abigen!(
        IPandaVaultLib,
        r#"[
            function penaltyFreeBlock(address gauge, uint256 tokenId) external view returns (uint256)
        ]"#
    );
}

/// Aerodrome Slipstream QuoterV2 (same factory as the router). Called via eth_call to price swaps
/// before sending, so every swap the keeper requests carries a real minimum-output floor.
pub mod quoter {
    use ethers::prelude::*;
    abigen!(
        IQuoterV2,
        r#"[
            function quoteExactInput(bytes path, uint256 amountIn) external returns (uint256 amountOut, uint160[] sqrtPriceX96AfterList, uint32[] initializedTicksCrossedList, uint256 gasEstimate)
            function factory() external view returns (address)
        ]"#
    );
}

/// OP-stack GasPriceOracle predeploy (0x420000000000000000000000000000000000000F on Base): quotes the
/// L1 data fee a transaction will be charged on top of its L2 execution gas.
pub mod gas_oracle {
    use ethers::prelude::*;
    abigen!(
        IGasPriceOracle,
        r#"[
            function getL1Fee(bytes data) external view returns (uint256)
        ]"#
    );
}

/// Plain ERC-20 reads: the vault's two tokens are generic (WETH, cbBTC, ... and USDC), so their
/// decimals and symbols are read from the token contracts at startup, never assumed.
pub mod erc20 {
    use ethers::prelude::*;
    abigen!(
        IErc20,
        r#"[
            function decimals() external view returns (uint8)
            function symbol() external view returns (string)
            function balanceOf(address) external view returns (uint256)
        ]"#
    );
}

/// PandaVault (the upgradeable platform's GENERIC vault, PandaFinancial-Upgradable/contracts/PandaVault.sol
/// at 8a97b65, and the Aero vault of branch wip-aero-vault, whose keeper-facing surface is the same: `rebalance`,
/// `forceExit` and every view below keep their names and shapes; it adds the user-only `claimDeferred`, the
/// `RewardsDeferred` event and the errors `StakeTooRecent` (a rebalance inside the gauge's early-unstake window),
/// `AeroPairUnsupported` and `NotPendingOwner`): one token A (volatile) / token B (quote) Slipstream pool per vault. The functions the
/// keeper calls, the events it tracks and every custom error the vault AND its linked library
/// PandaVaultLib can revert with. The library runs by DELEGATECALL, so its reverts surface from the
/// vault address with the same selectors, so one list decodes both. The two OpenZeppelin errors the
/// vault can bubble up (SafeERC20, ReentrancyGuard) are listed too.
pub mod vault {
    use ethers::prelude::*;
    abigen!(
        IPandaVault,
        r#"[
            struct RebalanceParams { uint256 tokenId; bool isAToB; uint256 rebalanceAmountIn; uint256 rebalanceAmountOutMin; int24 newTickLower; int24 newTickUpper; uint256 amount0MinMint; uint256 amount1MinMint; uint256 decreaseAmount0Min; uint256 decreaseAmount1Min; uint256 deadline; }
            struct GuardRails { uint16 maxRebalanceSlippageBps; uint32 twapWindow; uint32 minRebalanceInterval; uint24 maxRangeSpacings; uint24 maxTwapDeviationTicks; }
            function rebalance(RebalanceParams calldata p) external returns (uint256 newTokenId)
            function forceExit(uint256[] calldata tokenIds) external returns (uint256 exited)
            function positionInfo(uint256 tokenId) external view returns (address owner, bool isZap, bool autoCompound)
            function reserves(uint256 tokenId) external view returns (uint256 a, uint256 b, uint256 aero)
            function lastRebalanceAt(uint256 tokenId) external view returns (uint256)
            function pendingUserAero(uint256 tokenId) external view returns (uint256)
            function feeBps(uint8 kind, address user) external view returns (uint256)
            function timeFeeCheckpoint(uint256 tokenId) external view returns (uint256)
            function spotTwapTicks() external view returns (int24 spotTick, int24 twapTick, uint24 maxDeviation)
            function rebalanceSwapFloor(bool isAToB, uint256 amountIn) external view returns (uint256)
            function compoundSwapFloor(uint256 aeroIn) external view returns (uint256)
            function compoundRoute() external view returns (address[] tokens, int24[] spacings, address[] pools)
            function guardRails() external view returns (GuardRails)
            function isShutdown() external view returns (bool)
            function openPositionCount() external view returns (uint256)
            function openPositionAt(uint256 i) external view returns (uint256)
            function MIN_COMPOUND_AERO() external view returns (uint256)
            function keeperBot() external view returns (address)
            function registry() external view returns (address)
            function pool() external view returns (address)
            function gauge() external view returns (address)
            function positionManager() external view returns (address)
            function swapRouter() external view returns (address)
            function tokenA() external view returns (address)
            function tokenB() external view returns (address)
            function aIsToken0() external view returns (bool)
            function tickSpacing() external view returns (int24)
            function aeroToken() external view returns (address)
            function claimDeferred(uint256 tokenId) external
            event PositionMinted(address indexed user, uint256 indexed tokenId, uint128 liquidity)
            event KeeperRebalanced(uint256 oldTokenId, uint256 newTokenId)
            event PositionWithdrawn(address indexed user, uint256 indexed tokenId)
            event EmergencyWithdrawn(address indexed user, uint256 indexed tokenId, uint256 amountA, uint256 amountB, uint256 aeroAmount)
            event RawNftWithdrawn(address indexed user, uint256 indexed tokenId, uint256 aeroAmount)
            event RewardsEarned(address indexed user, uint256 indexed tokenId, uint256 aeroAmount)
            event RewardsClaimed(address indexed user, uint256 indexed tokenId, uint256 aeroAmount, uint256 valueB)
            event RewardsCompounded(address indexed user, uint256 indexed tokenId, uint256 aeroIn, uint256 amountOut)
            event ReserveUpdated(address indexed user, uint256 indexed tokenId, uint256 a, uint256 b, uint256 aero)
            event AutoCompoundSet(address indexed user, uint256 indexed tokenId, bool enabled)
            event FeeCharged(address indexed user, uint8 indexed kind, address indexed token, address to, uint256 amount)
            event ForceExited(address indexed user, uint256 indexed tokenId, uint256 amountA, uint256 amountB, uint256 aeroAmount, bool fullySent)
            event MigratedOut(address indexed user, uint256 indexed tokenId, address indexed toVault, uint256 newTokenId, uint256 amountA, uint256 amountB)
            event MigratedIn(address indexed user, uint256 indexed tokenId, address indexed fromVault, uint256 amountA, uint256 amountB)
            event ToppedUp(address indexed user, uint256 indexed tokenId, uint256 amountA, uint256 amountB, uint128 liquidity)
            event RewardsDeferred(address indexed user, uint256 indexed tokenId, uint256 grossAero, uint256 netAero)
            error ZeroAddress()
            error Unauthorized()
            error UnknownPosition()
            error DeadlineExpired()
            error InvalidSwapAmount()
            error InvalidRoute()
            error PoolTokenMismatch()
            error RouterFactoryMismatch()
            error InconsistentDeployment()
            error DepositsClosed()
            error RebalancesPaused()
            error RebalanceTooSoon()
            error VaultIsShutdown()
            error NotShutdown()
            error BadMigrationTarget()
            error FeeCheckNeedsMoreGas()
            error InvalidSwapPath()
            error SlippageExceeded()
            error InvalidTickRange()
            error OneSidedMintRequiresSwap()
            error RangeMustContainPrice()
            error RangeTooWide()
            error SpotFarFromTwap()
            error TwapUnavailable()
            error SafeERC20FailedOperation(address token)
            error ReentrancyGuardReentrantCall()
            error StakeTooRecent()
            error AeroPairUnsupported()
            error NotPendingOwner()
        ]"#
    );
}

/// PandaRegistry: the platform-wide rebalance pause switch (the safety module flips it at once) and,
/// for the startup log, the emergency admin. Guard rails, the keeper and the shutdown flag are read
/// through the vault (`guardRails()`, `keeperBot()`, `isShutdown()`), which asks the registry for
/// THIS vault's values.
pub mod registry {
    use ethers::prelude::*;
    abigen!(
        IPandaRegistry,
        r#"[
            function rebalancesPaused() external view returns (bool)
            function emergencyAdmin() external view returns (address)
            function isShutdown(address vault) external view returns (bool)
        ]"#
    );
}

/// PandaTvlReader (fee policy v3): measures the platform's 30-day average size on chain and moves the fee
/// stage by itself, one step a week at most, when somebody samples it once a day. The keeper is that
/// somebody (TVL_READER_ADDRESS, set on ONE keeper process). A sample may take several `poke(maxPositions)`
/// calls (one transaction can value about 650 positions on Base); starting one reverts TooSoon within
/// SAMPLE_INTERVAL of the last sample, so the keeper reads `lastSampleAt` and `sweeping()` first.
pub mod tvl_reader {
    use ethers::prelude::*;
    abigen!(
        IPandaTvlReader,
        r#"[
            function poke(uint256 maxPositions) external returns (bool done, uint256 total, uint256 avg)
            function sweeping() external view returns (bool inProgress, uint256 vaultIndex, uint256 positionIndex, uint256 startedAt)
            function lastSampleAt() external view returns (uint256)
            function SAMPLE_INTERVAL() external view returns (uint256)
            function stage() external view returns (uint256)
            function sampleCount() external view returns (uint256)
            event Sampled(uint256 total, uint256 average, uint256 samples)
            event StageMoved(uint256 indexed from, uint256 indexed to, uint256 average)
            event SweepProgress(uint256 vaultIndex, uint256 positionIndex, uint256 valueE18)
            error TooSoon()
            error ZeroBatch()
        ]"#
    );
}

pub use erc20::IErc20;
pub use gas_oracle::IGasPriceOracle;
pub use tvl_reader::{IPandaTvlReader, SampledFilter, StageMovedFilter, SweepProgressFilter};
pub mod multicall {
    use ethers::prelude::*;
    abigen!(
        IMulticall3,
        r#"[
            struct Call3 { address target; bool allowFailure; bytes callData; }
            struct Result3 { bool success; bytes returnData; }
            function aggregate3(Call3[] calls) external payable returns (Result3[] returnData)
            function getBlockNumber() external view returns (uint256 blockNumber)
        ]"#
    );
}

pub use multicall::{Call3, IMulticall3};
pub use aero_gauge::IAeroGauge;
pub use aero_gauge_factory::IAeroGaugeFactory;
pub use gauge::IGauge;
pub use gauge_factory::IGaugeFactory;
pub use leaf_voter::ILeafVoter;
pub use vault_lib::IPandaVaultLib;
pub use pool::IPool;
pub use position_manager::IPositionManager;
pub use quoter::IQuoterV2;
pub use registry::IPandaRegistry;
pub use vault::{
    AutoCompoundSetFilter, EmergencyWithdrawnFilter, ForceExitedFilter, IPandaVault, IPandaVaultErrors, IPandaVaultEvents,
    KeeperRebalancedFilter, MigratedInFilter, MigratedOutFilter, PositionMintedFilter, PositionWithdrawnFilter, RawNftWithdrawnFilter, RebalanceParams,
    RewardsClaimedFilter, RewardsCompoundedFilter, RewardsEarnedFilter, ToppedUpFilter,
};

/// Base-wide market price sources (NOT the vault's pool), so AERO and ETH are priced the same way for
/// every vault: AERO per WETH from the new-factory AERO/WETH CL200 pool, and USDC per WETH from the
/// new-factory WETH/USDC CL50 pool. Both are read on-chain for token order and decimals at startup.
pub const DEFAULT_AERO_WETH_POOL: &str = "0x4e506648d493C8870f55e870480f92f2F33ecE51";
pub const DEFAULT_WETH_USDC_POOL: &str = "0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A";
/// Base mainnet tokens the market sources are expected to hold.
pub const BASE_WETH: &str = "0x4200000000000000000000000000000000000006";
pub const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
/// Aerodrome's AERO, the token DEFAULT_AERO_WETH_POOL holds. After the Aero merge (2026-10-22) one old AERO is
/// one new AERO through Aero's migration portal and the old pool keeps trading, so in Aero mode it still prices
/// the new AERO when no Aero AERO/WETH pool is configured.
pub const AERODROME_AERO: &str = "0x940181a94A35A4569E4529A3CDfB74e38FD98631";
/// Aero on Base (docs/cloud-tasks/aero-facts.md, verified 2026-10-20): the new AERO token, the LeafVoter CL gauges
/// settle against and the CLGaugeFactory holding the early-unstake penalty. The voter and the factory are read
/// from the gauge (`voter()`, `gaugeFactory()`); these are the fallbacks when those reads fail.
pub const AERO_TOKEN: &str = "0x3f5bF3EdAc59d650D8fDf9657a71F2f904A3a4Ee";
pub const AERO_LEAF_VOTER: &str = "0x382409fad8503FbCdbB27A4211551fa03e884078";
pub const AERO_CL_GAUGE_FACTORY: &str = "0xa02d1F4E4243ff0d350F478c44B0Fd37c7bF848C";

/// Fee kinds the vault passes to its fee policy (PandaVault.FEE_*).
pub const FEE_REWARD: u8 = 0;
pub const FEE_REBALANCE_SWAP: u8 = 4;
pub const FEE_COMPOUND_SWAP: u8 = 5;
/// The dormant time fee (a per-year rate on a position's liquidity, 0 at launch; vault ceiling 2% a year).
pub const FEE_TIME: u8 = 6;

/// Base mainnet GasPriceOracle predeploy.
pub const GAS_PRICE_ORACLE: &str = "0x420000000000000000000000000000000000000F";
