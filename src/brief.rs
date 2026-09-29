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
        writeln!(b, "  Current champion: {}", registry::champion(store, now)?)?;
    }

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

    // 5. Health.
    writeln!(b, "\nHEALTH")?;
    let pool_n = store.count("SELECT COUNT(*) FROM pool_obs WHERE ts > ?1", &[&since])?;
    let coin_n = store.count("SELECT COUNT(*) FROM price_1m WHERE source = 'coinbase' AND ts > ?1", &[&since])?;
    let rows_n = store.count("SELECT COUNT(*) FROM keeper_rows WHERE ts > ?1", &[&since])?;
    writeln!(b, "  Pool reads {pool_n} (target ~2,880) · Coinbase minutes {coin_n} (target 1,440) · keeper rows {rows_n}")?;
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
    writeln!(b, "\nReply to this email or run `aikeeper note \"...\"` to teach it; notes are kept with the day's decisions.")?;
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
