//! Alerts: an email the moment something that matters goes wrong, instead of waiting for the daily
//! brief. Checked every 10 minutes. Each alert is sent when it starts, repeated at most every 6
//! hours while it lasts, and followed by a "resolved" email when it clears; no more than 12 alert
//! emails go out in a day however bad things get. Without email settings they are logged and shown
//! in the brief.
//!
//! What is watched (all read-only):
//! * the live vault's keeper heartbeat: the signed deposit-width feed on the website (posted
//!   every 5 minutes, by the new vault's keeper since 2026-10-07) older than 30 minutes means that
//!   keeper process has stopped;
//! * each keeper wallet's ETH on Base, which pays every rebalance's gas: the original vault's keeper
//!   and the keeper of every vault of the multi-vault platform (read from the vault's `keeperBot()`);
//! * the website: the home page and the Worker's health route failing twice in a row;
//! * each keeper's own history, per vault: no new rows for 3 hours while the A.I is reading the
//!   bucket fine;
//! * the keeper's decisions: a runaway-guard skip, or a position left out of range past the crash
//!   valve's 24-hour cap;
//! * the A.I itself: its Base reads, its pool-event feed, its liquidity map, its market data or its
//!   bucket access stalling, and its database approaching the disk budget;
//! * rule drift (the live vault's keeper, AIK_PRIMARY_VAULT): the keeper choosing a width the A.I's copy of its rules would not (its rules have
//!   changed and the shadow replay needs updating).

use crate::config::Config;
use crate::net::{Rpc, Web};
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};

/// The original vault's keeper wallet. Each newer vault's keeper is read from the vault itself
/// (`keeperBot()`, which asks the platform registry), so a vault the platform adds is watched too.
pub const KEEPER_WALLET: &str = "0x940e46DBB87218C1dD9EeecF33DD9C36b0A6e8a8";
/// PandaVault.keeperBot() (verified 2026-10-07: the WETH/USDC vault answers 0x5fb0...af60).
const SEL_KEEPER_BOT: &str = "0x12f3d4a2";

/// How a vault is named in alerts and the brief.
pub fn vault_name(vault: &str) -> String {
    if vault == crate::collect::bucket::OLD {
        "the original vault".into()
    } else if vault.len() == 42 {
        format!("the new vault ({}…{})", &vault[..6], &vault[38..])
    } else {
        format!("vault {vault}")
    }
}

/// The vaults of the multi-vault platform the A.I has keeper history for.
pub fn new_vaults(store: &Store) -> Result<Vec<String>> {
    let mut st = store.db.prepare("SELECT DISTINCT vault FROM keeper_rows WHERE tbl = 'keeper_runs' AND vault != 'old' ORDER BY vault")?;
    let r = st.query_map([], |r| r.get(0))?;
    Ok(r.filter_map(|x| x.ok()).collect())
}
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

    // Keeper gas wallets: the original vault's, and each new vault's keeper as the vault reports it.
    let mut wallets: Vec<(String, String, String)> = vec![("keeper-gas".into(), crate::collect::bucket::OLD.into(), KEEPER_WALLET.into())];
    for v in new_vaults(store)? {
        match rpc.call("eth_call", json!([{ "to": v, "data": SEL_KEEPER_BOT }, "latest"])).await {
            Ok(r) => match r.as_str().filter(|s| s.len() >= 66) {
                Some(word) => wallets.push((format!("keeper-gas:{v}"), v.clone(), format!("0x{}", &word[word.len() - 40..]))),
                None => tracing::warn!("alerts: keeperBot() of {v} answered {r}"),
            },
            Err(e) => tracing::warn!("alerts: keeperBot() of {v}: {e:#}"),
        }
    }
    for (key, vault, wallet) in &wallets {
        match rpc.call("eth_getBalance", json!([wallet, "latest"])).await {
            Ok(v) => {
                let wei = v.as_str().and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(u128::MAX);
                let eth = wei as f64 / 1e18;
                if eth < KEEPER_MIN_ETH {
                    out.push(f(key, format!(
                        "The keeper of {} ({wallet}) holds {eth:.5} ETH on Base, below {KEEPER_MIN_ETH} ETH. Its rebalances will start failing when it runs out. Top it up.",
                        vault_name(vault)
                    )));
                }
            }
            Err(e) => tracing::warn!("alerts: keeper balance {wallet}: {e:#}"),
        }
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

    // The keepers' own history, the A.I's own feeds. Each keeper writes every minute; the A.I reads
    // every file (the per-minute tables), so the newest of those is the freshest signal, per vault.
    let bucket_ok: i64 = store.meta_get("bucket_ok_ts")?.and_then(|v| v.parse().ok()).unwrap_or(0);
    if cfg.bucket.is_some() {
        if bucket_ok > 0 && now - bucket_ok > 3 * 3600 {
            out.push(f(
                "ai-bucket",
                format!("The A.I has not been able to read the keeper's history bucket for {} hours.", (now - bucket_ok) / 3600),
            ));
        }
        let old = crate::collect::bucket::OLD;
        let mut vaults = vec![old.to_string()];
        vaults.extend(new_vaults(store)?);
        for v in &vaults {
            let newest: Option<i64> = if v == old {
                store.db.query_row(
                    "SELECT MAX(t) FROM (SELECT MAX(ts) AS t FROM keeper_rows WHERE vault = 'old' AND tbl IN ('width_evals', 'keeper_decisions')
                                         UNION ALL SELECT MAX(ts) FROM width_min UNION ALL SELECT MAX(ts) FROM decision_min WHERE vault = 'old')",
                    [],
                    |r| r.get(0),
                )?
            } else {
                store.db.query_row(
                    "SELECT MAX(t) FROM (SELECT MAX(ts) AS t FROM keeper_rows WHERE vault = ?1 AND tbl IN ('width_evals', 'keeper_decisions')
                                         UNION ALL SELECT MAX(ts) FROM width_min_v WHERE vault = ?1 UNION ALL SELECT MAX(ts) FROM decision_min WHERE vault = ?1)",
                    [v],
                    |r| r.get(0),
                )?
            };
            // Only meaningful right after a successful sync: the bucket is read every 30 minutes.
            if let Some(n) = newest {
                if bucket_ok > n && bucket_ok - n > 3 * 3600 {
                    let key = if v == old { "keeper-history".to_string() } else { format!("keeper-history:{v}") };
                    out.push(f(&key, format!(
                        "The keeper of {}: its newest history row is {} hours older than the A.I's last successful read, so it has stopped writing its log (it writes every minute).",
                        vault_name(v),
                        (bucket_ok - n) / 3600
                    )));
                }
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
    // The pool trades every few seconds, so 30 minutes without a recorded swap means the event feed stalled.
    let last_swap: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM swaps", [], |r| r.get(0))?;
    if last_swap.map(|t| now - t > 30 * 60).unwrap_or(false) {
        out.push(f("ai-flow", "The A.I has recorded no pool events for over 30 minutes (its public RPC log reads are failing).".into()));
    }
    let last_map: Option<i64> = store.db.query_row("SELECT MAX(ts) FROM tick_liquidity", [], |r| r.get(0))?;
    if last_map.map(|t| now - t > 2 * 3600).unwrap_or(false) {
        out.push(f("ai-liquidity-map", "The A.I's liquidity map of the pool is over 2 hours old (taken every 10 minutes).".into()));
    }
    // The database against its disk budget (the VM's disk is 30 GB; the rest is the system).
    let db_bytes: u64 =
        ["", "-wal"].iter().filter_map(|sfx| std::fs::metadata(format!("{}{sfx}", cfg.db_path().display())).ok()).map(|m| m.len()).sum();
    let budget = cfg.disk_budget_gb * 1e9;
    if budget > 0.0 && db_bytes as f64 > 0.8 * budget {
        out.push(f("ai-disk", format!(
            "The A.I's database is {:.1} GB, over 80% of its {:.0} GB budget. Grow the VM disk (about $0.04 per GB a month) or raise AIK_DISK_BUDGET_GB.",
            db_bytes as f64 / 1e9, cfg.disk_budget_gb
        )));
    }

    // Keeper decisions of the last day (every one, from decision_min, plus the older sampled rows):
    // runaway guard, valve cap exceeded.
    let mut st = store.db.prepare(
        "SELECT json_set(json, '$.vault', vault) FROM keeper_rows WHERE tbl = 'keeper_decisions' AND ts > ?1
         UNION ALL
         SELECT json_object('token_id', token_id, 'decision', decision, 'reason', reason, 'outside_secs', outside_secs, 'vault', vault)
           FROM decision_min WHERE ts > ?1",
    )?;
    let decisions: Vec<Value> = st
        .query_map([now - 86_400], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    let in_vault = |d: &Value| vault_name(d["vault"].as_str().unwrap_or(crate::collect::bucket::OLD));
    if let Some(d) =
        decisions.iter().find(|d| d["decision"] == "skipped" && d["reason"].as_str().unwrap_or("").contains("MAX_REBALANCES_PER_DAY"))
    {
        out.push(f(
            "keeper-runaway",
            format!(
                "The keeper of {} hit its runaway guard: {} (position {}).",
                in_vault(d),
                d["reason"].as_str().unwrap_or(""),
                d["token_id"]
            ),
        ));
    }
    if let Some(d) = decisions.iter().find(|d| d["outside_secs"].as_f64().unwrap_or(0.0) > 26.0 * 3600.0) {
        out.push(f(
            "keeper-stuck",
            format!(
                "Position {} in {} has been out of range for {:.1} hours, past the keeper's 24-hour crash-valve cap. It should have been re-centred.",
                d["token_id"],
                in_vault(d),
                d["outside_secs"].as_f64().unwrap_or(0.0) / 3600.0
            ),
        ));
    }

    // Rule drift, checked on the last day's width tables of the keeper the A.I follows.
    let mut st = store.db.prepare("SELECT ts, json FROM keeper_rows WHERE tbl = 'width_evals' AND vault = ?2 AND ts > ?1")?;
    let tables: Vec<(i64, String)> =
        st.query_map(params![now - 86_400, cfg.primary_vault], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(|r| r.ok()).collect();
    let (mut n, mut ok) = (0, 0);
    for (ts, raw) in &tables {
        let Some(t) = serde_json::from_str::<Value>(raw).ok().and_then(|v| crate::model::shadow::Table::from_keeper(&v)) else { continue };
        let p = crate::registry::policy_at(store, *ts, &cfg.primary_vault)?;
        let nets: Vec<(i64, f64)> = t.rows.iter().map(|r| (r.h as i64, r.net)).collect();
        n += 1;
        if crate::model::shadow::choose(&t, &nets, &p) == t.choice {
            ok += 1;
        }
    }
    if n >= 6 && ok < n {
        out.push(f("rule-drift", format!(
            "The keeper of {} chose a width the A.I's copy of its rules would not in {} of {n} tables in the last day. The keeper's rules have changed; the A.I's shadow replay needs updating.",
            vault_name(&cfg.primary_vault),
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
