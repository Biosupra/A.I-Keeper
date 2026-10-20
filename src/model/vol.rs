//! Volatility: how fast ETH's price is moving, in the keeper's own unit (fraction per sqrt(hour);
//! 0.005 means a typical one-hour move of about 0.5%). The keeper's whole width choice rests on
//! this number, so forecasting it better than the keeper does is the A.I's first job.
//!
//! Forecast models (each predicts the realised volatility of the next `horizon`):
//! * `keeper`  the keeper's own estimator: max(last 24 h, last 48 h). The baseline to beat.
//! * `persist` the last `horizon` (at least one hour) repeated.
//! * `ewma`    exponentially weighted hourly variance (RiskMetrics, lambda 0.97 per hour).
//! * `har`     HAR-RV: tomorrow from today, the last week and the last month, refitted nightly
//!   on a year of one-minute Coinbase prices (see `har.rs`).
//! * `dvol`    Deribit's DVOL: the options market's own forecast (annualised %), converted.
//! * `blend`   the others combined, each weighted by how well it has scored over the last 30
//!   days. This is where the A.I learns: weights move toward whatever has been right.

use crate::store::Store;
use anyhow::Result;
use std::collections::BTreeMap;

/// The part of a TIME-SORTED price series in [from, to), found by binary search. Every caller
/// passes a sorted series (store.prices and store.pool_ticks order by time), and this is what lets
/// the historical replay run a year of hourly forecasts in seconds on a 1 GB machine.
fn window(prices: &[(i64, f64)], from: i64, to: i64) -> &[(i64, f64)] {
    let a = prices.partition_point(|x| x.0 < from);
    let b = prices.partition_point(|x| x.0 < to).max(a);
    &prices[a..b]
}

/// Realised volatility over [from, to) from one-minute closes, fraction per sqrt(hour).
/// Returns None unless at least 80% of the window is covered. `prices` must be sorted by time.
pub fn realised(prices: &[(i64, f64)], from: i64, to: i64) -> Option<f64> {
    let (mut ss, mut covered) = (0.0, 0i64);
    let mut prev: Option<(i64, f64)> = None;
    for &(t, p) in window(prices, from, to) {
        if let Some((pt, pp)) = prev {
            let dt = t - pt;
            if dt > 0 && dt <= 300 && pp > 0.0 && p > 0.0 {
                let r = (p / pp).ln();
                ss += r * r;
                covered += dt;
            }
        }
        prev = Some((t, p));
    }
    let span = (to - from).max(1);
    if covered < span * 8 / 10 {
        return None;
    }
    Some((ss / (covered as f64 / 3600.0)).sqrt())
}

/// EWMA of hourly squared returns (lambda per hour), over the last `days`. `prices` sorted by time.
pub fn ewma(prices: &[(i64, f64)], now: i64, days: i64, lambda: f64) -> Option<f64> {
    let mut hourly: BTreeMap<i64, f64> = BTreeMap::new();
    for &(t, p) in window(prices, now - days * 86_400, now) {
        hourly.insert(t / 3600, p); // last close of each hour
    }
    let closes: Vec<f64> = hourly.values().copied().collect();
    if closes.len() < 48 {
        return None;
    }
    let mut var = {
        let first: Vec<f64> = closes.windows(2).take(24).map(|w| (w[1] / w[0]).ln().powi(2)).collect();
        first.iter().sum::<f64>() / first.len() as f64
    };
    for w in closes.windows(2).skip(24) {
        let r2 = (w[1] / w[0]).ln().powi(2);
        var = lambda * var + (1.0 - lambda) * r2;
    }
    Some(var.sqrt())
}

/// DVOL (annualised percent) as fraction per sqrt(hour).
pub fn dvol_hourly(pct: f64) -> f64 {
    pct / 100.0 / (8760f64).sqrt()
}

/// DVOL (annualised percent) nearest before `now` (within 2 hours), as fraction per sqrt(hour).
pub fn dvol(store: &Store, now: i64) -> Result<Option<f64>> {
    let v: Option<f64> =
        store.db.query_row("SELECT value FROM dvol WHERE ts <= ?1 AND ts > ?1 - 7200 ORDER BY ts DESC LIMIT 1", [now], |r| r.get(0)).ok();
    Ok(v.map(dvol_hourly))
}

/// Market regime by realised volatility per sqrt(hour): the keeper's calm/storm line is ~0.5%.
pub fn regime(vol: f64) -> &'static str {
    if vol < 0.005 {
        "calm"
    } else if vol < 0.010 {
        "active"
    } else {
        "storm"
    }
}

/// Every base model's forecast (all but `blend`) for the realised volatility over the next
/// `horizon_h` hours, from one-minute prices before `now` (at least the last 30 days), the HAR
/// weights in force and the DVOL reading at `now`. Pure: the live forecasts and the historical
/// replay (`crate::replay`) both call this, so the replay grades exactly the live models.
pub fn base_forecasts(
    prices: &[(i64, f64)],
    now: i64,
    horizon_h: i64,
    har: Option<&super::har::Har>,
    dvol: Option<f64>,
) -> BTreeMap<&'static str, f64> {
    let mut out = BTreeMap::new();
    let r24 = realised(prices, now - 86_400, now);
    let r48 = realised(prices, now - 2 * 86_400, now);
    if let (Some(a), Some(b)) = (r24, r48) {
        out.insert("keeper", a.max(b));
    }
    let h = horizon_h.max(1);
    if let Some(p) = realised(prices, now - h * 3600, now) {
        out.insert("persist", p);
    }
    if let Some(e) = ewma(prices, now, 7, 0.97) {
        out.insert("ewma", e);
    }
    if let Some(v) = har.and_then(|h| super::har::forecast_with(h, prices, now)) {
        out.insert("har", v);
    }
    if let Some(d) = dvol {
        out.insert("dvol", d);
    }
    out
}

/// Every model's forecast for the realised volatility over the next `horizon_h` hours, using only
/// data before `now` (so it can be run "as of" a past moment for shadow rows).
pub fn forecasts(store: &Store, now: i64, horizon_h: i64) -> Result<BTreeMap<&'static str, f64>> {
    let prices = store.prices("coinbase", now - 30 * 86_400, now)?;
    let har: Option<super::har::Har> = store.meta_get("har")?.and_then(|raw| serde_json::from_str(&raw).ok());
    let mut out = base_forecasts(&prices, now, horizon_h, har.as_ref(), dvol(store, now)?);
    if let Some(b) = blend(store, &out, now)? {
        out.insert("blend", b);
    }
    Ok(out)
}

/// Combines base forecasts as a weighted mean of VARIANCES (variance is what adds up over time).
/// `weight(model)` is 1.0 for a model without enough graded history.
pub fn blend_with(base: &BTreeMap<&'static str, f64>, weight: impl Fn(&str) -> f64) -> Option<f64> {
    if base.len() < 2 {
        return None;
    }
    let (mut num, mut den) = (0.0, 0.0);
    for (m, v) in base {
        let w = weight(m);
        num += w * v * v;
        den += w;
    }
    Some((num / den).sqrt())
}

/// Blend weight from a model's graded 24 h forecasts: 1 / mean QLIKE with at least 10 of them.
pub fn blend_weight(n: i64, mean_loss: Option<f64>) -> f64 {
    match (n >= 10, mean_loss) {
        (true, Some(l)) if l > 1e-9 => 1.0 / l,
        _ => 1.0,
    }
}

/// Combines the base forecasts, weighting each by the inverse of its mean QLIKE loss on the 24 h
/// volatility target over the last 30 days (at least 10 graded forecasts each). Equal weights
/// until enough history exists. Only forecasts whose outcome was already known at `now` count
/// (`target_ts <= now`), so a replayed moment never learns from its own future.
pub fn blend(store: &Store, base: &BTreeMap<&'static str, f64>, now: i64) -> Result<Option<f64>> {
    let mut weights = BTreeMap::new();
    for m in base.keys() {
        let (n, loss): (i64, Option<f64>) = store.db.query_row(
            "SELECT COUNT(*), AVG(score) FROM predictions
             WHERE kind = 'vol24h' AND model = ?1 AND score IS NOT NULL AND made_ts > ?2 AND target_ts <= ?3",
            rusqlite::params![m, now - 30 * 86_400, now],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        weights.insert(*m, blend_weight(n, loss));
    }
    Ok(blend_with(base, |m| weights.get(m).copied().unwrap_or(1.0)))
}

/// QLIKE loss of a variance forecast (0 when perfect; punishes under-forecasting risk harder).
pub fn qlike(forecast_vol: f64, realised_vol: f64) -> f64 {
    let ratio = (realised_vol * realised_vol) / (forecast_vol * forecast_vol).max(1e-18);
    ratio - ratio.ln() - 1.0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic geometric Brownian path, one-minute steps, vol per sqrt(hour).
    pub fn gbm(start: i64, minutes: usize, vol_h: f64, seed: u64) -> Vec<(i64, f64)> {
        let mut s = seed.max(1);
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let sd = vol_h / 60f64.sqrt();
        let mut p = 2700.0;
        let mut out = Vec::with_capacity(minutes);
        for i in 0..minutes {
            let (u1, u2) = (next().max(1e-12), next());
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
            p *= (sd * z).exp();
            out.push((start + i as i64 * 60, p));
        }
        out
    }

    #[test]
    fn realised_recovers_true_vol() {
        let path = gbm(0, 3 * 1440, 0.006, 7);
        let v = realised(&path, 0, 3 * 86_400).unwrap();
        assert!((v - 0.006).abs() < 0.0004, "{v}");
    }

    #[test]
    fn realised_needs_coverage() {
        let path = gbm(0, 600, 0.006, 3);
        assert!(realised(&path, 0, 86_400).is_none());
    }

    #[test]
    fn ewma_close_to_truth() {
        let path = gbm(0, 10 * 1440, 0.005, 11);
        let v = ewma(&path, 10 * 86_400, 10, 0.97).unwrap();
        assert!((v - 0.005).abs() < 0.0015, "{v}");
    }

    /// The binary-search window gives exactly what the old full scan gave.
    #[test]
    fn windowed_results_equal_the_full_scan() {
        fn realised_scan(prices: &[(i64, f64)], from: i64, to: i64) -> Option<f64> {
            let (mut ss, mut covered) = (0.0, 0i64);
            let mut prev: Option<(i64, f64)> = None;
            for &(t, p) in prices.iter().filter(|(t, _)| *t >= from && *t < to) {
                if let Some((pt, pp)) = prev {
                    let dt = t - pt;
                    if dt > 0 && dt <= 300 && pp > 0.0 && p > 0.0 {
                        let r = (p / pp).ln();
                        ss += r * r;
                        covered += dt;
                    }
                }
                prev = Some((t, p));
            }
            if covered < (to - from).max(1) * 8 / 10 {
                return None;
            }
            Some((ss / (covered as f64 / 3600.0)).sqrt())
        }
        // A path with a gap and duplicate timestamps at the edges.
        let mut path = gbm(0, 5 * 1440, 0.006, 5);
        path.retain(|(t, _)| !(100_000..130_000).contains(t));
        path.insert(1000, path[999]);
        for (from, to) in [(0, 86_400), (3_600, 90_000), (86_400, 3 * 86_400), (99_960, 140_000), (0, 5 * 86_400), (7 * 86_400, 8 * 86_400)]
        {
            assert_eq!(realised(&path, from, to), realised_scan(&path, from, to), "[{from}, {to})");
        }
    }

    #[test]
    fn qlike_zero_when_right() {
        assert!(qlike(0.005, 0.005).abs() < 1e-12);
        assert!(qlike(0.004, 0.006) > qlike(0.008, 0.006), "under-forecasting risk costs more");
    }
}
