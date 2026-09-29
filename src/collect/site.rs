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
