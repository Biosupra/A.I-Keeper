//! Aerodrome becomes Aero at 2026-10-22 00:00 UTC. Pure logic, no I/O, so every rule here is unit-tested;
//! `collect::gauge` feeds it chain reads.
//!
//! Two things change for the A.I (verified sources in docs/cloud-tasks/sourcify, facts in
//! docs/cloud-tasks/aero-facts.md; the maths follows the live keeper's port, docs/cloud-tasks/reference):
//! - Aerodrome's gauge keeps answering `rewardRate()` (0.503 AERO/s) after its last period ends, but pays
//!   nothing: the rate counts as 0 once `periodFinish <= now`, and always from the launch
//!   ([`aerodrome_rate_effective`], [`aerodrome_rewards_end`]). Stored raw values are never rewritten.
//! - Aero's gauge has no `rewardRate`, `periodFinish`, `left` or `rewardRateByEpoch`: emissions settle through
//!   the LeafVoter, so the rate is the voter's instantaneous rate ([`voter_gauge_rate_wei_per_s`]) and,
//!   where positions' `earned` is sampled, the measured earned-delta rate ([`EarnedRateMeter`]). The
//!   early-unstake window is in BLOCKS ([`PenaltyWindow`]), never seconds.

use std::collections::HashMap;
use std::str::FromStr;

/// The Aero launch: 2026-10-22 00:00 UTC (AerodromeMigration.MIGRATION_OPEN 1792620000 is 22:00 UTC the day
/// before). Aerodrome's gauges are deactivated then, and its last reward period ends at this epoch flip.
pub const LAUNCH_TS: i64 = 1_792_627_200;

/// Base mainnet tokens (docs/cloud-tasks/aero-facts.md).
pub const WETH: &str = "0x4200000000000000000000000000000000000006";
/// Aerodrome's AERO (the OLD token; one old AERO = one new AERO through the migration portal).
pub const OLD_AERO: &str = "0x940181a94A35A4569E4529A3CDfB74e38FD98631";
/// Aero's AERO (the NEW token).
pub const NEW_AERO: &str = "0x3f5bF3EdAc59d650D8fDf9657a71F2f904A3a4Ee";
/// The LeafVoter CL gauges settle against and the CLGaugeFactory holding the penalty and the cap. The A.I reads
/// both from the gauge (`voter()`, `gaugeFactory()`); these are the fallbacks when those reads fail.
pub const LEAF_VOTER: &str = "0x382409fad8503FbCdbB27A4211551fa03e884078";
pub const CL_GAUGE_FACTORY: &str = "0xa02d1F4E4243ff0d350F478c44B0Fd37c7bF848C";

// ------------------------------------------------------------------------------------ gauge mode

/// `AIK_GAUGE_MODE`: which gauge AIK_GAUGE is. `auto` (the default) probes it once.
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
            other => Err(format!("AIK_GAUGE_MODE={other:?} is not one of auto, aerodrome, aero")),
        }
    }
}

/// The gauge the A.I is reading, decided once by the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaugeMode {
    /// Aerodrome Slipstream CLGauge: `rewardRate()`, `periodFinish()`, `earned(address, uint256)`.
    Aerodrome,
    /// Aero CLGauge: `earned(address, uint256[])`, `depositBlock`, emissions settled through the LeafVoter.
    Aero,
}

impl GaugeMode {
    pub fn name(self) -> &'static str {
        match self {
            GaugeMode::Aerodrome => "aerodrome",
            GaugeMode::Aero => "aero",
        }
    }

    pub fn from_name(s: &str) -> Option<GaugeMode> {
        match s {
            "aerodrome" => Some(GaugeMode::Aerodrome),
            "aero" => Some(GaugeMode::Aero),
            _ => None,
        }
    }
}

/// The probe: `rewardRate()` answers = Aerodrome; `earned(address, uint256[])` answers and `rewardRate()` reverts
/// = Aero; neither = no mode. An explicit setting is still checked against the probe, so a mis-set variable never
/// reads the wrong gauge. The two flags say whether the gauge ANSWERED (a revert is a `false`; a transport failure
/// is the caller's business and never reaches here).
pub fn detect_gauge_mode(setting: GaugeModeSetting, reward_rate_answered: bool, earned_array_answered: bool) -> Result<GaugeMode, String> {
    let probed = match (reward_rate_answered, earned_array_answered) {
        (true, _) => Some(GaugeMode::Aerodrome),
        (false, true) => Some(GaugeMode::Aero),
        (false, false) => None,
    };
    match (setting, probed) {
        (_, None) => Err("AIK_GAUGE answers neither rewardRate() (Aerodrome) nor earned(address,uint256[]) (Aero): check AIK_GAUGE".into()),
        (GaugeModeSetting::Auto, Some(m)) => Ok(m),
        (GaugeModeSetting::Aerodrome, Some(GaugeMode::Aerodrome)) => Ok(GaugeMode::Aerodrome),
        (GaugeModeSetting::Aero, Some(GaugeMode::Aero)) => Ok(GaugeMode::Aero),
        (GaugeModeSetting::Aerodrome, Some(GaugeMode::Aero)) => Err(
            "AIK_GAUGE_MODE=aerodrome but AIK_GAUGE has no rewardRate() and answers the Aero earned(address,uint256[]); set AIK_GAUGE_MODE=aero (or auto)"
                .into(),
        ),
        (GaugeModeSetting::Aero, Some(GaugeMode::Aerodrome)) => Err(
            "AIK_GAUGE_MODE=aero but AIK_GAUGE answers rewardRate(), which only Aerodrome's gauge has; set AIK_GAUGE_MODE=aerodrome (or auto)"
                .into(),
        ),
    }
}

// ------------------------------------------------------------------------------------ phantom Aerodrome rate

/// Whether Aerodrome's gauge actually pays at `now`: its period has not finished and Aero has not launched.
/// An unknown `periodFinish` before the launch is taken as paying (today's behaviour); from the launch nothing is.
pub fn aerodrome_paying(period_finish: Option<i64>, now: i64) -> bool {
    now < LAUNCH_TS && period_finish.map(|f| f > now).unwrap_or(true)
}

/// The Aerodrome rate that is really paid: the raw `rewardRate()` (or `rewardRateByEpoch`), or 0 once
/// `periodFinish <= now` or from the launch. The raw value is stored next to it, never replaced.
pub fn aerodrome_rate_effective(raw: f64, period_finish: Option<i64>, now: i64) -> f64 {
    if aerodrome_paying(period_finish, now) {
        raw
    } else {
        0.0
    }
}

/// When Aerodrome's rewards end, for replays and comparisons: the launch, or earlier when the gauge's latest
/// `periodFinish` reading (`latest = (read at, periodFinish)`) shows the last period already over and never
/// renewed. A renewal after a gap shows up as a later reading with a future `periodFinish`, so a short gap at an
/// epoch flip does not end the rewards.
pub fn aerodrome_rewards_end(latest: Option<(i64, i64)>) -> i64 {
    match latest {
        Some((read_at, finish)) if finish <= read_at => finish.min(LAUNCH_TS),
        _ => LAUNCH_TS,
    }
}

// ------------------------------------------------------------------------------------ penalty window

/// Pips denominator of Aero's gauge factory (1e6 = 100%).
pub const MAX_PIPS: u64 = 1_000_000;

/// Aero's early-unstake penalty as `CLGaugeFactory.effectivePenaltyConfig(gauge)` reports it: rewards of a
/// position unstaked or claimed while `block.number < depositBlock + minStakeBlocks` lose `penaltyRate` pips
/// (`Gauge._applyPenalty`). `minStakeBlocks` 0 = no window (Base at launch); the admin can change both at any time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PenaltyWindow {
    pub min_stake_blocks: u64,
    pub penalty_rate_pips: u64,
}

impl PenaltyWindow {
    /// Whether unstaking early would forfeit anything at all (the gauge's own condition).
    pub fn active(&self) -> bool {
        self.min_stake_blocks > 0 && self.penalty_rate_pips > 0
    }

    /// First block at which `tokenId` forfeits nothing: `depositBlock + minStakeBlocks`. None when no window
    /// applies or the deposit block is unknown (0 = not staked).
    pub fn penalty_free_block(&self, deposit_block: u64) -> Option<u64> {
        if !self.active() || deposit_block == 0 {
            return None;
        }
        Some(deposit_block.saturating_add(self.min_stake_blocks))
    }

    /// Blocks still to wait at `head` before nothing is forfeited (0 = free). Block numbers only: never seconds.
    pub fn blocks_left(&self, deposit_block: u64, head: u64) -> u64 {
        self.penalty_free_block(deposit_block).map(|free| free.saturating_sub(head)).unwrap_or(0)
    }

    /// Whether a reading taken somewhere between blocks `head_lo` and `head_hi` may be inside the window. Only a
    /// reading known to be past the window counts as outside it (an unknown block or deposit counts as inside).
    pub fn maybe_inside(&self, deposit_block: Option<u64>, head_lo: Option<u64>) -> bool {
        if !self.active() {
            return false;
        }
        match (deposit_block.filter(|d| *d > 0), head_lo) {
            (Some(d), Some(lo)) => self.blocks_left(d, lo) > 0,
            _ => true,
        }
    }
}

// ------------------------------------------------------------------------------------ measured rate

/// One position's `earned` reading: AERO wei (the account's `deferredEmissions` already out), the NFT's
/// liquidity, whether the pool tick is inside its range, and whether it may be inside the penalty window (where
/// `earned` reads low and the first delta after the window would be a spike).
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
    ts: i64,
    earned_wei: f64,
    liquidity: u128,
}

/// Emission rate per unit of in-range liquidity measured from positions' own `earned`:
/// `sum(delta earned) / sum(liquidity x delta t)` over the positions that stayed in range (same liquidity, no claim
/// in between) since their previous reading. AERO wei per second per unit of liquidity.
#[derive(Clone, Debug, Default)]
pub struct EarnedRateMeter {
    baselines: HashMap<u64, Baseline>,
}

/// One measured rate: wei per second per unit L, over how many position-steps and how many liquidity-seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeasuredRate {
    pub wei_per_s_per_l: f64,
    pub positions: usize,
    pub liquidity_seconds: f64,
}

impl EarnedRateMeter {
    /// Feeds one reading and returns what it measured (None when no position could be compared with its previous
    /// reading). `observe_sums` gives the raw sums for averaging over many readings.
    #[cfg(test)]
    pub fn observe(&mut self, now: i64, samples: &[EarnedSample]) -> Option<MeasuredRate> {
        let (delta, l_seconds, counted) = self.observe_sums(now, samples);
        if counted == 0 || l_seconds <= 0.0 {
            return None;
        }
        Some(MeasuredRate { wei_per_s_per_l: delta / l_seconds, positions: counted, liquidity_seconds: l_seconds })
    }

    /// (sum of earned deltas, sum of liquidity-seconds, positions counted) for one reading.
    pub fn observe_sums(&mut self, now: i64, samples: &[EarnedSample]) -> (f64, f64, usize) {
        let (mut delta_sum, mut l_seconds, mut counted) = (0.0f64, 0.0f64, 0usize);
        let mut next: HashMap<u64, Baseline> = HashMap::with_capacity(samples.len());
        for s in samples {
            if !s.in_range || s.penalised || s.liquidity == 0 || !s.earned_wei.is_finite() {
                // Out of range it earns nothing, inside the window `earned` reads low: no baseline until it is back.
                continue;
            }
            if let Some(b) = self.baselines.get(&s.token_id) {
                let dt = now - b.ts;
                // A drop means a claim or a re-stake reset the checkpoint; a liquidity change a top-up.
                if dt > 0 && b.liquidity == s.liquidity && s.earned_wei >= b.earned_wei {
                    delta_sum += s.earned_wei - b.earned_wei;
                    l_seconds += s.liquidity as f64 * dt as f64;
                    counted += 1;
                }
            }
            next.insert(s.token_id, Baseline { ts: now, earned_wei: s.earned_wei, liquidity: s.liquidity });
        }
        self.baselines = next;
        (delta_sum, l_seconds, counted)
    }

    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.baselines.len()
    }
}

// ------------------------------------------------------------------------------------ on-chain rate

/// `LeafVoter.gaugeStates(gauge)` (ILeafVoter.GaugeState, verified LeafVoter 0x3824...4078): every field is static,
/// so the return is 12 words: ceiling, claimed, lastSettlement, isRegistered, isActivated, surplus, lastIndex,
/// lastTimeIndex, then the Point inline (bias int128, slope int128, ts uint48, permanentStakeBalance uint128).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GaugeState {
    pub ceiling: u128,
    pub last_settlement: u64,
    pub is_registered: bool,
    pub is_activated: bool,
    pub point: GaugePoint,
}

/// The gauge's voting weight, decaying linearly from `ts` (IVoterCommon.Point).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GaugePoint {
    pub bias: i128,
    pub slope: i128,
    pub ts: u64,
    pub permanent_stake_balance: u128,
}

fn word_i128(w: &str) -> i128 {
    // A two's-complement 256-bit word whose value fits in 128 bits: the low 32 hex digits.
    let low = &w[w.len().saturating_sub(32)..];
    u128::from_str_radix(low, 16).map(|v| v as i128).unwrap_or(0)
}

pub fn decode_gauge_state(words: &[String]) -> Option<GaugeState> {
    if words.len() < 12 {
        return None;
    }
    let u = |i: usize| crate::chainmath::word_u128(&words[i]);
    Some(GaugeState {
        ceiling: u(0),
        last_settlement: u(2) as u64,
        is_registered: u(3) != 0,
        is_activated: u(4) != 0,
        point: GaugePoint { bias: word_i128(&words[8]), slope: word_i128(&words[9]), ts: u(10) as u64, permanent_stake_balance: u(11) },
    })
}

/// `effectivePenaltyConfig(gauge)` returns the struct (minStakeBlocks, penaltyRate) as two words.
pub fn decode_penalty_config(words: &[String]) -> Option<PenaltyWindow> {
    if words.len() < 2 {
        return None;
    }
    let u = |i: usize| crate::chainmath::word_u128(&words[i]).min(u64::MAX as u128) as u64;
    Some(PenaltyWindow { min_stake_blocks: u(0), penalty_rate_pips: u(1) })
}

/// A dynamic `uint256[]` return (`stakedValues(depositor)`): offset, length, items.
pub fn decode_uint_array(words: &[String]) -> Option<Vec<u128>> {
    let off = crate::chainmath::word_u128(words.first()?) as usize / 32;
    let n = crate::chainmath::word_u128(words.get(off)?) as usize;
    if words.len() < off + 1 + n {
        return None;
    }
    Some(words[off + 1..off + 1 + n].iter().map(|w| crate::chainmath::word_u128(w)).collect())
}

/// Call data for `earned(address account, uint256[] tokenIds)` (the gauge returns the SUM over the ids plus the
/// account's `deferredEmissions`).
pub fn earned_array_call(account: &str, token_ids: &[u128]) -> String {
    let mut s = crate::chainmath::selector("earned(address,uint256[])");
    s += &format!("{:0>64}", account.trim_start_matches("0x").to_ascii_lowercase());
    s += &format!("{:064x}", 64);
    s += &format!("{:064x}", token_ids.len());
    for id in token_ids {
        s += &format!("{id:064x}");
    }
    s
}

/// Fixed point scale of the voter's `emissionsPerVP` (ProtocolConstants.PRECISION).
pub const VOTER_PRECISION: f64 = 1e18;

/// AERO wei per second the LeafVoter credits the gauge right now, the way `LeafAllocationLibrary._accrueGaugeSegment`
/// accrues it: the gauge's weight at `now` (`max(permanentStakeBalance + bias - slope x (now - point.ts), 0)`) times
/// the chain's `emissionsPerVP` over PRECISION, capped at the gauge's per-second `emissionCap` (0 = deactivated).
/// The GROSS rate: the gauge takes the referral share out before an LP sees it ([`net_of_referral`]).
pub fn voter_gauge_rate_wei_per_s(point: &GaugePoint, emissions_per_vp: f64, cap_wei_per_s: f64, now: i64) -> f64 {
    let elapsed = (now - point.ts as i64).max(0) as f64;
    let weight = point.permanent_stake_balance as f64 + point.bias as f64 - point.slope as f64 * elapsed;
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

/// The LP's share of a gross gauge rate after Aero's referral cut (`referralConfig(gauge).share`, pips; the gauge
/// takes it only when a referral address is set).
pub fn net_of_referral(gross: f64, referral_share_pips: u64) -> f64 {
    gross * (1.0 - referral_share_pips.min(MAX_PIPS) as f64 / MAX_PIPS as f64)
}

/// Per unit of in-range staked liquidity: the pool spreads every settled wei over `stakedLiquidity`; with none in
/// range nobody earns (0 here). The brief divides the same way in SQL.
#[cfg(test)]
pub fn rate_per_staked_liquidity(rate_wei_per_s: f64, staked_liquidity: u128) -> f64 {
    if staked_liquidity == 0 {
        return 0.0;
    }
    rate_wei_per_s / staked_liquidity as f64
}

/// The exact on-chain rate between two readings: the rise of `LeafVoter.projectedCumulativeRewardShare(gauge)`
/// over the seconds between them (gross AERO wei per second). None when no time passed or it went backwards.
pub fn projected_share_rate(earlier: (i64, f64), later: (i64, f64)) -> Option<f64> {
    let ((t0, v0), (t1, v1)) = (earlier, later);
    if t1 <= t0 || v1 < v0 || !v1.is_finite() || !v0.is_finite() {
        return None;
    }
    Some((v1 - v0) / (t1 - t0) as f64)
}

/// How far a measured per-L rate sits from the chain's: `measured / on_chain` (1.0 = agreement).
pub fn cross_check_ratio(measured: f64, on_chain: f64) -> Option<f64> {
    (measured > 0.0 && on_chain > 0.0 && measured.is_finite() && on_chain.is_finite()).then(|| measured / on_chain)
}

// ------------------------------------------------------------------------------------ AERO price source

/// Where the AERO price comes from (`AIK_AERO_WETH_POOL`, default = Aerodrome's AERO/WETH CL200 pool, which holds
/// the OLD AERO).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AeroPriceSource {
    /// The configured pool holds WETH and the gauge's AERO: use it.
    Configured,
    /// Aero mode, the configured pool is unusable (not set yet, or not WETH + new AERO): price the new AERO from
    /// Aerodrome's AERO/WETH pool, where old AERO trades one for one with the new.
    AerodromeFallback,
    /// No usable pool: no AERO price.
    Unavailable,
}

impl AeroPriceSource {
    pub fn name(self) -> &'static str {
        match self {
            AeroPriceSource::Configured => "configured",
            AeroPriceSource::AerodromeFallback => "aerodrome-fallback",
            AeroPriceSource::Unavailable => "unavailable",
        }
    }
}

pub fn aero_price_source(mode: GaugeMode, configured_pool_ok: bool, aerodrome_pool_ok: bool) -> AeroPriceSource {
    match (mode, configured_pool_ok, aerodrome_pool_ok) {
        (_, true, _) => AeroPriceSource::Configured,
        (GaugeMode::Aero, false, true) => AeroPriceSource::AerodromeFallback,
        _ => AeroPriceSource::Unavailable,
    }
}

/// Whether a pool's two tokens are WETH and `aero`; Some(aero is token0) when they are.
pub fn aero_pool_order(token0: &str, token1: &str, aero: &str) -> Option<bool> {
    let (a, b, w, x) = (token0.to_ascii_lowercase(), token1.to_ascii_lowercase(), WETH.to_ascii_lowercase(), aero.to_ascii_lowercase());
    if a == w && b == x {
        Some(false)
    } else if a == x && b == w {
        Some(true)
    } else {
        None
    }
}

/// The pool tick expressed as AERO per WETH = 1.0001^tick (both tokens 18 decimals): the tick itself when WETH is
/// token0, its negative when AERO is.
pub fn aero_per_weth_tick(tick: i64, aero_is_token0: bool) -> i64 {
    if aero_is_token0 {
        -tick
    } else {
        tick
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainmath::words;

    // ---------------------------------------------------------------- launch and phantom rule

    #[test]
    fn the_launch_is_thursday_2026_10_22_midnight_utc() {
        let d = chrono::DateTime::from_timestamp(LAUNCH_TS, 0).unwrap();
        assert_eq!(d.format("%Y-%m-%d %H:%M:%S %a").to_string(), "2026-10-22 00:00:00 Thu");
        // Two hours after the migration portal opens (AerodromeMigration.MIGRATION_OPEN = 1792620000).
        assert_eq!(LAUNCH_TS - 1_792_620_000, 7_200);
        // An Aerodrome epoch boundary: epochs start Thursday 00:00 UTC (liquidity.rs).
        assert_eq!(LAUNCH_TS.rem_euclid(604_800), 0);
    }

    #[test]
    fn the_phantom_rule_zeroes_the_rate_at_and_after_period_finish() {
        let raw = 0.503;
        let finish = LAUNCH_TS;
        // Before periodFinish: paid.
        assert_eq!(aerodrome_rate_effective(raw, Some(finish), finish - 1), raw);
        // AT periodFinish (periodFinish <= now) and after: nothing, whatever rewardRate() still says.
        assert_eq!(aerodrome_rate_effective(raw, Some(finish), finish), 0.0);
        assert_eq!(aerodrome_rate_effective(raw, Some(finish), finish + 86_400 * 30), 0.0);
        // A period that ended before the launch (a gap at an epoch flip) is phantom too while it lasts.
        let flip = LAUNCH_TS - 7 * 86_400;
        assert_eq!(aerodrome_rate_effective(raw, Some(flip), flip + 60), 0.0);
        assert_eq!(aerodrome_rate_effective(raw, Some(flip + 7 * 86_400), flip + 60), raw, "renewed");
        // From the launch nothing is paid even if periodFinish were renewed or unknown.
        assert_eq!(aerodrome_rate_effective(raw, Some(LAUNCH_TS + 7 * 86_400), LAUNCH_TS + 1), 0.0);
        assert_eq!(aerodrome_rate_effective(raw, None, LAUNCH_TS), 0.0);
        // Unknown periodFinish before the launch: today's behaviour (the raw rate).
        assert_eq!(aerodrome_rate_effective(raw, None, LAUNCH_TS - 1), raw);
        assert!(aerodrome_paying(Some(finish), finish - 1) && !aerodrome_paying(Some(finish), finish));
    }

    #[test]
    fn the_end_of_aerodrome_rewards_for_replays() {
        // No reading at all, or the last reading shows a running period: the launch.
        assert_eq!(aerodrome_rewards_end(None), LAUNCH_TS);
        assert_eq!(aerodrome_rewards_end(Some((LAUNCH_TS - 3600, LAUNCH_TS))), LAUNCH_TS);
        // The last reading (after the launch) shows the final period ended at the launch.
        assert_eq!(aerodrome_rewards_end(Some((LAUNCH_TS + 3600, LAUNCH_TS))), LAUNCH_TS);
        // Ended earlier and never renewed: from then.
        assert_eq!(aerodrome_rewards_end(Some((LAUNCH_TS - 1800, LAUNCH_TS - 3600))), LAUNCH_TS - 3600);
        // A renewal past the launch never extends the rewards past it.
        assert_eq!(aerodrome_rewards_end(Some((LAUNCH_TS + 99 * 86_400, LAUNCH_TS + 7 * 86_400))), LAUNCH_TS);
    }

    // ---------------------------------------------------------------- mode probe

    #[test]
    fn the_probe_picks_the_gauge_by_what_answers_and_refuses_the_rest() {
        use GaugeModeSetting::*;
        assert_eq!(detect_gauge_mode(Auto, true, false), Ok(GaugeMode::Aerodrome));
        assert_eq!(detect_gauge_mode(Auto, false, true), Ok(GaugeMode::Aero));
        // Both answering is read as Aerodrome: only its gauge has rewardRate().
        assert_eq!(detect_gauge_mode(Auto, true, true), Ok(GaugeMode::Aerodrome));
        assert!(detect_gauge_mode(Auto, false, false).unwrap_err().contains("neither"));
        // An explicit mode is checked against the gauge, never trusted blindly.
        assert_eq!(detect_gauge_mode(Aerodrome, true, false), Ok(GaugeMode::Aerodrome));
        assert_eq!(detect_gauge_mode(Aero, false, true), Ok(GaugeMode::Aero));
        assert!(detect_gauge_mode(Aerodrome, false, true).unwrap_err().contains("AIK_GAUGE_MODE=aerodrome"));
        assert!(detect_gauge_mode(Aero, true, false).unwrap_err().contains("AIK_GAUGE_MODE=aero"));
        assert!(detect_gauge_mode(Aero, false, false).is_err());
        assert!(detect_gauge_mode(Aerodrome, false, false).is_err());
        // The variable's spellings.
        assert_eq!("auto".parse::<GaugeModeSetting>(), Ok(Auto));
        assert_eq!("".parse::<GaugeModeSetting>(), Ok(Auto));
        assert_eq!(" Aerodrome ".parse::<GaugeModeSetting>(), Ok(Aerodrome));
        assert_eq!("AERO".parse::<GaugeModeSetting>(), Ok(Aero));
        assert!("velodrome".parse::<GaugeModeSetting>().is_err());
        assert_eq!(GaugeMode::from_name(GaugeMode::Aero.name()), Some(GaugeMode::Aero));
        assert_eq!(GaugeMode::from_name(GaugeMode::Aerodrome.name()), Some(GaugeMode::Aerodrome));
    }

    // ---------------------------------------------------------------- decoding

    fn w(v: u128) -> String {
        format!("{v:064x}")
    }
    fn neg(v: i128) -> String {
        format!("{}{:032x}", "f".repeat(32), v as u128)
    }

    #[test]
    fn aero_decode_gauge_states_penalty_struct_and_arrays() {
        // gaugeStates: ceiling, claimed, lastSettlement, isRegistered, isActivated, surplus, lastIndex,
        // lastTimeIndex, point.bias, point.slope, point.ts, point.permanentStakeBalance.
        let data = format!(
            "0x{}{}{}{}{}{}{}{}{}{}{}{}",
            w(5_000),
            w(4_000),
            w(1_792_627_200),
            w(1),
            w(1),
            w(77),
            w(123),
            w(456),
            w(1_000_000),
            neg(-3),
            w(1_792_627_100),
            w(7_000)
        );
        let s = decode_gauge_state(&words(&data)).unwrap();
        assert_eq!(s.ceiling, 5_000);
        assert_eq!(s.last_settlement, 1_792_627_200);
        assert!(s.is_registered && s.is_activated);
        assert_eq!(s.point, GaugePoint { bias: 1_000_000, slope: -3, ts: 1_792_627_100, permanent_stake_balance: 7_000 });
        // A negative bias (a desynced point) decodes as negative, and the rate then floors at zero weight.
        let mut v = words(&data);
        v[8] = neg(-5);
        v[11] = w(0);
        let p = decode_gauge_state(&v).unwrap().point;
        assert_eq!(p.bias, -5);
        assert_eq!(voter_gauge_rate_wei_per_s(&p, 1e18, 1e30, p.ts as i64), 0.0);
        assert_eq!(decode_gauge_state(&v[..11]), None, "eleven words is not the verified layout");

        // effectivePenaltyConfig: the struct (minStakeBlocks, penaltyRate) as two words. Base at launch: (0, 100%).
        let pc = decode_penalty_config(&words(&format!("0x{}{}", w(0), w(1_000_000)))).unwrap();
        assert_eq!(pc, PenaltyWindow { min_stake_blocks: 0, penalty_rate_pips: MAX_PIPS });
        assert!(!pc.active());
        assert_eq!(decode_penalty_config(&words(&format!("0x{}", w(5)))), None);

        // stakedValues: offset 0x20, length 3, items.
        let arr = format!("0x{}{}{}{}{}", w(32), w(3), w(7_610_845), w(7_610_846), w(9));
        assert_eq!(decode_uint_array(&words(&arr)), Some(vec![7_610_845, 7_610_846, 9]));
        assert_eq!(decode_uint_array(&words(&format!("0x{}{}", w(32), w(0)))), Some(vec![]));
        assert_eq!(decode_uint_array(&words(&format!("0x{}{}{}", w(32), w(2), w(1)))), None, "truncated");

        // earned(address,uint256[]): selector, account, offset 0x40, length, ids. The gauge answers one word: the SUM.
        let call = earned_array_call("0x61986AFae9B07314d855E7C54949E30A866c93a8", &[7_610_845, 9]);
        assert!(call.starts_with(&crate::chainmath::selector("earned(address,uint256[])")));
        let body = words(&call[10..]);
        assert_eq!(body.len(), 5);
        assert_eq!(crate::chainmath::word_address(&body[0]), "0x61986afae9b07314d855e7c54949e30a866c93a8");
        assert_eq!(crate::chainmath::word_u128(&body[1]), 64);
        assert_eq!(crate::chainmath::word_u128(&body[2]), 2);
        assert_eq!(crate::chainmath::word_u128(&body[3]), 7_610_845);
        assert_eq!(earned_array_call("0x0000000000000000000000000000000000000000", &[]).len(), 10 + 3 * 64);
    }

    // ---------------------------------------------------------------- penalty window in blocks

    #[test]
    fn the_window_is_blocks_only_zero_today_and_whatever_the_admin_sets_later() {
        let today = PenaltyWindow { min_stake_blocks: 0, penalty_rate_pips: MAX_PIPS };
        assert!(!today.active());
        assert_eq!(today.penalty_free_block(52_900_000), None);
        assert_eq!(today.blocks_left(52_900_000, 52_900_000), 0);
        assert!(!today.maybe_inside(None, None), "no window: nothing is ever inside it");
        let five = PenaltyWindow { min_stake_blocks: 5, penalty_rate_pips: MAX_PIPS };
        assert_eq!(five.penalty_free_block(1_000), Some(1_005));
        assert_eq!(five.blocks_left(1_000, 1_004), 1);
        assert_eq!(five.blocks_left(1_000, 1_005), 0, "block.number < deposit + min is the gauge's condition");
        assert!(five.maybe_inside(Some(1_000), Some(1_004)));
        assert!(!five.maybe_inside(Some(1_000), Some(1_005)));
        assert!(five.maybe_inside(None, Some(2_000)), "unknown deposit block: counted as inside");
        assert!(five.maybe_inside(Some(1_000), None), "unknown head block: counted as inside");
        let later = PenaltyWindow { min_stake_blocks: 300, penalty_rate_pips: 250_000 };
        assert_eq!(later.blocks_left(1_000, 1_100), 200);
        let free = PenaltyWindow { min_stake_blocks: 300, penalty_rate_pips: 0 };
        assert!(!free.active());
        // Base's block time changes (2 s -> 200 ms): the wait is the same 5 BLOCKS either way. Nothing here takes
        // a time or a block time at all, so no seconds can creep in.
        assert_eq!(five.blocks_left(1_000, 1_000), five.min_stake_blocks);
    }

    // ---------------------------------------------------------------- a mock that follows Aero's maths

    const Q128: f64 = 340_282_366_920_938_463_463_374_607_431_768_211_456.0; // 2^128

    /// LeafVoter + CLPool + CLGauge in miniature, with the accounting of the verified sources: the voter credits
    /// `weight x emissionsPerVP / PRECISION` per second capped at `cap`, `earned` projects the unsettled share into
    /// `rewardGrowthGlobalX128` over `stakedLiquidity`, a position's growth inside its range only moves while the
    /// tick is inside it, and `earned` is `(growthInside - checkpoint) x L / Q128` less the penalty inside the
    /// window and the referral share, plus the account's deferred emissions.
    struct MockAero {
        point: GaugePoint,
        emissions_per_vp: f64,
        cap: f64,
        ceiling: f64,
        settled_at: i64,
        last_cumulative: f64,
        staked_liquidity: u128,
        tick: i32,
        positions: Vec<(u64, u128, i32, i32, f64, f64, u64)>, // id, L, lower, upper, growth inside, checkpoint, deposit block
        deferred: f64,
        referral_share_pips: u64,
        window: PenaltyWindow,
        head_block: u64,
    }

    impl MockAero {
        fn projected(&self, now: i64) -> f64 {
            self.ceiling + voter_gauge_rate_wei_per_s(&self.point, self.emissions_per_vp, self.cap, now) * (now - self.settled_at) as f64
        }
        fn settle(&mut self, now: i64) {
            let projected = self.projected(now);
            let delta = projected - self.last_cumulative;
            self.ceiling = projected;
            self.settled_at = now;
            self.last_cumulative = projected;
            if self.staked_liquidity > 0 {
                let g = delta * Q128 / self.staked_liquidity as f64;
                let tick = self.tick;
                for p in &mut self.positions {
                    if p.2 <= tick && tick < p.3 {
                        p.4 += g;
                    }
                }
            }
        }
        fn in_range(&self, i: usize) -> bool {
            let p = &self.positions[i];
            p.2 <= self.tick && self.tick < p.3
        }
        fn penalised(&self, i: usize) -> bool {
            self.window.active() && self.head_block < self.positions[i].6 + self.window.min_stake_blocks
        }
        fn earned(&self, i: usize, now: i64) -> f64 {
            let p = &self.positions[i];
            let unsettled = (self.projected(now) - self.last_cumulative).max(0.0);
            let extra = if self.staked_liquidity > 0 && self.in_range(i) { unsettled * Q128 / self.staked_liquidity as f64 } else { 0.0 };
            let mut r = (p.4 + extra - p.5) * p.1 as f64 / Q128;
            if r > 0.0 {
                if self.penalised(i) {
                    r -= r * self.window.penalty_rate_pips as f64 / MAX_PIPS as f64;
                }
                r -= r * self.referral_share_pips as f64 / MAX_PIPS as f64;
            }
            r + self.deferred
        }
        fn samples(&self, now: i64) -> Vec<EarnedSample> {
            (0..self.positions.len())
                .map(|i| EarnedSample {
                    token_id: self.positions[i].0,
                    earned_wei: self.earned(i, now) - self.deferred,
                    liquidity: self.positions[i].1,
                    in_range: self.in_range(i),
                    penalised: self.penalised(i),
                })
                .collect()
        }
        fn on_chain_rate_per_l(&self, now: i64) -> f64 {
            let gross = voter_gauge_rate_wei_per_s(&self.point, self.emissions_per_vp, self.cap, now);
            rate_per_staked_liquidity(net_of_referral(gross, self.referral_share_pips), self.staked_liquidity)
        }
    }

    fn mock() -> MockAero {
        // A permanent allocation of 1,000,000 AERO of voting weight at 1e-9 AERO per VP-second: 0.001 AERO/s, shared
        // by 2e16 of in-range staked liquidity (other stakers included). Position 3 is out of range throughout.
        MockAero {
            point: GaugePoint { bias: 0, slope: 0, ts: 1_000_000, permanent_stake_balance: 1_000_000_000_000_000_000_000_000 },
            emissions_per_vp: 1e9,
            cap: 1e17,
            ceiling: 0.0,
            settled_at: 1_000_000,
            last_cumulative: 0.0,
            staked_liquidity: 20_000_000_000_000_000,
            tick: 0,
            positions: vec![
                (1, 4_000_000_000_000_000, -200, 200, 0.0, 0.0, 100),
                (2, 1_000_000_000_000_000, -100, 100, 0.0, 0.0, 100),
                (3, 9_000_000_000_000_000, 1_000, 2_000, 0.0, 0.0, 100),
            ],
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
    fn the_on_chain_rate_and_the_measured_rate_agree_on_a_mock_that_follows_aeros_maths() {
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000i64;
        assert_eq!(meter.observe(t, &m.samples(t)), None, "the first reading only sets baselines");
        assert_eq!(meter.tracked(), 2, "only the in-range positions carry a baseline");
        for minute in 1..=10 {
            t += 60;
            m.head_block += 30;
            if minute % 3 == 0 {
                m.settle(t); // a swap crossing a tick or a claim settles the pool now and then
            }
            let r = meter.observe(t, &m.samples(t)).expect("a rate every minute from the second reading on");
            assert_eq!(r.positions, 2);
            assert!(close(r.wei_per_s_per_l, m.on_chain_rate_per_l(t), 1e-6), "minute {minute}");
            assert!(close(cross_check_ratio(r.wei_per_s_per_l, m.on_chain_rate_per_l(t)).unwrap(), 1.0, 1e-6));
        }
        // The number itself: 0.001 AERO/s over 2e16 in-range staked liquidity.
        assert!(close(m.on_chain_rate_per_l(t), 1e15 / 2e16, 1e-12));
        // The projected-share delta between two readings is the same gross rate as the closed form.
        let (a, b) = ((t, m.projected(t)), (t + 300, m.projected(t + 300)));
        assert!(close(projected_share_rate(a, b).unwrap(), voter_gauge_rate_wei_per_s(&m.point, m.emissions_per_vp, m.cap, t), 1e-9));
        assert_eq!(projected_share_rate(b, b), None);
        assert_eq!(projected_share_rate(b, (t + 600, 0.0)), None, "a deregistered gauge reports 0");
    }

    #[test]
    fn rate_changes_caps_referrals_claims_and_the_window_are_handled() {
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000i64;
        meter.observe(t, &m.samples(t));
        t += 60;
        let before = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
        // A new emissionsPerVP: the voter settles at the old rate first.
        m.settle(t);
        m.emissions_per_vp *= 2.5;
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
        assert!(close(r, before * 2.5, 1e-6) && close(r, m.on_chain_rate_per_l(t), 1e-6));
        // The cap binds: paid at the cap, not above it. A zero cap (deactivated gauge) pays nothing.
        m.settle(t);
        m.emissions_per_vp = 1e30;
        t += 60;
        let capped = meter.observe(t, &m.samples(t)).unwrap().wei_per_s_per_l;
        assert!(close(capped, m.cap / m.staked_liquidity as f64, 1e-6), "capped {capped}");
        assert_eq!(voter_gauge_rate_wei_per_s(&m.point, 1e9, 0.0, t), 0.0);
        // A decaying stake: weight = bias - slope x elapsed, floored at zero.
        let p = GaugePoint { bias: 1_000_000, slope: 10, ts: 100, permanent_stake_balance: 0 };
        assert!(close(voter_gauge_rate_wei_per_s(&p, 1e18, 1e30, 100 + 50_000), 500_000.0, 1e-12));
        assert_eq!(voter_gauge_rate_wei_per_s(&p, 1e18, 1e30, 100 + 200_000), 0.0);
        assert!(close(voter_gauge_rate_wei_per_s(&p, 1e18, 1e30, 50), 1_000_000.0, 1e-12), "a clock behind the point");
        assert_eq!(rate_per_staked_liquidity(1e15, 0), 0.0, "nothing in range: nobody earns");

        // The referral share separates the measured (net) rate from the gross one.
        let mut m = mock();
        m.referral_share_pips = 50_000;
        let mut meter = EarnedRateMeter::default();
        let t = 1_000_000i64;
        meter.observe(t, &m.samples(t));
        let r = meter.observe(t + 60, &m.samples(t + 60)).unwrap().wei_per_s_per_l;
        let gross = rate_per_staked_liquidity(voter_gauge_rate_wei_per_s(&m.point, m.emissions_per_vp, m.cap, t + 60), m.staked_liquidity);
        assert!(close(cross_check_ratio(r, gross).unwrap(), 0.95, 1e-6));
        assert!(close(r, m.on_chain_rate_per_l(t + 60), 1e-6));
        assert_eq!(cross_check_ratio(0.0, gross), None);

        // Out of range, a claim, a deferred balance and a penalty window never corrupt the measurement.
        let mut m = mock();
        let mut meter = EarnedRateMeter::default();
        let mut t = 1_000_000i64;
        meter.observe(t, &m.samples(t));
        let expected = m.on_chain_rate_per_l(t);
        m.settle(t);
        m.tick = 150; // inside 1's range, outside 2's
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 1);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
        m.settle(t);
        m.tick = 0;
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 1, "back in range: a fresh baseline first");
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 2);
        m.settle(t);
        m.positions[0].5 = m.positions[0].4; // a claim on position 1
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 1);
        m.deferred = 5e18;
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 2);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
        m.window = PenaltyWindow { min_stake_blocks: 5, penalty_rate_pips: MAX_PIPS };
        m.settle(t);
        m.positions[1].6 = m.head_block - 2; // re-staked 2 blocks ago under a 5-block window
        m.positions[1].5 = m.positions[1].4;
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 1);
        m.head_block += 30;
        t += 60;
        assert_eq!(meter.observe(t, &m.samples(t)).unwrap().positions, 1, "first reading after the window only sets the baseline");
        t += 60;
        let r = meter.observe(t, &m.samples(t)).unwrap();
        assert_eq!(r.positions, 2);
        assert!(close(r.wei_per_s_per_l, expected, 1e-6));
    }

    // ---------------------------------------------------------------- AERO price

    #[test]
    fn the_aero_price_falls_back_to_the_aerodrome_pool_only_in_aero_mode() {
        use AeroPriceSource::*;
        assert_eq!(aero_price_source(GaugeMode::Aero, true, true), Configured);
        assert_eq!(aero_price_source(GaugeMode::Aero, false, true), AerodromeFallback);
        assert_eq!(aero_price_source(GaugeMode::Aero, false, false), Unavailable);
        assert_eq!(aero_price_source(GaugeMode::Aerodrome, true, true), Configured);
        assert_eq!(aero_price_source(GaugeMode::Aerodrome, false, true), Unavailable);
        assert_eq!(aero_price_source(GaugeMode::Aerodrome, false, false), Unavailable);
        // Token order: Aerodrome's pool is WETH/AERO (WETH token0); a new AERO pool sorts AERO first (0x3f5b < 0x4200).
        assert_eq!(aero_pool_order(WETH, OLD_AERO, OLD_AERO), Some(false));
        assert_eq!(aero_pool_order(&NEW_AERO.to_lowercase(), WETH, NEW_AERO), Some(true));
        assert_eq!(aero_pool_order(WETH, OLD_AERO, NEW_AERO), None, "the old pool does not hold the new AERO");
        assert_eq!(aero_pool_order(WETH, "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", NEW_AERO), None);
        assert!(NEW_AERO.to_lowercase() < WETH.to_lowercase());
        // The same price either way round: AERO per WETH = 1.0001^tick with WETH token0, 1.0001^-tick with AERO token0.
        assert_eq!(aero_per_weth_tick(81_200, false), 81_200);
        assert_eq!(aero_per_weth_tick(-81_200, true), 81_200);
    }
}
