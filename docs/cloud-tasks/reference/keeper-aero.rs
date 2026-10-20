//! Aero gauge support (the Aerodrome + Velodrome merge, 2026-10-22 00:00 UTC). Pure logic, no I/O, so every
//! rule in here is unit-tested; `keeper.rs` feeds it chain reads.
//!
//! What Aero's gauge changes for the keeper (verified sources in docs/cloud-tasks/sourcify, facts in
//! docs/cloud-tasks/aero-facts.md):
//! - no `rewardRate()`: emissions reach the gauge through the LeafVoter's settlement (`gaugeStates`,
//!   `emissionsPerVP`, the per-gauge `emissionCap`), so the emission rate per unit of liquidity is MEASURED from
//!   the `earned` deltas of the vault's in-range positions ([`EarnedRateMeter`]) and CROSS-CHECKED against what the
//!   voter is paying ([`voter_gauge_rate_wei_per_s`], [`ProjectedShareMeter`]);
//! - `earned(account, uint256[] tokenIds)` returns a SUM and adds the account's `deferredEmissions`;
//! - the early-unstake window is in BLOCKS (`depositBlock(tokenId) + minStakeBlocks`), never seconds
//!   ([`PenaltyWindow`]): Base blocks are 2 s today and 200 ms from November 2026, so block numbers are compared;
//! - the AERO price pool may not exist yet (old AERO = new AERO one for one), see [`aero_price_source`];
//! - there is no QuoterV2: the swap minimum comes from the pool price and the vault's TWAP floor
//!   ([`expected_out_raw`], [`min_out_without_quoter`]).

use crate::math::Pair;
use ethers::types::U256;
use std::collections::HashMap;
use std::str::FromStr;

/// `GAUGE_MODE`: which gauge the vault stakes in. `auto` (the default) probes the gauge once at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeModeSetting {
    Auto,
    Aerodrome,
    Aero,
}

impl FromStr for GaugeModeSetting {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(GaugeModeSetting::Auto),
            "aerodrome" => Ok(GaugeModeSetting::Aerodrome),
            "aero" => Ok(GaugeModeSetting::Aero),
            other => Err(format!(
                "GAUGE_MODE={other:?} is not one of auto, aerodrome, aero"
            )),
        }
    }
}

/// The gauge the keeper is actually running against, decided once at startup and never changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeMode {
    /// Aerodrome Slipstream CLGauge: `rewardRate()`, `earned(address, uint256)`, `depositTimestamp`, seconds-based
    /// `minStakeTimes(pool)` on the gauge factory.
    Aerodrome,
    /// Aero CLGauge: `earned(address, uint256[])`, `depositBlock`, block-based `effectivePenaltyConfig(gauge)`,
    /// emissions settled through the LeafVoter.
    Aero,
}

impl GaugeMode {
    pub fn name(self) -> &'static str {
        match self {
            GaugeMode::Aerodrome => "aerodrome",
            GaugeMode::Aero => "aero",
        }
    }
}

/// The startup probe: `rewardRate()` answers = Aerodrome; `earned(address, uint256[])` answers and `rewardRate()`
/// does not = Aero; neither = refuse to start. An explicit `GAUGE_MODE` is still checked against the probe, so a
/// mis-set variable can never run the wrong bindings against a live gauge. Transport failures are the caller's
/// business (they are retried before this is called); the two flags say whether the gauge ANSWERED the call.
pub fn detect_gauge_mode(
    setting: GaugeModeSetting,
    reward_rate_answered: bool,
    earned_array_answered: bool,
) -> Result<GaugeMode, String> {
    let probed = match (reward_rate_answered, earned_array_answered) {
        (true, _) => Some(GaugeMode::Aerodrome),
        (false, true) => Some(GaugeMode::Aero),
        (false, false) => None,
    };
    match (setting, probed) {
        (_, None) => Err(
            "the vault's gauge answers neither rewardRate() (Aerodrome) nor earned(address,uint256[]) (Aero); refusing to start: check VAULT_ADDRESS / GAUGE_ADDRESS and the RPC"
                .to_string(),
        ),
        (GaugeModeSetting::Auto, Some(m)) => Ok(m),
        (GaugeModeSetting::Aerodrome, Some(GaugeMode::Aerodrome)) => Ok(GaugeMode::Aerodrome),
        (GaugeModeSetting::Aero, Some(GaugeMode::Aero)) => Ok(GaugeMode::Aero),
        (GaugeModeSetting::Aerodrome, Some(GaugeMode::Aero)) => {
            Err("GAUGE_MODE=aerodrome but the gauge has no rewardRate() and answers the Aero earned(address,uint256[]); set GAUGE_MODE=aero (or auto)".to_string())
        }
        (GaugeModeSetting::Aero, Some(GaugeMode::Aerodrome)) => {
            Err("GAUGE_MODE=aero but the gauge answers rewardRate(), which only Aerodrome's gauge has; set GAUGE_MODE=aerodrome (or auto)".to_string())
        }
    }
}

// ------------------------------------------------------------------------------------ penalty window

/// Pips denominator of Aero's gauge factory (1e6 = 100%).
pub const MAX_PIPS: u64 = 1_000_000;

/// Aero's early-unstake penalty as `CLGaugeFactory.effectivePenaltyConfig(gauge)` reports it: a position unstaked
/// or claimed while `block.number < depositBlock + minStakeBlocks` forfeits `penaltyRate` pips of its accrued
/// emissions (the vault refuses such a rebalance with `StakeTooRecent`). `minStakeBlocks` 0 = no window (Base at
/// launch); the admin can change both at any time, so they are re-read every heartbeat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PenaltyWindow {
    pub min_stake_blocks: u64,
    pub penalty_rate_pips: u64,
}

impl PenaltyWindow {
    /// Whether unstaking early would forfeit anything at all (the gauge's own `_applyPenalty` condition).
    pub fn active(&self) -> bool {
        self.min_stake_blocks > 0 && self.penalty_rate_pips > 0
    }

    /// First block at which unstaking `tokenId` forfeits nothing: `depositBlock + minStakeBlocks`. None when no
    /// window applies or the deposit block is unknown (0 = not staked). The same number the vault library's
    /// `penaltyFreeBlock(gauge, tokenId)` returns.
    pub fn penalty_free_block(&self, deposit_block: u64) -> Option<u64> {
        if !self.active() || deposit_block == 0 {
            return None;
        }
        Some(deposit_block.saturating_add(self.min_stake_blocks))
    }

    /// Blocks the keeper still has to wait at `head` before a rebalance forfeits nothing (0 = free to go). Block
    /// numbers only: the wait is never expressed in seconds.
    pub fn blocks_left(&self, deposit_block: u64, head: u64) -> u64 {
        self.penalty_free_block(deposit_block)
            .map(|free| free.saturating_sub(head))
            .unwrap_or(0)
    }
}

// ------------------------------------------------------------------------------------ measured rate

/// One position's `earned` reading this minute, as the keeper sees it: the gauge's answer for `[tokenId]` with the
/// vault's `deferredEmissions` taken out (the gauge adds that account-wide balance to every answer), the NFT's
/// liquidity, whether the pool tick is inside its range, and whether it is still inside the penalty window (where
/// `earned` reads 0 and would make the first delta after the window a spike).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EarnedSample {
    pub token_id: u64,
    pub earned_wei: f64,
    pub liquidity: u128,
    pub in_range: bool,
    pub penalised: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Baseline {
    ts: u64,
    earned_wei: f64,
    liquidity: u128,
}

/// Emission rate per unit of in-range liquidity, measured from the vault's own positions:
/// `sum(delta earned) / sum(liquidity x delta t)` over the positions that stayed in range (same liquidity, no claim
/// in between) since their previous reading. AERO wei per second per unit of liquidity, the quantity Aerodrome's
/// `rewardRate / stakedLiquidity` used to give. Nothing is persisted: after a restart the first reading only sets
/// the baselines and the rate exists from the second minute on.
#[derive(Clone, Debug, Default)]
pub struct EarnedRateMeter {
    baselines: HashMap<u64, Baseline>,
}

/// One measured rate: wei per second per unit L, over how many positions and how many liquidity-seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasuredRate {
    pub wei_per_s_per_l: f64,
    pub positions: usize,
    pub liquidity_seconds: f64,
}

impl EarnedRateMeter {
    /// Feeds this minute's readings and returns the rate they measured, or None when no position could be compared
    /// with its previous reading (first reading after a start, everything out of range, every baseline reset).
    pub fn observe(&mut self, now: u64, samples: &[EarnedSample]) -> Option<MeasuredRate> {
        let mut delta_sum = 0.0f64;
        let mut l_seconds = 0.0f64;
        let mut counted = 0usize;
        let mut next: HashMap<u64, Baseline> = HashMap::with_capacity(samples.len());
        for s in samples {
            if !s.in_range || s.penalised || s.liquidity == 0 || !s.earned_wei.is_finite() {
                // Out of range it earns nothing, inside the window `earned` is 0: no baseline until it is back.
                continue;
            }
            if let Some(b) = self.baselines.get(&s.token_id) {
                let dt = now.saturating_sub(b.ts);
                // A drop means a claim or a re-stake reset the position's checkpoint; a liquidity change a top-up.
                if dt > 0 && b.liquidity == s.liquidity && s.earned_wei >= b.earned_wei {
                    delta_sum += s.earned_wei - b.earned_wei;
                    l_seconds += s.liquidity as f64 * dt as f64;
                    counted += 1;
                }
            }
            next.insert(
                s.token_id,
                Baseline {
                    ts: now,
                    earned_wei: s.earned_wei,
                    liquidity: s.liquidity,
                },
            );
        }
        self.baselines = next;
        if counted == 0 || l_seconds <= 0.0 {
            return None;
        }
        Some(MeasuredRate {
            wei_per_s_per_l: delta_sum / l_seconds,
            positions: counted,
            liquidity_seconds: l_seconds,
        })
    }

    pub fn tracked(&self) -> usize {
        self.baselines.len()
    }
}

/// The width model's unit: USD per year per unit of liquidity, from a measured wei/s/L rate and the AERO price.
/// The same conversion `policy::emission_rate_per_l` makes from Aerodrome's `rewardRate / stakedLiquidity`.
pub fn measured_rate_usd_per_year_per_l(wei_per_s_per_l: f64, aero_usd: f64) -> f64 {
    wei_per_s_per_l / 1e18 * 31_536_000.0 * aero_usd
}

// ------------------------------------------------------------------------------------ on-chain rate

/// `LeafVoter.gaugeStates(gauge).point`: the gauge's voting weight, decaying linearly from `ts`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GaugePoint {
    pub bias: i128,
    pub slope: i128,
    pub ts: u64,
    pub permanent_stake_balance: u128,
}

/// Fixed point scale of the voter's `emissionsPerVP` (ProtocolConstants.PRECISION).
pub const VOTER_PRECISION: f64 = 1e18;

/// AERO wei per second the LeafVoter is crediting the gauge right now, derived from view calls the way
/// `LeafAllocationLibrary._accrueGaugeSegment` accrues it: the gauge's weight at `now`
/// (`max(permanentStakeBalance + bias - slope x (now - point.ts), 0)`) times the chain's `emissionsPerVP` over
/// PRECISION, capped at the gauge's per-second `emissionCap`. The voter applies the cap per settlement segment
/// (`cap x duration` against the segment's allocation), so for a weight that moves little within a segment the
/// instantaneous `min(rate, cap)` is the same number. This is the GROSS rate: the gauge takes the referral share
/// out before an LP sees it (see [`net_of_referral`]).
pub fn voter_gauge_rate_wei_per_s(
    point: &GaugePoint,
    emissions_per_vp: f64,
    cap_wei_per_s: f64,
    now: u64,
) -> f64 {
    let elapsed = now.saturating_sub(point.ts) as f64;
    let weight =
        point.permanent_stake_balance as f64 + point.bias as f64 - point.slope as f64 * elapsed;
    if weight.is_nan() || weight <= 0.0 || emissions_per_vp.is_nan() || emissions_per_vp <= 0.0 {
        return 0.0;
    }
    let rate = weight * emissions_per_vp / VOTER_PRECISION;
    if cap_wei_per_s > 0.0 {
        rate.min(cap_wei_per_s)
    } else {
        0.0
    }
}

/// The LP's share of a gross gauge rate after Aero's referral cut (`referralConfig(gauge).share`, pips).
pub fn net_of_referral(gross: f64, referral_share_pips: u64) -> f64 {
    gross * (1.0 - referral_share_pips.min(MAX_PIPS) as f64 / MAX_PIPS as f64)
}

/// Per unit of in-range staked liquidity: `CLPool._updateRewardsGrowthGlobal` spreads every settled wei over
/// `stakedLiquidity`; with none in range the emissions bank into `rollover` and nobody earns them (0 here).
pub fn rate_per_staked_liquidity(rate_wei_per_s: f64, staked_liquidity: u128) -> f64 {
    if staked_liquidity == 0 {
        return 0.0;
    }
    rate_wei_per_s / staked_liquidity as f64
}

/// The exact on-chain rate between two heartbeats: the rise of `LeafVoter.projectedCumulativeRewardShare(gauge)`
/// (what `settleGauge` would credit now, cap and weekly segments included) over the seconds between the readings.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProjectedShareMeter {
    last: Option<(u64, f64)>,
}

impl ProjectedShareMeter {
    /// Returns gross AERO wei per second since the previous reading, or None at the first reading, when no time
    /// passed, or when the projection went backwards (a deregistered gauge reports 0).
    pub fn observe(&mut self, now: u64, cumulative_share_wei: f64) -> Option<f64> {
        let prev = self.last.replace((now, cumulative_share_wei));
        let (t0, v0) = prev?;
        if now <= t0 || cumulative_share_wei < v0 || !cumulative_share_wei.is_finite() {
            return None;
        }
        Some((cumulative_share_wei - v0) / (now - t0) as f64)
    }
}

/// How far a measured per-L rate sits from the chain's: `measured / on_chain` (1.0 = agreement). None when either
/// is not a positive number.
pub fn cross_check_ratio(
    measured_wei_per_s_per_l: f64,
    on_chain_wei_per_s_per_l: f64,
) -> Option<f64> {
    if measured_wei_per_s_per_l > 0.0
        && on_chain_wei_per_s_per_l > 0.0
        && measured_wei_per_s_per_l.is_finite()
        && on_chain_wei_per_s_per_l.is_finite()
    {
        Some(measured_wei_per_s_per_l / on_chain_wei_per_s_per_l)
    } else {
        None
    }
}

// ------------------------------------------------------------------------------------ AERO price source

/// Where the AERO price comes from in each mode (`AERO_WETH_POOL_ADDRESS`, default = Aerodrome's AERO/WETH CL200
/// pool, which holds the OLD AERO).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeroPriceSource {
    /// The configured pool holds WETH and the vault's AERO: use it.
    Configured,
    /// Aero mode, the configured pool is unusable (not set yet, or it does not hold the NEW AERO): price the new
    /// AERO from Aerodrome's AERO/WETH pool, where old AERO keeps trading one for one with the new.
    AerodromeFallback,
    /// No usable pool: adaptive width off, as today.
    Unavailable,
}

pub fn aero_price_source(
    mode: GaugeMode,
    configured_pool_ok: bool,
    aerodrome_pool_ok: bool,
) -> AeroPriceSource {
    match (mode, configured_pool_ok, aerodrome_pool_ok) {
        (_, true, _) => AeroPriceSource::Configured,
        (GaugeMode::Aero, false, true) => AeroPriceSource::AerodromeFallback,
        _ => AeroPriceSource::Unavailable,
    }
}

// ------------------------------------------------------------------------------------ swap minimum without a quoter

/// Raw output of the vault pool for `amount_in_raw` of the sold token at the pool's raw ratio (raw token1 per raw
/// token0, i.e. `sqrtPrice^2`), after the pool fee (`fee_pips` of the input). No price impact: the pool's Metaquoter
/// takes pool addresses rather than a path, so in Aero mode the keeper prices the swap from the pool state itself.
pub fn expected_out_raw(
    pair: &Pair,
    is_a_to_b: bool,
    amount_in_raw: f64,
    raw_ratio_1_per_0: f64,
    fee_pips: u32,
) -> f64 {
    if amount_in_raw.is_nan()
        || amount_in_raw <= 0.0
        || raw_ratio_1_per_0.is_nan()
        || raw_ratio_1_per_0 <= 0.0
    {
        return 0.0;
    }
    let zero_for_one = is_a_to_b == pair.a_is_token0;
    let net_in = amount_in_raw * (1.0 - fee_pips.min(1_000_000) as f64 / 1e6);
    if zero_for_one {
        net_in * raw_ratio_1_per_0
    } else {
        net_in / raw_ratio_1_per_0
    }
}

/// `rebalanceAmountOutMin` in Aero mode: the keeper's own `SWAP_SLIPPAGE_BPS` under the pool-price output, never
/// below the vault's TWAP floor (`rebalanceSwapFloor` = TWAP output minus the guard rails' `maxRebalanceSlippageBps`,
/// which the vault enforces anyway). Returns None when the pool price itself cannot clear the floor: the vault would
/// revert SlippageExceeded, so the keeper skips the heartbeat, as it does today when the quote is under the floor.
pub fn min_out_without_quoter(
    expected_out_raw: f64,
    vault_floor: U256,
    slippage_bps: u64,
) -> Option<U256> {
    let expected = U256::from(expected_out_raw.max(0.0).floor() as u128);
    if expected < vault_floor {
        return None;
    }
    let own = crate::math::apply_slippage(expected, slippage_bps.min(9_999));
    Some(own.max(vault_floor))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------- mode probe

    #[test]
    fn the_probe_picks_the_gauge_by_what_answers_and_refuses_the_rest() {
        use GaugeModeSetting::*;
        assert_eq!(
            detect_gauge_mode(Auto, true, false),
            Ok(GaugeMode::Aerodrome)
        );
        assert_eq!(detect_gauge_mode(Auto, false, true), Ok(GaugeMode::Aero));
        // Both answering is read as Aerodrome: only its gauge has rewardRate().
        assert_eq!(
            detect_gauge_mode(Auto, true, true),
            Ok(GaugeMode::Aerodrome)
        );
        assert!(detect_gauge_mode(Auto, false, false)
            .unwrap_err()
            .contains("refusing to start"));
        // An explicit mode is checked against the gauge, never trusted blindly.
        assert_eq!(
            detect_gauge_mode(Aerodrome, true, false),
            Ok(GaugeMode::Aerodrome)
        );
        assert_eq!(detect_gauge_mode(Aero, false, true), Ok(GaugeMode::Aero));
        assert!(detect_gauge_mode(Aerodrome, false, true)
            .unwrap_err()
            .contains("GAUGE_MODE=aerodrome"));
        assert!(detect_gauge_mode(Aero, true, false)
            .unwrap_err()
            .contains("GAUGE_MODE=aero"));
        assert!(detect_gauge_mode(Aero, false, false).is_err());
        assert!(detect_gauge_mode(Aerodrome, false, false).is_err());
        // The variable's spellings.
        assert_eq!("auto".parse::<GaugeModeSetting>(), Ok(Auto));
        assert_eq!("".parse::<GaugeModeSetting>(), Ok(Auto));
        assert_eq!(" Aerodrome ".parse::<GaugeModeSetting>(), Ok(Aerodrome));
        assert_eq!("AERO".parse::<GaugeModeSetting>(), Ok(Aero));
        assert!("velodrome".parse::<GaugeModeSetting>().is_err());
    }

    // ---------------------------------------------------------------- penalty window

    #[test]
    fn the_block_window_is_zero_today_five_tomorrow_and_whatever_the_admin_sets_later() {
        // Base at launch: factory-wide minStakeBlocks 0, penaltyRate 100%: no window at all.
        let today = PenaltyWindow {
            min_stake_blocks: 0,
            penalty_rate_pips: MAX_PIPS,
        };
        assert!(!today.active());
        assert_eq!(today.penalty_free_block(52_900_000), None);
        assert_eq!(today.blocks_left(52_900_000, 52_900_000), 0);
        // A per-gauge override of 5 blocks (the factory's DEFAULT_MIN_STAKE_BLOCKS).
        let five = PenaltyWindow {
            min_stake_blocks: 5,
            penalty_rate_pips: MAX_PIPS,
        };
        assert!(five.active());
        assert_eq!(five.penalty_free_block(1_000), Some(1_005));
        assert_eq!(five.blocks_left(1_000, 1_000), 5);
        assert_eq!(five.blocks_left(1_000, 1_004), 1);
        assert_eq!(
            five.blocks_left(1_000, 1_005),
            0,
            "block.number < deposit + min is the gauge's penalty condition"
        );
        assert_eq!(five.blocks_left(1_000, 2_000), 0);
        // A later admin change (setMinStakeBlocks / setPenaltyConfig) is just a new value: nothing is cached.
        let later = PenaltyWindow {
            min_stake_blocks: 300,
            penalty_rate_pips: 250_000,
        };
        assert_eq!(later.blocks_left(1_000, 1_100), 200);
        // A zero rate means nothing is forfeited, whatever the block count.
        let free = PenaltyWindow {
            min_stake_blocks: 300,
            penalty_rate_pips: 0,
        };
        assert!(!free.active());
        assert_eq!(free.blocks_left(1_000, 1_000), 0);
        // Unknown deposit block (0 = not staked): no window to wait for; the vault's own check decides.
        assert_eq!(five.penalty_free_block(0), None);
        // Base's block time changes in November 2026 (2 s -> 200 ms): the wait is the same 5 BLOCKS either way,
        // never a number of seconds derived from a constant.
        assert_eq!(five.blocks_left(1_000, 1_000), five.min_stake_blocks);
    }

    // ---------------------------------------------------------------- a mock that follows Aero's maths

    const Q128: f64 = 3.402_823_669_209_384_6e38; // 2^128
    const PRECISION: f64 = 1e18;

    /// A LeafVoter + CLPool + CLGauge in miniature, with the accounting of the verified sources: the voter credits
    /// `weight x emissionsPerVP / PRECISION` per second (capped per segment at `cap x duration`), `earned` projects
    /// the unsettled share into `rewardGrowthGlobalX128` over `stakedLiquidity`, a position's reward growth inside
    /// its range only moves while the tick is inside it, and `earned` is `(growthInside - checkpoint) x L / Q128`
    /// (penalty inside the window, referral share, plus the account's deferred emissions).
    struct MockAero {
        point: GaugePoint,
        emissions_per_vp: f64,
        cap: f64,
        /// LeafVoter.gaugeStates(gauge).ceiling: credited up to `settled_at`.
        ceiling: f64,
        settled_at: u64,
        /// CLGauge.lastCumulativeRewardShare / CLPool.rewardGrowthGlobalX128 as last written.
        last_cumulative: f64,
        reward_growth_global_x128: f64,
        staked_liquidity: u128,
        tick: i32,
        positions: Vec<MockPosition>,
        deferred: f64,
        referral_share_pips: u64,
        window: PenaltyWindow,
        head_block: u64,
    }

    struct MockPosition {
        id: u64,
        liquidity: u128,
        lower: i32,
        upper: i32,
        growth_inside_x128: f64,
        checkpoint_x128: f64,
        deposit_block: u64,
    }

    impl MockAero {
        fn projected_cumulative_reward_share(&self, now: u64) -> f64 {
            let rate =
                voter_gauge_rate_wei_per_s(&self.point, self.emissions_per_vp, self.cap, now);
            self.ceiling + rate * now.saturating_sub(self.settled_at) as f64
        }

        /// CLPool._settleToBlock + CLGauge.settleGauge: credit the unsettled share to the growth accumulators.
        fn settle(&mut self, now: u64) {
            let projected = self.projected_cumulative_reward_share(now);
            let delta = projected - self.last_cumulative;
            self.ceiling = projected;
            self.settled_at = now;
            self.last_cumulative = projected;
            if self.staked_liquidity > 0 {
                let growth = delta * Q128 / self.staked_liquidity as f64;
                self.reward_growth_global_x128 += growth;
                for p in &mut self.positions {
                    if p.lower <= self.tick && self.tick < p.upper {
                        p.growth_inside_x128 += growth;
                    }
                }
            }
        }

        fn in_range(&self, p: &MockPosition) -> bool {
            p.lower <= self.tick && self.tick < p.upper
        }

        /// `earned(vault, [id])` as the gauge computes it at `now` without writing state.
        fn earned(&self, id: u64, now: u64) -> f64 {
            let p = self
                .positions
                .iter()
                .find(|p| p.id == id)
                .expect("position");
            let projected = self.projected_cumulative_reward_share(now);
            let unsettled = (projected - self.last_cumulative).max(0.0);
            let extra = if self.staked_liquidity > 0 && self.in_range(p) {
                unsettled * Q128 / self.staked_liquidity as f64
            } else {
                0.0
            };
            let mut reward =
                (p.growth_inside_x128 + extra - p.checkpoint_x128) * p.liquidity as f64 / Q128;
            if reward > 0.0 {
                if self.window.active()
                    && self.head_block < p.deposit_block + self.window.min_stake_blocks
                {
                    reward -= reward * self.window.penalty_rate_pips as f64 / MAX_PIPS as f64;
                }
                reward -= reward * self.referral_share_pips as f64 / MAX_PIPS as f64;
            }
            reward + self.deferred
        }

        fn samples(&self, now: u64) -> Vec<EarnedSample> {
            self.positions
                .iter()
                .map(|p| EarnedSample {
                    token_id: p.id,
                    earned_wei: self.earned(p.id, now) - self.deferred,
                    liquidity: p.liquidity,
                    in_range: self.in_range(p),
                    penalised: self.window.active()
                        && self.head_block < p.deposit_block + self.window.min_stake_blocks,
                })
                .collect()
        }

        fn on_chain_rate_per_l(&self, now: u64) -> f64 {
            let gross =
                voter_gauge_rate_wei_per_s(&self.point, self.emissions_per_vp, self.cap, now);
            rate_per_staked_liquidity(
                net_of_referral(gross, self.referral_share_pips),
                self.staked_liquidity,
            )
        }
    }

    fn mock() -> MockAero {
        // A permanent allocation of 1,000,000 AERO of voting weight at 1e-9 AERO per VP-second: 0.001 AERO/s.
        let positions = vec![
            MockPosition {
                id: 1,
                liquidity: 4_000_000_000_000_000,
                lower: -200,
                upper: 200,
                growth_inside_x128: 0.0,
                checkpoint_x128: 0.0,
                deposit_block: 100,
            },
            MockPosition {
                id: 2,
                liquidity: 1_000_000_000_000_000,
                lower: -100,
                upper: 100,
                growth_inside_x128: 0.0,
                checkpoint_x128: 0.0,
                deposit_block: 100,
            },
            // Out of range the whole time: earns nothing and must not dilute the measurement.
            MockPosition {
                id: 3,
                liquidity: 9_000_000_000_000_000,
                lower: 1_000,
                upper: 2_000,
                growth_inside_x128: 0.0,
                checkpoint_x128: 0.0,
                deposit_block: 100,
            },
        ];
        // Other stakers hold in-range liquidity too: stakedLiquidity is the pool's whole in-range staked amount.
        MockAero {
            point: GaugePoint {
                bias: 0,
                slope: 0,
                ts: 1_000_000,
                permanent_stake_balance: 1_000_000_000_000_000_000_000_000,
            },
            emissions_per_vp: 1e9,
            cap: 1e17,
            ceiling: 0.0,
            settled_at: 1_000_000,
            last_cumulative: 0.0,
            reward_growth_global_x128: 0.0,
            staked_liquidity: 20_000_000_000_000_000,
            tick: 0,
            positions,
            deferred: 0.0,
            referral_share_pips: 0,
            window: PenaltyWindow::default(),
            head_block: 1_000,
        }
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * b.abs().max(f64::MIN_POSITIVE)
    }

    #[test]
    fn the_measured_rate_agrees_with_the_chain_on_the_mock() {
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000u64;
        // First reading only sets the baselines: no rate yet (a fresh start has no history).
        assert_eq!(meter.observe(t, &m.samples(t)), None);
        assert_eq!(
            meter.tracked(),
            2,
            "only the in-range positions carry a baseline"
        );
        let mut agreed = 0;
        for minute in 1..=10 {
            t += 60;
            m.head_block += 30;
            // The pool settles now and then (a swap crossing a tick, a claim); the projection must not care.
            if minute % 3 == 0 {
                m.settle(t);
            }
            let r = meter
                .observe(t, &m.samples(t))
                .expect("a rate every minute from the second reading on");
            assert_eq!(r.positions, 2);
            let on_chain = m.on_chain_rate_per_l(t);
            assert!(
                close(r.wei_per_s_per_l, on_chain, 1e-6),
                "minute {minute}: measured {} vs chain {on_chain}",
                r.wei_per_s_per_l
            );
            assert!(close(
                cross_check_ratio(r.wei_per_s_per_l, on_chain).unwrap(),
                1.0,
                1e-6
            ));
            agreed += 1;
        }
        assert_eq!(agreed, 10);
        // The number itself: 0.001 AERO/s over 2e16 in-range staked liquidity.
        assert!(close(m.on_chain_rate_per_l(t), 1e15 / 2e16, 1e-12));
    }

    #[test]
    fn the_projected_share_delta_is_the_same_rate_the_closed_form_gives() {
        let m = mock();
        let mut meter = ProjectedShareMeter::default();
        let t0 = 1_000_000u64;
        assert_eq!(
            meter.observe(t0, m.projected_cumulative_reward_share(t0)),
            None
        );
        let r = meter
            .observe(t0 + 300, m.projected_cumulative_reward_share(t0 + 300))
            .unwrap();
        assert!(close(
            r,
            voter_gauge_rate_wei_per_s(&m.point, m.emissions_per_vp, m.cap, t0 + 300),
            1e-9
        ));
        // No time passed, or a projection that went backwards (deregistered gauge): no rate.
        assert_eq!(meter.observe(t0 + 300, 1.0), None);
        assert_eq!(meter.observe(t0 + 600, 0.0), None);
    }

    #[test]
    fn a_rate_change_mid_run_shows_up_within_one_sample() {
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000u64;
        meter.observe(t, &m.samples(t));
        t += 60;
        let before = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
        // The root chain refreshes emissionsPerVP (a new epoch's allocation): the voter settles at the old rate first.
        m.settle(t);
        m.emissions_per_vp *= 2.5;
        for _ in 0..3 {
            t += 60;
            let r = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
            assert!(
                close(r, before * 2.5, 1e-6),
                "measured {r} vs expected {}",
                before * 2.5
            );
            assert!(close(r, m.on_chain_rate_per_l(t), 1e-6));
        }
        // And the cap binds: a huge scalar is paid at the cap, not above it.
        m.settle(t);
        m.emissions_per_vp = 1e30;
        t += 60;
        let capped = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
        assert!(
            close(capped, m.cap / m.staked_liquidity as f64, 1e-6),
            "capped {capped}"
        );
    }

    #[test]
    fn out_of_range_claims_restarts_and_the_window_do_not_corrupt_the_measurement() {
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000u64;
        meter.observe(t, &m.samples(t));
        t += 60;
        let expected = m.on_chain_rate_per_l(t);
        // Position 2 leaves its range for a minute (the tick moves to 150: inside 1's range, outside 2's).
        m.settle(t);
        m.tick = 150;
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(
            r.positions, 1,
            "only the position that stayed in range is measured"
        );
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
        // Back in range: it needs a fresh baseline first, then counts again.
        m.settle(t);
        m.tick = 0;
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 1);
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 2);
        // The owner claims position 1's rewards: its checkpoint moves up, `earned` drops, that position is reset.
        m.settle(t);
        let p = &mut m.positions[0];
        p.checkpoint_x128 = p.growth_inside_x128;
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 1);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
        // A deferred balance appearing on the account (a withdraw the gauge could not pay) shifts every answer by
        // the same amount; the keeper takes it out before measuring, so nothing changes.
        m.deferred = 5e18;
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 2);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
        // A restart: a new meter has no history, so no rate until its second reading, then the right one.
        let mut fresh = EarnedRateMeter::default();
        t += 60;
        assert_eq!(fresh.observe(t, &m.samples(t)), None);
        t += 60;
        assert!(close(
            fresh.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l,
            expected,
            1e-6
        ));
        // A penalty window: a position re-staked 2 blocks ago under a 5-block window reads 0 and is left out until
        // the window has passed, so its first real delta is not read as a spike.
        m.window = PenaltyWindow {
            min_stake_blocks: 5,
            penalty_rate_pips: MAX_PIPS,
        };
        m.settle(t);
        m.positions[1].deposit_block = m.head_block - 2;
        m.positions[1].checkpoint_x128 = m.positions[1].growth_inside_x128;
        t += 60;
        let r = fresh.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 1);
        m.head_block += 30;
        t += 60;
        assert_eq!(
            fresh.observe(t, &m.samples(t)).unwrap().positions,
            1,
            "first reading after the window only sets the baseline"
        );
        t += 60;
        let r = fresh.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 2);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
    }

    #[test]
    fn the_referral_share_is_what_separates_measured_from_gross() {
        let mut m = mock();
        m.referral_share_pips = 50_000; // 5% to a referral
        let mut meter = EarnedRateMeter::default();
        let t = 1_000_000u64;
        meter.observe(t, &m.samples(t));
        let r = meter
            .observe(t + 60, &m.samples(t + 60))
            .unwrap()
            .wei_per_s_per_l;
        let gross = rate_per_staked_liquidity(
            voter_gauge_rate_wei_per_s(&m.point, m.emissions_per_vp, m.cap, t + 60),
            m.staked_liquidity,
        );
        assert!(close(cross_check_ratio(r, gross).unwrap(), 0.95, 1e-6));
        assert!(
            close(r, m.on_chain_rate_per_l(t + 60), 1e-6),
            "net of the referral share they agree"
        );
        assert_eq!(cross_check_ratio(0.0, gross), None);
        assert_eq!(cross_check_ratio(r, 0.0), None);
    }

    #[test]
    fn the_voter_rate_decays_with_the_slope_and_stops_at_zero_weight_or_cap() {
        // A decaying (non-permanent) stake: weight = bias - slope x elapsed, floored at 0.
        let p = GaugePoint {
            bias: 1_000_000,
            slope: 10,
            ts: 100,
            permanent_stake_balance: 0,
        };
        assert!(close(
            voter_gauge_rate_wei_per_s(&p, PRECISION, 1e30, 100),
            1_000_000.0,
            1e-12
        ));
        assert!(close(
            voter_gauge_rate_wei_per_s(&p, PRECISION, 1e30, 100 + 50_000),
            500_000.0,
            1e-12
        ));
        assert_eq!(
            voter_gauge_rate_wei_per_s(&p, PRECISION, 1e30, 100 + 200_000),
            0.0
        );
        // The cap, a zero cap (deactivated gauge), a zero scalar (suspended chain) and a clock behind the point.
        assert_eq!(
            voter_gauge_rate_wei_per_s(&p, PRECISION, 1_000.0, 100),
            1_000.0
        );
        assert_eq!(voter_gauge_rate_wei_per_s(&p, PRECISION, 0.0, 100), 0.0);
        assert_eq!(voter_gauge_rate_wei_per_s(&p, 0.0, 1e30, 100), 0.0);
        assert!(close(
            voter_gauge_rate_wei_per_s(&p, PRECISION, 1e30, 50),
            1_000_000.0,
            1e-12
        ));
        // Permanent weight does not decay.
        let perm = GaugePoint {
            bias: 0,
            slope: 0,
            ts: 100,
            permanent_stake_balance: 7_000,
        };
        assert!(close(
            voter_gauge_rate_wei_per_s(&perm, 2.0 * PRECISION, 1e30, 1_000_000),
            14_000.0,
            1e-12
        ));
        // Nothing in range: the pool banks the emissions into rollover, nobody earns per unit L.
        assert_eq!(rate_per_staked_liquidity(1e15, 0), 0.0);
        assert!(close(
            measured_rate_usd_per_year_per_l(1e15 / 2e16, 0.5),
            1e15 / 2e16 / 1e18 * 31_536_000.0 * 0.5,
            1e-12
        ));
    }

    // ---------------------------------------------------------------- AERO price

    #[test]
    fn the_aero_price_falls_back_to_the_aerodrome_pool_only_in_aero_mode() {
        use AeroPriceSource::*;
        // Aero mode: the configured (new) pool when it holds the new AERO, else the Aerodrome pool (old AERO = new
        // AERO one for one), else nothing.
        assert_eq!(aero_price_source(GaugeMode::Aero, true, true), Configured);
        assert_eq!(
            aero_price_source(GaugeMode::Aero, false, true),
            AerodromeFallback
        );
        assert_eq!(
            aero_price_source(GaugeMode::Aero, false, false),
            Unavailable
        );
        // Aerodrome mode is exactly today's behaviour: the configured pool or nothing, never a fallback.
        assert_eq!(
            aero_price_source(GaugeMode::Aerodrome, true, true),
            Configured
        );
        assert_eq!(
            aero_price_source(GaugeMode::Aerodrome, false, true),
            Unavailable
        );
        assert_eq!(
            aero_price_source(GaugeMode::Aerodrome, false, false),
            Unavailable
        );
    }

    // ---------------------------------------------------------------- min-out floor

    #[test]
    fn the_min_out_floor_without_a_quoter_follows_the_pool_price_and_never_undercuts_the_vault() {
        let pair = Pair {
            a_is_token0: true,
            dec_a: 18,
            dec_b: 6,
        }; // WETH/USDC
           // 2,500 USDC per WETH: raw ratio = 2500e6 / 1e18.
        let ratio = 2_500e6 / 1e18;
        // Sell 1 WETH at a 5 bps pool fee: 2,498.75 USDC.
        let out = expected_out_raw(&pair, true, 1e18, ratio, 500);
        assert!(close(out, 2_498.75e6, 1e-9), "{out}");
        // Sell 2,500 USDC: 0.99950 WETH.
        let out_a = expected_out_raw(&pair, false, 2_500e6, ratio, 500);
        assert!(close(out_a, 0.9995e18, 1e-9), "{out_a}");
        // The cbBTC/USDC order (A = token1) maps the direction the other way round.
        let btc = Pair {
            a_is_token0: false,
            dec_a: 8,
            dec_b: 6,
        };
        let ratio_btc = 1e8 / 100_000e6; // raw cbBTC per raw USDC at $100,000
        assert!(close(
            expected_out_raw(&btc, true, 1e8, ratio_btc, 0),
            100_000e6,
            1e-9
        ));
        assert!(close(
            expected_out_raw(&btc, false, 100_000e6, ratio_btc, 0),
            1e8,
            1e-9
        ));
        assert_eq!(expected_out_raw(&pair, true, 0.0, ratio, 500), 0.0);
        assert_eq!(expected_out_raw(&pair, true, 1e18, 0.0, 500), 0.0);

        // The keeper's 40 bps under the pool price is the usual minimum, above the vault's 100 bps TWAP floor.
        let floor = U256::from(2_475_000_000u64); // TWAP 2,500 minus 100 bps
        let min = min_out_without_quoter(out, floor, 40).unwrap();
        assert_eq!(min, U256::from((2_498.75e6 * 0.996) as u64));
        assert!(min > floor);
        // A pool price far under the TWAP: the vault floor wins when the keeper's own would be lower...
        let min2 = min_out_without_quoter(2_480e6, floor, 40).unwrap();
        assert_eq!(min2, floor);
        // ...and when even the pool price is under the floor the swap is not sent (SlippageExceeded on chain).
        assert_eq!(min_out_without_quoter(2_470e6, floor, 40), None);
        // Zero floor (the vault could not read its TWAP... it reverts before here, but the arithmetic holds).
        assert_eq!(
            min_out_without_quoter(1_000.0, U256::zero(), 100),
            Some(U256::from(990u64))
        );
    }
}
