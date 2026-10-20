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
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

pub struct MadeSummary {
    pub vol_models: usize,
    pub exit_rows: usize,
}

#[allow(clippy::too_many_arguments)]
fn insert(store: &Store, made: i64, target: i64, kind: &str, model: &str, key: &str, value: f64, inputs: &Value) -> Result<()> {
    store.db.execute(
        "INSERT INTO predictions (made_ts, target_ts, kind, model, key, value, inputs, regime) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![made, target, kind, model, key, value, inputs.to_string(), inputs["regime"].as_str()],
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
    let inputs = json!({ "vol24h": f24, "vol1h": f1, "regime": current_regime(store, now)? });
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

/// The market regime at `now` (calm / active / storm) by the last 24 h of Coinbase volatility.
pub fn current_regime(store: &Store, now: i64) -> Result<Option<&'static str>> {
    let prices = store.prices("coinbase", now - 86_400, now)?;
    Ok(vol::realised(&prices, now - 86_400, now).map(vol::regime))
}

/// (model, hours, mean loss).
pub type ModelScore = (String, i64, f64);

/// Each model's mean 24 h QLIKE over every hour of one regime the A.I holds (live and replayed),
/// PAIRED: only hours in which every model forecast count (HAR needs 90 days of history and DVOL
/// can be missing, so unpaired means would compare different hours). Best first.
pub fn regime_scores(store: &Store, regime: &str, now: i64) -> Result<Vec<ModelScore>> {
    let mut st = store.db.prepare_cached(
        "WITH hours AS (
           SELECT made_ts, COUNT(DISTINCT model) AS n FROM predictions
           WHERE kind = 'vol24h' AND regime = ?1 AND score IS NOT NULL AND made_ts > ?2 AND target_ts <= ?3
           GROUP BY made_ts
         ), paired AS (SELECT made_ts FROM hours WHERE n = (SELECT MAX(n) FROM hours))
         SELECT model, COUNT(*), AVG(score) FROM predictions
         WHERE kind = 'vol24h' AND score IS NOT NULL AND made_ts IN (SELECT made_ts FROM paired)
         GROUP BY model ORDER BY AVG(score) ASC",
    )?;
    let rows = st.query_map(params![regime, 0, now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The volatility model the A.I follows. First choice: the best 24 h QLIKE over all history held
/// among forecasts made in the SAME regime as now, paired over at least 50 hours; the replayed
/// years (`crate::replay`) supply these, so a model that fails in storms loses storms even
/// before a live storm. Otherwise the best over the last 30 days (at least 10 grades); otherwise
/// the keeper's own estimator. Only forecasts whose outcome was known at `now` count.
pub fn champion(store: &Store, now: i64) -> Result<String> {
    if let Some(r) = current_regime(store, now)? {
        if let Some((m, n, _)) = regime_scores(store, r, now)?.into_iter().next() {
            if n >= 50 {
                return Ok(m);
            }
        }
    }
    let best: Option<String> = store
        .db
        .query_row(
            "SELECT model FROM predictions WHERE kind = 'vol24h' AND score IS NOT NULL AND made_ts > ?1 AND target_ts <= ?2
             GROUP BY model HAVING COUNT(*) >= 10 ORDER BY AVG(score) ASC LIMIT 1",
            [now - 30 * 86_400, now],
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
///
/// Vaults: decisions are made from ONE keeper's tables, the vault the A.I follows (`vault`, the live one),
/// so each hour has one decision. All keepers run the same pool with the same width engine, so the
/// scorecards pool every vault's graded decisions; each is graded with its own vault's tables and policy.
pub fn make_shadow(store: &Store, now: i64, vault: &str) -> Result<usize> {
    let mut st = store.db.prepare(
        "SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND vault = ?2 AND ts > ?1
           AND ts / 3600 NOT IN (SELECT made_ts / 3600 FROM shadow WHERE vault = ?2) ORDER BY ts",
    )?;
    let rows: Vec<(i64, String)> =
        st.query_map(params![now - 14 * 86_400, vault], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
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
        // Every strategy picks its width with the keeper's own hysteresis rule, under the policy the
        // keeper was running at that moment.
        let policy = policy_at(store, ts, vault)?;
        let scores = table.rescore(ai_vol);
        let ai_choice = shadow::choose(&table, &scores, &policy);
        for (m, v) in &f {
            let mv = table.vol * v / keeper_style;
            let c = shadow::choose(&table, &table.rescore(mv), &policy);
            store.db.execute(
                "INSERT OR IGNORE INTO shadow_model (made_ts, model, vol, choice) VALUES (?1, ?2, ?3, ?4)",
                params![ts, m, mv, c],
            )?;
        }
        store.db.execute(
            "INSERT OR IGNORE INTO shadow (made_ts, start_tick, position_usd, keeper_choice, ai_choice, keeper_vol, ai_vol, ai_model, detail, vault)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![ts, table.tick, table.position_usd, table.choice, ai_choice, table.vol, ai_vol, model, json!({ "ai_scores": scores, "ratio_to_keeper_style": ratio, "forecasts": f, "table": table }).to_string(), vault],
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
    // Shadow rows: each is graded once the 24 h after it are fully covered by prices AND by the
    // keeper's hourly tables (the bucket is read every 6 h, so tables can arrive later than prices).
    let open: Vec<i64> = {
        let mut st = store.db.prepare("SELECT made_ts FROM shadow WHERE resolved_ts IS NULL AND made_ts + 86400 <= ?1 ORDER BY made_ts")?;
        let rows = st.query_map([now], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for made in open {
        match grade_shadow(store, made)? {
            Some(()) => {
                store.db.execute("UPDATE shadow SET resolved_ts = ?1 WHERE made_ts = ?2", params![now, made])?;
                graded += 1;
            }
            None if now > made + 3 * 86_400 => {
                store.db.execute("UPDATE shadow SET resolved_ts = ?1 WHERE made_ts = ?2", params![now, made])?;
            }
            None => {}
        }
    }
    Ok(graded)
}

/// Rewards that were never paid count as zero: from `end` (Aerodrome's end, `gauge::rewards_end_for`) every
/// hour's table pays no reward (its gross APR is 0), while the scores and choices each strategy acts on stay the
/// keeper's own, unchanged: the width rules are not touched, only what was really earned. The hour in force at
/// `end` is split there, so not a minute after it is counted. `end` None (an Aero vault) changes nothing.
pub fn without_phantom_rewards(mut hours: Vec<shadow::Hour>, end: Option<i64>) -> Vec<shadow::Hour> {
    let Some(end) = end else { return hours };
    if let Some(i) = hours.iter().rposition(|h| h.table.ts < end) {
        if hours.get(i + 1).map(|n| n.table.ts != end).unwrap_or(true) {
            let mut split = hours[i].clone();
            split.table.ts = end;
            hours.insert(i + 1, split);
        }
    }
    for h in hours.iter_mut().filter(|h| h.table.ts >= end) {
        for r in &mut h.table.rows {
            r.gross_apr = 0.0;
        }
    }
    hours
}

/// The gauge mode a graded window ending at `to` ran in: 'aero', 'aerodrome', or 'aerodrome-ended' when
/// Aerodrome's rewards ended before its end (they count as zero from then).
pub fn window_gauge_mode(end: Option<i64>, to: i64) -> &'static str {
    match end {
        None => "aero",
        Some(e) if e < to => "aerodrome-ended",
        Some(_) => "aerodrome",
    }
}

/// A vault's keeper's rebalance policy as its most recent start at or before `t` logged it.
pub fn policy_at(store: &Store, t: i64, vault: &str) -> Result<shadow::Policy> {
    let raw: Option<String> = store
        .db
        .query_row(
            "SELECT json FROM keeper_rows WHERE tbl = 'keeper_runs' AND vault = ?2 AND ts <= ?1 ORDER BY ts DESC LIMIT 1",
            params![t, vault],
            |r| r.get(0),
        )
        .ok();
    Ok(match raw.and_then(|r| serde_json::from_str::<Value>(&r).ok()) {
        Some(v) => shadow::Policy::from_keeper_run(&v),
        None => shadow::Policy::default(),
    })
}

/// Replays the 24 h after a shadow decision under the keeper's exact rules for the keeper's own
/// strategy, every model's strategy and every fixed width, and stores the results. None when the
/// window is not yet covered (prices < 90%, or fewer than 20 hourly tables, or none after hour 23).
pub fn grade_shadow(store: &Store, made: i64) -> Result<Option<()>> {
    let end = made + 86_400;
    // The decision is graded with the tables and policy of the vault whose keeper it shadowed.
    let vault: String = store.db.query_row("SELECT vault FROM shadow WHERE made_ts = ?1", [made], |r| r.get(0))?;
    // Every keeper table stored for the window (one per 10 minutes live), so each action is taken
    // with the table the keeper had at that moment.
    let tables: Vec<shadow::Table> = {
        let mut st = store
            .db
            .prepare("SELECT json FROM keeper_rows WHERE tbl = 'width_evals' AND vault = ?3 AND ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        let rows = st.query_map(params![made, end, vault], |r| r.get::<_, String>(0))?;
        let mut v: Vec<shadow::Table> = rows
            .filter_map(|r| r.ok())
            .filter_map(|d| serde_json::from_str::<Value>(&d).ok())
            .filter_map(|d| shadow::Table::from_keeper(&d))
            .collect();
        v.dedup_by_key(|t| t.ts);
        v
    };
    let hours_covered = tables.iter().map(|t| (t.ts - made) / 3600).collect::<std::collections::BTreeSet<_>>().len();
    if hours_covered < 20 || tables.last().map(|t| t.ts < made + 23 * 3600).unwrap_or(true) || tables[0].ts > made + 600 {
        return Ok(None);
    }
    let path = store.pool_path(made - 600, end)?;
    let policy = policy_at(store, made, &vault)?;
    // Aerodrome's rewards after they ended were never paid (its gauge still reports a rate): counted as zero.
    let rewards_end = crate::collect::gauge::rewards_end_for(store, &vault);
    let gauge_mode = window_gauge_mode(rewards_end, end);
    let keeper_hours = without_phantom_rewards(shadow::hours_for(&tables, None, &policy), rewards_end);
    let Some(keeper) = shadow::replay(&policy, &keeper_hours, &path, made, 86_400, None) else { return Ok(None) };

    // The NEW rules (keeper WIDTH_MODE=regime) on the same window, in dollars, next to what the table
    // rules did or would have done (`keeper` above). Scored whichever of the two the keeper runs.
    let run: Option<Value> = store
        .db
        .query_row(
            "SELECT json FROM keeper_rows WHERE tbl = 'keeper_runs' AND vault = ?2 AND ts <= ?1 ORDER BY ts DESC LIMIT 1",
            params![made, vault],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let rp = crate::model::regime::Params::from_keeper_run(run.as_ref());
    let live_mode = run.as_ref().and_then(|r| r["policy"]["width_mode"].as_str()).unwrap_or("table").to_string();
    let long_path = store.pool_path(made - rp.long_window_secs - 3600, end)?;
    let vols = crate::model::regime::VolPath::new(&long_path);
    if let Some(new) = crate::model::regime::replay(&rp, &policy, &keeper_hours, &path, &vols, made, 86_400) {
        store.db.execute(
            "INSERT OR REPLACE INTO rules_compare (made_ts, vault, live_mode, old_end, old_rewards, old_moves, old_in_range, new_end, new_rewards, new_moves, new_in_range, gauge_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                made,
                vault,
                live_mode,
                keeper.usd_end,
                keeper.usd_rewards,
                keeper.rebalances + keeper.recuts,
                keeper.in_range_fraction,
                new.usd_end,
                new.usd_rewards,
                new.rebalances + new.recuts,
                new.in_range_fraction,
                gauge_mode
            ],
        )?;
    }

    // Each model: its own volatility at each hour (from that hour's shadow_model row; an hour it
    // has no row for keeps its previous one).
    let models: Vec<String> = {
        let mut st = store.db.prepare("SELECT model FROM shadow_model WHERE made_ts = ?1")?;
        let rows = st.query_map([made], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut model_results: Vec<(String, shadow::Replay)> = Vec::new();
    for m in &models {
        // The model's volatility VIEW is refreshed hourly (its shadow_model rows) and expressed as a
        // ratio to the keeper's own volatility, so it is applied to every 10-minute table in between.
        let ratios: Vec<(i64, f64)> = {
            let mut st = store.db.prepare(
                "SELECT m.made_ts, m.vol / s.keeper_vol FROM shadow_model m JOIN shadow s ON s.made_ts = m.made_ts
                 WHERE m.model = ?1 AND m.made_ts >= ?2 AND m.made_ts < ?3 AND s.keeper_vol > 0 ORDER BY m.made_ts",
            )?;
            let rows = st.query_map(params![m, made - 3600, end], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let vols: Vec<f64> = tables
            .iter()
            .map(|t| {
                let i = ratios.partition_point(|r| r.0 <= t.ts);
                let ratio = if i > 0 { ratios[i - 1].1 } else { 1.0 };
                t.vol * ratio
            })
            .collect();
        let hours = without_phantom_rewards(shadow::hours_for(&tables, Some(&vols), &policy), rewards_end);
        if let Some(r) = shadow::replay(&policy, &hours, &path, made, 86_400, None) {
            store.db.execute(
                "UPDATE shadow_model SET net_per_day = ?1, replay = ?2 WHERE made_ts = ?3 AND model = ?4",
                params![r.net_per_day, serde_json::to_string(&r)?, made, m],
            )?;
            model_results.push((m.clone(), r));
        }
    }
    // Fixed widths: the ceiling a perfect width-picker could have reached.
    let fixed: Vec<(i64, f64)> = tables[0]
        .rows
        .iter()
        .filter_map(|r| shadow::replay(&policy, &keeper_hours, &path, made, 86_400, Some(r.h as i64)).map(|x| (r.h as i64, x.net_per_day)))
        .collect();
    let best = fixed.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1));
    let ai_model: String = store.db.query_row("SELECT ai_model FROM shadow WHERE made_ts = ?1", [made], |r| r.get(0))?;
    let ai = model_results.iter().find(|(m, _)| *m == ai_model).map(|(_, r)| r.net_per_day).unwrap_or(keeper.net_per_day);
    store.db.execute(
        "UPDATE shadow SET keeper_net_per_day = ?1, ai_net_per_day = ?2, best_hindsight = ?3, best_net = ?4, replay = ?5, gauge_mode = ?7 WHERE made_ts = ?6",
        params![
            keeper.net_per_day,
            ai,
            best.map(|b| b.0),
            best.map(|b| b.1),
            json!({ "keeper": keeper, "fixed": fixed, "policy": policy, "gauge_mode": gauge_mode, "rewards_end": rewards_end }).to_string(),
            made,
            gauge_mode
        ],
    )?;
    Ok(Some(()))
}

/// Result of checking the A.I's copy of the keeper's rules against what the live keeper did.
///
/// Rebalances are scored separately for the keeper's CURRENT rules (every run since the first one
/// with the same major.minor version as the latest) and for older versions. The A.I copies the
/// current rules only, so older rebalances, made under different rules, are expected to differ
/// and are reported for reference, not as misses.
#[derive(Debug, Default)]
pub struct Fidelity {
    /// Width tables whose logged choice the hysteresis rule reproduces.
    pub choices: (i64, i64),
    /// Real rebalances under the current rules whose time the replay reproduces within 10 minutes.
    pub rebalances: (i64, i64),
    /// Logged "TWAP outside for X" clocks the A.I measures within 10 minutes.
    pub clocks: (i64, i64),
    /// Real rebalances under the current rules whose new width the rule reproduces for the position's real size.
    pub widths: (i64, i64),
    /// The same two checks for rebalances made by older keeper versions (different rules).
    pub older_rebalances: (i64, i64),
    pub older_widths: (i64, i64),
    /// The keeper version whose rules are current (major.minor, e.g. "0.8"), and since when.
    pub rules_version: Option<String>,
    pub rules_since: Option<i64>,
    pub notes: Vec<String>,
}

/// major.minor of a keeper version string ("0.8.1" -> "0.8").
fn rules_of(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// The keeper's current rules version and the time of the first run under it (after the last run
/// of any other version). Runs that predate version logging count as other versions.
pub fn current_rules(store: &Store, vault: &str) -> Result<Option<(String, i64)>> {
    let runs: Vec<(i64, Option<String>)> = {
        let mut st = store.db.prepare(
            "SELECT ts, json_extract(json, '$.keeper_version') FROM keeper_rows WHERE tbl = 'keeper_runs' AND vault = ?1 ORDER BY ts",
        )?;
        let r = st.query_map([vault], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let Some(latest) = runs.iter().rev().find_map(|(_, v)| v.clone()) else { return Ok(None) };
    let want = rules_of(&latest);
    let mut since: Option<i64> = None;
    for (ts, v) in &runs {
        match v.as_deref().map(rules_of) {
            Some(r) if r == want => {
                if since.is_none() {
                    since = Some(*ts);
                }
            }
            _ => since = None,
        }
    }
    Ok(since.map(|s| (want, s)))
}

fn parse_dur(s: &str) -> Option<i64> {
    // keeper fmt_dur: "5h03m", "42m52s", "40s"
    let s = s.trim();
    if let Some((h, rest)) = s.split_once('h') {
        return Some(h.parse::<i64>().ok()? * 3600 + rest.trim_end_matches('m').parse::<i64>().ok()? * 60);
    }
    if let Some((m, rest)) = s.split_once('m') {
        return Some(m.parse::<i64>().ok()? * 60 + rest.trim_end_matches('s').parse::<i64>().ok()?);
    }
    s.trim_end_matches('s').parse().ok()
}

/// TWAP (mean of minute ticks over the policy window) at `t` from a minute path.
fn twap_at(path: &[(i64, f64, Option<f64>)], t: i64, secs: i64) -> Option<f64> {
    let w: Vec<f64> = path.iter().filter(|x| x.0 > t - secs && x.0 <= t).map(|x| x.1).collect();
    (w.len() >= 2).then(|| w.iter().sum::<f64>() / w.len() as f64)
}

/// Checks the rules engine against one vault's keeper's own history rows in the A.I's database.
pub fn fidelity(store: &Store, vault: &str) -> Result<Fidelity> {
    let mut f = Fidelity::default();
    // 1. Width choice: every logged width table.
    let rows: Vec<(i64, String)> = {
        let mut st = store.db.prepare("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND vault = ?1 ORDER BY ts")?;
        let r = st.query_map([vault], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let mut tables: Vec<shadow::Table> = Vec::new();
    for (ts, raw) in &rows {
        let Some(t) = serde_json::from_str::<Value>(raw).ok().and_then(|v| shadow::Table::from_keeper(&v)) else { continue };
        let p = policy_at(store, *ts, vault)?;
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, r.net)).collect();
        let c = shadow::choose(&t, &nets, &p);
        f.choices.1 += 1;
        if c == t.choice {
            f.choices.0 += 1;
        } else if f.notes.len() < 10 {
            f.notes.push(format!("width table {ts}: keeper chose {} but the rule gives {c}", t.choice));
        }
        tables.push(t);
    }
    // 2. Out-of-range clocks: every logged "TWAP outside for X".
    let decisions: Vec<Value> = {
        let mut st = store
            .db
            .prepare("SELECT json FROM keeper_rows WHERE tbl = 'keeper_decisions' AND vault = ?1 AND json LIKE '%TWAP outside for%'")?;
        let r = st.query_map([vault], |r| r.get::<_, String>(0))?;
        r.filter_map(|x| x.ok()).filter_map(|x| serde_json::from_str(&x).ok()).collect()
    };
    let mut seen = std::collections::HashSet::new();
    for d in decisions {
        let (Some(ts), Some(lo), Some(hi), Some(reason)) =
            (d["ts"].as_i64(), d["tick_lower"].as_f64(), d["tick_upper"].as_f64(), d["reason"].as_str())
        else {
            continue;
        };
        if !seen.insert((ts, lo as i64, hi as i64)) {
            continue; // the same decision logged for several positions with one range
        }
        let Some(logged) = reason.strip_prefix("TWAP outside for ").and_then(|r| r.split(" of ").next()).and_then(parse_dur) else {
            continue;
        };
        let p = policy_at(store, ts, vault)?;
        let path = store.pool_path(ts - logged - 2 * 3600, ts + 60)?;
        // Walk back from ts while the TWAP stays outside the range.
        let mut t = ts / 60 * 60;
        let mut ours = 0;
        while let Some(tw) = twap_at(&path, t, p.twap_secs) {
            if tw >= lo && tw < hi {
                break;
            }
            ours = ts - t;
            t -= 60;
            if ours > logged + 3 * 3600 {
                break;
            }
        }
        f.clocks.1 += 1;
        if (ours - logged).abs() <= 600 {
            f.clocks.0 += 1;
        } else if f.notes.len() < 20 {
            f.notes.push(format!("clock at {ts} range [{lo}, {hi}): keeper said {logged}s outside, A.I measures {ours}s"));
        }
    }
    // 3. Real rebalances: replay from the position's real range and compare the time.
    let events: Vec<Value> = {
        let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'position_events' AND vault = ?1 ORDER BY ts")?;
        let r = st.query_map([vault], |r| r.get::<_, String>(0))?;
        r.filter_map(|x| x.ok()).filter_map(|x| serde_json::from_str::<Value>(&x).ok()).filter(|v| v["kind"] == "rebalance").collect()
    };
    let starts: Vec<i64> = {
        let mut st = store.db.prepare("SELECT ts FROM keeper_rows WHERE tbl = 'keeper_runs' AND vault = ?1 ORDER BY ts")?;
        let r = st.query_map([vault], |r| r.get(0))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let rules = current_rules(store, vault)?;
    f.rules_version = rules.as_ref().map(|r| r.0.clone());
    f.rules_since = rules.as_ref().map(|r| r.1);
    let mut done = std::collections::HashSet::new();
    for e in events {
        let m = &e["meta"];
        let (Some(ts), Some(lo), Some(hi)) = (e["ts"].as_i64(), m["old_tick_lower"].as_f64(), m["old_tick_upper"].as_f64()) else {
            continue;
        };
        if !done.insert((ts / 60, lo as i64, hi as i64)) {
            continue; // several positions with the same range rebalanced in one heartbeat
        }
        let p = policy_at(store, ts, vault)?;
        let path = store.pool_path(ts - 40 * 3600, ts + 3 * 3600)?;
        // Start where the TWAP was last inside the range (or at the keeper's last restart, which
        // resets its clocks), so the replay's clock starts where the keeper's did.
        let mut t_in = ts / 60 * 60;
        while let Some(tw) = twap_at(&path, t_in, p.twap_secs) {
            if tw >= lo && tw < hi {
                break;
            }
            t_in -= 60;
            if ts - t_in > 36 * 3600 {
                break;
            }
        }
        let restart = starts.iter().copied().filter(|s| *s > t_in && *s < ts).max();
        let start = restart.unwrap_or(t_in);
        let hours: Vec<shadow::Hour> = {
            let near: Vec<shadow::Table> = tables.iter().filter(|t| t.ts <= ts + 3600).cloned().collect();
            let from = near.iter().rposition(|t| t.ts <= start).unwrap_or(0);
            shadow::hours_for(&near[from..], None, &p)
        };
        if hours.is_empty() {
            continue;
        }
        let init = shadow::Start { lower: lo, upper: hi, mint: (lo + hi) / 2.0, last_rebalance: None, deposit_known: None };
        let Some(r) = shadow::replay_from(&p, &hours, &path, start, ts - start + 3 * 3600, None, Some(init)) else {
            f.notes.push(format!("rebalance at {ts}: not enough price data to replay"));
            continue;
        };
        let current = f.rules_since.map(|since| ts >= since).unwrap_or(true);
        let era = if current { "" } else { " [older keeper version]" };
        if current {
            f.rebalances.1 += 1;
        } else {
            f.older_rebalances.1 += 1;
        }
        match r.actions.first() {
            Some((t, kind, to)) if (t - ts).abs() <= 600 => {
                if current {
                    f.rebalances.0 += 1;
                } else {
                    f.older_rebalances.0 += 1;
                }
                // The width for THIS position's size (the keeper re-scores at its real value).
                let at = hours.iter().rposition(|h| h.table.ts <= *t).unwrap_or(0);
                let size = m["position_usd"].as_f64().unwrap_or(0.0);
                let sized = hours[at].table.nets_for_size(&hours[at].nets, size);
                let sized_choice = shadow::choose(&hours[at].table, &sized, &p);
                let opened = m["half_spacings"].as_i64().unwrap_or(-1);
                let w = if current { &mut f.widths } else { &mut f.older_widths };
                w.1 += 1;
                if sized_choice == opened {
                    w.0 += 1;
                }
                f.notes.push(format!(
                    "rebalance at {ts}{era}: replay acted at {t} ({:+} s, {kind}); width for a ${size:.2} position {sized_choice} vs keeper {opened} (at $1,000: {to})",
                    t - ts
                ));
            }
            Some((t, kind, _)) => f.notes.push(format!("rebalance at {ts}{era}: replay {kind} at {t} ({:+} min)", (t - ts) / 60)),
            None => f.notes.push(format!("rebalance at {ts}{era}: replay took no action")),
        }
    }
    Ok(f)
}

/// The model whose OWN width choices earned the most in shadow mode over `days` (at least
/// `min_n` graded decisions): (model, decisions, mean net per day). Not the one the A.I follows:
/// that is `champion`, chosen by forecast accuracy.
pub fn profit_champion(store: &Store, now: i64, days: i64, min_n: i64) -> Result<Option<(String, i64, f64)>> {
    Ok(store
        .db
        .query_row(
            "SELECT model, COUNT(*), AVG(net_per_day) FROM shadow_model WHERE net_per_day IS NOT NULL AND made_ts > ?1
             GROUP BY model HAVING COUNT(*) >= ?2 ORDER BY AVG(net_per_day) DESC LIMIT 1",
            params![now - days * 86_400, min_n],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
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
        "SELECT kind, model, COUNT(*), AVG(score) FROM predictions WHERE score IS NOT NULL AND made_ts > ?1 AND replay IS NULL
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
        let tick = v["current_tick"].as_i64().unwrap();
        // Price history before the table (for the A.I's volatility).
        for (t, p) in gbm(ts - 3 * 86_400, 4 * 1440, 0.004, 5) {
            s.put_price("coinbase", t, p, None, None, None).unwrap();
        }
        // A flat pool path through the next day, with the pool fee, as the collector writes it.
        for i in -20..1500 {
            let t = ts / 60 * 60 + i * 60;
            s.db.execute(
                "INSERT INTO pool_obs (ts, block, tick, sqrt_price, liquidity, fee_pips, base_fee_gwei, weth_usd) VALUES (?1, 0, ?2, '0', '0', 400, 0.005, 2715.0)",
                params![t, tick],
            )
            .unwrap();
        }
        // The keeper's hourly width tables for the whole window (the same table each hour).
        for hour in 0..25 {
            let mut row = v.clone();
            row["ts"] = json!(ts + hour * 3600);
            s.db.execute(
                "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES (?1, 0, 'width_evals', ?2, ?3)",
                params![format!("o{hour}"), ts + hour * 3600, row.to_string()],
            )
            .unwrap();
        }
        assert_eq!(make_shadow(&s, ts + 25 * 3600, crate::collect::bucket::OLD).unwrap(), 25);
        assert_eq!(make_shadow(&s, ts + 25 * 3600, crate::collect::bucket::OLD).unwrap(), 0, "not twice");
        // Not gradable before 24 h have passed; gradable after.
        assert_eq!(resolve(&s, ts + 3600).unwrap(), 0);
        let graded = resolve(&s, ts + 86_400 + 60).unwrap();
        assert!(graded >= 1, "the first decision's window is covered");
        let (_, sh) = scorecard(&s, ts + 86_400 + 60, 30).unwrap();
        let sh = sh.unwrap();
        assert!(sh.keeper_mean > 0.0 && sh.ai_mean > 0.0, "flat price: every strategy earns");
        assert!(sh.hindsight_mean >= sh.keeper_mean - 1e-12, "the fixed-width ceiling is at least the keeper's result on a flat day");
        let models: i64 = s.count("SELECT COUNT(*) FROM shadow_model WHERE net_per_day IS NOT NULL", &[]).unwrap();
        assert!(models >= 3, "every model graded: {models}");
        let rep: String = s.db.query_row("SELECT replay FROM shadow WHERE resolved_ts IS NOT NULL LIMIT 1", [], |r| r.get(0)).unwrap();
        let rep: Value = serde_json::from_str(&rep).unwrap();
        assert_eq!(rep["keeper"]["rebalances"], 0, "a flat day needs no rebalance");
        assert_eq!(rep["policy"]["confirmation_secs"], 21600, "keeper default policy when no keeper_runs row");
    }

    /// The shadow and rules-compare grading of the first decision in a flat day starting at `ts`, with the keeper's
    /// hourly table (`vault` 'old', or the followed vault on Aero when `on_aero`): (keeper rewards, new-rules
    /// rewards, gauge mode, keeper rebalances, model choices).
    fn graded_around(ts: i64, on_aero: bool) -> (f64, f64, String, i64, Vec<(String, i64)>) {
        let s = Store::memory().unwrap();
        let vault = crate::collect::bucket::OLD;
        if on_aero {
            s.meta_set("gauge_mode", "aero").unwrap();
            s.meta_set("gauge_mode_vault", vault).unwrap();
        }
        let raw = std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap();
        let mut v: Value = serde_json::from_str(&raw).unwrap();
        v["ts"] = json!(ts);
        let tick = v["current_tick"].as_i64().unwrap();
        for (t, p) in gbm(ts - 3 * 86_400, 4 * 1440, 0.004, 5) {
            s.put_price("coinbase", t, p, None, None, None).unwrap();
        }
        for i in -4400..1500 {
            let t = ts / 60 * 60 + i * 60;
            s.db.execute(
                "INSERT INTO pool_obs (ts, block, tick, sqrt_price, liquidity, fee_pips, base_fee_gwei, weth_usd) VALUES (?1, 0, ?2, '0', '0', 400, 0.005, 2715.0)",
                params![t, tick],
            )
            .unwrap();
        }
        for hour in 0..25 {
            let mut row = v.clone();
            row["ts"] = json!(ts + hour * 3600);
            s.db.execute(
                "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES (?1, 0, 'width_evals', ?2, ?3)",
                params![format!("o{hour}"), ts + hour * 3600, row.to_string()],
            )
            .unwrap();
        }
        assert_eq!(make_shadow(&s, ts + 25 * 3600, vault).unwrap(), 25);
        grade_shadow(&s, ts).unwrap().expect("the window is covered");
        let (old, new, mode): (f64, f64, String) =
            s.db.query_row("SELECT old_rewards, new_rewards, gauge_mode FROM rules_compare WHERE made_ts = ?1", [ts], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        let shadow_mode: String = s.db.query_row("SELECT gauge_mode FROM shadow WHERE made_ts = ?1", [ts], |r| r.get(0)).unwrap();
        assert_eq!(mode, shadow_mode);
        let rep: String = s.db.query_row("SELECT replay FROM shadow WHERE made_ts = ?1", [ts], |r| r.get(0)).unwrap();
        let rep: Value = serde_json::from_str(&rep).unwrap();
        assert_eq!(rep["gauge_mode"], json!(mode));
        let choices: Vec<(String, i64)> = {
            let mut st = s.db.prepare("SELECT model, choice FROM shadow_model WHERE made_ts = ?1 ORDER BY model").unwrap();
            st.query_map([ts], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        (old, new, mode, rep["keeper"]["rebalances"].as_i64().unwrap(), choices)
    }

    #[test]
    fn comparisons_after_the_launch_count_no_aerodrome_rewards_and_keep_every_decision() {
        let launch = crate::aero::LAUNCH_TS;
        // A day well before the launch: rewards as the keeper's table pays them.
        let (old_before, new_before, mode, reb_before, ch_before) = graded_around(launch - 3 * 86_400, false);
        assert!(old_before > 0.0 && new_before > 0.0, "{old_before} {new_before}");
        assert_eq!(mode, "aerodrome");
        // A day that starts 12 h before the launch: only the first 12 hours pay.
        let (old_mid, new_mid, mode, reb_mid, ch_mid) = graded_around(launch - 12 * 3600, false);
        assert_eq!(mode, "aerodrome-ended");
        assert!((old_mid / old_before - 0.5).abs() < 0.01, "{old_mid} vs {old_before}");
        assert!((new_mid / new_before - 0.5).abs() < 0.01, "{new_mid} vs {new_before}");
        // A day after the launch: zero Aerodrome rewards in both rule sets, whatever the gauge still reports.
        let (old_after, new_after, mode, reb_after, ch_after) = graded_around(launch + 3600, false);
        assert_eq!((old_after, new_after, mode.as_str()), (0.0, 0.0, "aerodrome-ended"));
        // The decisions are the keeper's own in every case: the width rules are not changed.
        assert_eq!((reb_before, reb_mid, reb_after), (0, 0, 0));
        assert_eq!(ch_before, ch_mid);
        assert_eq!(ch_before, ch_after);
        // The vault the A.I follows on Aero: its keeper's tables carry Aero's real rate, nothing is zeroed.
        let (old_aero, new_aero, mode, _, _) = graded_around(launch + 3600, true);
        assert_eq!(mode, "aero");
        assert!((old_aero - old_before).abs() < 1e-12 && (new_aero - new_before).abs() < 1e-12);
    }

    #[test]
    fn phantom_rewards_are_cut_at_the_end_and_nothing_else_changes() {
        let raw = std::fs::read_to_string("tests/fixtures/width_evals.json").unwrap();
        let t = shadow::Table::from_keeper(&serde_json::from_str::<Value>(&raw).unwrap()).unwrap();
        let p = shadow::Policy::default();
        let tables: Vec<shadow::Table> = (0..3).map(|i| shadow::Table { ts: 1_000 + i * 3_600, ..t.clone() }).collect();
        let hours = shadow::hours_for(&tables, None, &p);
        assert_eq!(without_phantom_rewards(hours.clone(), None).len(), 3, "no end: unchanged");
        let cut = without_phantom_rewards(hours.clone(), Some(5_000));
        assert_eq!(cut.iter().map(|h| h.table.ts).collect::<Vec<_>>(), vec![1_000, 4_600, 5_000, 8_200], "split at the end");
        for h in &cut {
            let paid = h.table.rows.iter().all(|r| r.gross_apr > 0.0);
            assert_eq!(paid, h.table.ts < 5_000, "rewards only before the end");
            let orig = hours.iter().rev().find(|o| o.table.ts <= h.table.ts).unwrap();
            assert_eq!((&h.nets, h.choice), (&orig.nets, orig.choice), "scores and choices are the keeper's own");
            assert_eq!(
                h.table.rows.iter().map(|r| r.rebalances).collect::<Vec<_>>(),
                orig.table.rows.iter().map(|r| r.rebalances).collect::<Vec<_>>()
            );
        }
        // An end exactly on a table, and an end before every table.
        assert_eq!(without_phantom_rewards(hours.clone(), Some(4_600)).len(), 3);
        assert!(without_phantom_rewards(hours, Some(0)).iter().all(|h| h.table.rows.iter().all(|r| r.gross_apr == 0.0)));
        assert_eq!(window_gauge_mode(None, 10), "aero");
        assert_eq!(window_gauge_mode(Some(10), 10), "aerodrome");
        assert_eq!(window_gauge_mode(Some(9), 10), "aerodrome-ended");
    }

    #[test]
    fn policy_follows_keeper_runs() {
        let s = Store::memory().unwrap();
        s.db.execute(
            "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('r', 0, 'keeper_runs', 100, ?1)",
            [json!({"policy": {"confirmation_secs": 10800}}).to_string()],
        )
        .unwrap();
        assert_eq!(policy_at(&s, 50, crate::collect::bucket::OLD).unwrap().confirmation_secs, 21_600, "before the start: defaults");
        assert_eq!(policy_at(&s, 150, crate::collect::bucket::OLD).unwrap().confirmation_secs, 10_800, "after: the logged policy");
    }

    #[test]
    fn current_rules_follow_the_latest_version_and_rollbacks() {
        let s = Store::memory().unwrap();
        let run = |ts: i64, v: Option<&str>| {
            let json = match v {
                Some(v) => format!("{{\"ts\": {ts}, \"keeper_version\": \"{v}\"}}"),
                None => format!("{{\"ts\": {ts}}}"),
            };
            s.db.execute(
                "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES (?1, 0, 'keeper_runs', ?2, ?3)",
                params![format!("r{ts}"), ts, json],
            )
            .unwrap();
        };
        assert_eq!(current_rules(&s, crate::collect::bucket::OLD).unwrap(), None, "no runs, no rules");
        run(100, None); // before versions were logged
        run(200, Some("0.7.0"));
        run(300, Some("0.8.0"));
        run(400, Some("0.8.1")); // a patch release keeps the rules
        assert_eq!(current_rules(&s, crate::collect::bucket::OLD).unwrap(), Some(("0.8".into(), 300)));
        run(500, Some("0.7.0")); // rolled back
        run(600, Some("0.8.1")); // and forward again: the rules count from here
        assert_eq!(current_rules(&s, crate::collect::bucket::OLD).unwrap(), Some(("0.8".into(), 600)));
    }

    #[test]
    fn profit_champion_needs_enough_graded_decisions() {
        let s = Store::memory().unwrap();
        let now = 2_000_000;
        for i in 0..12 {
            s.db.execute(
                "INSERT INTO shadow_model (made_ts, model, vol, choice, net_per_day) VALUES (?1, 'ewma', 0.004, 8, 0.002)",
                [now - 1000 - i],
            )
            .unwrap();
            s.db.execute(
                "INSERT INTO shadow_model (made_ts, model, vol, choice, net_per_day) VALUES (?1, 'keeper', 0.005, 16, 0.0004)",
                [now - 1000 - i],
            )
            .unwrap();
        }
        // A model with a better mean but only 3 decisions does not count.
        for i in 0..3 {
            s.db.execute(
                "INSERT INTO shadow_model (made_ts, model, vol, choice, net_per_day) VALUES (?1, 'lucky', 0.003, 4, 0.05)",
                [now - 1000 - i],
            )
            .unwrap();
        }
        let (m, n, mean) = profit_champion(&s, now, 30, 10).unwrap().unwrap();
        assert_eq!((m.as_str(), n), ("ewma", 12));
        assert!((mean - 0.002).abs() < 1e-12);
        assert_eq!(profit_champion(&s, now, 30, 20).unwrap(), None);
    }

    #[test]
    fn regime_scores_compare_the_same_hours() {
        // dvol forecast all 100 hours: easy ones (loss 0.5) then hard ones (2.0). HAR only exists
        // for the hard half (loss 1.5). Unpaired, dvol's easy hours would make it look better
        // (1.25 vs 1.5); paired on the hours both forecast, HAR is better (1.5 vs 2.0).
        let s = Store::memory().unwrap();
        let ins = |t: i64, m: &str, score: f64| {
            s.db.execute(
                "INSERT INTO predictions (made_ts, target_ts, kind, model, key, value, score, regime) VALUES (?1, ?2, 'vol24h', ?3, '', 0.005, ?4, 'calm')",
                params![t, t + 86_400, m, score],
            )
            .unwrap();
        };
        let t0 = 1_700_000_000;
        for i in 0..100 {
            let t = t0 + i * 3600;
            ins(t, "dvol", if i < 50 { 0.5 } else { 2.0 });
            if i >= 50 {
                ins(t, "har", 1.5);
            }
        }
        let now = t0 + 200 * 86_400;
        let r = regime_scores(&s, "calm", now).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].0.as_str(), r[0].1), ("har", 50));
        assert!((r[0].2 - 1.5).abs() < 1e-12 && (r[1].2 - 2.0).abs() < 1e-12, "{r:?}");
        assert!(regime_scores(&s, "storm", now).unwrap().is_empty());
        // Forecasts whose outcome was not yet known at `now` do not count.
        assert!(regime_scores(&s, "calm", t0).unwrap().is_empty());
    }

    #[test]
    fn migrations_add_missing_columns() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE pool_obs (ts INTEGER PRIMARY KEY); CREATE TABLE shadow (made_ts INTEGER PRIMARY KEY); CREATE TABLE shadow_model (made_ts INTEGER, model TEXT); CREATE TABLE price_1m (source TEXT, ts INTEGER, close REAL); CREATE TABLE predictions (id INTEGER PRIMARY KEY, kind TEXT, model TEXT, made_ts INTEGER);").unwrap();
        crate::store::migrate(&db).unwrap();
        crate::store::migrate(&db).unwrap(); // idempotent
        let cols = |t: &str| -> Vec<String> {
            let mut st = db.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            st.query_map([], |r| r.get::<_, String>(1)).unwrap().map(|c| c.unwrap()).collect()
        };
        assert!(cols("pool_obs").contains(&"reward_rate".to_string()));
        assert!(cols("predictions").contains(&"regime".to_string()) && cols("predictions").contains(&"replay".to_string()));
        assert!(cols("shadow").contains(&"replay".to_string()) && cols("shadow").contains(&"best_net".to_string()));
        assert!(cols("shadow_model").contains(&"replay".to_string()));
        // Full capture (2026-09-30): an old database gains the new columns too.
        for c in ["staked_liquidity", "aero_weth_tick", "aero_usd", "reward_reserve", "period_finish", "gauge_fees0", "gauge_fees1"] {
            assert!(cols("pool_obs").contains(&c.to_string()), "{c}");
        }
        assert!(cols("price_1m").contains(&"volume".to_string()));
    }
}
