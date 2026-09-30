//! The daily brief: one plain-English email a day to the owner. Built from the numbers by code (no
//! paid AI service; the budget is $0). If email is not configured it is written to
//! `<data dir>/briefs/YYYY-MM-DD.txt` instead.

use crate::config::{Config, Email};
use crate::registry;
use crate::store::Store;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fmt::Write;

fn pct(x: f64) -> String {
    format!("{:+.3}%", x * 100.0)
}

fn day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// "2026-09-23 22:53 UTC".
pub fn when(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

fn hm(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%H:%M").to_string()).unwrap_or_default()
}

/// First and last close of a price_1m source in the window, as "a -> b (+x%)".
fn move_line(store: &Store, source: &str, since: i64, now: i64, fmt: &dyn Fn(f64) -> String) -> Result<Option<String>> {
    let p = store.prices(source, since - 3600, now)?;
    Ok(match (p.first(), p.last()) {
        (Some((_, a)), Some((_, z))) if *a > 0.0 => Some(format!("{} -> {} ({:+.2}%)", fmt(*a), fmt(*z), (z / a - 1.0) * 100.0)),
        _ => None,
    })
}

fn usd_short(x: f64) -> String {
    if x.abs() >= 1e6 {
        format!("${:.2}M", x / 1e6)
    } else if x.abs() >= 1e3 {
        format!("${:.1}k", x / 1e3)
    } else {
        format!("${x:.2}")
    }
}

/// Swap totals of a window: count, volume, largest, WETH bought, WETH sold, lowest and highest tick.
type SwapTotals = (i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<i64>, Option<i64>);

/// The pool section: everything the A.I saw happen in the Panda pool and its gauge.
fn pool_section(b: &mut String, store: &Store, since: i64, now: i64) -> Result<()> {
    writeln!(b, "\nPOOL (last 24 h, read straight from Base)")?;
    // Swaps: token0 is WETH, token1 USDC; amount0 < 0 means WETH left the pool (traders bought it).
    let (n, vol, big, bought, sold, lo, hi): SwapTotals = store.db.query_row(
        "SELECT COUNT(*), SUM(usd), MAX(usd),
                SUM(CASE WHEN CAST(amount0 AS REAL) < 0 THEN -CAST(amount0 AS REAL) ELSE 0 END) / 1e18,
                SUM(CASE WHEN CAST(amount0 AS REAL) > 0 THEN CAST(amount0 AS REAL) ELSE 0 END) / 1e18,
                MIN(tick), MAX(tick)
         FROM swaps WHERE ts > ?1",
        [since],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    if n == 0 {
        writeln!(b, "  Swaps: none recorded yet")?;
    } else {
        writeln!(
            b,
            "  Swaps: {n} · volume {} · largest {} · traders bought {:.2} WETH and sold {:.2} WETH · range ${:.0}-${:.0}",
            usd_short(vol.unwrap_or(0.0)),
            usd_short(big.unwrap_or(0.0)),
            bought.unwrap_or(0.0),
            sold.unwrap_or(0.0),
            crate::chainmath::tick_to_price(lo.unwrap_or(0) as f64),
            crate::chainmath::tick_to_price(hi.unwrap_or(0) as f64)
        )?;
    }
    // Other LPs: mints/burns per minute sums.
    let lp: Vec<(String, i64, i64, Option<f64>)> = {
        let mut st = store.db.prepare(
            "SELECT kind, SUM(events), SUM(in_range_events), SUM(width_ticks_mean * events) / NULLIF(SUM(CASE WHEN width_ticks_mean IS NULL THEN 0 ELSE events END), 0)
             FROM lp_minute WHERE ts > ?1 GROUP BY kind",
        )?;
        let r = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        r.filter_map(|x| x.ok()).collect()
    };
    let get = |k: &str| lp.iter().find(|x| x.0 == k).map(|x| (x.1, x.2, x.3)).unwrap_or((0, 0, None));
    let (mints, mints_in, mint_w) = get("mint");
    let (burns, burns_in, _) = get("burn");
    if mints + burns > 0 {
        writeln!(
            b,
            "  Other LPs: {mints} range opens ({mints_in} around the price{}), {burns} closes ({burns_in} around the price)",
            mint_w.map(|w| format!(", average ±{:.2}%", w / 200.0)).unwrap_or_default()
        )?;
    }
    let gg: Vec<(String, i64, f64, i64)> = {
        let mut st =
            store.db.prepare("SELECT kind, SUM(events), SUM(amount), SUM(token_ids) FROM gauge_minute WHERE ts > ?1 GROUP BY kind")?;
        let r = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        r.filter_map(|x| x.ok()).collect()
    };
    let g = |k: &str| gg.iter().find(|x| x.0 == k).map(|x| (x.1, x.2)).unwrap_or((0, 0.0));
    if !gg.is_empty() {
        writeln!(
            b,
            "  Gauge: {} stakes, {} unstakes, {:.0} AERO claimed by stakers, {} early-withdraw penalties",
            g("stake").0,
            g("unstake").0,
            g("claim_rewards").1,
            g("early_withdraw_penalty").0
        )?;
    }
    // Staked liquidity at the price, now and a day ago, and Panda's share of it.
    let obs = |at: i64| -> Option<(f64, f64, Option<f64>, Option<f64>)> {
        store
            .db
            .query_row(
                "SELECT CAST(liquidity AS REAL), CAST(staked_liquidity AS REAL), reward_rate, aero_usd FROM pool_obs
                 WHERE ts <= ?1 AND staked_liquidity IS NOT NULL ORDER BY ts DESC LIMIT 1",
                [at],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
    };
    if let Some((active, staked, rate, aero)) = obs(now) {
        let before = obs(since).map(|(a, s, _, _)| if a > 0.0 { format!(" (a day ago {:.0}%)", s / a * 100.0) } else { String::new() });
        writeln!(
            b,
            "  Staked share of the liquidity at the price: {:.0}%{}",
            if active > 0.0 { staked / active * 100.0 } else { 0.0 },
            before.unwrap_or_default()
        )?;
        let panda: Option<f64> = store
            .db
            .query_row(
                "SELECT SUM(CAST(liquidity AS REAL)) FROM position_min WHERE in_range = 1 AND ts = (SELECT MAX(ts) FROM position_min)",
                [],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        if let (Some(p), true) = (panda, staked > 0.0) {
            writeln!(
                b,
                "  Panda's in-range positions hold {:.2}% of the staked liquidity at the price (their share of the emissions right now)",
                p / staked * 100.0
            )?;
        }
        if let Some(r) = rate {
            let per_day = r * 86_400.0;
            let usd = aero.map(|a| format!(" (≈ {})", usd_short(per_day * a))).unwrap_or_default();
            let finish = store.series_at("gauge", "period_finish", now)?.map(|(_, v)| v as i64).filter(|f| *f > now);
            let share = store.series_at("gauge", "voter_share", now)?.map(|(_, v)| v);
            writeln!(
                b,
                "  Emissions: {:.0} AERO a day to stakers{usd}{}{}",
                per_day,
                finish.map(|f| format!(" · this epoch ends in {:.0} h", (f - now) as f64 / 3600.0)).unwrap_or_default(),
                share.map(|s| format!(" · the pool holds {:.2}% of all Aerodrome votes", s * 100.0)).unwrap_or_default()
            )?;
        }
        // Where the staked competition sits: staked liquidity at ±1/2/5% relative to at the price.
        let map_ts: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM tick_liquidity", [], |r| r.get(0))?;
        if let (Some(mt), Some((_, cur))) = (map_ts, store.series_at("pool", "map_current_tick", now)?) {
            if let Some((a, s, _, _)) = obs(mt) {
                let prof = crate::collect::liquidity::profile(store, mt, cur as i64, a, s)?;
                let at = |dt: i64| prof.iter().rev().find(|r| r.0 <= cur as i64 + dt).map(|r| r.2);
                if let Some(here) = at(0).filter(|h| *h > 0.0) {
                    let cell = |dt: i64| at(dt).map(|v| format!("{:.2}x", v / here)).unwrap_or_else(|| "-".into());
                    writeln!(
                        b,
                        "  Staked liquidity vs at the price ({} UTC): -5% {} · -2% {} · -1% {} | +1% {} · +2% {} · +5% {}",
                        hm(mt),
                        cell(-500),
                        cell(-200),
                        cell(-100),
                        cell(100),
                        cell(200),
                        cell(500)
                    )?;
                }
            }
        }
    }
    // The competition, from DefiLlama.
    let yields_ts: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM pool_yields", [], |r| r.get(0))?;
    if let Some(yt) = yields_ts {
        let rows: Vec<(String, Option<String>, f64, Option<f64>)> = {
            let mut st = store.db.prepare(
                "SELECT project, meta, tvl_usd, apy FROM pool_yields WHERE ts = ?1 AND symbol = 'WETH-USDC' ORDER BY tvl_usd DESC LIMIT 4",
            )?;
            let r = st.query_map([yt], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            r.filter_map(|x| x.ok()).collect()
        };
        if !rows.is_empty() {
            let parts: Vec<String> = rows
                .iter()
                .map(|(p, m, tvl, apy)| {
                    format!("{} {} APY {:.0}%", m.clone().unwrap_or_else(|| p.clone()), usd_short(*tvl), apy.unwrap_or(0.0))
                })
                .collect();
            writeln!(b, "  Aerodrome WETH/USDC pools (DefiLlama): {}", parts.join(" · "))?;
        }
    }
    Ok(())
}

/// Builds the brief for the 24 hours ending at `now`.
pub fn build(store: &Store, now: i64) -> Result<(String, String)> {
    let since = now - 86_400;
    let mut b = String::new();
    let subject = format!("Panda A.I Keeper: daily brief {}", day(now));
    writeln!(b, "Panda A.I Keeper — daily brief for {} (UTC)", day(now))?;
    writeln!(b, "Watch-only: it cannot sign, send or change anything.\n")?;

    // 1. Market.
    writeln!(b, "MARKET (last 24 h)")?;
    let coin = store.prices("coinbase", since - 3600, now)?;
    match (coin.first(), coin.last()) {
        (Some((_, a)), Some((_, z))) => writeln!(b, "  ETH: ${a:.0} -> ${z:.0} ({:+.2}%)", (z / a - 1.0) * 100.0)?,
        _ => writeln!(b, "  ETH: no price data")?,
    }
    if let Some(v) = crate::model::vol::realised(&coin, since, now) {
        writeln!(b, "  Volatility: {:.2}% per sqrt(hour) (keeper's calm/storm line is about 0.5%)", v * 100.0)?;
    }
    let f = crate::model::vol::forecasts(store, now, 24)?;
    if !f.is_empty() {
        let s: Vec<String> = f.iter().map(|(m, v)| format!("{m} {:.2}%", v * 100.0)).collect();
        writeln!(b, "  Next-24 h volatility forecasts: {}", s.join(", "))?;
        let graded = store.count(
            "SELECT COUNT(*) FROM predictions WHERE kind = 'vol24h' AND score IS NOT NULL AND made_ts > ?1",
            &[&(now - 30 * 86_400)],
        )?;
        writeln!(
            b,
            "  Forecast champion (the model the A.I follows): {}{}",
            registry::champion(store, now)?,
            if graded == 0 { " (by default: no 24 h forecast graded yet; each needs 24 h, and a model needs 10)" } else { "" }
        )?;
        match registry::profit_champion(store, now, 30, 10)? {
            Some((m, n, mean)) => {
                writeln!(b, "  Profit champion (best shadow earnings, 30 days): {m} {} /day over {n} decisions", pct(mean))?
            }
            None => writeln!(b, "  Profit champion (best shadow earnings, 30 days): not enough graded decisions yet")?,
        }
    }
    let fmt_usd = |x: f64| format!("${x:.0}");
    let fmt_aero = |x: f64| format!("${x:.4}");
    let fmt_ratio = |x: f64| format!("{x:.5}");
    if let Some(l) = move_line(store, "cb:BTC-USD", since, now, &fmt_usd)? {
        writeln!(b, "  BTC: {l}")?;
    }
    if let Some(l) = move_line(store, "cb:AERO-USD", since, now, &fmt_aero)? {
        writeln!(b, "  AERO: {l}")?;
    }
    if let Some(l) = move_line(store, "cb:ETH-BTC", since, now, &fmt_ratio)? {
        writeln!(b, "  ETH/BTC: {l}")?;
    }
    let funding = store.series_at("deribit", "ETH-PERPETUAL.funding_8h", now)?;
    let oi_now = store.series_at("deribit", "ETH-PERPETUAL.open_interest", now)?;
    let oi_then = store.series_at("deribit", "ETH-PERPETUAL.open_interest", since)?;
    if let Some((_, fr)) = funding {
        let oi = match (oi_now, oi_then) {
            (Some((_, a)), Some((_, z))) if z > 0.0 => {
                format!(" · open interest {} ({:+.1}% in 24 h)", usd_short(a), (a / z - 1.0) * 100.0)
            }
            (Some((_, a)), _) => format!(" · open interest {}", usd_short(a)),
            _ => String::new(),
        };
        writeln!(b, "  ETH perpetual (Deribit): funding {:+.4}% per 8 h{oi}", fr * 100.0)?;
    }
    let dvol_eth: Option<f64> = store.db.query_row("SELECT value FROM dvol ORDER BY ts DESC LIMIT 1", [], |r| r.get(0)).ok();
    let dvol_btc = store.series_at("deribit", "BTC.dvol", now)?.map(|x| x.1);
    let fng = store.series_at("alternative", "fear_greed", now)?.map(|x| x.1);
    if dvol_eth.is_some() || dvol_btc.is_some() || fng.is_some() {
        writeln!(
            b,
            "  Options-implied volatility (DVOL): ETH {} · BTC {} · Fear & Greed {}",
            dvol_eth.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()),
            dvol_btc.map(|v| format!("{v:.1}")).unwrap_or_else(|| "-".into()),
            fng.map(|v| format!("{v:.0}/100")).unwrap_or_else(|| "-".into())
        )?;
    }
    if let (Some((_, sp)), Some((_, bid)), Some((_, ask))) = (
        store.series_at("coinbase_book", "ETH-USD.spread_bps", now)?,
        store.series_at("coinbase_book", "ETH-USD.bid_usd_1pct", now)?,
        store.series_at("coinbase_book", "ETH-USD.ask_usd_1pct", now)?,
    ) {
        writeln!(b, "  Coinbase ETH-USD book: spread {sp:.2} bp · within 1%: {} bids, {} asks", usd_short(bid), usd_short(ask))?;
    }
    pool_section(&mut b, store, since, now)?;

    // 2. What the keeper did.
    writeln!(b, "\nKEEPER ACTIVITY (from its history bucket)")?;
    let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'position_events' AND ts > ?1 ORDER BY ts")?;
    let events: Vec<Value> =
        st.query_map([since], |r| r.get::<_, String>(0))?.filter_map(|r| r.ok()).filter_map(|s| serde_json::from_str(&s).ok()).collect();
    let count = |k: &str| events.iter().filter(|e| e["kind"] == k).count();
    writeln!(
        b,
        "  Deposits {} · Rebalances {} · Reward payouts {} · Withdrawals {}",
        count("deposit"),
        count("rebalance"),
        count("reward_paid"),
        count("withdraw") + count("emergency_exit")
    )?;
    let dep_usd: f64 = events.iter().filter(|e| e["kind"] == "deposit").filter_map(|e| e["deposit_usd"].as_f64()).sum();
    if dep_usd > 0.0 {
        writeln!(b, "  New deposits: ${dep_usd:.2}")?;
    }
    let gas: f64 = events.iter().filter(|e| e["kind"] == "rebalance").filter_map(|e| e["cost_usd"].as_f64()).sum();
    if count("rebalance") > 0 {
        writeln!(b, "  Rebalance gas paid by the keeper: ${gas:.2}")?;
    }
    let last_width: Option<(i64, String)> = store
        .db
        .query_row("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' ORDER BY ts DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok();
    if let Some((ts, raw)) = last_width {
        let v: Value = serde_json::from_str(&raw).unwrap_or_default();
        writeln!(
            b,
            "  Latest width table ({} min old): keeper would open at ±{:.1}%",
            (now - ts) / 60,
            v["choice"].as_f64().unwrap_or(0.0) * 0.5
        )?;
    }

    // 3. Shadow mode.
    writeln!(b, "\nSHADOW MODE (A.I width choice vs the keeper's, replayed on real prices)")?;
    for (label, days) in [("7 days", 7), ("30 days", 30)] {
        let (_, sh) = registry::scorecard(store, now, days)?;
        match sh {
            Some(s) => {
                writeln!(
                    b,
                    "  {label}: {} decisions graded · keeper {} /day · A.I {} /day · A.I better {} of {} · same choice {} · best possible {} /day",
                    s.n,
                    pct(s.keeper_mean),
                    pct(s.ai_mean),
                    s.ai_better,
                    s.n,
                    s.same_choice,
                    pct(s.hindsight_mean)
                )?;
                if s.days < 30 {
                    writeln!(
                        b,
                        "      (only {} independent days: an early signal, not evidence. Treat any lead as noise before ~30 days.)",
                        s.days
                    )?;
                }
                if days == 7 {
                    for (m, n, mean, beat) in &s.models {
                        writeln!(b, "      {m:<8} {} /day, beat the keeper {beat} of {n}", pct(*mean))?;
                    }
                }
            }
            None => writeln!(b, "  {label}: nothing graded yet (each decision needs 24 h of prices after it)")?,
        }
    }
    let recent: Vec<(i64, i64, i64)> = {
        let mut st =
            store.db.prepare("SELECT made_ts, keeper_choice, ai_choice FROM shadow WHERE made_ts > ?1 ORDER BY made_ts DESC LIMIT 3")?;
        let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    for (t, k, a) in recent {
        writeln!(
            b,
            "  {} UTC: keeper ±{:.1}%, A.I ±{:.1}%",
            chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%H:%M").to_string()).unwrap_or_default(),
            k as f64 * 0.5,
            a as f64 * 0.5
        )?;
    }

    // 4. Forecast scorecard.
    writeln!(b, "\nFORECAST SCORECARD (30 days; lower is better)")?;
    let (lines, _) = registry::scorecard(store, now, 30)?;
    if lines.is_empty() {
        writeln!(b, "  Nothing graded yet.")?;
    }
    for l in lines.iter().filter(|l| l.n >= 3) {
        let what = match l.kind.as_str() {
            "vol24h" => "24 h volatility (QLIKE)",
            "vol1h" => "1 h volatility (QLIKE)",
            "exit24h" => "range exit, all widths (Brier)",
            k => k,
        };
        writeln!(b, "  {what:<32} {:<18} {:.4}  (n={})", l.model, l.mean_score, l.n)?;
    }

    // 4b. Headlines.
    let news_n = store.count("SELECT COUNT(*) FROM news WHERE ts > ?1", &[&since])?;
    if news_n > 0 {
        writeln!(b, "\nNEWS (last 24 h: {news_n} headlines kept)")?;
        let mut st = store.db.prepare("SELECT source, title FROM news WHERE ts > ?1 ORDER BY ts DESC LIMIT 6")?;
        let rows = st.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (src, title) in rows.filter_map(|r| r.ok()) {
            let t: String = title.chars().take(110).collect();
            writeln!(b, "  [{src}] {t}")?;
        }
    }

    // 5. Health. During the first day the targets are scaled to how long the A.I has been collecting.
    writeln!(b, "\nHEALTH")?;
    let pool_n = store.count("SELECT COUNT(*) FROM pool_obs WHERE ts > ?1", &[&since])?;
    let coin_n = store.count("SELECT COUNT(*) FROM price_1m WHERE source = 'coinbase' AND ts > ?1", &[&since])?;
    let rows_n = store.count("SELECT COUNT(*) FROM keeper_rows WHERE ts > ?1", &[&since])?;
    let first_obs: Option<i64> = store.db.query_row("SELECT MIN(ts) FROM pool_obs", [], |r| r.get(0))?;
    match first_obs.filter(|f| *f > since + 1800) {
        Some(start) => writeln!(
            b,
            "  Pool reads {pool_n} since the A.I started at {} UTC (target ~{}: 2 a minute; a full day is 2,880) · Coinbase minutes {coin_n} · keeper rows {rows_n}",
            hm(start),
            (now - start) / 30
        )?,
        None => writeln!(b, "  Pool reads {pool_n} (target ~2,880) · Coinbase minutes {coin_n} (target 1,440) · keeper rows {rows_n}")?,
    }
    // Everything else it captured in the day.
    let block_since: Option<i64> = store.db.query_row("SELECT MIN(block) FROM pool_obs WHERE ts > ?1", [since], |r| r.get(0))?;
    let logs_n: i64 = match block_since {
        Some(bl) => store.count("SELECT COALESCE(SUM(logs), 0) FROM chain_log_archive WHERE from_block >= ?1", &[&bl])?,
        None => 0,
    };
    let swaps_n = store.count("SELECT COUNT(*) FROM swaps WHERE ts > ?1", &[&since])?;
    let files_n = store.count("SELECT COUNT(*) FROM bucket_archive WHERE ts > ?1", &[&since])?;
    let posmin_n = store.count("SELECT COUNT(*) FROM position_min WHERE ts > ?1", &[&since])?;
    let maps_n = store.count("SELECT COUNT(DISTINCT ts) FROM tick_liquidity WHERE ts > ?1", &[&since])?;
    let opt_n = store.count("SELECT COUNT(DISTINCT ts) FROM options_hourly WHERE ts > ?1", &[&since])?;
    let series_n = store.count("SELECT COUNT(*) FROM series WHERE ts > ?1", &[&since])?;
    let db_bytes = std::fs::metadata(store.db.path().unwrap_or("")).map(|m| m.len() as f64).unwrap_or(0.0);
    let db_size = if db_bytes >= 1e9 { format!("{:.2} GB", db_bytes / 1e9) } else { format!("{:.0} MB", db_bytes / 1e6) };
    writeln!(
        b,
        "  Captured: {logs_n} pool/gauge events ({swaps_n} swaps) · {files_n} keeper files · {posmin_n} position-minutes · {maps_n} liquidity maps · {opt_n} option boards · {series_n} market readings · database {db_size}"
    )?;
    let bad: Vec<(String, i64, String)> = {
        let mut st = store.db.prepare("SELECT url, status, COALESCE(note, '') FROM site_checks WHERE ts > ?1 AND (status < 200 OR status >= 400) ORDER BY ts DESC LIMIT 5")?;
        let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if bad.is_empty() {
        writeln!(b, "  Website: all checks OK")?;
    }
    for (u, s, n) in bad {
        writeln!(b, "  Website problem: {u} -> {s} {n}")?;
    }
    let feed: Option<String> = store
        .db
        .query_row("SELECT note FROM site_checks WHERE url LIKE '%/api/keeper/width' ORDER BY ts DESC LIMIT 1", [], |r| r.get(0))
        .ok();
    if let Some(n) = feed.filter(|n| !n.is_empty()) {
        writeln!(b, "  Keeper deposit-width feed: {n}")?;
    }

    // 5b. Alerts of the last day, and whether the A.I's copy of the keeper's rules still matches.
    let alerts: Vec<(i64, String, String, String)> = {
        let mut st = store.db.prepare("SELECT ts, key, state, message FROM alerts WHERE ts > ?1 ORDER BY ts")?;
        let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if alerts.is_empty() {
        writeln!(b, "  Alerts: none in the last 24 h")?;
    }
    for (t, key, state, msg) in alerts {
        let at = chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%H:%M").to_string()).unwrap_or_default();
        writeln!(b, "  Alert {at} UTC [{state}] {key}: {msg}")?;
    }
    let fid = crate::registry::fidelity(store)?;
    writeln!(
        b,
        "  Keeper-rules check: {} of {} width choices and {} of {} out-of-range clocks reproduced (all history)",
        fid.choices.0, fid.choices.1, fid.clocks.0, fid.clocks.1
    )?;
    match (&fid.rules_version, fid.rules_since) {
        (Some(v), Some(since_ts)) => writeln!(
            b,
            "  Rebalances under the keeper's current rules ({v}.x, since {}): {} of {} times and {} of {} new widths reproduced",
            when(since_ts),
            fid.rebalances.0,
            fid.rebalances.1,
            fid.widths.0,
            fid.widths.1
        )?,
        _ => writeln!(
            b,
            "  Rebalances: {} of {} times and {} of {} new widths reproduced",
            fid.rebalances.0, fid.rebalances.1, fid.widths.0, fid.widths.1
        )?,
    }
    if fid.older_rebalances.1 > 0 {
        writeln!(
            b,
            "  (Older keeper versions had different rules, so their rebalances are not expected to match: {} of {} times, {} of {} widths. For reference only.)",
            fid.older_rebalances.0, fid.older_rebalances.1, fid.older_widths.0, fid.older_widths.1
        )?;
    }

    // 6. Owner notes.
    let notes: Vec<String> = {
        let mut st = store.db.prepare("SELECT text FROM notes WHERE ts > ?1 ORDER BY ts")?;
        let rows = st.query_map([since], |r| r.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if !notes.is_empty() {
        writeln!(b, "\nYOUR NOTES (last 24 h)")?;
        for n in notes {
            writeln!(b, "  - {n}")?;
        }
    }
    writeln!(b, "\nReply to this email to teach it: your reply is read within 10 minutes and kept with the day's decisions.")?;
    Ok((subject, b))
}

pub async fn send(email: &Email, subject: &str, body: &str) -> Result<()> {
    use lettre::{
        message::header::ContentType, transport::smtp::authentication::Credentials, AsyncSmtpTransport, AsyncTransport, Message,
        Tokio1Executor,
    };
    crate::guard::check_environment(std::env::vars())?; // never send while a key is present
    let msg = Message::builder()
        .from(format!("Panda A.I Keeper <{}>", email.smtp_user).parse()?)
        .to(email.to.parse()?)
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())?;
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::relay(&email.smtp_host)?
        .credentials(Credentials::new(email.smtp_user.clone(), email.smtp_password.clone()))
        .build();
    mailer.send(msg).await.context("sending the brief")?;
    Ok(())
}

/// Builds today's brief and emails it (or writes it to disk). Returns where it went.
pub async fn deliver(cfg: &Config, store: &Store, now: i64, force_file: bool) -> Result<String> {
    let (subject, body) = build(store, now)?;
    let dir = cfg.data_dir.join("briefs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.txt", day(now)));
    std::fs::write(&path, &body)?;
    match (&cfg.email, force_file) {
        (Some(e), false) => {
            send(e, &subject, &body).await?;
            Ok(format!("emailed to {} (copy at {})", e.to, path.display()))
        }
        _ => Ok(format!("written to {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_builds_on_empty_and_real_rows() {
        let s = Store::memory().unwrap();
        let (subject, body) = build(&s, 1_790_700_000).unwrap();
        assert!(subject.contains("daily brief"));
        assert!(body.contains("Watch-only"));
        let ev = std::fs::read_to_string("tests/fixtures/position_events.json").unwrap();
        s.db.execute("INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('o', 0, 'position_events', 1790679373, ?1)", [ev.trim()])
            .unwrap();
        s.db.execute("INSERT INTO notes (ts, text) VALUES (1790690000, 'too narrow today')", []).unwrap();
        let (_, body) = build(&s, 1_790_700_000).unwrap();
        assert!(body.contains("Deposits 1"), "{body}");
        assert!(body.contains("New deposits: $23.07"), "{body}");
        assert!(body.contains("too narrow today"));
    }
}
