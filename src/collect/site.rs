//! Watches the Panda website from the outside, GET only. The Worker's POST routes write to
//! production (a referral row was once created by a test), so the A.I never sends anything but GET.

use crate::net::Web;
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;
use std::time::Instant;

/// Pages and read-only API routes checked every hour.
pub const PATHS: &[&str] =
    &["/", "/dashboard/", "/defi/swap/", "/docs/whitepaper/", "/learn/", "/api/health", "/api/keeper/width", "/api/defi/rates"];

pub struct Check {
    pub url: String,
    pub status: u16,
    pub ms: u128,
    pub note: String,
}

pub async fn check_all(web: &Web, store: &Store, base: &str, now: i64) -> Result<Vec<Check>> {
    let mut out = Vec::new();
    for p in PATHS {
        let url = format!("{}{}", base.trim_end_matches('/'), p);
        let t0 = Instant::now();
        let (status, note) = match web.get_raw(&url, &[]).await {
            Ok((s, body)) => {
                let note = if p.starts_with("/api/keeper/width") && s == 200 {
                    // The keeper signs a fresh width every five minutes; stale means the feed stopped.
                    // Reply: { width: {...}, age_secs } (worker/api.ts keeperWidthGet).
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                    match v["age_secs"].as_f64() {
                        Some(a) => format!("width feed age {} min", (a / 60.0).round()),
                        None => "width feed empty".into(),
                    }
                } else if s == 403 && body.windows(12).any(|w| w == b"verification") {
                    "Cloudflare bot check (not an outage)".into()
                } else {
                    String::new()
                };
                (s, note)
            }
            Err(e) => (0, format!("{e:#}").chars().take(200).collect()),
        };
        let ms = t0.elapsed().as_millis();
        store.db.execute(
            "INSERT INTO site_checks (ts, url, status, ms, note) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, url, status, ms as i64, note],
        )?;
        out.push(Check { url, status, ms, note });
    }
    Ok(out)
}

/// The website's own daily digest and weekly agent test, written by Claude on the Worker (worker/ops.ts) and read with
/// the A.I's token (`AIK_SITE_REPORT_TOKEN`), GET only. `None` when no token is set; a failed read is one line.
pub async fn ops_section(web: &Web, base: &str, token: Option<&str>, now: i64) -> Option<String> {
    let token = token?;
    let url = format!("{}/api/ops/report", base.trim_end_matches('/'));
    let auth = format!("Bearer {token}");
    Some(match web.get_json(&url, &[("authorization", auth.as_str())]).await {
        Ok(v) => ops_words(&v, now),
        Err(e) => {
            let why: String = format!("{e:#}").chars().take(160).collect();
            format!("THE WEBSITE: no report today (could not read it: {why}).\n\n")
        }
    })
}

/// The report in the brief's words: today's digest, and the agent test on the day it ran.
pub fn ops_words(v: &serde_json::Value, now: i64) -> String {
    let today = crate::brief::day(now);
    let (d, a) = (&v["digest"], &v["agent_test"]);
    let mut b = match (d["day"].as_str(), d["text"].as_str()) {
        _ if v["configured"] == false => "THE WEBSITE: off (the site's Claude key is not set).\n".to_string(),
        (Some(day), Some(text)) if day == today => {
            format!("THE WEBSITE (Claude's look at the site's own records for the last 24 hours)\n{}\n", text.trim())
        }
        (Some(day), None) if day == today => {
            format!("THE WEBSITE: no digest today ({}).\n", d["note"].as_str().unwrap_or("no answer recorded"))
        }
        (Some(day), _) => format!("THE WEBSITE: no digest for today yet (the newest is from {day}).\n"),
        _ => "THE WEBSITE: no digest yet.\n".to_string(),
    };
    if a["day"].as_str() == Some(today.as_str()) {
        b += &match a["text"].as_str() {
            Some(t) => format!("\nAGENT TEST (weekly: a Claude agent used the agent API the way a customer would)\n{}\n", t.trim()),
            None => format!("\nAGENT TEST: no report this week ({}).\n", a["note"].as_str().unwrap_or("no answer recorded")),
        };
    }
    if let (Some(m), Some(c)) = (v["month_usd"].as_f64(), v["cap_usd"].as_f64()) {
        b += &format!("(Website Claude: ${m:.2} of this month's ${c:.2} limit.)\n");
    }
    b + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ops_report_in_plain_words() {
        let now = 1_792_400_000; // 2026-10-19
        let v = json!({"configured": true, "month_usd": 1.234, "cap_usd": 8.0,
            "digest": {"day": "2026-10-19", "text": "PROBLEMS USERS HIT\n• none", "note": null},
            "agent_test": {"day": "2026-10-19", "text": "RESULT: PASS", "note": null}});
        let s = ops_words(&v, now);
        assert!(s.starts_with("THE WEBSITE (") && s.contains("• none") && s.contains("AGENT TEST") && s.contains("RESULT: PASS"), "{s}");
        assert!(s.contains("$1.23 of this month's $8.00 limit"));
        // Yesterday's digest and last week's test: said once, not repeated.
        let old = json!({"configured": true, "digest": {"day": "2026-10-18", "text": "x"}, "agent_test": {"day": "2026-10-13", "text": "RESULT: PASS"}});
        let s = ops_words(&old, now);
        assert!(s.contains("no digest for today yet (the newest is from 2026-10-18)") && !s.contains("AGENT TEST"), "{s}");
        let off = json!({"configured": false, "digest": null, "agent_test": null});
        assert!(ops_words(&off, now).contains("off (the site's Claude key is not set)"));
        let failed = json!({"configured": true, "digest": {"day": "2026-10-19", "text": null, "note": "the API credits are used up until they refill"}});
        assert!(ops_words(&failed, now).contains("no digest today (the API credits are used up"));
    }
}
