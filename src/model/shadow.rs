//! Shadow mode: for every width table the live keeper produces, the A.I makes its own width choice
//! and both are graded later on what the price actually did. No order is ever sent; this only
//! writes rows to the A.I's own database.
//!
//! How the A.I chooses. The keeper's table (`width_evals`) scores every width with its own
//! simulator at the keeper's volatility estimate. The A.I keeps that simulator's physics and swaps
//! in its own volatility forecast. A random walk that must travel `a` at volatility `s` behaves
//! like one that must travel `a * s0 / s` at volatility `s0`, so under the A.I's volatility a width
//! `h` behaves like the keeper's row at `h_eff = h * s_keeper / s_ai`: that row's time in range,
//! rebalances and (rescaled by `s_ai / s_keeper`) divergence apply, while the revenue per day in
//! range stays that of the real width `h`. The A.I picks the width with the best resulting score.
//!
//! How both are graded (`replay`): a $1,000 position at each width is replayed on the pool's real
//! one-minute ticks for the next 24 hours with keeper-like rules (rebalance after 6 h outside the
//! range, 2 h cooldown), earning the table's reward rate (times its emission capture) while in range and paying the table's
//! per-rebalance cost; divergence against holding each range's starting mix is measured with exact
//! concentrated-liquidity maths. Market direction is removed, as in the keeper's own score.

use crate::chainmath::{liquidity_for_value, position_value, tick_to_price};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const USER_SHARE: f64 = 0.8;
pub const CONFIRM_SECS: i64 = 6 * 3600;
pub const COOLDOWN_SECS: i64 = 2 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub h: f64,
    pub gross_apr: f64,
    pub in_range: f64,
    pub rebalances: f64,
    pub divergence: f64,
    pub capture: f64,
    pub net_ex_div: f64,
    pub net: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub ts: i64,
    pub tick: f64,
    pub vol: f64,
    pub position_usd: f64,
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
            choice: v["choice"].as_i64()?,
            rows,
        })
    }

    /// Revenue per day (fraction of position) the keeper's row implies.
    fn revenue(r: &Row) -> f64 {
        r.gross_apr / 365.0 * USER_SHARE * r.in_range * r.capture
    }

    /// Cost of one rebalance (fraction of position) implied by the row: revenue minus the score
    /// before divergence, spread over the expected rebalances.
    pub fn cost_per_rebalance(r: &Row) -> f64 {
        if r.rebalances > 1e-9 {
            ((Self::revenue(r) - r.net_ex_div) / r.rebalances).max(0.0)
        } else {
            0.0
        }
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
            net_ex_div: l(a.net_ex_div, b.net_ex_div),
            net: l(a.net, b.net),
        }
    }

    /// The A.I's score for every width at its own volatility, and its choice.
    pub fn rescore(&self, ai_vol: f64) -> (i64, Vec<(i64, f64)>) {
        let k = if ai_vol > 0.0 { self.vol / ai_vol } else { 1.0 };
        let scores: Vec<(i64, f64)> = self
            .rows
            .iter()
            .map(|r| {
                let eff = self.at(r.h * k);
                let revenue = r.gross_apr / 365.0 * USER_SHARE * eff.in_range * eff.capture;
                let cost = Self::cost_per_rebalance(r) * eff.rebalances;
                let divergence = eff.divergence / k.max(1e-9);
                (r.h as i64, revenue - cost - divergence)
            })
            .collect();
        let best = scores.iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|s| s.0).unwrap_or(self.choice);
        (best, scores)
    }
}

/// Replays a position of width `h` on a tick path (one-minute (ts, tick) from `start`) for
/// `secs`, returning its net result per day as a fraction of the starting value, or None if the
/// path covers less than 90% of the window.
pub fn replay(table: &Table, h: i64, path: &[(i64, f64)], start: i64, secs: i64) -> Option<f64> {
    let row = table.rows.iter().find(|r| r.h as i64 == h)?;
    let covered = path.iter().filter(|(t, _)| *t >= start && *t < start + secs).count() as i64;
    if covered * 60 < secs * 9 / 10 {
        return None;
    }
    // The reward rate is flat across the replay (the table's rate at decision time), so the
    // keeper's emission_capture is applied: emissions are richest when the price is moving, which is
    // exactly when a narrow range is most often out, and a flat rate would otherwise overpay narrow
    // ranges (found 2026-09-29: without it the replay credited +/-1% with ~15% too much reward).
    let rate_per_sec = row.gross_apr / (365.0 * 86_400.0) * USER_SHARE * row.capture;
    let cost = Table::cost_per_rebalance(row);
    let half = h as f64 * 50.0;

    let mut value = 1.0; // position value, fraction of the start
    let mut center = table.tick;
    let (mut lo_p, mut hi_p) = (tick_to_price(center - half), tick_to_price(center + half));
    let mut p0 = tick_to_price(center);
    let mut liq = liquidity_for_value(value, lo_p, hi_p, p0);
    // Holdings at the range's start, for divergence (value vs just holding them).
    let (mut hold_x, mut hold_y) = holdings(liq, lo_p, hi_p, p0);

    let (mut revenue, mut costs, mut divergence) = (0.0, 0.0, 0.0);
    let (mut outside_since, mut last_rebalance): (Option<i64>, i64) = (None, i64::MIN / 2);
    let mut prev_t = start;
    for &(t, tick) in path.iter().filter(|(t, _)| *t >= start && *t < start + secs) {
        let dt = (t - prev_t).clamp(0, 300) as f64;
        prev_t = t;
        let inside = tick > center - half && tick < center + half;
        if inside {
            revenue += rate_per_sec * dt * value;
            outside_since = None;
            continue;
        }
        let since = *outside_since.get_or_insert(t);
        if t - since >= CONFIRM_SECS && t - last_rebalance >= COOLDOWN_SECS {
            // Close the range: book its divergence against holding its starting mix, pay the cost,
            // open a new one centred on the current tick.
            let p = tick_to_price(tick);
            let v = position_value(liq, lo_p, hi_p, p);
            divergence += v - (hold_x * p + hold_y);
            value = v * (1.0 - cost);
            costs += v * cost;
            center = tick;
            lo_p = tick_to_price(center - half);
            hi_p = tick_to_price(center + half);
            p0 = tick_to_price(center);
            liq = liquidity_for_value(value, lo_p, hi_p, p0);
            (hold_x, hold_y) = holdings(liq, lo_p, hi_p, p0);
            last_rebalance = t;
            outside_since = None;
        }
    }
    // Close out the open range at the last price.
    if let Some(&(_, tick)) = path.iter().rev().find(|(t, _)| *t < start + secs) {
        let p = tick_to_price(tick);
        divergence += position_value(liq, lo_p, hi_p, p) - (hold_x * p + hold_y);
    }
    let days = secs as f64 / 86_400.0;
    Some((revenue - costs + divergence) / days)
}

fn holdings(l: f64, pa: f64, pb: f64, p: f64) -> (f64, f64) {
    let (sa, sb) = (pa.sqrt(), pb.sqrt());
    let sp = p.sqrt().clamp(sa, sb);
    (l * (sb - sp) / (sp * sb), l * (sp - sa))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Table {
        let v: Value = serde_json::from_str(&std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap()).unwrap();
        Table::from_keeper(&v).expect("real width_evals row parses")
    }

    #[test]
    fn parses_real_table_and_reconciles_revenue() {
        let t = fixture();
        assert_eq!(t.rows.first().unwrap().h, 2.0);
        assert!(t.rows.len() >= 10);
        // Cost per rebalance for h=2 is a small fraction of the position (swap fee + gas).
        let c = Table::cost_per_rebalance(&t.rows[0]);
        assert!(c > 0.0 && c < 0.002, "{c}");
    }

    #[test]
    fn same_vol_reproduces_keeper_scores() {
        let t = fixture();
        let (_, scores) = t.rescore(t.vol);
        for (r, (_, s)) in t.rows.iter().zip(&scores) {
            assert!((r.net - s).abs() < 1e-12, "h={} keeper {} vs rescored {}", r.h, r.net, s);
        }
    }

    #[test]
    fn higher_vol_never_prefers_narrower() {
        let t = fixture();
        let (calm, _) = t.rescore(t.vol * 0.6);
        let (wild, _) = t.rescore(t.vol * 1.6);
        assert!(wild >= calm, "calm {calm} wild {wild}");
    }

    #[test]
    fn replay_flat_price_earns_revenue_only() {
        let t = fixture();
        let path: Vec<(i64, f64)> = (0..1440).map(|i| (t.ts + i * 60, t.tick)).collect();
        let r = replay(&t, 4, &path, t.ts, 86_400).unwrap();
        let row = t.rows.iter().find(|r| r.h == 4.0).unwrap();
        let expect = row.gross_apr / 365.0 * USER_SHARE * row.capture;
        assert!((r - expect).abs() / expect < 0.01, "{r} vs {expect}");
    }

    #[test]
    fn replay_rebalances_after_six_hours_out() {
        let t = fixture();
        // Jump 3% up at minute 10 and stay there: h=2 (±1%) leaves, rebalances after 6 h.
        let jump = t.tick + 300.0;
        let path: Vec<(i64, f64)> = (0..1440).map(|i| (t.ts + i * 60, if i < 10 { t.tick } else { jump })).collect();
        let narrow = replay(&t, 2, &path, t.ts, 86_400).unwrap();
        let wide = replay(&t, 16, &path, t.ts, 86_400).unwrap();
        assert!(wide > narrow, "a wide range should ride out a 3% move better: wide {wide} narrow {narrow}");
        assert!(replay(&t, 2, &path[..600], t.ts, 86_400).is_none(), "needs coverage");
    }
}
