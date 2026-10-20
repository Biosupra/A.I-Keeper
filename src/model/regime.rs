//! The keeper's `WIDTH_MODE=regime` rules (PF-Upgradeable-Vault-Keeper src/regime.rs, 2026-10-13),
//! replayed on the real pool path in DOLLARS so the A.I can score the new rules and what the old
//! table rules would have done on the same windows (the owner's measure: balance + rewards).
//!
//! The port: three widths from the pool's own volatility (24 h and 72 h, minute ticks, fraction per
//! √hour). Verified calm (both windows under `calm_vol`, 72 h of path) = narrow; both under
//! `busy_vol` = mid; otherwise wide. In range: widen once the market clears the line by
//! `widen_margin` for `widen_after_secs`, narrow after `narrow_after_secs`; never while the spot is
//! already outside. Out of range: the keeper's ordinary confirmation, cooldown, daily cap and
//! fee/deviation hold, then hold while the price makes new lows (highs) until `stable_secs` without
//! one or `max_wait_secs` in all, and re-open at the plain tier. The settings come from the keeper's
//! own `keeper_runs` row when it runs this mode, and the keeper's defaults otherwise, so the new
//! rules are scored before the switch as well.

use super::shadow::{self, Hour, Policy, Replay, TICK_SPACING};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub calm_vol: f64,
    pub busy_vol: f64,
    pub narrow: i64,
    pub mid: i64,
    pub wide: i64,
    pub long_window_secs: i64,
    pub stable_secs: i64,
    pub max_wait_secs: i64,
    pub widen_after_secs: i64,
    pub narrow_after_secs: i64,
    pub widen_margin: f64,
    pub confirmation_secs: i64,
    pub cooldown_secs: i64,
    /// A fall out of range is held this long for the price to come back before the bottom rule may
    /// re-open it (keeper REGIME_RETURN_WAIT_SECS; 0 = off). Every fall; rises are never held this way.
    pub return_wait_secs: i64,
}

impl Default for Params {
    /// The keeper's compiled defaults (wip-regime-width 5eb5d47).
    fn default() -> Self {
        Params {
            calm_vol: 0.0043,
            busy_vol: 0.0055,
            narrow: 4,
            mid: 8,
            wide: 20,
            long_window_secs: 259_200,
            stable_secs: 21_600,
            max_wait_secs: 86_400,
            widen_after_secs: 3_600,
            narrow_after_secs: 172_800,
            widen_margin: 0.0004,
            confirmation_secs: 3_600,
            cooldown_secs: 3_600,
            return_wait_secs: 604_800,
        }
    }
}

impl Params {
    /// From a keeper `keeper_runs` row: its `policy.regime` block when the keeper runs the mode,
    /// the defaults otherwise.
    pub fn from_keeper_run(v: Option<&Value>) -> Params {
        let mut out = Params::default();
        let Some(v) = v else { return out };
        let p = &v["policy"];
        if p["width_mode"].as_str() != Some("regime") {
            return out;
        }
        let r = &p["regime"];
        let f = |k: &str, d: f64| r[k].as_f64().unwrap_or(d);
        let i = |k: &str, d: i64| r[k].as_i64().unwrap_or(d);
        out.calm_vol = f("calm_vol", out.calm_vol);
        out.busy_vol = f("busy_vol", out.busy_vol);
        out.narrow = i("narrow_half_spacings", out.narrow);
        out.mid = i("mid_half_spacings", out.mid);
        out.wide = i("wide_half_spacings", out.wide);
        out.long_window_secs = i("long_window_secs", out.long_window_secs);
        out.stable_secs = i("stable_secs", out.stable_secs);
        out.max_wait_secs = i("max_wait_secs", out.max_wait_secs);
        out.widen_after_secs = i("widen_after_secs", out.widen_after_secs);
        out.narrow_after_secs = i("narrow_after_secs", out.narrow_after_secs);
        out.widen_margin = f("widen_margin", out.widen_margin);
        out.return_wait_secs = i("return_wait_secs", out.return_wait_secs);
        out.confirmation_secs = p["confirmation_secs"].as_i64().unwrap_or(out.confirmation_secs);
        out.cooldown_secs = p["cooldown_secs"].as_i64().unwrap_or(out.cooldown_secs);
        out
    }

    fn tier(&self, short: Option<f64>, long: Option<f64>, long_known: bool, shift: f64) -> i64 {
        let Some(s) = short.map(|v| v - shift) else { return self.wide };
        let l = long.map(|v| v - shift);
        if long_known && s < self.calm_vol && l.is_some_and(|l| l < self.calm_vol) {
            return self.narrow;
        }
        if s < self.busy_vol && l.is_none_or(|l| l < self.busy_vol) {
            return self.mid;
        }
        self.wide
    }
}

/// Realised volatility (fraction per √hour) over trailing windows of a minute path, the way the
/// keeper's `SampleWindow::realised_hourly_vol` measures it: squared tick changes between samples at
/// most five minutes apart, at least 30 minutes of them.
pub struct VolPath {
    ts: Vec<i64>,
    cum_sq: Vec<f64>,
    cum_secs: Vec<i64>,
}

impl VolPath {
    pub fn new(path: &[(i64, f64, Option<f64>)]) -> VolPath {
        let mut ts = Vec::with_capacity(path.len());
        let (mut cum_sq, mut cum_secs) = (vec![0.0], vec![0i64]);
        for (i, x) in path.iter().enumerate() {
            ts.push(x.0);
            if i > 0 {
                let gap = x.0 - path[i - 1].0;
                let (sq, secs) = if gap > 0 && gap <= 300 { ((x.1 - path[i - 1].1).powi(2), gap) } else { (0.0, 0) };
                cum_sq.push(cum_sq[i - 1] + sq);
                cum_secs.push(cum_secs[i - 1] + secs);
            }
        }
        VolPath { ts, cum_sq, cum_secs }
    }

    /// Volatility over (t - window, t].
    pub fn vol(&self, t: i64, window: i64) -> Option<f64> {
        let hi = self.ts.partition_point(|&x| x <= t);
        let lo = self.ts.partition_point(|&x| x <= t - window);
        if hi < 2 || lo + 1 >= hi {
            return None;
        }
        let first = lo.max(1);
        let sq = self.cum_sq[hi - 1] - self.cum_sq[first - 1];
        let secs = self.cum_secs[hi - 1] - self.cum_secs[first - 1];
        if secs < 1800 {
            return None;
        }
        Some((sq / secs as f64 * 3600.0).sqrt() * shadow::LN_TICK)
    }

    pub fn coverage(&self, t: i64) -> i64 {
        self.ts.first().map(|&f| t - f).unwrap_or(0)
    }
}

/// Reward rate per second per dollar in range for width `h`, from the keeper table in force: its
/// row when the table has the width, otherwise scaled from the widest row by concentration (a
/// range's liquidity per dollar goes as 1 / (1 - 1.0001^(-spacing·h/2))).
fn rate(hour: &Hour, h: i64, p: &Policy) -> f64 {
    let t = &hour.table;
    let widest = t.rows.last().expect("table has rows");
    let (apr, capture) = if (h as f64) <= widest.h {
        let r = t.row(h);
        (r.gross_apr, r.capture)
    } else {
        let conc = |x: f64| 1.0 / (1.0 - 1.0001f64.powf(-TICK_SPACING * x / 2.0));
        (widest.gross_apr * conc(h as f64) / conc(widest.h), 1.0)
    };
    apr / (365.0 * 86_400.0) * p.user_reward_share * capture
}

fn snap(tick: f64, h: i64) -> (f64, f64) {
    let base = (tick / TICK_SPACING).round() * TICK_SPACING;
    (base - TICK_SPACING * h as f64, base + TICK_SPACING * h as f64)
}

/// A $1 position opened at `start` at the regime width, replayed for `secs` under the regime rules.
/// `hours` = the keeper tables in force (for reward rates, gas and position size); `vols` must cover
/// from `long_window_secs` before `start`. Dollar results in `usd_end` / `usd_rewards`.
pub fn replay(
    rp: &Params,
    p: &Policy,
    hours: &[Hour],
    path: &[(i64, f64, Option<f64>)],
    vols: &VolPath,
    start: i64,
    secs: i64,
) -> Option<Replay> {
    let end = start + secs;
    let window: Vec<&(i64, f64, Option<f64>)> = path.iter().filter(|x| x.0 >= start && x.0 < end).collect();
    if (window.len() as i64) * 60 < secs * 9 / 10 || hours.is_empty() {
        return None;
    }
    let hour_at = |t: i64| -> &Hour { &hours[hours.partition_point(|h| h.table.ts <= t).saturating_sub(1)] };
    let reading = |t: i64, shift: f64| {
        let known = vols.coverage(t) >= rp.long_window_secs;
        rp.tier(vols.vol(t, 86_400), vols.vol(t, rp.long_window_secs), known, shift)
    };
    let first = window[0];
    let mut h = reading(start, 0.0);
    let (mut lower, mut upper) = snap(first.1, h);
    let mut liq = 1.0 / shadow::value_per_l(first.1, lower, upper).max(1e-300);
    let mut out = Replay::default();
    let mut outside_since: Option<i64> = None;
    let mut hold_since: Option<i64> = None;
    let mut extreme: Option<(bool, f64, i64)> = None;
    let mut want: Option<(i64, i64)> = None;
    let mut last_move: Option<i64> = None;
    let mut recent: VecDeque<i64> = VecDeque::new();
    let mut twap: VecDeque<(i64, f64)> = path.iter().filter(|x| x.0 >= start - p.twap_secs && x.0 < start).map(|x| (x.0, x.1)).collect();
    let mut fee = first.2.unwrap_or(500.0);
    let (mut in_range_secs, mut total_secs, mut prev_t) = (0i64, 0i64, start);
    for &&(t, tick, f) in &window {
        if let Some(f) = f {
            fee = f;
        }
        let dt = (t - prev_t).clamp(0, 300);
        prev_t = t;
        total_secs += dt;
        let hr = hour_at(t);
        let usd = liq * shadow::value_per_l(tick, lower, upper);
        if tick >= lower && tick < upper {
            in_range_secs += dt;
            out.usd_rewards += rate(hr, h, p) * dt as f64 * usd;
        }
        twap.push_back((t, tick));
        while twap.front().is_some_and(|x| x.0 < t - p.twap_secs) {
            twap.pop_front();
        }
        let tw = match twap.front() {
            Some(f0) if twap.len() >= 2 && t - f0.0 >= p.twap_secs / 2 => twap.iter().map(|x| x.1).sum::<f64>() / twap.len() as f64,
            _ => continue,
        };
        while recent.front().is_some_and(|r| t - *r >= 86_400) {
            recent.pop_front();
        }
        let paced = recent.len() < p.max_rebalances_per_day && last_move.is_none_or(|l| t - l >= rp.cooldown_secs);
        let mut act: Option<(i64, &str)> = None;
        if tw >= lower && tw < upper {
            (outside_since, hold_since, extreme) = (None, None, None);
            let spot_in = tick >= lower && tick < upper;
            let down = reading(t, 0.0);
            let up = reading(t, rp.widen_margin);
            let target = if down < h {
                down
            } else if up > h {
                up
            } else {
                h
            };
            if target == h || !spot_in {
                want = None;
            } else {
                let since = match want {
                    Some((w, s)) if w == target => s,
                    _ => {
                        want = Some((target, t));
                        t
                    }
                };
                let need = if target > h { rp.widen_after_secs } else { rp.narrow_after_secs };
                if t - since >= need && paced {
                    act = Some((target, "recut"));
                }
            }
        } else {
            want = None;
            let below = tw < lower;
            match extreme {
                Some((b, x, _)) if b == below => {
                    if (below && tick < x) || (!below && tick > x) {
                        extreme = Some((below, tick, t));
                    }
                }
                _ => extreme = Some((below, tick, t)),
            }
            let out_since = *outside_since.get_or_insert(t);
            if t - out_since >= rp.confirmation_secs && paced {
                let held = t - *hold_since.get_or_insert(t);
                let fee_or_deviation = fee > p.fee_hold_pips || (tick - tw).abs() > p.max_twap_deviation_ticks;
                // A fall is first held for the price to come back (the owner's 7-day rule); the bottom
                // rule and its cap count from the end of that wait. Rises are not held this way.
                let return_wait = match extreme {
                    Some((true, _, _)) if rp.return_wait_secs > 0 => rp.return_wait_secs,
                    _ => 0,
                };
                let still_moving = t - out_since < return_wait
                    || (t - out_since < return_wait + rp.max_wait_secs && extreme.is_some_and(|e| t - e.2 < rp.stable_secs));
                let blocked = still_moving || (held < p.hold_max_secs && fee_or_deviation);
                if !blocked {
                    act = Some((reading(t, 0.0), "rebalance"));
                } else if still_moving {
                    out.held_out_minutes += (dt / 60) as u32;
                }
            }
        }
        if let Some((to, kind)) = act {
            let cost = usd * 0.5 * fee / 1e6 + if hr.table.position_usd > 0.0 { hr.table.gas_usd / hr.table.position_usd } else { 0.0 };
            h = to;
            (lower, upper) = snap(tick, h);
            liq = (usd - cost).max(0.0) / shadow::value_per_l(tick, lower, upper).max(1e-300);
            out.costs += cost;
            if kind == "recut" {
                out.recuts += 1;
            } else {
                out.rebalances += 1;
            }
            out.actions.push((t, kind.to_string(), to));
            last_move = Some(t);
            recent.push_back(t);
            (outside_since, hold_since, extreme, want) = (None, None, None, None);
        }
    }
    let last = window.last()?;
    out.usd_end = liq * shadow::value_per_l(last.1, lower, upper);
    out.in_range_fraction = if total_secs > 0 { in_range_secs as f64 / total_secs as f64 } else { 0.0 };
    out.revenue = out.usd_rewards;
    out.net_per_day = (out.usd_end + out.usd_rewards - 1.0) / (secs as f64 / 86_400.0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::shadow::{Row, Table};

    fn table(ts: i64) -> Table {
        let row = |h: f64, apr: f64| Row {
            h,
            gross_apr: apr,
            in_range: 1.0,
            rebalances: 0.0,
            divergence: 0.0,
            capture: 1.0,
            exec_fee_pips: Some(400.0),
            net_ex_div: 0.0,
            net: 0.0,
        };
        Table {
            ts,
            tick: -197_000.0,
            vol: 0.004,
            position_usd: 1000.0,
            gas_usd: 0.02,
            choice: 16,
            rows: vec![row(4.0, 1.6), row(8.0, 0.8), row(16.0, 0.4)],
        }
    }
    fn hours(ts: i64) -> Vec<Hour> {
        let t = table(ts);
        vec![Hour { nets: vec![], choice: 16, table: t }]
    }
    /// A quiet minute path: tick wiggles by ±1 around `base`, from `from` for `mins` minutes.
    fn quiet(from: i64, mins: i64, base: f64) -> Vec<(i64, f64, Option<f64>)> {
        (0..mins).map(|i| (from + i * 60, base + (i % 2) as f64, Some(400.0))).collect()
    }

    #[test]
    fn verified_calm_opens_narrow_and_a_flat_day_keeps_the_dollar() {
        let rp = Params::default();
        let p = Policy::default();
        let t0 = 10_000_000;
        let path = quiet(t0 - 80 * 3600, 104 * 60, -197_000.0);
        let r = replay(&rp, &p, &hours(t0 - 3600), &path, &VolPath::new(&path), t0, 86_400).unwrap();
        assert_eq!(r.actions.len(), 0);
        assert!((r.usd_end - 1.0).abs() < 1e-3, "flat path: the balance stays at $1 ({})", r.usd_end);
        // Narrow tier (4 spacings): its row's 160% gross x 0.8 share for a day.
        let want = 1.6 / 365.0 * 0.8;
        assert!((r.usd_rewards - want).abs() < want * 0.02, "rewards {} vs {}", r.usd_rewards, want);
    }

    #[test]
    fn a_fall_waits_for_the_bottom_then_reopens() {
        let rp = Params { return_wait_secs: 0, ..Params::default() };
        let p = Policy::default();
        let t0 = 20_000_000;
        let mut path = quiet(t0 - 80 * 3600, 80 * 60, -197_000.0);
        // Falls 600 ticks over 4 hours (out of a ±2% range), then sits flat.
        for i in 0..(20 * 60) {
            let x = if i < 240 { -197_000.0 - 2.5 * i as f64 } else { -197_600.0 + (i % 2) as f64 };
            path.push((t0 + i * 60, x, Some(400.0)));
        }
        let r = replay(&rp, &p, &hours(t0 - 3600), &path, &VolPath::new(&path), t0, 20 * 3600).unwrap();
        let first = r.actions.iter().find(|a| a.1 == "rebalance").expect("re-opened");
        assert!(first.0 >= t0 + 240 * 60 + 21_600 - 60, "re-opened only 6 h after the last new low ({})", first.0 - t0);
        assert!(r.held_out_minutes > 0);
    }

    #[test]
    fn a_fall_is_held_for_the_price_to_come_back_and_a_rise_is_not() {
        let rp = Params::default(); // 7-day return wait
        let p = Policy::default();
        let t0 = 20_000_000;
        // The same fall as above: no re-open inside the first day (the week-long wait), and it is held.
        let mut path = quiet(t0 - 80 * 3600, 80 * 60, -197_000.0);
        for i in 0..(24 * 60) {
            let x = if i < 240 { -197_000.0 - 2.5 * i as f64 } else { -197_600.0 + (i % 2) as f64 };
            path.push((t0 + i * 60, x, Some(400.0)));
        }
        let r = replay(&rp, &p, &hours(t0 - 3600), &path, &VolPath::new(&path), t0, 24 * 3600).unwrap();
        assert!(!r.actions.iter().any(|a| a.1 == "rebalance"), "held for the return: {:?}", r.actions);
        assert!(r.held_out_minutes > 0);
        // The price comes back into the range on day two: still no rebalance, and it is earning again.
        let mut back = quiet(t0 - 80 * 3600, 80 * 60, -197_000.0);
        for i in 0..(48 * 60) {
            let x = if i < 240 {
                -197_000.0 - 2.5 * i as f64
            } else if i < 24 * 60 {
                -197_600.0 + (i % 2) as f64
            } else if i < 24 * 60 + 240 {
                -197_600.0 + 2.5 * (i - 24 * 60) as f64
            } else {
                -197_000.0 + (i % 2) as f64
            };
            back.push((t0 + i * 60, x, Some(400.0)));
        }
        let r2 = replay(&rp, &p, &hours(t0 - 3600), &back, &VolPath::new(&back), t0, 48 * 3600).unwrap();
        assert!(!r2.actions.iter().any(|a| a.1 == "rebalance"), "came back: nothing to re-open: {:?}", r2.actions);
        assert!(
            r2.in_range_fraction > 0.45 && r2.in_range_fraction < 0.6,
            "earning before the fall and after the return ({})",
            r2.in_range_fraction
        );
        // A RISE of the same size re-opens after six quiet hours, as before.
        let mut rise = quiet(t0 - 80 * 3600, 80 * 60, -197_000.0);
        for i in 0..(20 * 60) {
            let x = if i < 240 { -197_000.0 + 2.5 * i as f64 } else { -196_400.0 - (i % 2) as f64 };
            rise.push((t0 + i * 60, x, Some(400.0)));
        }
        let r3 = replay(&rp, &p, &hours(t0 - 3600), &rise, &VolPath::new(&rise), t0, 20 * 3600).unwrap();
        let first = r3.actions.iter().find(|a| a.1 == "rebalance").expect("a rise re-opens");
        assert!(
            first.0 >= t0 + 240 * 60 + 21_600 - 60 && first.0 < t0 + 240 * 60 + 21_600 + 1800,
            "six quiet hours after the top ({})",
            first.0 - t0
        );
    }

    #[test]
    fn regime_settings_come_from_the_keeper_only_in_regime_mode() {
        let run = serde_json::json!({"policy": {"width_mode": "regime", "confirmation_secs": 3600, "cooldown_secs": 3600,
            "regime": {"busy_vol": 0.006, "wide_half_spacings": 18, "return_wait_secs": 259200}}});
        let p = Params::from_keeper_run(Some(&run));
        assert_eq!((p.busy_vol, p.wide, p.calm_vol), (0.006, 18, 0.0043));
        assert_eq!(p.return_wait_secs, 259_200, "the return wait is read from the keeper's run row");
        let no_wait = serde_json::json!({"policy": {"width_mode": "regime", "regime": {}}});
        assert_eq!(
            Params::from_keeper_run(Some(&no_wait)).return_wait_secs,
            604_800,
            "a keeper that does not log it yet gets the 7-day default"
        );
        let table_mode = serde_json::json!({"policy": {"width_mode": "table", "regime": {"busy_vol": 0.9}}});
        assert_eq!(Params::from_keeper_run(Some(&table_mode)), Params::default());
        // And the OLD rules keep their own timers when the keeper logs the regime's.
        let old = Policy::from_keeper_run(&run);
        assert_eq!((old.confirmation_secs, old.cooldown_secs), (21_600, 7_200));
    }

    #[test]
    fn the_table_replay_reports_dollars_too() {
        let p = Policy::default();
        let t0 = 30_000_000;
        let path = quiet(t0 - 3600, 25 * 60, -197_000.0);
        let hs = shadow::hours_for(&[table(t0 - 600)], None, &p);
        let r = shadow::replay(&p, &hs, &path, t0, 86_400, Some(16)).unwrap();
        assert!((r.usd_end - 1.0).abs() < 1e-3);
        assert!((r.usd_rewards - 0.4 / 365.0 * 0.8).abs() < 1e-4, "rewards {}", r.usd_rewards);
    }
}
