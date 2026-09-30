//! Alerts: an email the moment something that matters goes wrong, instead of waiting for the daily
//! brief. Checked every 10 minutes. Each alert is sent when it starts, repeated at most every 6
//! hours while it lasts, and followed by a "resolved" email when it clears; no more than 12 alert
//! emails go out in a day however bad things get. Without email settings they are logged and shown
//! in the brief.
//!
//! What is watched (all read-only):
//! * the live keeper's heartbeat: its signed deposit-width feed on the website (posted every
//!   5 minutes) older than 30 minutes means the keeper process has stopped;
//! * the keeper wallet's ETH on Base, which pays every rebalance's gas;
//! * the website: the home page and the Worker's health route failing twice in a row;
//! * the keeper's own history: no new rows for 3 hours while the A.I is reading the bucket fine;
//! * the keeper's decisions: a runaway-guard skip, or a position left out of range past the crash
//!   valve's 24-hour cap;
//! * the A.I itself: its Base reads, its market data, or its bucket access stalling;
//! * rule drift: the keeper choosing a width the A.I's copy of its rules would not (its rules have
//!   changed and the shadow replay needs updating).

use crate::config::Config;
use crate::net::{Rpc, Web};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};

pub const KEEPER_WALLET: &str = "0x940e46DBB87218C1dD9EeecF33DD9C36b0A6e8a8";
/// Below this the keeper may soon be unable to pay for a rebalance (~$0.03 each at 2026 gas).
pub const KEEPER_MIN_ETH: f64 = 0.001;
const RESEND_SECS: i64 = 6 * 3600;
const MAX_PER_DAY: i64 = 12;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub key: String,
    pub message: String,
}

fn f(key: &str, message: String) -> Finding {
    Finding { key: key.to_string(), message }
}

/// Everything wrong right now. Network checks that themselves fail are reported as findings only
/// when that failure is informative (the website); otherwise they are skipped this round.
pub async fn evaluate(cfg: &Config, store: &Store, web: &Web, rpc: &Rpc, now: i64) -> Result<Vec<Finding>> {
    let mut out = Vec::new();

    // Keeper heartbeat via its deposit-width feed.
    let feed_url = format!("{}/api/keeper/width", cfg.site.trim_end_matches('/'));
    match web.get_json(&feed_url, &[]).await {
        Ok(v) => match v["age_secs"].as_f64() {
            Some(age) if age > 1800.0 => out.push(f("keeper-heartbeat", format!(
                "The live keeper's deposit-width feed is {:.0} minutes old (it posts every 5). The keeper process has probably stopped: positions are not being watched. Check Railway.",
                age / 60.0
            ))),
            None => out.push(f("keeper-heartbeat", "The live keeper's deposit-width feed is empty. The keeper may not be running.".into())),
            _ => {}
        },
        Err(e) => tracing::warn!("alerts: width feed: {e:#}"),
    }

    // Keeper gas wallet.
    match rpc.call("eth_getBalance", json!([KEEPER_WALLET, "latest"])).await {
        Ok(v) => {
            let wei = v.as_str().and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(u128::MAX);
            let eth = wei as f64 / 1e18;
            if eth < KEEPER_MIN_ETH {
                out.push(f("keeper-gas", format!(
                    "The keeper wallet ({KEEPER_WALLET}) holds {eth:.5} ETH on Base, below {KEEPER_MIN_ETH} ETH. Rebalances will start failing when it runs out. Top it up."
                )));
            }
        }
        Err(e) => tracing::warn!("alerts: keeper balance: {e:#}"),
    }

    // Website: two consecutive failures of the same URL.
    for path in ["/", "/api/health"] {
        let url = format!("{}{}", cfg.site.trim_end_matches('/'), path);
        let status = match web.get_raw(&url, &[]).await {
            Ok((s, _)) => s,
            Err(_) => 0,
        };
        let key = format!("site-fail:{path}");
        let prev: i64 = store.meta_get(&key)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if (200..400).contains(&status) {
            store.meta_set(&key, "0")?;
        } else {
            store.meta_set(&key, &(prev + 1).to_string())?;
            if prev + 1 >= 2 {
                out.push(f(&format!("site:{path}"), format!("pandafinancial.org{path} is failing (HTTP {status}, twice in a row).")));
            }
        }
    }

    // The keeper's own history, the A.I's own feeds.
    let newest_keeper: Option<i64> =
        store.db.query_row("SELECT MAX(ts) FROM keeper_rows WHERE tbl IN ('width_evals', 'keeper_decisions')", [], |r| r.get(0))?;
    let bucket_ok: i64 = store.meta_get("bucket_ok_ts")?.and_then(|v| v.parse().ok()).unwrap_or(0);
    if cfg.bucket.is_some() {
        if bucket_ok > 0 && now - bucket_ok > 13 * 3600 {
            out.push(f(
                "ai-bucket",
                format!("The A.I has not been able to read the keeper's history bucket for {} hours.", (now - bucket_ok) / 3600),
            ));
        }
        if let Some(n) = newest_keeper {
            // Only meaningful right after a successful sync: the bucket is read every 6 h.
            if bucket_ok > n && bucket_ok - n > 3 * 3600 {
                out.push(f("keeper-history", format!(
                    "The keeper's newest history row is {} hours older than the A.I's last successful read: the keeper has stopped writing its log (it writes every minute).",
                    (bucket_ok - n) / 3600
                )));
            }
        }
    }
    let last_pool: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM pool_obs", [], |r| r.get(0))?;
    if last_pool.map(|t| now - t > 15 * 60).unwrap_or(false) {
        out.push(f("ai-chain", "The A.I has not read the pool on Base for over 15 minutes (its public RPC endpoints are failing).".into()));
    }
    let last_coin: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM price_1m WHERE source = 'coinbase'", [], |r| r.get(0))?;
    if last_coin.map(|t| now - t > 45 * 60).unwrap_or(false) {
        out.push(f("ai-market", "The A.I has had no Coinbase ETH prices for over 45 minutes.".into()));
    }

    // Keeper decisions ingested in the last day: runaway guard, valve cap exceeded.
    let mut st = store.db.prepare("SELECT json FROM keeper_rows WHERE tbl = 'keeper_decisions' AND ts > ?1")?;
    let decisions: Vec<Value> = st
        .query_map([now - 86_400], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    if let Some(d) =
        decisions.iter().find(|d| d["decision"] == "skipped" && d["reason"].as_str().unwrap_or("").contains("MAX_REBALANCES_PER_DAY"))
    {
        out.push(f(
            "keeper-runaway",
            format!("The keeper hit its runaway guard: {} (position {}).", d["reason"].as_str().unwrap_or(""), d["token_id"]),
        ));
    }
    if let Some(d) = decisions.iter().find(|d| d["outside_secs"].as_f64().unwrap_or(0.0) > 26.0 * 3600.0) {
        out.push(f(
            "keeper-stuck",
            format!(
            "Position {} has been out of range for {:.1} hours, past the keeper's 24-hour crash-valve cap. It should have been re-centred.",
            d["token_id"],
            d["outside_secs"].as_f64().unwrap_or(0.0) / 3600.0
        ),
        ));
    }

    // Rule drift, checked on the last day's width tables.
    let mut st = store.db.prepare("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND ts > ?1")?;
    let tables: Vec<(i64, String)> = st.query_map([now - 86_400], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
    let (mut n, mut ok) = (0, 0);
    for (ts, raw) in &tables {
        let Some(t) = serde_json::from_str::<Value>(raw).ok().and_then(|v| crate::model::shadow::Table::from_keeper(&v)) else { continue };
        let p = crate::registry::policy_at(store, *ts)?;
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, r.net)).collect();
        n += 1;
        if crate::model::shadow::choose(&t, &nets, &p) == t.choice {
            ok += 1;
        }
    }
    if n >= 6 && ok < n {
        out.push(f("rule-drift", format!(
            "The keeper chose a width the A.I's copy of its rules would not in {} of {n} tables in the last day. The keeper's rules have changed; the A.I's shadow replay needs updating.",
            n - ok
        )));
    }
    Ok(out)
}

/// What to send now, given what is wrong now and what was open before. Pure, so it is unit-tested.
/// `open` is (key, first_seen, last_sent). Returns (to_send, still_open, resolved).
#[allow(clippy::type_complexity)]
pub fn plan(findings: &[Finding], open: &[(String, i64, i64)], now: i64) -> (Vec<(Finding, bool)>, Vec<(String, i64, i64)>, Vec<String>) {
    let mut send = Vec::new();
    let mut still = Vec::new();
    for fi in findings {
        match open.iter().find(|o| o.0 == fi.key) {
            None => {
                send.push((fi.clone(), true));
                still.push((fi.key.clone(), now, now));
            }
            Some((_, first, last)) if now - last >= RESEND_SECS => {
                send.push((fi.clone(), false));
                still.push((fi.key.clone(), *first, now));
            }
            Some(o) => still.push(o.clone()),
        }
    }
    let resolved: Vec<String> = open.iter().filter(|o| !findings.iter().any(|fi| fi.key == o.0)).map(|o| o.0.clone()).collect();
    (send, still, resolved)
}

/// One round: evaluate, email what is new (within the daily cap), record everything.
pub async fn run(cfg: &Config, store: &Store, web: &Web, rpc: &Rpc, now: i64) -> Result<usize> {
    let findings = evaluate(cfg, store, web, rpc, now).await?;
    let open: Vec<(String, i64, i64)> =
        serde_json::from_str(&store.meta_get("alerts_open")?.unwrap_or_else(|| "[]".into())).unwrap_or_default();
    let (send, still, resolved) = plan(&findings, &open, now);
    let day = now / 86_400;
    let sent_today: i64 = store
        .meta_get("alerts_sent_day")?
        .and_then(|v| v.split_once(':').and_then(|(d, n)| (d.parse::<i64>().ok() == Some(day)).then(|| n.parse().ok()).flatten()))
        .unwrap_or(0);
    let mut sent = 0;
    let mut outbox: Vec<(String, String)> = Vec::new();
    for (fi, first) in &send {
        store.db.execute("INSERT INTO alerts (ts, key, state, message) VALUES (?1, ?2, 'open', ?3)", params![now, fi.key, fi.message])?;
        outbox.push((format!("ALERT{}: {}", if *first { "" } else { " (still)" }, fi.key), fi.message.clone()));
    }
    for key in &resolved {
        store.db.execute("INSERT INTO alerts (ts, key, state, message) VALUES (?1, ?2, 'resolved', 'cleared')", params![now, key])?;
        outbox.push((format!("Resolved: {key}"), format!("The alert \"{key}\" has cleared.")));
    }
    for (subject, body) in outbox {
        if sent_today + sent >= MAX_PER_DAY {
            tracing::warn!("alerts: daily email cap reached; logged only: {subject}");
            continue;
        }
        match &cfg.email {
            Some(e) => match crate::brief::send(
                e,
                &format!("Panda A.I Keeper — {subject}"),
                &format!(
                    "{body}\n\n(The A.I Keeper is watch-only; it cannot act on this. Sent {}.)",
                    chrono::DateTime::from_timestamp(now, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
                ),
            )
            .await
            {
                Ok(()) => sent += 1,
                Err(err) => tracing::warn!("alerts: email failed: {err:#}"),
            },
            None => tracing::warn!("alert (no email configured): {subject}: {body}"),
        }
    }
    store.meta_set("alerts_open", &serde_json::to_string(&still)?)?;
    store.meta_set("alerts_sent_day", &format!("{day}:{}", sent_today + sent))?;
    Ok(findings.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fi(k: &str) -> Finding {
        Finding { key: k.into(), message: format!("{k} is wrong") }
    }

    #[test]
    fn new_alert_is_sent_once_then_resent_after_six_hours_then_resolved() {
        let (send, open, res) = plan(&[fi("a")], &[], 1000);
        assert_eq!(send.len(), 1);
        assert!(send[0].1, "first notice");
        assert!(res.is_empty());
        let (send, open2, _) = plan(&[fi("a")], &open, 1000 + 600);
        assert!(send.is_empty(), "not repeated within 6 h");
        let (send, open3, _) = plan(&[fi("a")], &open2, 1000 + RESEND_SECS);
        assert_eq!(send.len(), 1);
        assert!(!send[0].1, "a reminder, not a first notice");
        assert_eq!(open3[0].1, 1000, "first-seen is kept");
        let (send, open4, res) = plan(&[], &open3, 1000 + RESEND_SECS + 600);
        assert!(send.is_empty());
        assert!(open4.is_empty());
        assert_eq!(res, vec!["a".to_string()]);
    }

    #[test]
    fn independent_alerts() {
        let (_, open, _) = plan(&[fi("a"), fi("b")], &[], 0);
        let (send, open, res) = plan(&[fi("b"), fi("c")], &open, 60);
        assert_eq!(send.len(), 1, "only c is new");
        assert_eq!(send[0].0.key, "c");
        assert_eq!(res, vec!["a".to_string()]);
        assert_eq!(open.len(), 2);
    }
}
