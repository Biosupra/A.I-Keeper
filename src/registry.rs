//! The prediction registry: the heart of "learning". Every forecast is written down with the time
//! it was made and the time it can be checked; when that time comes the A.I grades itself against
//! what actually happened. The scorecard is the honest record of whether it is getting better than
//! the keeper, and the `blend` model uses these grades to re-weight itself.
//!
//! Forecast kinds:
//! * `vol24h` / `vol1h`: realised volatility of ETH-USD over the next 24 h / 1 h. Graded with
//!   QLIKE (0 = perfect; lower is better).
//! * `exit24h` (key `h=<width>`): probability the pool price leaves a range of that width, centred
//!   on the current tick, within 24 h. Graded with the Brier score (0 = perfect; 0.25 = coin flip).
//! * shadow rows (table `shadow`): the keeper's width choice and the A.I's, both replayed.

use crate::model::{exit, shadow, vol};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};

pub struct MadeSummary {
    pub vol_models: usize,
    pub exit_rows: usize,
}

#[allow(clippy::too_many_arguments)]
fn insert(store: &Store, made: i64, target: i64, kind: &str, model: &str, key: &str, value: f64, inputs: &Value) -> Result<()> {
    store.db.execute(
        "INSERT INTO predictions (made_ts, target_ts, kind, model, key, value, inputs) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![made, target, kind, model, key, value, inputs.to_string()],
    )?;
    Ok(())
}

/// Latest pool tick at or before `t` (within 10 minutes).
pub fn tick_at(store: &Store, t: i64) -> Result<Option<f64>> {
    let path = store.pool_ticks(t - 600, t + 1)?;
    Ok(path.last().map(|(_, tick)| *tick))
}

/// Makes this hour's forecasts. Idempotent per hour (skips if already made this hour).
pub fn make_predictions(store: &Store, now: i64) -> Result<MadeSummary> {
    let hour = now / 3600 * 3600;
    let done = store.count("SELECT COUNT(*) FROM predictions WHERE made_ts >= ?1 AND made_ts < ?2", &[&hour, &(hour + 3600)])?;
    if done > 0 {
        return Ok(MadeSummary { vol_models: 0, exit_rows: 0 });
    }
    let f24 = vol::forecasts(store, now, 24)?;
    let f1 = vol::forecasts(store, now, 1)?;
    let inputs = json!({ "vol24h": f24, "vol1h": f1 });
    for (m, v) in &f24 {
        insert(store, now, now + 86_400, "vol24h", m, "", *v, &inputs)?;
    }
    for m in ["persist", "ewma", "keeper", "blend"] {
        if let Some(v) = f1.get(m) {
            insert(store, now, now + 3600, "vol1h", m, "", *v, &inputs)?;
        }
    }
    let mut exit_rows = 0;
    if let Some(start_tick) = tick_at(store, now)? {
        let coin = store.prices("coinbase", now - 91 * 86_400, now)?;
        // Exits are graded on the pool's own price, which measures calmer than Coinbase at
        // one-minute scale (see make_shadow). Whether that holds over 24 h is an empirical question,
        // so the keeper-style and blend forecasts are ALSO issued rescaled to the pool's basis
        // ("@pool"), and the scorecard settles which basis predicts exits better.
        let pool_basis = pool_basis(store, now, f24.get("keeper").copied())?;
        for &h in exit::WIDTHS {
            let a = exit::half_width_log(h);
            let key = format!("h={h}");
            let inp = json!({ "start_tick": start_tick, "a": a, "pool_basis": pool_basis });
            for (m, v) in &f24 {
                insert(store, now, now + 86_400, "exit24h", &format!("barrier:{m}"), &key, exit::exit_probability(a, *v, 24.0), &inp)?;
                exit_rows += 1;
                if let (Some(b), true) = (pool_basis, matches!(*m, "keeper" | "blend")) {
                    insert(
                        store,
                        now,
                        now + 86_400,
                        "exit24h",
                        &format!("barrier:{m}@pool"),
                        &key,
                        exit::exit_probability(a, v * b, 24.0),
                        &inp,
                    )?;
                    exit_rows += 1;
                }
            }
            if let Some(p) = exit::empirical(&coin, now, 90, h) {
                insert(store, now, now + 86_400, "exit24h", "empirical", &key, p, &inp)?;
                exit_rows += 1;
            }
        }
    }
    Ok(MadeSummary { vol_models: f24.len(), exit_rows })
}

/// Ratio of the pool's keeper-style volatility (max of 24 h and 48 h, from pool ticks) to the same
/// estimator on Coinbase. None until the A.I holds 48 h of pool prices.
pub fn pool_basis(store: &Store, now: i64, coinbase_keeper_style: Option<f64>) -> Result<Option<f64>> {
    let Some(c) = coinbase_keeper_style.filter(|c| *c > 0.0) else { return Ok(None) };
    let path: Vec<(i64, f64)> =
        store.pool_ticks(now - 2 * 86_400, now)?.into_iter().map(|(t, tick)| (t, crate::chainmath::tick_to_price(tick))).collect();
    let (Some(a), Some(b)) = (vol::realised(&path, now - 86_400, now), vol::realised(&path, now - 2 * 86_400, now)) else {
        return Ok(None);
    };
    Ok(Some(a.max(b) / c))
}

/// The volatility model with the best 24 h QLIKE over the last 30 days (at least 10 grades),
/// or `blend`, or the keeper's own estimator while nothing has been graded yet.
pub fn champion(store: &Store, now: i64) -> Result<String> {
    let best: Option<String> = store
        .db
        .query_row(
            "SELECT model FROM predictions WHERE kind = 'vol24h' AND score IS NOT NULL AND made_ts > ?1
             GROUP BY model HAVING COUNT(*) >= 10 ORDER BY AVG(score) ASC LIMIT 1",
            [now - 30 * 86_400],
            |r| r.get(0),
        )
        .ok();
    Ok(best.unwrap_or_else(|| "keeper".into()))
}

/// Shadow rows for keeper width tables that have none yet. The A.I's volatility is computed AS OF
/// the table's own time, so the comparison is fair even when the table arrives hours later.
///
/// Basis: the A.I's models read Coinbase ETH-USD, the keeper reads its own pool, and the two
/// measure differently (on 2026-09-29 the keeper's estimator read 0.43% while the same estimator on
/// Coinbase read 0.54%, at every sampling interval: a basis gap, not noise). So the A.I applies its
/// RELATIVE view to the keeper's own number: ai_vol = keeper_vol * (model / keeper-style), both on
/// Coinbase. With model = keeper-style this is exactly the keeper's volatility.
pub fn make_shadow(store: &Store, now: i64) -> Result<usize> {
    let mut st = store.db.prepare(
        "SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND ts > ?1
           AND ts / 3600 NOT IN (SELECT made_ts / 3600 FROM shadow) ORDER BY ts",
    )?;
    let rows: Vec<(i64, String)> =
        st.query_map([now - 14 * 86_400], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
    let mut made = 0;
    let mut last_hour = -1;
    for (ts, raw) in rows {
        if ts / 3600 == last_hour {
            continue; // one per hour
        }
        let Ok(v) = serde_json::from_str::<Value>(&raw) else { continue };
        let Some(table) = shadow::Table::from_keeper(&v) else { continue };
        let model = champion(store, ts)?;
        let f = vol::forecasts(store, ts, 24)?;
        let (Some(&mine), Some(&keeper_style)) = (f.get(model.as_str()).or_else(|| f.get("keeper")), f.get("keeper")) else { continue };
        let ratio = mine / keeper_style;
        let ai_vol = table.vol * ratio;
        let (ai_choice, scores) = table.rescore(ai_vol);
        for (m, v) in &f {
            let mv = table.vol * v / keeper_style;
            let (c, _) = table.rescore(mv);
            store.db.execute(
                "INSERT OR IGNORE INTO shadow_model (made_ts, model, vol, choice) VALUES (?1, ?2, ?3, ?4)",
                params![ts, m, mv, c],
            )?;
        }
        store.db.execute(
            "INSERT OR IGNORE INTO shadow (made_ts, start_tick, position_usd, keeper_choice, ai_choice, keeper_vol, ai_vol, ai_model, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![ts, table.tick, table.position_usd, table.choice, ai_choice, table.vol, ai_vol, model, json!({ "ai_scores": scores, "ratio_to_keeper_style": ratio, "forecasts": f, "table": table }).to_string()],
        )?;
        last_hour = ts / 3600;
        made += 1;
    }
    Ok(made)
}

/// Grades everything whose time has come. Returns how many were graded.
pub fn resolve(store: &Store, now: i64) -> Result<usize> {
    let mut graded = 0;
    // Forecasts.
    let open: Vec<(i64, i64, i64, String, String, f64, String)> = {
        let mut st = store.db.prepare(
            "SELECT id, made_ts, target_ts, kind, key, value, COALESCE(inputs, '{}') FROM predictions
             WHERE resolved_ts IS NULL AND target_ts <= ?1 ORDER BY target_ts LIMIT 5000",
        )?;
        let rows = st.query_map([now - 300], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (id, made, target, kind, key, value, inputs) in open {
        let outcome_score: Option<(f64, f64)> = match kind.as_str() {
            "vol24h" | "vol1h" => {
                let p = store.prices("coinbase", made, target)?;
                vol::realised(&p, made, target).map(|real| (real, vol::qlike(value, real)))
            }
            "exit24h" => {
                let inp: Value = serde_json::from_str(&inputs).unwrap_or_default();
                let h: i64 = key.trim_start_matches("h=").parse().unwrap_or(0);
                match inp["start_tick"].as_f64() {
                    Some(t0) => {
                        let path = store.pool_ticks(made, target)?;
                        let covered = path.len() as i64 * 60 >= (target - made) * 9 / 10;
                        covered.then(|| {
                            let half = h as f64 * exit::TICKS_PER_SPACING;
                            let out = path.iter().any(|(_, t)| (t - t0).abs() > half);
                            let o = if out { 1.0 } else { 0.0 };
                            (o, (value - o).powi(2))
                        })
                    }
                    None => None,
                }
            }
            _ => None,
        };
        match outcome_score {
            Some((o, s)) => {
                store
                    .db
                    .execute("UPDATE predictions SET outcome = ?1, score = ?2, resolved_ts = ?3 WHERE id = ?4", params![o, s, now, id])?;
                graded += 1;
            }
            // Data never arrived: close it out ungraded after two days rather than retry forever.
            None if now > target + 2 * 86_400 => {
                store.db.execute("UPDATE predictions SET resolved_ts = ?1 WHERE id = ?2", params![now, id])?;
            }
            None => {}
        }
    }
    // Shadow rows.
    let open: Vec<(i64, String)> = {
        let mut st = store.db.prepare("SELECT made_ts, detail FROM shadow WHERE resolved_ts IS NULL AND made_ts + 86400 <= ?1")?;
        let rows = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (made, detail) in open {
        let d: Value = serde_json::from_str(&detail).unwrap_or_default();
        let Ok(table) = serde_json::from_value::<shadow::Table>(d["table"].clone()) else { continue };
        let path = store.pool_ticks(made, made + 86_400)?;
        let (ai_choice, keeper_choice): (i64, i64) =
            store.db.query_row("SELECT ai_choice, keeper_choice FROM shadow WHERE made_ts = ?1", [made], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let all: Vec<(i64, f64)> =
            table.rows.iter().filter_map(|r| shadow::replay(&table, r.h as i64, &path, made, 86_400).map(|n| (r.h as i64, n))).collect();
        let get = |h: i64| all.iter().find(|(x, _)| *x == h).map(|(_, n)| *n);
        match (get(keeper_choice), get(ai_choice)) {
            (Some(k), Some(a)) => {
                let best = all.iter().max_by(|x, y| x.1.total_cmp(&y.1)).copied();
                store.db.execute(
                    "UPDATE shadow SET keeper_net_per_day = ?1, ai_net_per_day = ?2, best_hindsight = ?3, best_net = ?4, resolved_ts = ?5 WHERE made_ts = ?6",
                    params![k, a, best.map(|b| b.0), best.map(|b| b.1), now, made],
                )?;
                for (h, n) in &all {
                    store.db.execute("UPDATE shadow_model SET net_per_day = ?1 WHERE made_ts = ?2 AND choice = ?3", params![n, made, h])?;
                }
                graded += 1;
            }
            _ if now > made + 3 * 86_400 => {
                store.db.execute("UPDATE shadow SET resolved_ts = ?1 WHERE made_ts = ?2", params![now, made])?;
            }
            _ => {}
        }
    }
    Ok(graded)
}

pub struct ScoreLine {
    pub kind: String,
    pub model: String,
    pub n: i64,
    pub mean_score: f64,
}

pub struct ShadowScore {
    pub n: i64,
    pub keeper_mean: f64,
    pub ai_mean: f64,
    pub ai_better: i64,
    pub same_choice: i64,
    /// Mean of the best width in hindsight: the ceiling no forecaster can beat.
    pub hindsight_mean: f64,
    /// Distinct UTC days the graded decisions span. Hourly decisions share most of their 24 h
    /// grading windows, so this, not `n`, is the real sample size.
    pub days: i64,
    /// Per model: (model, graded, mean net/day of its own choice, times it beat the keeper).
    pub models: Vec<(String, i64, f64, i64)>,
}

pub fn scorecard(store: &Store, now: i64, days: i64) -> Result<(Vec<ScoreLine>, Option<ShadowScore>)> {
    let mut st = store.db.prepare(
        "SELECT kind, model, COUNT(*), AVG(score) FROM predictions WHERE score IS NOT NULL AND made_ts > ?1
         GROUP BY kind, model ORDER BY kind, AVG(score)",
    )?;
    let lines = st
        .query_map([now - days * 86_400], |r| Ok(ScoreLine { kind: r.get(0)?, model: r.get(1)?, n: r.get(2)?, mean_score: r.get(3)? }))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let sh: (i64, Option<f64>, Option<f64>, Option<i64>, Option<i64>) = store.db.query_row(
        "SELECT COUNT(*), AVG(keeper_net_per_day), AVG(ai_net_per_day),
                SUM(CASE WHEN ai_net_per_day > keeper_net_per_day THEN 1 ELSE 0 END),
                SUM(CASE WHEN ai_choice = keeper_choice THEN 1 ELSE 0 END)
         FROM shadow WHERE keeper_net_per_day IS NOT NULL AND made_ts > ?1",
        [now - days * 86_400],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let since = now - days * 86_400;
    let mut st = store.db.prepare(
        "SELECT m.model, COUNT(*), AVG(m.net_per_day), SUM(CASE WHEN m.net_per_day > s.keeper_net_per_day THEN 1 ELSE 0 END)
         FROM shadow_model m JOIN shadow s ON s.made_ts = m.made_ts
         WHERE m.net_per_day IS NOT NULL AND s.keeper_net_per_day IS NOT NULL AND m.made_ts > ?1
         GROUP BY m.model ORDER BY AVG(m.net_per_day) DESC",
    )?;
    let models =
        st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
    // Hindsight: the best width's own replay result, from the per-model table when a model picked it,
    // else from the detail JSON is not needed: best_hindsight's net is recomputed cheaply below.
    let hindsight_mean: f64 = store
        .db
        .query_row("SELECT AVG(best_net) FROM shadow WHERE best_net IS NOT NULL AND made_ts > ?1", [since], |r| r.get::<_, Option<f64>>(0))?
        .unwrap_or(0.0);
    let distinct_days: i64 = store.db.query_row(
        "SELECT COUNT(DISTINCT made_ts / 86400) FROM shadow WHERE keeper_net_per_day IS NOT NULL AND made_ts > ?1",
        [since],
        |r| r.get(0),
    )?;
    let shadow = (sh.0 > 0).then(|| ShadowScore {
        n: sh.0,
        keeper_mean: sh.1.unwrap_or(0.0),
        ai_mean: sh.2.unwrap_or(0.0),
        ai_better: sh.3.unwrap_or(0),
        same_choice: sh.4.unwrap_or(0),
        hindsight_mean,
        days: distinct_days,
        models,
    });
    Ok((lines, shadow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vol::tests::gbm;

    /// End to end on synthetic data: prices -> forecasts -> a day later -> grades.
    #[test]
    fn predict_then_grade() {
        let s = Store::memory().unwrap();
        let t0 = 1_790_000_000 / 86_400 * 86_400;
        let days = 60;
        let path = gbm(t0 - days * 86_400, (days as usize + 2) * 1440, 0.005, 42);
        for (t, p) in &path {
            s.put_price("coinbase", *t, *p, None, None, None).unwrap();
            s.put_price("pool", *t, *p, None, None, Some(crate::chainmath::price_to_tick(*p))).unwrap();
        }
        let m = make_predictions(&s, t0).unwrap();
        assert!(m.vol_models >= 3, "keeper, persist, ewma at least");
        assert!(m.exit_rows > 0);
        assert_eq!(make_predictions(&s, t0 + 60).unwrap().vol_models, 0, "once per hour");
        let n = resolve(&s, t0 + 86_400 + 600).unwrap();
        assert!(n > 0);
        let (lines, _) = scorecard(&s, t0 + 86_400 + 600, 30).unwrap();
        let keeper = lines.iter().find(|l| l.kind == "vol24h" && l.model == "keeper").unwrap();
        assert!(keeper.mean_score < 0.2, "true vol is constant, so the keeper estimator should be close: {}", keeper.mean_score);
        let brier: Vec<&ScoreLine> = lines.iter().filter(|l| l.kind == "exit24h").collect();
        assert!(!brier.is_empty() && brier.iter().all(|l| l.mean_score <= 1.0));
    }

    #[test]
    fn shadow_row_made_and_graded() {
        let s = Store::memory().unwrap();
        let raw = std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let ts = v["ts"].as_i64().unwrap();
        let tick = v["current_tick"].as_f64().unwrap();
        // Price history before the table (for the A.I's volatility) and a flat path after it.
        for (t, p) in gbm(ts - 3 * 86_400, 3 * 1440, 0.004, 5) {
            s.put_price("coinbase", t, p, None, None, None).unwrap();
        }
        for i in 0..1500 {
            s.put_price("pool", ts / 60 * 60 + i * 60, 2715.0, None, None, Some(tick)).unwrap();
        }
        s.db.execute("INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('o', 0, 'width_evals', ?1, ?2)", params![ts, raw.trim()])
            .unwrap();
        assert_eq!(make_shadow(&s, ts + 60).unwrap(), 1);
        assert_eq!(make_shadow(&s, ts + 120).unwrap(), 0, "not twice");
        assert_eq!(resolve(&s, ts + 86_400 + 60).unwrap(), 1);
        let (_, sh) = scorecard(&s, ts + 86_400 + 60, 30).unwrap();
        let sh = sh.unwrap();
        assert_eq!(sh.n, 1);
        assert!(sh.keeper_mean > 0.0 && sh.ai_mean > 0.0, "flat price: both earn");
    }
}
