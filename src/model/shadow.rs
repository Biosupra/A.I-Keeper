//! Shadow mode: for every hourly width table the live keeper produces, the keeper's strategy and
//! each A.I model's strategy are replayed over the next 24 hours of real pool prices under the
//! keeper's EXACT rules, and graded. No order is ever sent; this only writes rows to the A.I's own
//! database.
//!
//! The rules are a port of the live keeper (Panda-Financial-KeeperBot, keeper 0.8.0, src/policy.rs
//! `decide`, `choose_width`, `best_for_position`, `should_hold_out`, `assess_recut`,
//! `confirm_recut`, `recut_gate`, `divergence_fraction`, and keeper.rs's use of them), with the
//! settings read from the keeper's own `keeper_runs` log so a policy change is followed
//! automatically:
//! * trigger on the 5-minute TWAP; rebalance only after it has been outside for
//!   `confirmation_secs` without re-entering; 2 h cooldown; at most 3 rebalances in 24 h;
//! * hold while the pool fee is above `fee_hold_pips` or spot is more than
//!   `max_twap_deviation_ticks` from the TWAP, for at most `hold_max_secs`;
//! * crash valve: when no width pays and even the widest would be thrown out
//!   `hold_out_widest_exits_per_day`, sit out, capped at `hold_out_max_secs` / `hold_out_max_ticks`;
//! * re-cut an in-range position when the table asks for a width at least `recut_min_ratio` away,
//!   worth `recut_margin` × the one-off cost over `recut_horizon_secs`, in the same direction for
//!   `recut_confirm_secs`, subject to the narrowing dwell/floor and the fresh-deposit widening dwell;
//! * a new range is the table's hysteresis choice (`choose_width`: the ±2% home width unless a
//!   narrower one is 10% better or a wider one 25% better), snapped to the 50-tick grid;
//! * divergence is the keeper's `divergence_fraction` (value lost against the range's mint-time
//!   mix), realised at each close and marked to market at the end.
//!
//! How the A.I's strategies differ from the keeper's: only in the width table they act on. The
//! keeper's table scores every width with its simulator at the keeper's volatility. A model keeps
//! that simulator's physics and swaps in its own volatility: a random walk that must travel `a` at
//! volatility `s` behaves like one travelling `a * s0 / s` at `s0`, so under the model's volatility
//! width `h` behaves like the keeper's row at `h_eff = h * s_keeper / s_model` (time in range,
//! rebalances, and divergence rescaled by `s_model / s_keeper`), while the reward per day in range
//! stays that of the real width `h`. With `s_model = s_keeper` this is exactly the keeper's table.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

/// ln(1.0001): one tick in log-price (keeper policy.rs LN_TICK).
pub const LN_TICK: f64 = 0.000_099_995_000_333_3;
pub const TICK_SPACING: f64 = 50.0;
/// The keeper's plausibility ceiling on gross APR (policy.rs MAX_PLAUSIBLE_APR).
const MAX_PLAUSIBLE_APR: f64 = 100.0;

fn default_gas() -> f64 {
    0.02
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Row {
    pub h: f64,
    pub gross_apr: f64,
    pub in_range: f64,
    pub rebalances: f64,
    pub divergence: f64,
    pub capture: f64,
    #[serde(default)]
    pub exec_fee_pips: Option<f64>,
    pub net_ex_div: f64,
    pub net: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub ts: i64,
    pub tick: f64,
    pub vol: f64,
    pub position_usd: f64,
    /// Gas of one rebalance in USD, as the keeper priced it for this table.
    #[serde(default = "default_gas")]
    pub gas_usd: f64,
    pub choice: i64,
    pub rows: Vec<Row>,
}

impl Table {
    /// From a keeper `width_evals` row (schema 4). None if it lacks what the comparison needs.
    pub fn from_keeper(v: &Value) -> Option<Table> {
        let f = |x: &Value, k: &str| x[k].as_f64();
        let mut rows: Vec<Row> = v["table"]
            .as_array()?
            .iter()
            .filter_map(|r| {
                Some(Row {
                    h: f(r, "half_spacings")?,
                    gross_apr: f(r, "gross_apr")?,
                    in_range: f(r, "in_range_fraction")?,
                    rebalances: f(r, "rebalances_per_day")?,
                    divergence: f(r, "divergence_per_day").unwrap_or(0.0),
                    capture: f(r, "emission_capture").unwrap_or(1.0),
                    exec_fee_pips: f(r, "exec_fee_pips"),
                    net_ex_div: f(r, "net_ex_divergence_per_day")?,
                    net: f(r, "net_per_day")?,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.h.total_cmp(&b.h));
        if rows.len() < 2 {
            return None;
        }
        Some(Table {
            ts: v["ts"].as_i64()?,
            tick: v["current_tick"].as_f64()?,
            vol: v["realised_vol_hourly"].as_f64()?,
            position_usd: v["position_usd"].as_f64().unwrap_or(1000.0),
            gas_usd: v["gas_usd"].as_f64().unwrap_or(default_gas()),
            choice: v["choice"].as_i64()?,
            rows,
        })
    }

    /// Cost of one rebalance as a fraction of the position: half a swap at the simulated execution
    /// fee plus gas (keeper `choose_width`). Rows written before exec_fee_pips was stored fall back
    /// to what the row itself implies.
    pub fn cost_per_rebalance(&self, r: &Row) -> f64 {
        match r.exec_fee_pips {
            Some(f) => 0.5 * f / 1e6 + if self.position_usd > 0.0 { self.gas_usd / self.position_usd } else { 0.0 },
            None if r.rebalances > 1e-9 => ((r.gross_apr / 365.0 * 0.8 * r.in_range * r.capture - r.net_ex_div) / r.rebalances).max(0.0),
            None => 0.0,
        }
    }

    pub fn row(&self, h: i64) -> &Row {
        self.rows.iter().min_by_key(|r| (r.h as i64 - h).abs()).expect("table has rows")
    }

    /// Row metrics at a fractional width, by linear interpolation (clamped to the table).
    fn at(&self, h: f64) -> Row {
        let rs = &self.rows;
        if h <= rs[0].h {
            return rs[0].clone();
        }
        if h >= rs[rs.len() - 1].h {
            return rs[rs.len() - 1].clone();
        }
        let i = rs.partition_point(|r| r.h <= h);
        let (a, b) = (&rs[i - 1], &rs[i]);
        let t = (h - a.h) / (b.h - a.h);
        let l = |x: f64, y: f64| x + (y - x) * t;
        Row {
            h,
            gross_apr: l(a.gross_apr, b.gross_apr),
            in_range: l(a.in_range, b.in_range),
            rebalances: l(a.rebalances, b.rebalances),
            divergence: l(a.divergence, b.divergence),
            capture: l(a.capture, b.capture),
            exec_fee_pips: match (a.exec_fee_pips, b.exec_fee_pips) {
                (Some(x), Some(y)) => Some(l(x, y)),
                _ => None,
            },
            net_ex_div: l(a.net_ex_div, b.net_ex_div),
            net: l(a.net, b.net),
        }
    }

    /// Every width's score at a given volatility (see the module comment). At the keeper's own
    /// volatility this returns the keeper's scores exactly.
    pub fn rescore(&self, vol: f64) -> Vec<(i64, f64)> {
        let k = if vol > 0.0 { self.vol / vol } else { 1.0 };
        self.rows
            .iter()
            .map(|r| {
                if (k - 1.0).abs() < 1e-12 {
                    return (r.h as i64, r.net);
                }
                let eff = self.at(r.h * k);
                let revenue = r.gross_apr / 365.0 * 0.8 * eff.in_range * eff.capture;
                let cost = self.cost_per_rebalance(r) * eff.rebalances;
                let divergence = eff.divergence / k.max(1e-9);
                (r.h as i64, revenue - cost - divergence)
            })
            .collect()
    }

    /// The keeper's scores for a position of a different size: gas is the only size-dependent term
    /// (keeper `net_for_position`), so each row moves by rebalances x (gas/table size - gas/size).
    pub fn nets_for_size(&self, nets: &[(i64, f64)], position_usd: f64) -> Vec<(i64, f64)> {
        if position_usd <= 0.0 || self.position_usd <= 0.0 {
            return nets.to_vec();
        }
        nets.iter()
            .map(|&(h, n)| {
                let r = self.row(h);
                (h, n + r.rebalances * (self.gas_usd / self.position_usd - self.gas_usd / position_usd))
            })
            .collect()
    }

    fn plausible(&self) -> bool {
        self.rows.iter().all(|r| r.net.is_finite() && r.gross_apr <= MAX_PLAUSIBLE_APR && r.gross_apr >= 0.0)
    }
}

/// The keeper's rebalance policy settings (keeper config.rs / policy.rs `PolicyParams`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub twap_secs: i64,
    pub confirmation_secs: i64,
    pub cooldown_secs: i64,
    pub max_rebalances_per_day: usize,
    pub fee_hold_pips: f64,
    pub max_twap_deviation_ticks: f64,
    pub hold_max_secs: i64,
    pub danger_zone_fraction: f64,
    pub min_position_usd: f64,
    pub preferred_half_width_spacings: i64,
    pub narrow_margin: f64,
    pub widen_margin: f64,
    pub hold_out_when_unprofitable: bool,
    pub hold_out_widest_exits_per_day: f64,
    pub hold_out_max_secs: i64,
    pub hold_out_max_ticks: f64,
    pub recut: bool,
    pub recut_min_ratio: f64,
    pub recut_horizon_secs: i64,
    pub recut_margin: f64,
    pub recut_confirm_secs: i64,
    pub recut_narrow_min_dwell_secs: i64,
    pub recut_widen_min_dwell_secs: i64,
    pub recut_narrow_margin: f64,
    pub recut_narrow_floor_spacings: i64,
    pub user_reward_share: f64,
}

impl Default for Policy {
    /// Keeper 0.8.0 as logged in keeper_runs on 2026-09-26 (and config.rs for the one setting the
    /// log omits, RECUT_WIDEN_MIN_DWELL_SECS = 43200).
    fn default() -> Self {
        Policy {
            twap_secs: 300,
            confirmation_secs: 21_600,
            cooldown_secs: 7_200,
            max_rebalances_per_day: 3,
            fee_hold_pips: 1000.0,
            max_twap_deviation_ticks: 50.0,
            hold_max_secs: 7_200,
            danger_zone_fraction: 0.0,
            min_position_usd: 0.5,
            preferred_half_width_spacings: 4,
            narrow_margin: 0.10,
            widen_margin: 0.25,
            hold_out_when_unprofitable: true,
            hold_out_widest_exits_per_day: 0.65,
            hold_out_max_secs: 86_400,
            hold_out_max_ticks: 400.0,
            recut: true,
            recut_min_ratio: 2.0,
            recut_horizon_secs: 86_400,
            recut_margin: 4.0,
            recut_confirm_secs: 10_800,
            recut_narrow_min_dwell_secs: 43_200,
            recut_widen_min_dwell_secs: 43_200,
            recut_narrow_margin: 4.0,
            recut_narrow_floor_spacings: 6,
            user_reward_share: 0.8,
        }
    }
}

impl Policy {
    /// The policy a keeper start logged (`keeper_runs` row); settings it omits keep their defaults.
    pub fn from_keeper_run(v: &Value) -> Policy {
        let p = &v["policy"];
        let mut out = Policy::default();
        macro_rules! set {
            ($field:ident, i64) => {
                if let Some(x) = p[stringify!($field)].as_i64() {
                    out.$field = x;
                }
            };
            ($field:ident, f64) => {
                if let Some(x) = p[stringify!($field)].as_f64() {
                    out.$field = x;
                }
            };
            ($field:ident, bool) => {
                if let Some(x) = p[stringify!($field)].as_bool() {
                    out.$field = x;
                }
            };
        }
        set!(twap_secs, i64);
        set!(confirmation_secs, i64);
        set!(cooldown_secs, i64);
        if let Some(x) = p["max_rebalances_per_day"].as_u64() {
            out.max_rebalances_per_day = x as usize;
        }
        set!(fee_hold_pips, f64);
        set!(max_twap_deviation_ticks, f64);
        set!(hold_max_secs, i64);
        set!(danger_zone_fraction, f64);
        set!(min_position_usd, f64);
        set!(preferred_half_width_spacings, i64);
        set!(narrow_margin, f64);
        set!(widen_margin, f64);
        set!(hold_out_when_unprofitable, bool);
        set!(hold_out_widest_exits_per_day, f64);
        set!(hold_out_max_secs, i64);
        set!(hold_out_max_ticks, f64);
        set!(recut, bool);
        set!(recut_min_ratio, f64);
        set!(recut_horizon_secs, i64);
        set!(recut_margin, f64);
        set!(recut_confirm_secs, i64);
        set!(recut_narrow_min_dwell_secs, i64);
        set!(recut_widen_min_dwell_secs, i64);
        set!(recut_narrow_margin, f64);
        set!(recut_narrow_floor_spacings, i64);
        set!(user_reward_share, f64);
        out
    }
}

/// Keeper `choose_width`'s hysteresis on a set of scores: the home width unless a narrower one is
/// `narrow_margin` better or a wider one `widen_margin` better (ties toward the home width).
pub fn choose(table: &Table, nets: &[(i64, f64)], p: &Policy) -> i64 {
    let lo = nets.iter().map(|x| x.0).min().unwrap_or(p.preferred_half_width_spacings);
    let hi = nets.iter().map(|x| x.0).max().unwrap_or(p.preferred_half_width_spacings);
    let preferred = p.preferred_half_width_spacings.clamp(lo, hi);
    if !table.plausible() || nets.iter().any(|x| !x.1.is_finite()) {
        return preferred;
    }
    let pref_net = nets.iter().find(|x| x.0 == preferred).map(|x| x.1).unwrap_or(0.0);
    let qualifies = |net: f64, margin: f64| {
        if pref_net <= 0.0 {
            net > 0.0 || net - pref_net >= margin * pref_net.abs()
        } else {
            net - pref_net >= margin * pref_net
        }
    };
    let (mut choice, mut choice_net) = (preferred, pref_net);
    if let Some(e) = nets.iter().filter(|x| x.0 < preferred).max_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0))) {
        if qualifies(e.1, p.narrow_margin) && e.1 > choice_net {
            choice = e.0;
            choice_net = e.1;
        }
    }
    if let Some(e) = nets.iter().filter(|x| x.0 > preferred).max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0))) {
        if qualifies(e.1, p.widen_margin) && e.1 > choice_net {
            choice = e.0;
        }
    }
    choice
}

/// Keeper `best_for_position` (the scores already include gas at the table's position size).
/// Ties go to the wider range.
pub fn best(nets: &[(i64, f64)]) -> Option<(i64, f64)> {
    nets.iter().filter(|x| x.1.is_finite()).copied().max_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
}

/// Keeper `divergence_fraction`: what a position on [lower, upper] minted at `mint` gave up by
/// `tick`, against simply holding its mint-time token mix, as a fraction of its value at mint.
pub fn divergence_fraction(mint: f64, tick: f64, lower: f64, upper: f64) -> f64 {
    let sqrt_at = |t: f64| (t * LN_TICK / 2.0).exp();
    let (sa, sb) = (sqrt_at(lower), sqrt_at(upper));
    let (s0, s1) = (sqrt_at(mint), sqrt_at(tick));
    let amounts = |s: f64| -> (f64, f64) {
        if s <= sa {
            ((sb - sa) / (sa * sb), 0.0)
        } else if s < sb {
            ((sb - s) / (s * sb), s - sa)
        } else {
            (0.0, sb - sa)
        }
    };
    let (a0, a1) = amounts(s0);
    let value_at_mint = a0 * s0 * s0 + a1;
    if value_at_mint <= 0.0 || !value_at_mint.is_finite() {
        return 0.0;
    }
    let (b0, b1) = amounts(s1);
    let lp_now = b0 * s1 * s1 + b1;
    let hold_now = a0 * s1 * s1 + a1;
    ((hold_now - lp_now) / value_at_mint).max(0.0)
}

fn snap(tick: f64, h: i64) -> (f64, f64) {
    let base = (tick / TICK_SPACING).round() * TICK_SPACING;
    let half = TICK_SPACING * h as f64;
    (base - half, base + half)
}

/// One hour of the replay: the keeper's table as of that hour, and the scores and choice the
/// strategy being replayed acts on.
#[derive(Clone, Debug)]
pub struct Hour {
    pub table: Table,
    pub nets: Vec<(i64, f64)>,
    pub choice: i64,
}

/// A strategy's hours from the keeper's tables. `vols = None` is the keeper itself (its own
/// scores and its logged choice); `Some(v)` gives the model's volatility for each table.
pub fn hours_for(tables: &[Table], vols: Option<&[f64]>, p: &Policy) -> Vec<Hour> {
    tables
        .iter()
        .enumerate()
        .map(|(i, t)| match vols {
            None => Hour { nets: t.rows.iter().map(|r| (r.h as i64, r.net)).collect(), choice: t.choice, table: t.clone() },
            Some(v) => {
                let nets = t.rescore(v[i]);
                let choice = choose(t, &nets, p);
                Hour { nets, choice, table: t.clone() }
            }
        })
        .collect()
}

/// A position already open when the replay starts (for checking the rules against a real
/// position): its range, its mint tick, its width and the lineage's pacing state.
#[derive(Clone, Debug)]
pub struct Start {
    pub lower: f64,
    pub upper: f64,
    pub mint: f64,
    pub last_rebalance: Option<i64>,
    pub deposit_known: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Replay {
    pub net_per_day: f64,
    pub revenue: f64,
    pub costs: f64,
    pub divergence: f64,
    pub rebalances: u32,
    pub recuts: u32,
    pub held_out_minutes: u32,
    pub in_range_fraction: f64,
    /// Every action taken: (time, "rebalance" or "recut", new width in spacings each side).
    #[serde(default)]
    pub actions: Vec<(i64, String, i64)>,
}

/// Replays a $`table.position_usd` position under the keeper's rules on a one-minute path of
/// `(ts, tick, fee_pips)` from `start` for `secs`. `hours` must be sorted and start at or before
/// `start`. `fixed = Some(h)` replays a fixed width (no re-cuts, no crash valve) for the hindsight
/// ceiling. None when the path covers less than 90% of the window.
pub fn replay(p: &Policy, hours: &[Hour], path: &[(i64, f64, Option<f64>)], start: i64, secs: i64, fixed: Option<i64>) -> Option<Replay> {
    replay_from(p, hours, path, start, secs, fixed, None)
}

/// `replay`, optionally from a position that is already open (`init`).
pub fn replay_from(
    p: &Policy,
    hours: &[Hour],
    path: &[(i64, f64, Option<f64>)],
    start: i64,
    secs: i64,
    fixed: Option<i64>,
    init: Option<Start>,
) -> Option<Replay> {
    let end = start + secs;
    let window: Vec<&(i64, f64, Option<f64>)> = path.iter().filter(|x| x.0 >= start && x.0 < end).collect();
    if (window.len() as i64) * 60 < secs * 9 / 10 || hours.is_empty() {
        return None;
    }
    let hour_at = |t: i64| -> &Hour {
        let i = hours.partition_point(|h| h.table.ts <= t);
        &hours[i.saturating_sub(1)]
    };
    let first = window[0];
    let mut h = fixed.unwrap_or(hour_at(start).choice);
    let (mut lower, mut upper) = snap(first.1, h);
    let mut mint = first.1;
    let mut last_rebalance: Option<i64> = None;
    let mut deposit_known: Option<i64> = None; // the keeper learns the deposit time once it is outside
    if let Some(s) = &init {
        (lower, upper, mint) = (s.lower, s.upper, s.mint);
        h = (((upper - lower) / (2.0 * TICK_SPACING)).round()) as i64;
        last_rebalance = s.last_rebalance;
        deposit_known = s.deposit_known;
    }
    let mut value = 1.0f64; // fraction of the starting position
    let mut out = Replay::default();
    let mut outside_since: Option<i64> = None;
    let mut hold_since: Option<i64> = None;
    let mut recut_candidate: Option<(bool, i64)> = None;
    let mut recent: VecDeque<i64> = VecDeque::new();
    let mut fee = first.2.unwrap_or(500.0);
    let mut twap: VecDeque<(i64, f64)> = VecDeque::new();
    let (mut in_range_secs, mut total_secs) = (0i64, 0i64);
    let mut prev_t = start;
    let recut_allowed = fixed.is_none() && p.recut;

    // TWAP history from just before the window.
    for x in path.iter().filter(|x| x.0 >= start - p.twap_secs && x.0 < start) {
        twap.push_back((x.0, x.1));
    }

    for &&(t, tick, f) in &window {
        if let Some(f) = f {
            fee = f;
        }
        let dt = (t - prev_t).clamp(0, 300);
        prev_t = t;
        let hr = hour_at(t);
        let table = &hr.table;
        // Revenue: the depositor's share of emissions while the SPOT is inside the range, at the
        // table's rate for this width, with its emission capture (keeper `choose_width`).
        let row = table.row(h);
        total_secs += dt;
        if tick >= lower && tick < upper {
            in_range_secs += dt;
            out.revenue += row.gross_apr / (365.0 * 86_400.0) * p.user_reward_share * row.capture * dt as f64 * value;
        }
        // 5-minute TWAP of the minute ticks, needing half the window covered (keeper twap_tick).
        twap.push_back((t, tick));
        while twap.front().map(|x| x.0 < t - p.twap_secs).unwrap_or(false) {
            twap.pop_front();
        }
        let twap_tick = match twap.front() {
            Some(first) if twap.len() >= 2 && t - first.0 >= p.twap_secs / 2 => twap.iter().map(|x| x.1).sum::<f64>() / twap.len() as f64,
            _ => continue,
        };
        while recent.front().map(|r| t - *r >= 86_400).unwrap_or(false) {
            recent.pop_front();
        }

        // ---- keeper `decide`
        let outside = {
            let (lo, hi) = (lower, upper);
            if twap_tick < lo || twap_tick >= hi {
                true
            } else if p.danger_zone_fraction > 0.0 {
                let m = (hi - lo) * p.danger_zone_fraction;
                twap_tick - lo <= m || hi - twap_tick <= m
            } else {
                false
            }
        };
        #[derive(PartialEq)]
        enum D {
            InRange,
            Wait,
            Rebalance,
        }
        let decision = if !outside {
            outside_since = None;
            hold_since = None;
            D::InRange
        } else {
            deposit_known.get_or_insert(start);
            let since = *outside_since.get_or_insert(t);
            let position_usd = table.position_usd * value;
            if t - since < p.confirmation_secs
                || position_usd < p.min_position_usd
                || recent.len() >= p.max_rebalances_per_day
                || last_rebalance.map(|l| t - l < p.cooldown_secs).unwrap_or(false)
            {
                D::Wait
            } else {
                let held_from = *hold_since.get_or_insert(t);
                let deviation = (tick - twap_tick).abs();
                if t - held_from < p.hold_max_secs && (fee > p.fee_hold_pips || deviation > p.max_twap_deviation_ticks) {
                    D::Wait
                } else {
                    D::Rebalance
                }
            }
        };

        // ---- keeper.rs: re-cut judgement on InRange, crash valve on Rebalance
        let mut act: Option<i64> = None;
        match decision {
            D::InRange if recut_allowed => {
                let asked = assess_recut(p, &hr.nets, tick, lower, upper, fee, table.gas_usd, table.position_usd * value).filter(|r| r.2);
                match asked {
                    None => recut_candidate = None,
                    Some((to, narrowing, _)) => {
                        let since = match recut_candidate {
                            Some((n, s)) if n == narrowing => s,
                            _ => {
                                recut_candidate = Some((narrowing, t));
                                t
                            }
                        };
                        if t - since >= p.recut_confirm_secs
                            && recut_gate(
                                p,
                                t,
                                tick,
                                twap_tick,
                                fee,
                                last_rebalance,
                                deposit_known,
                                &recent,
                                narrowing,
                                table.position_usd * value,
                            )
                        {
                            act = Some(to);
                            out.recuts += 1;
                            out.actions.push((t, "recut".into(), to));
                        }
                    }
                }
            }
            D::Rebalance => {
                recut_candidate = None;
                let hold_out = fixed.is_none()
                    && best(&hr.nets).map(|(_, b)| {
                        let widest = table.rows.iter().max_by(|a, b| a.h.total_cmp(&b.h)).map(|r| r.rebalances).unwrap_or(0.0);
                        let outside_secs = outside_since.map(|s| t - s).unwrap_or(0);
                        let beyond = (lower - tick).max(tick - upper).max(0.0);
                        p.hold_out_when_unprofitable
                            && b <= 0.0
                            && widest >= p.hold_out_widest_exits_per_day
                            && outside_secs < p.hold_out_max_secs
                            && beyond < p.hold_out_max_ticks
                    }) == Some(true);
                if hold_out {
                    out.held_out_minutes += (dt / 60) as u32;
                } else {
                    let to = fixed.unwrap_or(hr.choice);
                    act = Some(to);
                    out.actions.push((t, "rebalance".into(), to));
                }
            }
            _ => recut_candidate = None,
        }

        if let Some(new_h) = act {
            // Close: realise divergence against the mint-time mix, pay half a swap at the pool's
            // fee plus gas, open the new range centred on the grid step nearest spot.
            let div = value * divergence_fraction(mint, tick, lower, upper);
            let cost = value * 0.5 * fee / 1e6 + if table.position_usd > 0.0 { table.gas_usd / table.position_usd } else { 0.0 };
            out.divergence += div;
            out.costs += cost;
            value -= div + cost;
            h = new_h;
            (lower, upper) = snap(tick, h);
            mint = tick;
            last_rebalance = Some(t);
            recent.push_back(t);
            outside_since = None;
            hold_since = None;
            recut_candidate = None;
            out.rebalances += 1;
        }
    }
    // Mark the open range to market.
    if let Some(last) = window.last() {
        out.divergence += value * divergence_fraction(mint, last.1, lower, upper);
    }
    out.in_range_fraction = if total_secs > 0 { in_range_secs as f64 / total_secs as f64 } else { 0.0 };
    out.net_per_day = (out.revenue - out.costs - out.divergence) / (secs as f64 / 86_400.0);
    Some(out)
}

/// Keeper `assess_recut`: Some((to, narrowing, worth_it)) when the table asks to re-cut.
#[allow(clippy::too_many_arguments)]
fn assess_recut(
    p: &Policy,
    nets: &[(i64, f64)],
    tick: f64,
    lower: f64,
    upper: f64,
    fee: f64,
    gas: f64,
    position_usd: f64,
) -> Option<(i64, bool, bool)> {
    if !p.recut || upper <= lower || nets.is_empty() {
        return None;
    }
    let from = (((upper - lower) / (2.0 * TICK_SPACING)).round().max(1.0)) as i64;
    let floor = p.recut_narrow_floor_spacings.max(1).min(from);
    let offered: Vec<(i64, f64)> = nets.iter().filter(|x| x.0 >= floor).copied().collect();
    let (to, best_net) = best(&offered)?;
    if to == from {
        return None;
    }
    let ratio = to.max(from) as f64 / to.min(from) as f64;
    if ratio < p.recut_min_ratio {
        return None;
    }
    let cur = nets.iter().min_by_key(|x| (x.0 - from).abs()).map(|x| x.1)?;
    let gain_per_day = best_net - cur;
    let realised = divergence_fraction((lower + upper) / 2.0, tick, lower, upper);
    let one_off = 0.5 * fee / 1e6 + if position_usd > 0.0 { gas / position_usd } else { 0.0 } + realised;
    let gain = gain_per_day * p.recut_horizon_secs as f64 / 86_400.0;
    let narrowing = to < from;
    let margin = if narrowing { p.recut_narrow_margin } else { p.recut_margin };
    Some((to, narrowing, gain_per_day > 0.0 && gain >= margin * one_off))
}

/// Keeper `recut_gate` (pacing and calm rules for a re-cut).
#[allow(clippy::too_many_arguments)]
fn recut_gate(
    p: &Policy,
    now: i64,
    tick: f64,
    twap: f64,
    fee: f64,
    last_rebalance: Option<i64>,
    deposit: Option<i64>,
    recent: &VecDeque<i64>,
    narrowing: bool,
    position_usd: f64,
) -> bool {
    if narrowing {
        let Some(anchor) = last_rebalance.or(deposit) else { return false };
        if now - anchor < p.recut_narrow_min_dwell_secs {
            return false;
        }
    } else if let (None, Some(d)) = (last_rebalance, deposit) {
        if now - d < p.recut_widen_min_dwell_secs {
            return false;
        }
    }
    position_usd >= p.min_position_usd
        && recent.len() < p.max_rebalances_per_day
        && last_rebalance.map(|l| now - l >= p.cooldown_secs).unwrap_or(true)
        && fee <= p.fee_hold_pips
        && (tick - twap).abs() <= p.max_twap_deviation_ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Table {
        let v: Value = serde_json::from_str(&std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap()).unwrap();
        Table::from_keeper(&v).expect("real width_evals row parses")
    }

    fn flat_path(t0: i64, tick: f64, minutes: i64, fee: f64) -> Vec<(i64, f64, Option<f64>)> {
        (-10..minutes).map(|i| (t0 + i * 60, tick, Some(fee))).collect()
    }

    /// A table where one width's score is replaced, to steer choices in tests.
    fn with_nets(t: &Table, f: impl Fn(i64) -> f64) -> Hour {
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, f(r.h as i64))).collect();
        let choice = choose(t, &nets, &Policy::default());
        Hour { table: t.clone(), nets, choice }
    }

    #[test]
    fn real_table_cost_matches_keeper_formula() {
        let t = fixture();
        let r = &t.rows[0];
        // Exact keeper formula vs what the row itself implies: must agree.
        let exact = t.cost_per_rebalance(r);
        let implied = (r.gross_apr / 365.0 * 0.8 * r.in_range * r.capture - r.net_ex_div) / r.rebalances;
        assert!((exact - implied).abs() < 1e-9, "{exact} vs {implied}");
    }

    #[test]
    fn same_vol_reproduces_keeper_scores_and_choice() {
        let t = fixture();
        let nets = t.rescore(t.vol);
        for (r, (_, s)) in t.rows.iter().zip(&nets) {
            assert_eq!(r.net, *s);
        }
        assert_eq!(choose(&t, &nets, &Policy::default()), t.choice, "keeper's hysteresis reproduces its logged choice");
    }

    #[test]
    fn rescore_is_continuous_near_keeper_vol() {
        let t = fixture();
        let a = t.rescore(t.vol * 1.000001);
        for (r, (_, s)) in t.rows.iter().zip(&a) {
            assert!((r.net - s).abs() < 1e-6, "h={} {} vs {}", r.h, r.net, s);
        }
    }

    #[test]
    fn hysteresis_rules() {
        let t = fixture();
        let p = Policy::default();
        // Home width 4 wins unless another is clearly better.
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, if r.h as i64 == 4 { 0.010 } else { 0.0105 })).collect();
        assert_eq!(choose(&t, &nets, &p), 4, "5% better is not enough for either side");
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, if r.h as i64 == 2 { 0.0112 } else { 0.010 })).collect();
        assert_eq!(choose(&t, &nets, &p), 2, "narrower 12% better wins");
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, if r.h as i64 == 16 { 0.012 } else { 0.010 })).collect();
        assert_eq!(choose(&t, &nets, &p), 4, "wider only 20% better does not");
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, if r.h as i64 == 16 { 0.013 } else { 0.010 })).collect();
        assert_eq!(choose(&t, &nets, &p), 16, "wider 30% better wins");
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, if r.h as i64 == 12 { 0.0001 } else { -0.001 })).collect();
        assert_eq!(choose(&t, &nets, &p), 12, "home loses money: any positive width wins");
    }

    #[test]
    fn divergence_matches_closed_form() {
        // Keeper test: zero at the mint price; about a quarter of the half-width at the edge.
        assert!(divergence_fraction(0.0, 0.0, -200.0, 200.0).abs() < 1e-12);
        let edge = divergence_fraction(0.0, 200.0, -200.0, 200.0);
        let s = (200.0 * LN_TICK / 2.0f64).exp();
        let closed = s * (s - 1.0) / 2.0; // s(s-1)/2 per the keeper's comment
        assert!((edge - closed).abs() / closed < 0.05, "{edge} vs {closed}");
    }

    #[test]
    fn flat_price_earns_revenue_only() {
        let t = fixture();
        let p = Policy::default();
        let hours = hours_for(std::slice::from_ref(&t), None, &p);
        let path = flat_path(t.ts, t.tick, 1440, 400.0);
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.rebalances, 0);
        let row = t.row(t.choice);
        let expect = row.gross_apr / 365.0 * 0.8 * row.capture;
        assert!((r.net_per_day - expect).abs() / expect < 0.01, "{} vs {expect}", r.net_per_day);
    }

    #[test]
    fn confirmation_then_one_rebalance() {
        let t = fixture();
        let p = Policy::default();
        let hours = vec![with_nets(&t, |h| if h == 2 { 0.01 } else { 0.0 })];
        assert_eq!(hours[0].choice, 2);
        let jump = t.tick + 400.0; // well outside ±1%
        let path: Vec<_> = (-10..1440).map(|i| (t.ts + i * 60, if i < 30 { t.tick } else { jump }, Some(400.0))).collect();
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.rebalances, 1, "one confirmed exit, then in range at the new level");
        assert!(r.divergence > 0.0 && r.costs > 0.0);
    }

    #[test]
    fn wick_that_re_enters_restarts_the_clock() {
        let t = fixture();
        let p = Policy::default();
        let hours = vec![with_nets(&t, |h| if h == 2 { 0.01 } else { 0.0 })];
        // Out for 5 h, back in for 1 h, out for 5 h: never 6 h continuously outside.
        let out = t.tick + 400.0;
        let path: Vec<_> = (-10..1440)
            .map(|i| {
                let m = i.max(0);
                let tick = if (30..330).contains(&m) || (390..690).contains(&m) { out } else { t.tick };
                (t.ts + i * 60, tick, Some(400.0))
            })
            .collect();
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.rebalances, 0);
    }

    #[test]
    fn fee_hold_then_max_hold() {
        let t = fixture();
        let p = Policy::default();
        let hours = vec![with_nets(&t, |h| if h == 2 { 0.01 } else { 0.0 })];
        let out = t.tick + 400.0;
        // High fee the whole time: rebalance only after confirmation (6 h) + max hold (2 h).
        let path: Vec<_> = (-10..1440).map(|i| (t.ts + i * 60, if i < 30 { t.tick } else { out }, Some(3000.0))).collect();
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.rebalances, 1);
        // Calm fee: rebalances at ~6 h; stormy fee: at ~8 h, so it earns 2 h less in the new range.
        let calm: Vec<_> = path.iter().map(|x| (x.0, x.1, Some(400.0))).collect();
        let rc = replay(&p, &hours, &calm, t.ts, 86_400, None).unwrap();
        assert!(rc.revenue > r.revenue);
    }

    #[test]
    fn crash_valve_sits_out_inside_its_caps_only() {
        let t = fixture();
        let p = Policy::default();
        // A storm table: every width loses money and even the widest exits more than 0.65 a day.
        let mut storm = t.clone();
        for r in &mut storm.rows {
            r.rebalances = r.rebalances.max(1.0);
        }
        let hours = vec![with_nets(&storm, |_| -0.001)];
        let h = hours[0].choice;
        assert_eq!(h, 4, "all widths equal and negative: the home width");
        let edge = h as f64 * 50.0;
        let run = |jump: f64| {
            let path: Vec<_> = (-10..1440).map(|i| (t.ts + i * 60, if i < 30 { t.tick } else { t.tick + jump }, Some(400.0))).collect();
            replay(&p, &hours, &path, t.ts, 86_400, None).unwrap()
        };
        // 200 ticks past the edge (< 400): the valve holds for the rest of the day.
        let held = run(edge + 200.0);
        assert_eq!(held.rebalances, 0);
        assert!(held.held_out_minutes > 0);
        // 600 ticks past the edge (>= 400): the valve lets it re-centre once confirmed.
        let moved = run(edge + 600.0);
        assert_eq!(moved.rebalances, 1);
        assert_eq!(moved.held_out_minutes, 0);
    }

    #[test]
    fn max_three_rebalances_a_day() {
        let t = fixture();
        let p = Policy { confirmation_secs: 600, cooldown_secs: 600, ..Policy::default() };
        let hours = vec![with_nets(&t, |h| if h == 2 { 0.01 } else { 0.0 })];
        // A staircase up 300 ticks every hour.
        let path: Vec<_> = (-10..1440).map(|i| (t.ts + i * 60, t.tick + 300.0 * (i.max(0) / 60) as f64, Some(400.0))).collect();
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.rebalances, 3);
    }

    #[test]
    fn widening_recut_after_three_hours_of_agreement() {
        let t = fixture();
        let p = Policy::default();
        // Opens at h=4 (home), table asks strongly for h=16; price stays put (in range).
        let asked = with_nets(&t, |h| match h {
            4 => 0.0,
            16 => 0.002,
            _ => -0.01,
        });
        let hours = vec![Hour { choice: 4, ..asked }];
        let path = flat_path(t.ts, t.tick, 1440, 400.0);
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.recuts, 1, "a fresh deposit with no known deposit time may widen after the 3 h agreement");
        assert_eq!(r.rebalances, 1);
    }

    #[test]
    fn narrowing_needs_an_anchor_and_the_floor() {
        let t = fixture();
        let p = Policy::default();
        let asked = with_nets(&t, |h| match h {
            16 => 0.0,
            2 => 0.01,
            6 => 0.009,
            _ => -0.01,
        });
        let hours = vec![Hour { choice: 16, ..asked }];
        let path = flat_path(t.ts, t.tick, 1440, 400.0);
        let r = replay(&p, &hours, &path, t.ts, 86_400, None).unwrap();
        assert_eq!(r.recuts, 0, "narrowing waits for a known rebalance or deposit time");
    }

    #[test]
    fn size_adjustment_is_exact_at_table_size() {
        let t = fixture();
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, r.net)).collect();
        assert_eq!(t.nets_for_size(&nets, t.position_usd), nets);
        // A smaller position pays more gas per dollar: every row with rebalances scores lower.
        let small = t.nets_for_size(&nets, 20.0);
        for ((_, a), ((_, b), r)) in nets.iter().zip(small.iter().zip(&t.rows)) {
            if r.rebalances > 0.0 {
                assert!(b < a);
            }
        }
    }

    #[test]
    fn keeper_run_policy_parses() {
        let v: Value =
            serde_json::json!({"policy": {"confirmation_secs": 10800, "recut": false, "max_rebalances_per_day": 5, "narrow_margin": 0.2}});
        let p = Policy::from_keeper_run(&v);
        assert_eq!(p.confirmation_secs, 10_800);
        assert!(!p.recut);
        assert_eq!(p.max_rebalances_per_day, 5);
        assert_eq!(p.narrow_margin, 0.2);
        assert_eq!(p.cooldown_secs, 7_200, "absent keys keep defaults");
    }
}
