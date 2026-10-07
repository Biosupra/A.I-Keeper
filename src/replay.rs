//! The past, replayed, so the A.I learns from good times AND bad ones instead of waiting for them.
//!
//! Owner, 2026-09-30: "it needs to learn everything, good times and bad; if it doesn't see the bad
//! times it won't know what not to do." The A.I holds a year of one-minute ETH prices and DVOL but
//! had only been graded on what happened since it started. Two replays fix that:
//!
//! 1. `forecasts`: WALK-FORWARD. For every hour of the past, every volatility model forecasts from
//!    data BEFORE that hour only (the same code as live, `vol::base_forecasts`); HAR is refitted
//!    weekly on the year before each moment; the blend weighs only forecasts whose outcome was
//!    already known. Each forecast is graded on the 24 hours that followed and labelled with the
//!    regime it was made in (calm / active / storm, by the previous day's volatility), so the A.I
//!    learns which model to trust in which kind of market (`registry::champion`).
//! 2. `days`: for every day of the past, every fixed width and every model's own width choices are
//!    run under the keeper's CURRENT rules (`shadow::replay`) on the real ETH path: what each would
//!    have earned, paid and lost, including on the worst days of the year.
//!
//! What is real and what is assumed (recorded per run in `replay_runs`): the prices, the exits,
//! the confirmation clocks, the rebalance costs and the divergence come from the real one-minute
//! path. Before the vault existed there are no emission figures, so every day is scored with a
//! real keeper width table from since the vault launched (the one at the median volatility) for
//! the emission rate, gas and position size, the pool fee at its median since launch, and the
//! models' Coinbase volatility scaled to the pool's basis (median ratio measured since launch).
//! Each day starts a fresh $1,000 position at the day's first price, like shadow mode.

use crate::model::{har, shadow, vol};
use crate::registry;
use crate::store::Store;
use anyhow::{bail, Result};
use rusqlite::params;
use serde_json::json;
use std::collections::{HashMap, VecDeque};

/// The models the A.I forecasts with (vol24h), whose width choices are replayed per day.
pub const MODELS: &[&str] = &["keeper", "persist", "ewma", "har", "dvol", "blend"];

fn slice(prices: &[(i64, f64)], from: i64, to: i64) -> &[(i64, f64)] {
    let a = prices.partition_point(|x| x.0 < from);
    let b = prices.partition_point(|x| x.0 < to).max(a);
    &prices[a..b]
}

fn tick_of(price: f64) -> f64 {
    (price / 1e12).ln() / 1.0001f64.ln()
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

#[derive(Debug, Default, PartialEq)]
pub struct ForecastSummary {
    pub hours: usize,
    pub rows: usize,
}

/// Walk-forward forecasts for every hour in [from, to] that has none yet (live hours are left as
/// they are). Only hours whose 24 h outcome is complete by `now` are replayed.
pub fn forecasts(store: &Store, from: i64, to: i64, now: i64) -> Result<ForecastSummary> {
    let to = to.min(now - 86_400 - 3600);
    let start = (from + 3599) / 3600 * 3600;
    let mut out = ForecastSummary::default();
    if start > to {
        return Ok(out);
    }
    let prices = store.prices("coinbase", start - 400 * 86_400, to + 2 * 86_400)?;
    // At least 30 days before the first replayed hour (the models' look-back).
    if prices.partition_point(|x| x.0 < start) < 30 * 1440 * 9 / 10 {
        bail!("replay needs Coinbase history (run `aikeeper backfill` first)");
    }
    let dvol_rows: Vec<(i64, f64)> = {
        let mut st = store.db.prepare("SELECT ts, value FROM dvol ORDER BY ts")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    // Same lookup as vol::dvol: the newest reading at or before t, at most 2 hours old.
    let dvol_at = |t: i64| -> Option<f64> {
        let i = dvol_rows.partition_point(|x| x.0 <= t);
        (i > 0).then(|| dvol_rows[i - 1]).filter(|x| x.0 > t - 7200).map(|x| vol::dvol_hourly(x.1))
    };
    let mut har_cur: Option<har::Har> = None;
    let mut har_at: Option<i64> = None;
    // Graded 24 h forecasts become usable by the blend only once their outcome is known.
    let mut pending: VecDeque<(i64, &'static str, i64, f64)> = VecDeque::new(); // (target, model, made, score)
    let mut known: HashMap<&'static str, VecDeque<(i64, f64)>> = HashMap::new(); // model -> (made, score)
    let mut tx = store.db.unchecked_transaction()?;
    let mut in_tx = 0;
    let mut h = start;
    while h <= to {
        if har_at.map(|t| h - t >= 7 * 86_400).unwrap_or(true) {
            har_cur = har::fit(&har::daily_series(slice(&prices, h - 400 * 86_400, h), h, 365));
            har_at = Some(h);
        }
        while pending.front().map(|p| p.0 <= h).unwrap_or(false) {
            let (_, m, made, score) = pending.pop_front().expect("front exists");
            known.entry(m).or_default().push_back((made, score));
        }
        for q in known.values_mut() {
            while q.front().map(|x| x.0 <= h - 30 * 86_400).unwrap_or(false) {
                q.pop_front();
            }
        }
        let prior = vol::realised(&prices, h - 86_400, h);
        let regime = prior.map(vol::regime);
        let exists: i64 =
            tx.query_row("SELECT COUNT(*) FROM predictions WHERE made_ts >= ?1 AND made_ts < ?2", params![h, h + 3600], |r| r.get(0))?;
        if exists > 0 {
            // A live hour: keep its forecasts, label their regime if they predate the label.
            tx.execute(
                "UPDATE predictions SET regime = ?1 WHERE made_ts >= ?2 AND made_ts < ?3 AND regime IS NULL",
                params![regime, h, h + 3600],
            )?;
            h += 3600;
            continue;
        }
        // Only hours whose outcome is in the data are replayed (a gap is retried on the next run),
        // and every replayed row is graded: an ungradeable row would sit in `resolve`'s queue forever.
        let real24 = vol::realised(&prices, h, h + 86_400);
        let real1 = vol::realised(&prices, h, h + 3600);
        if real24.is_none() {
            h += 3600;
            continue;
        }
        let hist = slice(&prices, h - 30 * 86_400, h);
        let weight = |m: &str| -> f64 {
            known
                .get(m)
                .filter(|q| !q.is_empty())
                .map(|q| vol::blend_weight(q.len() as i64, Some(q.iter().map(|x| x.1).sum::<f64>() / q.len() as f64)))
                .unwrap_or(1.0)
        };
        let mut f24 = vol::base_forecasts(hist, h, 24, har_cur.as_ref(), dvol_at(h));
        if let Some(b) = vol::blend_with(&f24, weight) {
            f24.insert("blend", b);
        }
        let mut f1 = vol::base_forecasts(hist, h, 1, har_cur.as_ref(), dvol_at(h));
        if let Some(b) = vol::blend_with(&f1, weight) {
            f1.insert("blend", b);
        }
        let inputs = json!({ "vol24h": f24, "vol1h": f1, "replay": true, "regime": regime, "prior_vol": prior }).to_string();
        let put = |kind: &str, horizon: i64, m: &str, v: f64, real: Option<f64>| -> Result<Option<f64>> {
            let score = real.map(|r| vol::qlike(v, r));
            tx.execute(
                "INSERT INTO predictions (made_ts, target_ts, kind, model, key, value, inputs, outcome, score, resolved_ts, regime, replay)
                 VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7, ?8, ?9, ?10, 1)",
                params![h, h + horizon, kind, m, v, inputs, real, score, now, regime],
            )?;
            Ok(score)
        };
        for (m, v) in &f24 {
            if let Some(score) = put("vol24h", 86_400, m, *v, real24)? {
                pending.push_back((h + 86_400, *m, h, score));
            }
            out.rows += 1;
        }
        if real1.is_some() {
            for m in ["persist", "ewma", "keeper", "blend"] {
                if let Some(v) = f1.get(m) {
                    put("vol1h", 3600, m, *v, real1)?;
                    out.rows += 1;
                }
            }
        }
        out.hours += 1;
        in_tx += 1;
        if in_tx >= 500 {
            tx.commit()?;
            tx = store.db.unchecked_transaction()?;
            in_tx = 0;
        }
        h += 3600;
    }
    tx.commit()?;
    Ok(out)
}

#[derive(Debug, Default)]
pub struct DaySummary {
    pub days: usize,
    pub rows: usize,
}

/// Replays every complete UTC day in [from, to] not replayed yet: every fixed width, and every
/// model's own width choices (from its hourly 24 h forecasts), under the keeper's current rules.
///
/// `vault`: whose keeper's current policy the replay applies (the live keeper's). The width tables it reads
/// for volatility, gas and size come from every vault's keeper: all run the same pool with the same engine.
pub fn days(store: &Store, from: i64, to: i64, now: i64, vault: &str) -> Result<DaySummary> {
    // Real keeper width tables (since the vault launched) give the emission rate, gas and size.
    // Only each table's time and volatility are read for all of them (the server holds one a
    // minute; parsing them all nightly would cost tens of MB); the full table only for the template.
    let vols: Vec<(i64, f64)> = {
        let mut st = store.db.prepare(
            "SELECT ts, CAST(json_extract(json, '$.realised_vol_hourly') AS REAL) FROM keeper_rows
             WHERE tbl = 'width_evals' AND json_extract(json, '$.realised_vol_hourly') IS NOT NULL ORDER BY ts",
        )?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.filter_map(|x| x.ok()).collect()
    };
    let mut by_vol: Vec<(i64, f64)> = vols.clone();
    by_vol.sort_by(|a, b| a.1.total_cmp(&b.1));
    // The table at the median volatility (or the nearest one that parses).
    let mid = by_vol.len() / 2;
    let mut template: Option<shadow::Table> = None;
    for k in 0..by_vol.len() {
        let i = if k % 2 == 0 { mid + k / 2 } else { mid.wrapping_sub(k / 2 + 1) };
        let Some(&(ts, _)) = by_vol.get(i) else { continue };
        let raw: String =
            store.db.query_row("SELECT json FROM keeper_rows WHERE tbl = 'width_evals' AND ts = ?1 LIMIT 1", [ts], |r| r.get(0))?;
        template = serde_json::from_str(&raw).ok().and_then(|v| shadow::Table::from_keeper(&v));
        if template.is_some() {
            break;
        }
    }
    let Some(template) = template else {
        bail!("replay needs at least one keeper width table (sync the keeper bucket first)");
    };

    let prices = store.prices("coinbase", from - 3 * 86_400, to + 2 * 86_400)?;
    // The pool measures calmer than Coinbase (a basis gap, see registry::make_shadow): the models'
    // Coinbase volatility is scaled by the median ratio of the keeper's own volatility to the
    // keeper-style estimator on Coinbase, measured on up to 200 of its tables.
    let basis_prices = store.prices("coinbase", vols[0].0 - 3 * 86_400, now)?;
    let step = (vols.len() / 200).max(1);
    let ratios: Vec<f64> = vols
        .iter()
        .step_by(step)
        .filter_map(|&(ts, v)| {
            let a = vol::realised(&basis_prices, ts - 86_400, ts)?;
            let b = vol::realised(&basis_prices, ts - 2 * 86_400, ts)?;
            let cb = a.max(b);
            (cb > 0.0).then(|| v / cb)
        })
        .collect();
    let basis = median(ratios).filter(|b| (0.3..=2.0).contains(b)).unwrap_or(1.0);
    let fees: Vec<f64> = {
        let mut st = store.db.prepare(
            "SELECT CAST(fee_pips AS REAL) FROM keeper_market_min WHERE fee_pips IS NOT NULL
             UNION ALL SELECT CAST(fee_pips AS REAL) FROM pool_obs WHERE fee_pips IS NOT NULL",
        )?;
        let r = st.query_map([], |r| r.get(0))?;
        r.filter_map(|x| x.ok()).collect()
    };
    let fee = median(fees).unwrap_or(500.0);
    let policy = registry::policy_at(store, now, vault)?;

    // Every model's hourly 24 h forecast (replayed or live), by (model, hour).
    let preds: HashMap<(String, i64), f64> = {
        let mut st = store.db.prepare(
            "SELECT model, made_ts / 3600 * 3600, value FROM predictions WHERE kind = 'vol24h' AND made_ts >= ?1 AND made_ts < ?2",
        )?;
        let r =
            st.query_map(params![from - 3600, to + 86_400], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, i64>(1)?), r.get::<_, f64>(2)?)))?;
        r.filter_map(|x| x.ok()).collect()
    };

    let assumptions = json!({
        "template_table_ts": template.ts, "template_vol": template.vol, "template_choice": template.choice,
        "gross_apr_by_width": template.rows.iter().map(|r| (r.h as i64, r.gross_apr)).collect::<Vec<_>>(),
        "gas_usd": template.gas_usd, "position_usd": template.position_usd,
        "pool_fee_pips": fee, "vol_basis_pool_over_coinbase": basis, "basis_samples": vols.len().div_ceil(step),
        "keeper_tables_available": vols.len(), "policy": policy,
        "note": "prices, exits, clocks, costs and divergence are real; emissions, gas and the pool fee are the levels measured since the vault launched",
    });

    let mut out = DaySummary::default();
    let last_day = (to.min(now - 86_400)) / 86_400 * 86_400 - 86_400;
    let mut d = from / 86_400 * 86_400;
    while d <= last_day {
        let done: i64 = store.db.query_row("SELECT COUNT(*) FROM replay_day WHERE day = ?1", [d], |r| r.get(0))?;
        if done > 0 {
            d += 86_400;
            continue;
        }
        let path: Vec<(i64, f64, Option<f64>)> =
            slice(&prices, d - 900, d + 86_400).iter().map(|&(t, p)| (t, tick_of(p), Some(fee))).collect();
        let day_vol = vol::realised(&prices, d, d + 86_400);
        let prior_vol = vol::realised(&prices, d - 86_400, d);
        let mut rows: Vec<(String, shadow::Replay)> = Vec::new();
        // Fixed widths: the template table, in force from before the day starts.
        let mut t0 = template.clone();
        t0.ts = d - 1;
        let fixed_hour = shadow::Hour { nets: t0.rows.iter().map(|r| (r.h as i64, r.net)).collect(), choice: t0.choice, table: t0.clone() };
        for r in &template.rows {
            let w = r.h as i64;
            if let Some(res) = shadow::replay(&policy, std::slice::from_ref(&fixed_hour), &path, d, 86_400, Some(w)) {
                rows.push((format!("fixed:{w}"), res));
            }
        }
        // Each model's own choices: its forecast each hour, on the pool's basis, through the keeper's rules.
        for m in MODELS {
            let hours: Option<Vec<shadow::Hour>> = (0..24)
                .map(|k| {
                    let at = d + k * 3600;
                    let v = preds.get(&(m.to_string(), at))? * basis;
                    let mut t = template.clone();
                    t.ts = at;
                    let nets = t.rescore(v);
                    let choice = shadow::choose(&t, &nets, &policy);
                    Some(shadow::Hour { table: t, nets, choice })
                })
                .collect();
            if let Some(hours) = hours {
                if let Some(res) = shadow::replay(&policy, &hours, &path, d, 86_400, None) {
                    rows.push((m.to_string(), res));
                }
            }
        }
        let tx = store.db.unchecked_transaction()?;
        for (strategy, r) in &rows {
            tx.execute(
                "INSERT OR REPLACE INTO replay_day (day, strategy, net_per_day, revenue, costs, divergence, rebalances, recuts, held_out_minutes, in_range, day_vol, prior_vol)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    d, strategy, r.net_per_day, r.revenue, r.costs, r.divergence, r.rebalances, r.recuts, r.held_out_minutes,
                    r.in_range_fraction, day_vol, prior_vol
                ],
            )?;
        }
        tx.commit()?;
        if !rows.is_empty() {
            out.days += 1;
            out.rows += rows.len();
        }
        d += 86_400;
    }
    store.db.execute(
        "INSERT OR REPLACE INTO replay_runs (ts, kind, assumptions) VALUES (?1, 'days', ?2)",
        params![now, assumptions.to_string()],
    )?;
    Ok(out)
}

/// Bump whenever the replay's logic changes: every replayed row is then deleted and the whole
/// history replayed again on the next run, so no result of an older replay survives a fix.
pub const VERSION: &str = "1";

/// Both replays over the last `days_back` days (each hour and day only once), or over all history
/// when the replay's VERSION changed. The day replay needs a keeper width table; without the keeper
/// bucket it is skipped and says so.
pub fn run(store: &Store, now: i64, days_back: i64, vault: &str) -> Result<String> {
    let first: Option<i64> = store.db.query_row("SELECT MIN(ts) FROM price_1m WHERE source = 'coinbase'", [], |r| r.get(0))?;
    let Some(first) = first else { bail!("no Coinbase history yet (run `aikeeper backfill` first)") };
    let mut days_back = days_back;
    let mut out = String::new();
    if store.meta_get("replay_version")?.as_deref() != Some(VERSION) {
        let tx = store.db.unchecked_transaction()?;
        let n = tx.execute("DELETE FROM predictions WHERE replay = 1", [])?;
        tx.execute("DELETE FROM replay_day", [])?;
        tx.execute("DELETE FROM replay_runs", [])?;
        tx.commit()?;
        days_back = (now - first) / 86_400 + 1;
        out = format!("replay v{VERSION}: {n} older replayed forecasts cleared, replaying all history; ");
    }
    // Forecasts need 30 days of history behind them.
    let from = (now - days_back * 86_400).max(first + 30 * 86_400);
    let f = forecasts(store, from, now, now)?;
    out += &format!("forecasts: {} hours replayed, {} graded forecasts", f.hours, f.rows);
    match days(store, from, now, now, vault) {
        Ok(d) => out += &format!("; days: {} replayed ({} strategy rows)", d.days, d.rows),
        Err(e) => out += &format!("; days: skipped ({e:#})"),
    }
    // Stamped only once the forecasts are complete: an interrupted rebuild starts over.
    store.meta_set("replay_version", VERSION)?;
    Ok(out)
}

fn regime_name(r: &str) -> &'static str {
    match r {
        "calm" => "calm days (under 0.5%/sqrt h)",
        "active" => "active days (0.5-1%)",
        _ => "storm days (over 1%)",
    }
}

/// The brief's THE PAST, REPLAYED section: what every replayed day taught, good days and bad.
/// None until a replay has run.
pub fn the_past(store: &Store, now: i64) -> Result<Option<String>> {
    use std::fmt::Write;
    let rows: Vec<(i64, String, f64, Option<f64>)> = {
        let mut st = store.db.prepare("SELECT day, strategy, net_per_day, day_vol FROM replay_day ORDER BY day")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let mut fc: Vec<(&str, Vec<registry::ModelScore>)> = Vec::new();
    for regime in ["calm", "active", "storm"] {
        let s = registry::regime_scores(store, regime, now)?;
        if !s.is_empty() {
            fc.push((regime, s));
        }
    }
    if rows.is_empty() && fc.is_empty() {
        return Ok(None);
    }
    let pct = |x: f64| format!("{:+.3}%", x * 100.0);
    let date = |d: i64| chrono::DateTime::from_timestamp(d, 0).map(|x| x.format("%Y-%m-%d").to_string()).unwrap_or_default();
    let mut b = String::new();
    let n_days = rows.iter().map(|r| r.0).collect::<std::collections::BTreeSet<_>>().len();
    let first = rows.first().map(|r| date(r.0)).unwrap_or_default();
    writeln!(
        b,
        "\nTHE PAST, REPLAYED ({n_days} days since {first}: every model and width on every day, using only what it could have seen then)"
    )?;

    // Which forecaster to trust in which market (paired: the same hours for every model).
    for (regime, ms) in &fc {
        let best = &ms[0];
        let keeper = ms.iter().find(|x| x.0 == "keeper").map(|x| format!(", keeper's own {:.3}", x.2)).unwrap_or_default();
        writeln!(
            b,
            "  Forecasts, {}: best {} {:.3}{keeper} (QLIKE, lower is better; {} hours)",
            regime_name(regime),
            best.0,
            best.2,
            best.1
        )?;
    }
    if let (Some(r), c) = (registry::current_regime(store, now)?, registry::champion(store, now)?) {
        writeln!(b, "  The last 24 h were {} -> the A.I follows {c}", r)?;
    }

    // What each width and each model's own choices would have earned, by the kind of day it was.
    let mut by: HashMap<(&'static str, &str), (f64, usize)> = HashMap::new();
    for (_, s, net, dv) in &rows {
        if let Some(v) = dv {
            let e = by.entry((vol::regime(*v), s.as_str())).or_default();
            e.0 += net;
            e.1 += 1;
        }
    }
    for regime in ["calm", "active", "storm"] {
        let mut fixed: Vec<(i64, f64, usize)> = by
            .iter()
            .filter(|((r, s), _)| *r == regime && s.starts_with("fixed:"))
            .filter_map(|((_, s), (sum, n))| Some((s.trim_start_matches("fixed:").parse().ok()?, sum / *n as f64, *n)))
            .collect();
        if fixed.is_empty() {
            continue;
        }
        fixed.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (best, worst) = (fixed[0], fixed[fixed.len() - 1]);
        let keeper = by.get(&(regime, "keeper")).map(|(s, n)| format!("; keeper-style choices {}", pct(s / *n as f64))).unwrap_or_default();
        writeln!(
            b,
            "  Widths, {} ({} days): best ±{:.1}% {} /day, worst ±{:.1}% {} /day{keeper}",
            regime_name(regime),
            best.2,
            best.0 as f64 * 0.5,
            pct(best.1),
            worst.0 as f64 * 0.5,
            pct(worst.1)
        )?;
    }
    // Each model's strategy, and its worst day, PAIRED: only days on which every model has a result
    // (HAR has none for the first 90 days of history), so each line covers the same days.
    let mut per_day: HashMap<i64, usize> = HashMap::new();
    for r in rows.iter().filter(|r| MODELS.contains(&r.1.as_str())) {
        *per_day.entry(r.0).or_default() += 1;
    }
    let full = per_day.values().copied().max().unwrap_or(0);
    let paired: std::collections::BTreeSet<i64> = per_day.iter().filter(|(_, n)| **n == full).map(|(d, _)| *d).collect();
    let mut lines: Vec<(f64, String)> = Vec::new();
    for m in MODELS {
        let mine: Vec<&(i64, String, f64, Option<f64>)> = rows.iter().filter(|r| r.1 == *m && paired.contains(&r.0)).collect();
        let Some(worst) = mine.iter().min_by(|a, b| a.2.total_cmp(&b.2)) else { continue };
        let mean = mine.iter().map(|r| r.2).sum::<f64>() / mine.len() as f64;
        lines.push((mean, format!("    {m:<8} {} /day; worst day {} {}", pct(mean), date(worst.0), pct(worst.2))));
    }
    lines.sort_by(|a, b| b.0.total_cmp(&a.0));
    if !lines.is_empty() {
        writeln!(b, "  Each model's own width choices under the keeper's current rules (the same {} days for each):", paired.len())?;
        for (_, l) in lines {
            writeln!(b, "{l}")?;
        }
    }
    // The bad times, named: the three worst days for the keeper-style strategy.
    let mut bad: Vec<&(i64, String, f64, Option<f64>)> = rows.iter().filter(|r| r.1 == "keeper").collect();
    bad.sort_by(|a, b| a.2.total_cmp(&b.2));
    if !bad.is_empty() {
        let s: Vec<String> =
            bad.iter().take(3).map(|r| format!("{} {} (vol {:.2}%)", date(r.0), pct(r.2), r.3.unwrap_or(0.0) * 100.0)).collect();
        writeln!(b, "  Worst days to learn from (keeper-style): {}", s.join(", "))?;
    }
    let assumed: Option<String> = store.db.query_row("SELECT assumptions FROM replay_runs ORDER BY ts DESC LIMIT 1", [], |r| r.get(0)).ok();
    if let Some(a) = assumed.and_then(|a| serde_json::from_str::<serde_json::Value>(&a).ok()) {
        writeln!(
            b,
            "  Assumed before the vault existed: emissions and gas from a keeper table at {:.2}% vol, pool fee {:.0} pips, pool/Coinbase vol basis {:.2}",
            a["template_vol"].as_f64().unwrap_or(0.0) * 100.0,
            a["pool_fee_pips"].as_f64().unwrap_or(0.0),
            a["vol_basis_pool_over_coinbase"].as_f64().unwrap_or(1.0)
        )?;
    }
    Ok(Some(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vol::tests::gbm;

    fn store_with_prices(days: usize, vol_h: f64) -> (Store, i64) {
        let s = Store::memory().unwrap();
        let start = 1_760_000_000 / 86_400 * 86_400;
        for (t, p) in gbm(start, days * 1440, vol_h, 42) {
            s.put_price("coinbase", t, p, None, None, None).unwrap();
        }
        (s, start)
    }

    #[test]
    fn a_new_replay_version_rebuilds_everything() {
        let (s, start) = store_with_prices(45, 0.005);
        let now = start + 45 * 86_400;
        let first = run(&s, now, 3, crate::collect::bucket::OLD).unwrap();
        assert!(first.contains("replaying all history"), "a fresh database replays everything: {first}");
        let count = |s: &Store| -> i64 { s.db.query_row("SELECT COUNT(*) FROM predictions WHERE replay = 1", [], |r| r.get(0)).unwrap() };
        let n = count(&s);
        assert!(n > 13 * 24 * 6, "{n}");
        // The same version: nothing new.
        assert!(run(&s, now, 3, crate::collect::bucket::OLD).unwrap().starts_with("forecasts: 0 hours"));
        // A stale row from an older replay logic, then a version change: all rebuilt, stale row gone.
        s.db.execute("UPDATE predictions SET value = -1 WHERE replay = 1 AND id = (SELECT MIN(id) FROM predictions WHERE replay = 1)", [])
            .unwrap();
        s.meta_set("replay_version", "0").unwrap();
        let again = run(&s, now, 3, crate::collect::bucket::OLD).unwrap();
        assert!(again.contains(&format!("{n} older replayed forecasts cleared")), "{again}");
        assert_eq!(count(&s), n);
        let stale: i64 = s.db.query_row("SELECT COUNT(*) FROM predictions WHERE value = -1", [], |r| r.get(0)).unwrap();
        assert_eq!(stale, 0);
        // A live forecast is never touched.
        s.db.execute(
            "INSERT INTO predictions (made_ts, target_ts, kind, model, key, value) VALUES (?1, ?2, 'vol24h', 'ewma', '', 0.004)",
            params![now, now + 86_400],
        )
        .unwrap();
        s.meta_set("replay_version", "0").unwrap();
        run(&s, now, 3, crate::collect::bucket::OLD).unwrap();
        let live: i64 = s.db.query_row("SELECT COUNT(*) FROM predictions WHERE replay IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(live, 1);
    }

    #[test]
    fn walk_forward_never_uses_the_future() {
        // Two worlds identical up to hour h, wildly different after it (a storm). Every forecast
        // made over a replayed week, for every model including HAR and the blend, must be the
        // same in both; only the grades differ.
        // (HAR needs 90 days of history before it can fit, so the worlds are 130 days long.)
        let (calm, start) = store_with_prices(130, 0.004);
        let (storm, _) = store_with_prices(130, 0.004);
        let now = start + 130 * 86_400;
        let h = start + 100 * 86_400;
        let later = gbm(h, 25 * 1440, 0.03, 9);
        let p0 = storm.prices("coinbase", h - 60, h).unwrap()[0].1;
        for (t, p) in later {
            storm.put_price("coinbase", t, p0 * p / 2700.0, None, None, None).unwrap();
        }
        let (from, to) = (h - 7 * 86_400, h);
        assert_eq!(forecasts(&calm, from, to, now).unwrap().hours, 7 * 24 + 1);
        assert_eq!(forecasts(&storm, from, to, now).unwrap().hours, 7 * 24 + 1);
        let read = |s: &Store| -> Vec<(i64, String, String, f64)> {
            let mut st = s.db.prepare("SELECT made_ts, kind, model, value FROM predictions ORDER BY made_ts, kind, model").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap().map(|x| x.unwrap()).collect()
        };
        let (a, b) = (read(&calm), read(&storm));
        assert_eq!(a.len(), b.len());
        assert!(a.iter().any(|x| x.2 == "har") && a.iter().any(|x| x.2 == "blend"), "HAR and blend are replayed");
        for (x, y) in a.iter().zip(&b) {
            assert_eq!((x.0, &x.1, &x.2), (y.0, &y.1, &y.2));
            assert!((x.3 - y.3).abs() < 1e-15, "{x:?} vs {y:?}: a forecast saw the future");
        }
        // ...and the grades DO see what followed: the last day's forecasts were far too low in the storm.
        let q = |s: &Store| -> f64 {
            s.db.query_row("SELECT AVG(score) FROM predictions WHERE kind = 'vol24h' AND made_ts > ?1", [h - 86_400], |r| r.get(0)).unwrap()
        };
        assert!(q(&storm) > 5.0 * q(&calm), "{} vs {}", q(&storm), q(&calm));
        // Grading matches live grading exactly.
        let (v, outcome, score): (f64, f64, f64) = storm
            .db
            .query_row(
                "SELECT value, outcome, score FROM predictions WHERE made_ts = ?1 AND kind = 'vol24h' AND model = 'ewma'",
                [h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let all = storm.prices("coinbase", h, h + 86_400).unwrap();
        assert!((outcome - vol::realised(&all, h, h + 86_400).unwrap()).abs() < 1e-15);
        assert!((score - vol::qlike(v, outcome)).abs() < 1e-15);
        // A second run adds nothing.
        assert_eq!(forecasts(&storm, from, to, now).unwrap().hours, 0);
    }

    #[test]
    fn replayed_blend_equals_the_live_blend() {
        // The blend in the replay is weighted in memory; live, from the database. After 12 days of
        // replay (enough for graded history), both must give the same number at every hour.
        let (s, start) = store_with_prices(70, 0.006);
        let now = start + 70 * 86_400;
        let (from, to) = (start + 31 * 86_400, start + 43 * 86_400);
        forecasts(&s, from, to, now).unwrap();
        let mut checked = 0;
        for h in [to - 48 * 3600, to - 5 * 3600, to] {
            let rows: Vec<(String, f64)> = {
                let mut st = s.db.prepare("SELECT model, value FROM predictions WHERE made_ts = ?1 AND kind = 'vol24h'").unwrap();
                st.query_map([h], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
            };
            let stored = rows.iter().find(|r| r.0 == "blend").unwrap().1;
            let base: std::collections::BTreeMap<&'static str, f64> =
                MODELS.iter().filter(|m| **m != "blend").filter_map(|m| rows.iter().find(|r| r.0 == *m).map(|r| (*m, r.1))).collect();
            let live = vol::blend(&s, &base, h).unwrap().unwrap();
            assert!((stored - live).abs() < 1e-12, "hour {h}: replay {stored} vs live {live}");
            checked += 1;
        }
        // The weights were not all equal (graded history existed).
        let graded: i64 =
            s.db.query_row("SELECT COUNT(*) FROM predictions WHERE kind = 'vol24h' AND target_ts <= ?1", [to], |r| r.get(0)).unwrap();
        assert!(graded > 100 && checked == 3);
    }

    #[test]
    fn replays_a_range_and_labels_regimes() {
        let (s, start) = store_with_prices(70, 0.006);
        let now = start + 70 * 86_400;
        let sum = forecasts(&s, start + 35 * 86_400, start + 40 * 86_400, now).unwrap();
        assert_eq!(sum.hours, 5 * 24 + 1);
        let regimes: i64 =
            s.db.query_row("SELECT COUNT(*) FROM predictions WHERE regime = 'active' AND replay = 1 AND kind = 'vol24h'", [], |r| r.get(0))
                .unwrap();
        assert!(regimes > 0, "0.6%/sqrt(h) paths are 'active'");
        // Nothing is made for hours whose outcome is not complete yet.
        let late = forecasts(&s, now - 3600, now, now).unwrap();
        assert_eq!(late.hours, 0);
    }

    #[test]
    fn days_replay_every_fixed_width_and_model() {
        let (s, start) = store_with_prices(45, 0.005);
        let now = start + 45 * 86_400;
        let table = std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap();
        s.db.execute(
            "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('t', 0, 'width_evals', ?1, ?2)",
            params![start + 40 * 86_400, table.trim()],
        )
        .unwrap();
        forecasts(&s, start + 35 * 86_400, start + 44 * 86_400, now).unwrap();
        let sum = days(&s, start + 38 * 86_400, start + 43 * 86_400, now, crate::collect::bucket::OLD).unwrap();
        assert!(sum.days >= 4, "{sum:?}");
        let fixed: i64 =
            s.db.query_row("SELECT COUNT(DISTINCT strategy) FROM replay_day WHERE strategy LIKE 'fixed:%'", [], |r| r.get(0)).unwrap();
        assert_eq!(fixed, 15, "every width of the keeper's table");
        let models: i64 =
            s.db.query_row("SELECT COUNT(DISTINCT strategy) FROM replay_day WHERE strategy NOT LIKE 'fixed:%'", [], |r| r.get(0)).unwrap();
        assert!(models >= 4, "{models}");
        // Idempotent.
        assert_eq!(days(&s, start + 38 * 86_400, start + 43 * 86_400, now, crate::collect::bucket::OLD).unwrap().days, 0);
        let a: String = s.db.query_row("SELECT assumptions FROM replay_runs", [], |r| r.get(0)).unwrap();
        let a: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert!(a["pool_fee_pips"].as_f64().is_some() && a["vol_basis_pool_over_coinbase"].as_f64().is_some(), "{a}");
        // The brief's section reads it all back.
        let text = the_past(&s, now).unwrap().unwrap();
        assert!(text.contains("THE PAST, REPLAYED") && text.contains("Widths,") && text.contains("Worst days"), "{text}");
    }
}
