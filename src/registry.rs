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
        // Every strategy picks its width with the keeper's own hysteresis rule, under the policy the
        // keeper was running at that moment.
        let policy = policy_at(store, ts)?;
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

/// The keeper's rebalance policy as its most recent start at or before `t` logged it.
pub fn policy_at(store: &Store, t: i64) -> Result<shadow::Policy> {
    let raw: Option<String> = store
        .db
        .query_row("SELECT json FROM keeper_rows WHERE tbl = 'keeper_runs' AND ts <= ?1 ORDER BY ts DESC LIMIT 1", [t], |r| r.get(0))
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
    // Every keeper table stored for the window (one per 10 minutes live), so each action is taken
    // with the table the keeper had at that moment.
    let tables: Vec<shadow::Table> = {
        let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'width_evals' AND ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        let rows = st.query_map([made, end], |r| r.get::<_, String>(0))?;
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
    let policy = policy_at(store, made)?;
    let keeper_hours = shadow::hours_for(&tables, None, &policy);
    let Some(keeper) = shadow::replay(&policy, &keeper_hours, &path, made, 86_400, None) else { return Ok(None) };

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
        let hours = shadow::hours_for(&tables, Some(&vols), &policy);
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
        "UPDATE shadow SET keeper_net_per_day = ?1, ai_net_per_day = ?2, best_hindsight = ?3, best_net = ?4, replay = ?5 WHERE made_ts = ?6",
        params![
            keeper.net_per_day,
            ai,
            best.map(|b| b.0),
            best.map(|b| b.1),
            json!({ "keeper": keeper, "fixed": fixed, "policy": policy }).to_string(),
            made
        ],
    )?;
    Ok(Some(()))
}

/// Result of checking the A.I's copy of the keeper's rules against what the live keeper did.
#[derive(Debug, Default)]
pub struct Fidelity {
    /// Width tables whose logged choice the hysteresis rule reproduces.
    pub choices: (i64, i64),
    /// Real rebalances whose time the replay reproduces within 10 minutes.
    pub rebalances: (i64, i64),
    /// Logged "TWAP outside for X" clocks the A.I measures within 10 minutes.
    pub clocks: (i64, i64),
    /// Real rebalances whose new width the rule reproduces for the position's real size.
    pub widths: (i64, i64),
    pub notes: Vec<String>,
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

/// Checks the rules engine against the keeper's own history rows in the A.I's database.
pub fn fidelity(store: &Store) -> Result<Fidelity> {
    let mut f = Fidelity::default();
    // 1. Width choice: every logged width table.
    let rows: Vec<(i64, String)> = {
        let mut st = store.db.prepare("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' ORDER BY ts")?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let mut tables: Vec<shadow::Table> = Vec::new();
    for (ts, raw) in &rows {
        let Some(t) = serde_json::from_str::<Value>(raw).ok().and_then(|v| shadow::Table::from_keeper(&v)) else { continue };
        let p = policy_at(store, *ts)?;
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
        let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'keeper_decisions' AND json LIKE '%TWAP outside for%'")?;
        let r = st.query_map([], |r| r.get::<_, String>(0))?;
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
        let p = policy_at(store, ts)?;
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
        let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'position_events' ORDER BY ts")?;
        let r = st.query_map([], |r| r.get::<_, String>(0))?;
        r.filter_map(|x| x.ok()).filter_map(|x| serde_json::from_str::<Value>(&x).ok()).filter(|v| v["kind"] == "rebalance").collect()
    };
    let starts: Vec<i64> = {
        let mut st = store.db.prepare("SELECT ts FROM keeper_rows WHERE tbl = 'keeper_runs' ORDER BY ts")?;
        let r = st.query_map([], |r| r.get(0))?;
        r.collect::<std::result::Result<_, _>>()?
    };
    let mut done = std::collections::HashSet::new();
    for e in events {
        let m = &e["meta"];
        let (Some(ts), Some(lo), Some(hi)) = (e["ts"].as_i64(), m["old_tick_lower"].as_f64(), m["old_tick_upper"].as_f64()) else {
            continue;
        };
        if !done.insert((ts / 60, lo as i64, hi as i64)) {
            continue; // several positions with the same range rebalanced in one heartbeat
        }
        let p = policy_at(store, ts)?;
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
        f.rebalances.1 += 1;
        match r.actions.first() {
            Some((t, kind, to)) if (t - ts).abs() <= 600 => {
                f.rebalances.0 += 1;
                // The width for THIS position's size (the keeper re-scores at its real value).
                let at = hours.iter().rposition(|h| h.table.ts <= *t).unwrap_or(0);
                let size = m["position_usd"].as_f64().unwrap_or(0.0);
                let sized = hours[at].table.nets_for_size(&hours[at].nets, size);
                let sized_choice = shadow::choose(&hours[at].table, &sized, &p);
                let opened = m["half_spacings"].as_i64().unwrap_or(-1);
                f.widths.1 += 1;
                if sized_choice == opened {
                    f.widths.0 += 1;
                }
                f.notes.push(format!(
                    "rebalance at {ts}: replay acted at {t} ({:+} s, {kind}); width for a ${size:.2} position {sized_choice} vs keeper {opened} (at $1,000: {to})",
                    t - ts
                ));
            }
            Some((t, kind, _)) => f.notes.push(format!("rebalance at {ts}: replay {kind} at {t} ({:+} min)", (t - ts) / 60)),
            None => f.notes.push(format!("rebalance at {ts}: replay took no action")),
        }
    }
    Ok(f)
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
        assert_eq!(make_shadow(&s, ts + 25 * 3600).unwrap(), 25);
        assert_eq!(make_shadow(&s, ts + 25 * 3600).unwrap(), 0, "not twice");
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

    #[test]
    fn policy_follows_keeper_runs() {
        let s = Store::memory().unwrap();
        s.db.execute(
            "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('r', 0, 'keeper_runs', 100, ?1)",
            [json!({"policy": {"confirmation_secs": 10800}}).to_string()],
        )
        .unwrap();
        assert_eq!(policy_at(&s, 50).unwrap().confirmation_secs, 21_600, "before the start: defaults");
        assert_eq!(policy_at(&s, 150).unwrap().confirmation_secs, 10_800, "after: the logged policy");
    }

    #[test]
    fn migrations_add_missing_columns() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE pool_obs (ts INTEGER PRIMARY KEY); CREATE TABLE shadow (made_ts INTEGER PRIMARY KEY); CREATE TABLE shadow_model (made_ts INTEGER, model TEXT);").unwrap();
        crate::store::migrate(&db).unwrap();
        crate::store::migrate(&db).unwrap(); // idempotent
        let cols = |t: &str| -> Vec<String> {
            let mut st = db.prepare(&format!("PRAGMA table_info({t})")).unwrap();
            st.query_map([], |r| r.get::<_, String>(1)).unwrap().map(|c| c.unwrap()).collect()
        };
        assert!(cols("pool_obs").contains(&"reward_rate".to_string()));
        assert!(cols("shadow").contains(&"replay".to_string()) && cols("shadow").contains(&"best_net".to_string()));
        assert!(cols("shadow_model").contains(&"replay".to_string()));
    }
}
