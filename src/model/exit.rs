//! "Will the price leave this range in the next 24 hours?" — the question behind every width.
//!
//! Widths are the keeper's: `h` spacings each side of the current tick, 50 ticks per spacing, so
//! h = 2 is about ±1% and h = 16 about ±8%.
//!
//! * `barrier:<vol model>`: the exact probability that a driftless random walk with that volatility
//!   touches either edge of a symmetric band within the horizon (double-barrier formula).
//! * `empirical`: how often it actually happened over the last 90 days, starting every hour.

use super::vol::realised;

pub const WIDTHS: &[i64] = &[2, 3, 4, 6, 8, 12, 16];
pub const TICKS_PER_SPACING: f64 = 50.0;

/// Half-width in log-price for `h` spacings.
pub fn half_width_log(h: i64) -> f64 {
    h as f64 * TICKS_PER_SPACING * 1.0001f64.ln()
}

fn norm_cdf(x: f64) -> f64 {
    // Abramowitz-Stegun 7.1.26 via erf; accurate to ~1e-7.
    let t = 1.0 / (1.0 + 0.327_591_1 * (x.abs() / std::f64::consts::SQRT_2));
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t + 0.254_829_592)
            * t
            * (-(x * x) / 2.0).exp();
    if x >= 0.0 {
        0.5 * (1.0 + y)
    } else {
        0.5 * (1.0 - y)
    }
}

/// Probability that Brownian motion with volatility `vol` (per sqrt hour) leaves (-a, a) within
/// `hours`, starting at 0.
pub fn exit_probability(a: f64, vol: f64, hours: f64) -> f64 {
    if vol <= 0.0 || hours <= 0.0 {
        return 0.0;
    }
    let s = vol * hours.sqrt();
    let x = a / s;
    if x > 3.0 {
        // Short horizon relative to the band: reflection bound (both edges), essentially exact here.
        return (4.0 * norm_cdf(-x)).clamp(0.0, 1.0);
    }
    // Eigenfunction series for the survival probability.
    let mut stay = 0.0;
    for k in 0..200 {
        let n = (2 * k + 1) as f64;
        let term = (4.0 / std::f64::consts::PI) * (if k % 2 == 0 { 1.0 } else { -1.0 }) / n
            * (-(n * n) * std::f64::consts::PI.powi(2) * s * s / (8.0 * a * a)).exp();
        stay += term;
        if term.abs() < 1e-12 {
            break;
        }
    }
    (1.0 - stay).clamp(0.0, 1.0)
}

/// Did a path leave (start - a, start + a) (log space) within [from, to)?
pub fn exited(path: &[(i64, f64)], from: i64, to: i64, start: f64, a: f64) -> bool {
    path.iter().filter(|(t, _)| *t >= from && *t < to).any(|(_, p)| (p / start).ln().abs() > a)
}

/// Fraction of hourly-started 24 h windows in the last `days` that left the band.
pub fn empirical(prices: &[(i64, f64)], now: i64, days: i64, h: i64) -> Option<f64> {
    let a = half_width_log(h);
    let (mut n, mut hit) = (0usize, 0usize);
    let mut t = now - days * 86_400;
    while t + 86_400 <= now {
        // Start price: last close at or before t (within 5 minutes).
        let i = prices.partition_point(|(pt, _)| *pt <= t);
        if i > 0 && t - prices[i - 1].0 <= 300 && realised(prices, t, t + 86_400).is_some() {
            n += 1;
            if exited(&prices[i..], t, t + 86_400, prices[i - 1].1, a) {
                hit += 1;
            }
        }
        t += 3600;
    }
    (n >= 24 * 30).then(|| hit as f64 / n as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vol::tests::gbm;

    #[test]
    fn formula_limits() {
        let a = half_width_log(4);
        assert!(exit_probability(a, 0.0001, 24.0) < 1e-6, "calm market: stays in");
        assert!(exit_probability(a, 0.05, 24.0) > 0.999, "wild market: leaves");
        // Monotone in width and vol.
        assert!(exit_probability(half_width_log(2), 0.005, 24.0) > exit_probability(half_width_log(8), 0.005, 24.0));
        assert!(exit_probability(a, 0.008, 24.0) > exit_probability(a, 0.004, 24.0));
        // The two branches agree where they meet.
        let s = a / 3.0;
        let lo = exit_probability(a, s * 1.0001, 1.0);
        let hi = exit_probability(a, s * 0.9999, 1.0);
        assert!((lo - hi).abs() < 1e-3, "{lo} vs {hi}");
    }

    #[test]
    fn formula_matches_simulation() {
        // 400 simulated days at 0.5%/sqrt(h), band ±2% (h = 4): compare the hit rate.
        let a = half_width_log(4);
        let (mut hit, n) = (0, 400);
        for seed in 0..n {
            let p = gbm(0, 1440, 0.005, seed as u64 * 7919 + 13);
            if exited(&p, 0, 86_400, 2700.0, a) {
                hit += 1;
            }
        }
        let sim = hit as f64 / n as f64;
        let exact = exit_probability(a, 0.005, 24.0);
        // Discrete one-minute monitoring slightly under-counts touches; allow sampling error.
        assert!((sim - exact).abs() < 0.08, "sim {sim} vs formula {exact}");
    }
}
