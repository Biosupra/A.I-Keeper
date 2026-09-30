//! HAR-RV (Corsi 2009): tomorrow's volatility as a weighted mix of today's, the last week's and
//! the last month's. Simple, famously hard to beat, and it learns: the weights are refitted every
//! night on all the one-minute history the A.I holds.

use super::vol::realised;
use crate::store::Store;
use anyhow::Result;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Har {
    pub b0: f64,
    pub bd: f64,
    pub bw: f64,
    pub bm: f64,
    pub n: usize,
}

/// Daily realised volatilities (UTC days) ending before `until`, oldest first.
pub fn daily_series(prices: &[(i64, f64)], until: i64, days: i64) -> Vec<Option<f64>> {
    let last_day = until / 86_400; // today (incomplete) excluded
    ((last_day - days)..last_day).map(|d| realised(prices, d * 86_400, (d + 1) * 86_400)).collect()
}

fn mean(xs: &[Option<f64>]) -> Option<f64> {
    let v: Vec<f64> = xs.iter().flatten().copied().collect();
    if v.len() * 10 < xs.len() * 8 {
        None
    } else {
        Some(v.iter().sum::<f64>() / v.len() as f64)
    }
}

fn features(s: &[Option<f64>], t: usize) -> Option<[f64; 4]> {
    if t < 29 {
        return None;
    }
    Some([1.0, s[t]?, mean(&s[t - 6..=t])?, mean(&s[t - 29..=t])?])
}

/// Ordinary least squares of y on X (4 columns) via the normal equations.
fn ols(xs: &[[f64; 4]], ys: &[f64]) -> Option<[f64; 4]> {
    let mut a = [[0.0f64; 5]; 4];
    for (x, y) in xs.iter().zip(ys) {
        for i in 0..4 {
            for j in 0..4 {
                a[i][j] += x[i] * x[j];
            }
            a[i][4] += x[i] * y;
        }
    }
    // Gaussian elimination with partial pivoting.
    for c in 0..4 {
        let p = (c..4).max_by(|&i, &j| a[i][c].abs().total_cmp(&a[j][c].abs()))?;
        a.swap(c, p);
        if a[c][c].abs() < 1e-18 {
            return None;
        }
        for r in 0..4 {
            if r != c {
                let f = a[r][c] / a[c][c];
                let pivot = a[c];
                for (k, cell) in a[r].iter_mut().enumerate().skip(c) {
                    *cell -= f * pivot[k];
                }
            }
        }
    }
    Some([a[0][4] / a[0][0], a[1][4] / a[1][1], a[2][4] / a[2][2], a[3][4] / a[3][3]])
}

/// Fits HAR on the daily series. Needs at least 60 usable days.
pub fn fit(series: &[Option<f64>]) -> Option<Har> {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for t in 29..series.len().saturating_sub(1) {
        if let (Some(x), Some(y)) = (features(series, t), series[t + 1]) {
            xs.push(x);
            ys.push(y);
        }
    }
    if xs.len() < 60 {
        return None;
    }
    let b = ols(&xs, &ys)?;
    Some(Har { b0: b[0], bd: b[1], bw: b[2], bm: b[3], n: xs.len() })
}

/// Refit on up to a year of Coinbase history and store the weights (called nightly).
pub fn refit(store: &Store, now: i64) -> Result<Option<Har>> {
    let prices = store.prices("coinbase", now - 400 * 86_400, now)?;
    let series = daily_series(&prices, now, 365);
    let fitted = fit(&series);
    if let Some(h) = fitted {
        store.meta_set("har", &serde_json::to_string(&h)?)?;
        store.meta_set("har_fitted_at", &now.to_string())?;
    }
    Ok(fitted)
}

/// Next-day forecast as of `now` with the given weights, from the last 30 days of `prices`.
pub fn forecast_with(h: &Har, prices: &[(i64, f64)], now: i64) -> Option<f64> {
    let series = daily_series(prices, now, 30);
    let x = features(&series, series.len() - 1)?;
    let v = h.b0 + h.bd * x[1] + h.bw * x[2] + h.bm * x[3];
    (v.is_finite() && v > 0.0).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ols_recovers_known_weights() {
        // y = 0.001 + 0.3 d + 0.4 w + 0.2 m, exactly.
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for i in 0..200 {
            let d = 0.004 + 0.001 * ((i * 7 % 13) as f64 / 13.0);
            let w = 0.004 + 0.001 * ((i * 5 % 11) as f64 / 11.0);
            let m = 0.004 + 0.001 * ((i * 3 % 17) as f64 / 17.0);
            xs.push([1.0, d, w, m]);
            ys.push(0.001 + 0.3 * d + 0.4 * w + 0.2 * m);
        }
        let b = ols(&xs, &ys).unwrap();
        for (got, want) in b.iter().zip([0.001, 0.3, 0.4, 0.2]) {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }
    }

    #[test]
    fn fit_needs_history() {
        let short: Vec<Option<f64>> = (0..50).map(|_| Some(0.005)).collect();
        assert!(fit(&short).is_none());
    }
}
