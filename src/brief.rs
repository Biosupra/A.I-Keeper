//! The daily brief: one email a day to the owner, built from the numbers by code (no paid AI
//! service; the budget is $0). If email is not configured it is written to
//! `<data dir>/briefs/YYYY-MM-DD.txt` instead.
//!
//! Written to be read by a person, not a machine (owner 2026-10-07: "right now it just looks like a
//! bunch of gibberish to me, it needs to speak to me like a human"). It opens with the short version
//! (does anything need you, how each vault did, the market, what the A.I learned), then explains
//! each part in sentences. Every number the old brief carried is still there, at the end, under
//! TECHNICAL DETAIL, for anyone who wants to check the working.
//!
//! Vaults (2026-10-07): the original vault's keeper and each vault of the multi-vault platform are
//! reported separately; the A.I follows the live one (`primary`, AIK_PRIMARY_VAULT).

use crate::alerts::vault_name;
use crate::collect::bucket::OLD;
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

/// "Tuesday 6 October".
fn long_day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%A %-d %B").to_string()).unwrap_or_default()
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

/// First and last close of a price_1m source in the window.
fn first_last(store: &Store, source: &str, since: i64, now: i64) -> Result<Option<(f64, f64)>> {
    let p = store.prices(source, since - 3600, now)?;
    Ok(match (p.first(), p.last()) {
        (Some((_, a)), Some((_, z))) if *a > 0.0 => Some((*a, *z)),
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

/// Thousands separators: 2349.2 -> "2,349".
fn commas(x: f64) -> String {
    let n = x.round().abs() as u64;
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if x < 0.0 && n > 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Dollars for a person: "$2,349" for larger amounts, "$12.73" for small ones, "$0.03" for cents.
fn money(x: f64) -> String {
    if x.abs() >= 1000.0 {
        format!("${}", commas(x))
    } else {
        format!("${x:.2}")
    }
}

/// "rose 1.90%", "fell 2.11%", "barely moved (+0.03%)".
fn moved(pct_change: f64) -> String {
    if pct_change.abs() < 0.25 {
        format!("barely moved ({pct_change:+.2}%)")
    } else if pct_change > 0.0 {
        format!("rose {pct_change:.2}%")
    } else {
        format!("fell {:.2}%", -pct_change)
    }
}

/// The keeper's own calm/active/storm lines (per sqrt hour).
fn vol_words(v: f64) -> &'static str {
    if v < 0.005 {
        "calm"
    } else if v < 0.01 {
        "fairly busy"
    } else {
        "stormy"
    }
}

fn fear_greed_words(x: f64) -> &'static str {
    match x as i64 {
        i64::MIN..=24 => "extreme fear",
        25..=44 => "fearful",
        45..=55 => "neutral",
        56..=74 => "greedy",
        _ => "extremely greedy",
    }
}

/// A share as a person reads it: "0.04%", "less than 0.01%".
fn share_words(x: f64) -> String {
    let p = x * 100.0;
    if p <= 0.0 {
        "none".into()
    } else if p < 0.01 {
        "less than 0.01%".into()
    } else if p < 1.0 {
        format!("{p:.2}%")
    } else {
        format!("{p:.1}%")
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// What one vault's keeper saw in the window and holds now.
#[derive(Default, Debug)]
struct VaultDay {
    vault: String,
    deposits: i64,
    deposit_usd: f64,
    top_ups: i64,
    top_up_usd: f64,
    withdrawals: i64,
    rebalances: i64,
    rebalance_gas: f64,
    payouts: i64,
    /// Open positions in the keeper's latest snapshot (none if it is more than 3 hours old).
    positions: i64,
    positions_usd: f64,
    in_range: i64,
    in_range_liquidity: f64,
    snapshot_ts: Option<i64>,
    /// The width the keeper would open a new position at (half-width, %), and when it said so.
    width: Option<(i64, f64)>,
    /// Newest row of any kind from this keeper.
    newest: Option<i64>,
}

fn vault_day(store: &Store, vault: &str, since: i64, now: i64) -> Result<VaultDay> {
    let mut d = VaultDay { vault: vault.to_string(), ..Default::default() };
    let events: Vec<Value> = {
        let mut st =
            store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'position_events' AND vault = ?2 AND ts > ?1 ORDER BY ts")?;
        let r = st.query_map(rusqlite::params![since, vault], |r| r.get::<_, String>(0))?;
        r.filter_map(|x| x.ok()).filter_map(|s| serde_json::from_str(&s).ok()).collect()
    };
    for e in &events {
        match e["kind"].as_str().unwrap_or("") {
            "deposit" => {
                d.deposits += 1;
                d.deposit_usd += e["deposit_usd"].as_f64().unwrap_or(0.0);
            }
            "top_up" => {
                d.top_ups += 1;
                d.top_up_usd += e["top_up_usd"].as_f64().unwrap_or(0.0);
            }
            // Every way a position can leave (a forced exit's batch transaction, `force_exit_batch`, is
            // the keeper's own and is counted once per position through `force_exit`).
            "withdraw" | "emergency_exit" | "emergency_withdraw" | "raw_nft_withdraw" | "force_exit" => d.withdrawals += 1,
            "rebalance" => {
                d.rebalances += 1;
                d.rebalance_gas += e["cost_usd"].as_f64().unwrap_or(0.0);
            }
            "reward_paid" => d.payouts += 1,
            _ => {}
        }
    }
    // Open positions: each position's newest snapshot, when the keeper's latest snapshot is recent.
    let latest: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM position_min WHERE vault = ?1", [vault], |r| r.get(0))?;
    if let Some(l) = latest.filter(|l| now - l < 3 * 3600) {
        d.snapshot_ts = Some(l);
        let mut st = store.db.prepare(
            "SELECT p.position_usd, p.in_range, CAST(p.liquidity AS REAL) FROM position_min p
             WHERE p.vault = ?1 AND p.ts > ?2 AND p.ts = (SELECT MAX(q.ts) FROM position_min q WHERE q.vault = ?1 AND q.token_id = p.token_id)",
        )?;
        let rows = st.query_map(rusqlite::params![vault, l - 900], |r| {
            Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<f64>>(2)?))
        })?;
        for (usd, inr, liq) in rows.filter_map(|x| x.ok()) {
            d.positions += 1;
            d.positions_usd += usd.unwrap_or(0.0);
            if inr == Some(1) {
                d.in_range += 1;
                d.in_range_liquidity += liq.unwrap_or(0.0);
            }
        }
    }
    let width: Option<(i64, String)> = store
        .db
        .query_row("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND vault = ?1 ORDER BY ts DESC LIMIT 1", [vault], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok();
    d.width = width.and_then(|(ts, raw)| {
        let v: Value = serde_json::from_str(&raw).ok()?;
        Some((ts, v["choice"].as_f64()? * 0.5))
    });
    d.newest = store.db.query_row(
        "SELECT MAX(t) FROM (SELECT MAX(ts) AS t FROM keeper_rows WHERE vault = ?1 UNION ALL SELECT MAX(ts) FROM position_min WHERE vault = ?1
                            UNION ALL SELECT MAX(ts) FROM decision_min WHERE vault = ?1)",
        [vault],
        |r| r.get(0),
    )?;
    Ok(d)
}

/// "3 deposits ($520.10), 1 top-up ($50.00), 2 withdrawals, 1 range move ($0.03 of gas), 4 reward payouts",
/// or None for a quiet day.
fn activity_words(d: &VaultDay) -> Option<String> {
    let mut parts = Vec::new();
    if d.deposits > 0 {
        parts.push(format!("{} ({})", plural(d.deposits, "new deposit", "new deposits"), money(d.deposit_usd)));
    }
    if d.top_ups > 0 {
        parts.push(format!("{} ({})", plural(d.top_ups, "top-up", "top-ups"), money(d.top_up_usd)));
    }
    if d.withdrawals > 0 {
        parts.push(plural(d.withdrawals, "withdrawal", "withdrawals"));
    }
    if d.rebalances > 0 {
        parts.push(format!("{} by the keeper ({} of gas)", plural(d.rebalances, "range move", "range moves"), money(d.rebalance_gas)));
    }
    if d.payouts > 0 {
        parts.push(plural(d.payouts, "reward payout", "reward payouts"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Swap totals of a window: count, volume, largest, WETH bought, WETH sold, lowest and highest tick.
type SwapTotals = (i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<i64>, Option<i64>);

fn swap_totals(store: &Store, since: i64) -> Result<SwapTotals> {
    // token0 is WETH, token1 USDC; amount0 < 0 means WETH left the pool (traders bought it).
    Ok(store.db.query_row(
        "SELECT COUNT(*), SUM(usd), MAX(usd),
                SUM(CASE WHEN CAST(amount0 AS REAL) < 0 THEN -CAST(amount0 AS REAL) ELSE 0 END) / 1e18,
                SUM(CASE WHEN CAST(amount0 AS REAL) > 0 THEN CAST(amount0 AS REAL) ELSE 0 END) / 1e18,
                MIN(tick), MAX(tick)
         FROM swaps WHERE ts > ?1",
        [since],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?)
}

/// The pool as read at or before `at`: (liquidity at the price, staked part of it, AERO per second to stakers, AERO price).
fn pool_obs(store: &Store, at: i64) -> Option<(f64, f64, Option<f64>, Option<f64>)> {
    store
        .db
        .query_row(
            "SELECT CAST(liquidity AS REAL), CAST(staked_liquidity AS REAL), reward_rate, aero_usd FROM pool_obs
             WHERE ts <= ?1 AND staked_liquidity IS NOT NULL ORDER BY ts DESC LIMIT 1",
            [at],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
}

/// The average half-width (%) of the ranges other LPs opened in the window.
fn other_lp_width(store: &Store, since: i64) -> Result<Option<f64>> {
    Ok(store
        .db
        .query_row(
            "SELECT SUM(width_ticks_mean * events) / NULLIF(SUM(CASE WHEN width_ticks_mean IS NULL THEN 0 ELSE events END), 0)
             FROM lp_minute WHERE ts > ?1 AND kind = 'mint'",
            [since],
            |r| r.get::<_, Option<f64>>(0),
        )?
        .map(|w| w / 200.0))
}

/// The pool section of the technical detail: everything the A.I saw happen in the Panda pool and its gauge.
fn pool_section(b: &mut String, store: &Store, since: i64, now: i64, vaults: &[VaultDay]) -> Result<()> {
    writeln!(b, "\nPOOL (last 24 h, read straight from Base)")?;
    let (n, vol, big, bought, sold, lo, hi) = swap_totals(store, since)?;
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
    if let Some((active, staked, rate, aero)) = pool_obs(store, now) {
        let before =
            pool_obs(store, since).map(|(a, s, _, _)| if a > 0.0 { format!(" (a day ago {:.0}%)", s / a * 100.0) } else { String::new() });
        writeln!(
            b,
            "  Staked share of the liquidity at the price: {:.0}%{}",
            if active > 0.0 { staked / active * 100.0 } else { 0.0 },
            before.unwrap_or_default()
        )?;
        if staked > 0.0 {
            for d in vaults.iter().filter(|d| d.snapshot_ts.is_some()) {
                writeln!(
                    b,
                    "  {}: in-range positions hold {:.4}% of the staked liquidity at the price (their share of the emissions right now)",
                    capitalise(&vault_name(&d.vault)),
                    d.in_range_liquidity / staked * 100.0
                )?;
            }
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
            if let Some((a, s, _, _)) = pool_obs(store, mt) {
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

/// Every vault the A.I has keeper history for: the live one first, the others after, the original vault last.
fn vaults_in_order(store: &Store, primary: &str) -> Result<Vec<String>> {
    let mut v = crate::alerts::new_vaults(store)?;
    v.sort_by_key(|x| x != primary);
    if !v.iter().any(|x| x == primary) && primary != OLD {
        v.insert(0, primary.to_string());
    }
    let has_old = store.count("SELECT COUNT(*) FROM keeper_rows WHERE vault = 'old'", &[])? > 0;
    if has_old || primary == OLD {
        v.retain(|x| x != OLD);
        v.push(OLD.to_string());
    }
    Ok(v)
}

/// Builds the brief for the 24 hours ending at `now`. `primary`: the vault the A.I follows.
pub fn build(store: &Store, now: i64, primary: &str) -> Result<(String, String)> {
    let since = now - 86_400;
    let order = vaults_in_order(store, primary)?;
    let vaults: Vec<VaultDay> = order.iter().map(|v| vault_day(store, v, since, now)).collect::<Result<_>>()?;

    // ---------- the facts every section draws on ----------
    let open_alerts: Vec<(String, i64, i64)> =
        serde_json::from_str(&store.meta_get("alerts_open")?.unwrap_or_else(|| "[]".into())).unwrap_or_default();
    let alert_text = |key: &str| -> String {
        store
            .db
            .query_row("SELECT message FROM alerts WHERE key = ?1 AND state = 'open' ORDER BY ts DESC LIMIT 1", [key], |r| r.get(0))
            .unwrap_or_else(|_| key.to_string())
    };
    let bad_sites: Vec<(String, i64, String)> = {
        let mut st = store.db.prepare("SELECT url, status, COALESCE(note, '') FROM site_checks WHERE ts > ?1 AND (status < 200 OR status >= 400) ORDER BY ts DESC LIMIT 5")?;
        let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let eth = first_last(store, "coinbase", since, now)?;
    let coin = store.prices("coinbase", since - 3600, now)?;
    let realised = crate::model::vol::realised(&coin, since, now);
    let forecasts = crate::model::vol::forecasts(store, now, 24)?;
    let champion = registry::champion(store, now)?;
    let (_, shadow7) = registry::scorecard(store, now, 7)?;
    let fid = registry::fidelity(store, primary)?;
    let live = vaults.first();

    let mut b = String::new();
    // ---------- subject ----------
    let status = if open_alerts.is_empty() && bad_sites.is_empty() {
        "all normal".to_string()
    } else {
        let n = open_alerts.len() as i64 + i64::from(!bad_sites.is_empty());
        format!("{} need a look", plural(n, "thing", "things"))
    };
    let mut subject = format!("Panda daily, {}: {status}", long_day(now));
    if let Some((a, z)) = eth {
        subject += &format!(" · ETH {:+.1}%", (z / a - 1.0) * 100.0);
    }
    if let Some(d) = live.filter(|d| d.snapshot_ts.is_some()) {
        subject += &format!(" · {} in {}", money(d.positions_usd), plural(d.positions, "position", "positions"));
    }

    writeln!(b, "Hi! Here is how Panda did in the 24 hours up to {}.", when(now))?;
    writeln!(b, "(I only watch and learn: I cannot move money, sign anything or change the keeper.)\n")?;

    // ---------- 1. the short version ----------
    writeln!(b, "THE SHORT VERSION")?;
    if open_alerts.is_empty() && bad_sites.is_empty() {
        writeln!(b, "• Nothing needs your attention.")?;
    } else {
        writeln!(b, "• NEEDS A LOOK:")?;
        for (key, first, _) in &open_alerts {
            writeln!(b, "    - {} (since {})", alert_text(key), when(*first))?;
        }
        if let Some((u, s, n)) = bad_sites.first() {
            writeln!(b, "{}", format!("    - The website had trouble: {u} answered {s} {n}").trim_end())?;
        }
    }
    for d in &vaults {
        let name = vault_name(&d.vault);
        let mut line = match d.snapshot_ts {
            Some(_) => format!(
                "• {}: {} worth {} in total, {} earning rewards right now",
                title(&d.vault, primary),
                plural(d.positions, "position", "positions"),
                money(d.positions_usd),
                d.in_range
            ),
            None => format!("• {}: no recent snapshot from its keeper", capitalise(&name)),
        };
        match activity_words(d) {
            Some(a) => line += &format!(". Today: {a}."),
            None => line += ". A quiet day: no deposits, withdrawals or range moves.",
        }
        writeln!(b, "{line}")?;
    }
    if let Some((a, z)) = eth {
        writeln!(
            b,
            "• ETH {} to {}{}.",
            moved((z / a - 1.0) * 100.0),
            money(z),
            realised.map(|v| format!(", a {} day", vol_words(v))).unwrap_or_default()
        )?;
    }
    if let Some(s) = &shadow7 {
        writeln!(b, "• The A.I {}.", ai_verdict(s))?;
    }

    // ---------- 2. the vaults ----------
    writeln!(b, "\nYOUR VAULTS")?;
    for d in &vaults {
        writeln!(b, "{}", title(&d.vault, primary))?;
        match activity_words(d) {
            Some(a) => writeln!(b, "  In the last 24 hours: {a}.")?,
            None => writeln!(b, "  In the last 24 hours: nothing happened (no deposits, top-ups, withdrawals or range moves).")?,
        }
        match d.snapshot_ts {
            Some(t) => {
                let out = d.positions - d.in_range;
                writeln!(
                    b,
                    "  Right now ({} UTC): {} worth {} together. {}",
                    hm(t),
                    plural(d.positions, "position", "positions"),
                    money(d.positions_usd),
                    if d.positions == 0 {
                        "It is empty.".to_string()
                    } else if out == 0 {
                        "All of them are inside their price range, so they are all earning.".to_string()
                    } else {
                        format!(
                            "{} earning; {} outside {} range for now (paused until the price comes back or the keeper moves {}).",
                            d.in_range,
                            out,
                            if out == 1 { "its" } else { "their" },
                            if out == 1 { "it" } else { "them" }
                        )
                    }
                )?;
            }
            None => match d.newest {
                Some(n) => writeln!(b, "  Its keeper's last report was {} ago ({}).", ago(now - n), when(n))?,
                None => writeln!(b, "  Nothing from its keeper yet.")?,
            },
        }
        if let Some((ts, w)) = d.width {
            if now - ts < 6 * 3600 {
                writeln!(
                    b,
                    "  A new deposit today would get a range of ±{w:.1}% around the price (the keeper picks this from how much ETH is swinging)."
                )?;
            }
        }
    }

    // ---------- 3. the market ----------
    writeln!(b, "\nTHE MARKET")?;
    match eth {
        Some((a, z)) => {
            let mut l = format!("  ETH {} over the day, from {} to {}.", moved((z / a - 1.0) * 100.0), money(a), money(z));
            if let Some(v) = realised {
                l += &format!(
                    " It was a {} day: prices swung about {:.2}% an hour on average (the keeper treats under 0.5% as calm).",
                    vol_words(v),
                    v * 100.0
                );
            }
            writeln!(b, "{l}")?;
        }
        None => writeln!(b, "  No ETH prices were recorded today.")?,
    }
    let mut others = Vec::new();
    for (src, name, digits) in [("cb:BTC-USD", "BTC", 0usize), ("cb:AERO-USD", "AERO", 4)] {
        if let Some((a, z)) = first_last(store, src, since, now)? {
            let price = if digits == 0 { money(z) } else { format!("${z:.digits$}") };
            others.push(format!("{name} {} to {price}", moved((z / a - 1.0) * 100.0)));
        }
    }
    if !others.is_empty() {
        writeln!(b, "  {}.", others.join("; "))?;
    }
    if let Some((_, fng)) = store.series_at("alternative", "fear_greed", now)? {
        writeln!(b, "  The crypto market mood is {} (Fear & Greed {:.0} out of 100).", fear_greed_words(fng), fng)?;
    }
    let (swaps_n, swap_vol, ..) = swap_totals(store, since)?;
    if swaps_n > 0 {
        writeln!(
            b,
            "  Our pool (WETH/USDC on Aerodrome) traded {} in {} swaps.",
            usd_short(swap_vol.unwrap_or(0.0)),
            commas(swaps_n as f64)
        )?;
    }
    if let Some((_, staked, Some(rate), aero)) = pool_obs(store, now) {
        let per_day = rate * 86_400.0;
        let mut l = format!("  Aerodrome pays this pool's stakers {} AERO a day", commas(per_day));
        if let Some(a) = aero {
            l += &format!(" (about {})", money(per_day * a));
        }
        if let Some((_, _, Some(week_rate), _)) = pool_obs(store, now - 7 * 86_400) {
            let ch = (rate / week_rate - 1.0) * 100.0;
            if ch.abs() >= 1.0 {
                l += &format!(", {:.0}% {} than a week ago", ch.abs(), if ch > 0.0 { "more" } else { "less" });
            } else {
                l += ", the same as a week ago";
            }
        }
        if let Some(f) = store.series_at("gauge", "period_finish", now)?.map(|(_, v)| v as i64).filter(|f| *f > now) {
            l += &format!(". The amount can change when the weekly vote ends in {}", ago(f - now));
        }
        writeln!(b, "{l}.")?;
        if staked > 0.0 {
            let share: f64 = vaults.iter().map(|d| d.in_range_liquidity).sum::<f64>() / staked;
            if share > 0.0 {
                let earn = aero
                    .map(|a| format!(", roughly {} a day for everyone in Panda together", money(share * per_day * a)))
                    .unwrap_or_default();
                writeln!(b, "  Panda's earning positions get {} of those rewards right now{earn}.", share_words(share))?;
            }
        }
    }
    if let Some(w) = other_lp_width(store, since)? {
        writeln!(
            b,
            "  Other liquidity providers opened ranges of about ±{w:.1}% on average. Narrower ranges earn more per dollar while the price stays inside, but need moving more often."
        )?;
    }

    // ---------- 4. what the A.I is learning ----------
    writeln!(b, "\nWHAT THE A.I IS LEARNING")?;
    writeln!(
        b,
        "  Every hour I pick my own range width next to the keeper's and check, a day later on the real prices, which would have earned more."
    )?;
    match &shadow7 {
        Some(s) => {
            writeln!(
                b,
                "  Last 7 days: {} decisions checked. We picked the same width {} times.",
                commas(s.n as f64),
                commas(s.same_choice as f64)
            )?;
            writeln!(
                b,
                "  On a $1,000 position the keeper's choices would have made about {} a day and mine about {} — {}.",
                money(s.keeper_mean * 1000.0),
                money(s.ai_mean * 1000.0),
                compare_words(s.ai_mean, s.keeper_mean)
            )?;
            writeln!(
                b,
                "  The best width with perfect hindsight would have made {} a day: that is the ceiling nobody can reach.",
                money(s.hindsight_mean * 1000.0)
            )?;
            if let Some((m, _, mean, beat)) = s.models.first() {
                writeln!(
                    b,
                    "  My most promising method so far is \"{m}\" (about {} a day, ahead of the keeper in {} of {} checks).",
                    money(mean * 1000.0),
                    beat,
                    s.n
                )?;
            }
            if s.days < 30 {
                writeln!(
                    b,
                    "  That is only {} days of evidence. I need about 30 before a lead means anything, so I am not suggesting any change to the keeper yet.",
                    s.days
                )?;
            }
        }
        None => writeln!(b, "  Nothing has been checked yet: each decision needs a full day of prices after it.")?,
    }
    if let Some(v) = forecasts.get(champion.as_str()).or_else(|| forecasts.get("keeper")) {
        writeln!(
            b,
            "  For tomorrow I expect a {} day (about {:.2}% an hour), using the \"{champion}\" forecast, which has been the most accurate lately.",
            vol_words(*v),
            v * 100.0
        )?;
    }
    if fid.choices.1 > 0 {
        let all = fid.choices.0 == fid.choices.1;
        writeln!(
            b,
            "  My copy of the keeper's rules ({}) matched {} of its {} width choices{}.",
            vault_name(primary),
            commas(fid.choices.0 as f64),
            commas(fid.choices.1 as f64),
            if all { ": I understand exactly how it decides" } else { ". Some did not match, so its rules may have changed" }
        )?;
    }

    // ---------- 5. is everything working ----------
    writeln!(b, "\nIS EVERYTHING WORKING?")?;
    let pool_n = store.count("SELECT COUNT(*) FROM pool_obs WHERE ts > ?1", &[&since])?;
    let coin_n = store.count("SELECT COUNT(*) FROM price_1m WHERE source = 'coinbase' AND ts > ?1", &[&since])?;
    let first_obs: Option<i64> = store.db.query_row("SELECT MIN(ts) FROM pool_obs", [], |r| r.get(0))?;
    let pool_target = first_obs.filter(|f| *f > since + 1800).map(|start| (now - start) / 30).unwrap_or(2880).max(1);
    writeln!(
        b,
        "  My data feeds: {:.0}% of the expected pool readings and {:.0}% of the expected ETH prices arrived.",
        (pool_n as f64 / pool_target as f64 * 100.0).min(100.0),
        (coin_n as f64 / 1440.0 * 100.0).min(100.0)
    )?;
    for d in &vaults {
        match d.newest {
            Some(n) if now - n < 3 * 3600 => {
                writeln!(b, "  The keeper of {} is reporting normally (last report {} ago).", vault_name(&d.vault), ago(now - n))?
            }
            Some(n) => writeln!(b, "  The keeper of {} last reported {} ago.", vault_name(&d.vault), ago(now - n))?,
            None => {}
        }
    }
    if bad_sites.is_empty() {
        writeln!(b, "  The website answered every check.")?;
    } else {
        for (u, s, n) in &bad_sites {
            writeln!(b, "  The website had a problem: {u} answered {s} {n}")?;
        }
    }
    let alerts: Vec<(i64, String, String, String)> = {
        let mut st = store.db.prepare("SELECT ts, key, state, message FROM alerts WHERE ts > ?1 ORDER BY ts")?;
        let rows = st.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if alerts.is_empty() {
        writeln!(b, "  No alerts in the last 24 hours.")?;
    } else {
        for (t, _, state, msg) in &alerts {
            if state == "resolved" {
                writeln!(b, "  {} UTC: an alert cleared.", hm(*t))?;
            } else {
                writeln!(b, "  {} UTC: alert: {msg}", hm(*t))?;
            }
        }
    }
    let db_bytes = std::fs::metadata(store.db.path().unwrap_or("")).map(|m| m.len() as f64).unwrap_or(0.0);
    if db_bytes > 0.0 {
        writeln!(
            b,
            "  My database is {} (plenty of room on its disk).",
            if db_bytes >= 1e9 { format!("{:.2} GB", db_bytes / 1e9) } else { format!("{:.0} MB", db_bytes / 1e6) }
        )?;
    }

    // ---------- 6. news and notes ----------
    let news_n = store.count("SELECT COUNT(*) FROM news WHERE ts > ?1", &[&since])?;
    if news_n > 0 {
        writeln!(b, "\nIN THE NEWS (6 of {news_n} crypto headlines I kept today)")?;
        let mut st = store.db.prepare("SELECT source, title FROM news WHERE ts > ?1 ORDER BY ts DESC LIMIT 6")?;
        let rows = st.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for (src, title) in rows.filter_map(|r| r.ok()) {
            let t: String = title.chars().take(110).collect();
            writeln!(b, "  • {t} ({src})")?;
        }
    }
    let notes: Vec<String> = {
        let mut st = store.db.prepare("SELECT text FROM notes WHERE ts > ?1 ORDER BY ts")?;
        let rows = st.query_map([since], |r| r.get(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if !notes.is_empty() {
        writeln!(b, "\nWHAT YOU TOLD ME (last 24 h)")?;
        for n in notes {
            writeln!(b, "  - {n}")?;
        }
    }
    writeln!(b, "\nReply to this email to teach me: I read replies within 10 minutes and keep them with the day's decisions.")?;

    // ---------- technical detail ----------
    writeln!(b, "\n\n============================================================")?;
    writeln!(b, "TECHNICAL DETAIL (for reference; you can skip this)")?;
    writeln!(b, "============================================================")?;
    technical(&mut b, store, since, now, primary, &vaults, &fid)?;
    Ok((subject, b))
}

/// "The new vault (0x6198…93a8, the live one)", "The original vault".
fn title(vault: &str, primary: &str) -> String {
    let name = capitalise(&vault_name(vault));
    match (vault == primary, name.strip_suffix(')')) {
        (true, Some(open)) => format!("{open}, the live one)"),
        (true, None) => format!("{name} (the live one)"),
        (false, _) => name,
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "3 h 20 min", "45 min", "2 days".
fn ago(secs: i64) -> String {
    let m = secs.max(0) / 60;
    if m < 60 {
        format!("{} min", m.max(1))
    } else if m < 48 * 60 {
        if m % 60 == 0 || m >= 10 * 60 {
            format!("{} h", (m + 30) / 60)
        } else {
            format!("{} h {} min", m / 60, m % 60)
        }
    } else {
        format!("{} days", (m + 720) / 1440)
    }
}

/// Mine against the keeper's, per day: "about the same", "mine slightly better", ...
fn compare_words(ai: f64, keeper: f64) -> &'static str {
    let d = (ai - keeper) * 1000.0; // dollars a day on $1,000
    if d.abs() < 0.1 {
        "about the same"
    } else if d > 0.0 {
        if d < 0.5 {
            "mine slightly better"
        } else {
            "mine clearly better"
        }
    } else if d > -0.5 {
        "the keeper's slightly better"
    } else {
        "the keeper's clearly better"
    }
}

/// One line for the short version.
fn ai_verdict(s: &registry::ShadowScore) -> String {
    let agree = if s.n > 0 { s.same_choice as f64 / s.n as f64 } else { 0.0 };
    let tail = if s.days < 30 { "; still learning, so no changes suggested" } else { "" };
    format!(
        "agreed with the keeper's range width {:.0}% of the time this week, and where it differed the result was {}{tail}",
        agree * 100.0,
        compare_words(s.ai_mean, s.keeper_mean)
    )
}

/// Everything the brief used to show, for checking the working. Keeper rows are per vault.
fn technical(
    b: &mut String,
    store: &Store,
    since: i64,
    now: i64,
    primary: &str,
    vaults: &[VaultDay],
    fid: &registry::Fidelity,
) -> Result<()> {
    writeln!(b, "\nMARKET (last 24 h)")?;
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
    pool_section(b, store, since, now, vaults)?;

    writeln!(b, "\nKEEPER ACTIVITY (from each keeper's history in the bucket)")?;
    for d in vaults {
        writeln!(
            b,
            "  {}: deposits {} ({}) · top-ups {} ({}) · rebalances {} (gas {}) · reward payouts {} · withdrawals {}",
            vault_name(&d.vault),
            d.deposits,
            money(d.deposit_usd),
            d.top_ups,
            money(d.top_up_usd),
            d.rebalances,
            money(d.rebalance_gas),
            d.payouts,
            d.withdrawals
        )?;
        if let Some((ts, w)) = d.width {
            writeln!(b, "      latest width table ({} min old): keeper would open at ±{w:.1}%", (now - ts) / 60)?;
        }
    }

    writeln!(b, "\nSHADOW MODE (A.I width choice vs the keeper's, replayed on real prices; decisions follow {})", vault_name(primary))?;
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
        writeln!(b, "  {} UTC: keeper ±{:.1}%, A.I ±{:.1}%", hm(t), k as f64 * 0.5, a as f64 * 0.5)?;
    }

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
    if let Some(s) = crate::replay::the_past(store, now)? {
        b.push_str(&s);
    }

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
    let feed: Option<String> = store
        .db
        .query_row("SELECT note FROM site_checks WHERE url LIKE '%/api/keeper/width' ORDER BY ts DESC LIMIT 1", [], |r| r.get(0))
        .ok();
    if let Some(n) = feed.filter(|n| !n.is_empty()) {
        writeln!(b, "  Keeper deposit-width feed: {n}")?;
    }
    writeln!(
        b,
        "  Keeper-rules check ({}): {} of {} width choices and {} of {} out-of-range clocks reproduced (all history)",
        vault_name(primary),
        fid.choices.0,
        fid.choices.1,
        fid.clocks.0,
        fid.clocks.1
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
    Ok(())
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
    let (subject, body) = build(store, now, &cfg.primary_vault)?;
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
    use crate::collect::bucket::PRIMARY_DEFAULT;

    #[test]
    fn brief_builds_on_empty_and_real_rows() {
        let s = Store::memory().unwrap();
        let (subject, body) = build(&s, 1_790_700_000, PRIMARY_DEFAULT).unwrap();
        assert!(subject.contains("Panda daily") && subject.contains("all normal"), "{subject}");
        assert!(body.contains("I only watch and learn") && body.contains("THE SHORT VERSION") && body.contains("TECHNICAL DETAIL"));
        let ev = std::fs::read_to_string("tests/fixtures/position_events.json").unwrap();
        s.db.execute(
            "INSERT INTO keeper_rows (obj, line, tbl, ts, json, vault) VALUES ('o', 0, 'position_events', 1790679373, ?1, 'old')",
            [ev.trim()],
        )
        .unwrap();
        s.db.execute("INSERT INTO notes (ts, text) VALUES (1790690000, 'too narrow today')", []).unwrap();
        let (_, body) = build(&s, 1_790_700_000, PRIMARY_DEFAULT).unwrap();
        assert!(body.contains("The original vault") && body.contains("1 new deposit ($23.07)"), "{body}");
        assert!(body.contains("deposits 1 ($23.07)"), "technical detail keeps the raw counts: {body}");
        assert!(body.contains("too narrow today"));
    }

    #[test]
    fn plain_words() {
        assert_eq!(moved(0.1), "barely moved (+0.10%)");
        assert_eq!(moved(1.9), "rose 1.90%");
        assert_eq!(moved(-2.11), "fell 2.11%");
        assert_eq!(money(2349.2), "$2,349");
        assert_eq!(money(12.734), "$12.73");
        assert_eq!(commas(1_234_567.0), "1,234,567");
        assert_eq!(vol_words(0.0034), "calm");
        assert_eq!(vol_words(0.007), "fairly busy");
        assert_eq!(vol_words(0.012), "stormy");
        assert_eq!(fear_greed_words(73.0), "greedy");
        assert_eq!(share_words(0.00004), "less than 0.01%");
        assert_eq!(share_words(0.0004), "0.04%");
        assert_eq!(compare_words(0.00059, 0.00064), "about the same", "$0.59 vs $0.64 a day on $1,000");
        assert_eq!(compare_words(0.0012, 0.0005), "mine clearly better");
        assert_eq!(compare_words(0.00040, 0.00064), "the keeper's slightly better");
        assert_eq!(ago(45 * 60), "45 min");
        assert_eq!(ago(3 * 3600 + 20 * 60), "3 h 20 min");
        assert_eq!(ago(36 * 3600), "36 h");
        assert_eq!(ago(3 * 86_400), "3 days");
    }
}
