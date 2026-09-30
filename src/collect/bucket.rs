//! Reads the live keeper's history bucket (gs://pfkeeperdata), read-only.
//!
//! Access: on the Google Cloud VM, a token for the VM's own service account comes from the metadata
//! server; that account holds only `roles/storage.objectViewer` on this one bucket, so the A.I can
//! list and read and nothing else. On a laptop, `AIK_GCS_TOKEN` can carry a short-lived token.
//!
//! Free-tier budget: Cloud Storage's always-free allowance is 5,000 list and 50,000 read operations
//! a month. The keeper writes a file per table per MINUTE, so reading everything would cost ~260,000
//! reads a month. `lean` mode (the default) therefore reads:
//!   * position_events and keeper_runs in full (sparse: files exist only when something happened),
//!   * width_evals: one file per 10 minutes (the newest in each 10-minute slot). The shadow replay
//!     needs the table the keeper acted on: with hourly tables a rebalance's new width came out
//!     wrong on real data (2026-09-29: +/-6% vs the keeper's +/-4.5%); with the minute tables it is
//!     exact, and the keeper's table moves little inside 10 minutes. ~4,300 reads a month;
//!   * keeper_decisions: one file per hour (the newest in each hour folder),
//!   * market_ticks: nothing live (the A.I reads the pool itself every 30 s), except in `History`
//!     mode (the one-time `backfill`), which reads every market_ticks file once to recover the
//!     keeper's minute-by-minute price path since its first day (~1,440 files a day, ~22,000 for
//!     the first fortnight: a one-time cost inside the 50,000 free reads).
//!
//! Live that is roughly 1,500 reads and 1,500 lists a month, inside the free allowance. There is
//! deliberately no "read everything" mode: position_snapshots alone would be ~43,000 reads a month.

use crate::net::{redact, Web};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use rusqlite::params;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

pub const TABLES: &[&str] = &["market_ticks", "width_evals", "keeper_decisions", "position_events", "position_snapshots", "keeper_runs"];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// Live: the free-tier plan described above.
    Lean,
    /// One-time backfill: Lean plus every market_ticks file.
    History,
}

pub struct Bucket<'a> {
    pub web: &'a Web,
    pub name: String,
    pub token: Option<String>,
}

impl<'a> Bucket<'a> {
    async fn token(&self) -> Result<String> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let v = self
            .web
            .get_json(
                "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token",
                &[("Metadata-Flavor", "Google")],
            )
            .await
            .context("no AIK_GCS_TOKEN and no Google Cloud metadata server (run on the VM, or set AIK_GCS_TOKEN)")?;
        v["access_token"].as_str().map(str::to_string).ok_or_else(|| anyhow!("metadata token reply without access_token"))
    }

    /// Object names under a prefix (handles paging).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let token = self.token().await?;
        let auth = format!("Bearer {token}");
        let mut names = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "https://storage.googleapis.com/storage/v1/b/{}/o?prefix={}&fields=items(name),nextPageToken&maxResults=1000",
                self.name,
                urlencoding::encode(prefix)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", urlencoding::encode(p)));
            }
            let v = self.web.get_json(&url, &[("Authorization", &auth)]).await?;
            for it in v["items"].as_array().into_iter().flatten() {
                if let Some(n) = it["name"].as_str() {
                    names.push(n.to_string());
                }
            }
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => break,
            }
        }
        Ok(names)
    }

    pub async fn read(&self, name: &str) -> Result<Vec<u8>> {
        let token = self.token().await?;
        let url = format!("https://storage.googleapis.com/storage/v1/b/{}/o/{}?alt=media", self.name, urlencoding::encode(name));
        let (status, body) = self.web.get_raw(&url, &[("Authorization", &format!("Bearer {token}"))]).await?;
        if status != 200 {
            bail!("GET {} -> HTTP {status}", redact(&url));
        }
        Ok(body)
    }
}

/// True when the bucket can be listed right now (used to record a healthy sync).
pub async fn readable(bucket: &Bucket<'_>) -> bool {
    bucket.list("v1/keeper_runs/").await.is_ok()
}

/// Decodes a gzipped JSONL file into rows.
pub fn parse_jsonl_gz(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut s = String::new();
    GzDecoder::new(bytes).read_to_string(&mut s)?;
    Ok(s.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// Stores rows of one object; returns how many were new.
pub fn store_rows(store: &Store, table: &str, obj: &str, rows: &[Value]) -> Result<usize> {
    let tx = store.db.unchecked_transaction()?;
    let mut n = 0;
    for (i, r) in rows.iter().enumerate() {
        let ts = r["ts"].as_i64().or_else(|| r["ts"].as_f64().map(|f| f as i64)).unwrap_or(0);
        n += tx.execute(
            "INSERT OR IGNORE INTO keeper_rows (obj, line, tbl, ts, json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![obj, i as i64, table, ts, r.to_string()],
        )?;
    }
    tx.execute("INSERT OR REPLACE INTO ingested (obj, ts, rows) VALUES (?1, strftime('%s','now'), ?2)", params![obj, rows.len() as i64])?;
    tx.commit()?;
    Ok(n)
}

/// Which objects in a day's listing to read under a mode. Objects are named
/// `v1/<table>/dt=YYYY-MM-DD/hour=HH/keeper-<unix>-<seq>.jsonl.gz`.
pub fn select(table: &str, names: &[String], mode: Mode) -> Vec<String> {
    if matches!(table, "position_events" | "keeper_runs") || (mode == Mode::History && table == "market_ticks") {
        return names.to_vec();
    }
    if table == "market_ticks" || table == "position_snapshots" {
        return vec![];
    }
    // width_evals: the newest file in each 10-minute slot; everything else: the newest per hour.
    // File names are `.../hour=HH/keeper-<unix seconds>-<seq>.jsonl.gz`.
    let slot_secs = if table == "width_evals" { 600 } else { 3600 };
    let mut by_slot: BTreeMap<String, (i64, String)> = BTreeMap::new();
    for n in names {
        let (folder, file) = n.rsplit_once('/').unwrap_or(("", n.as_str()));
        let unix: i64 = file.strip_prefix("keeper-").and_then(|r| r.split('-').next()).and_then(|u| u.parse().ok()).unwrap_or(0);
        let key = format!("{folder}/{}", unix / slot_secs);
        let e = by_slot.entry(key).or_insert((i64::MIN, String::new()));
        if unix > e.0 || (unix == e.0 && *n > e.1) {
            *e = (unix, n.clone());
        }
    }
    by_slot.into_values().map(|(_, n)| n).collect()
}

/// Syncs the given days (YYYY-MM-DD) of every table. Returns (objects read, rows stored).
pub async fn sync_days(bucket: &Bucket<'_>, store: &Store, days: &[String], mode: Mode) -> Result<(usize, usize)> {
    let (mut objs, mut rows) = (0, 0);
    for table in TABLES {
        for day in days {
            let prefix = format!("v1/{table}/dt={day}/");
            let names = match bucket.list(&prefix).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!("bucket list {prefix}: {e:#}");
                    continue;
                }
            };
            for name in select(table, &names, mode) {
                let seen = store.count("SELECT COUNT(*) FROM ingested WHERE obj = ?1", &[&name])?;
                if seen > 0 {
                    continue;
                }
                match bucket.read(&name).await.and_then(|b| parse_jsonl_gz(&b)) {
                    Ok(parsed) => {
                        rows += store_rows(store, table, &name, &parsed)?;
                        objs += 1;
                    }
                    Err(e) => tracing::warn!("bucket read {name}: {e:#}"),
                }
            }
        }
    }
    Ok((objs, rows))
}

pub fn days_between(from: &str, to_ts: i64) -> Vec<String> {
    let Ok(start) = chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d") else { return vec![] };
    let end = chrono::DateTime::from_timestamp(to_ts, 0).map(|d| d.date_naive()).unwrap_or(start);
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        out.push(d.format("%Y-%m-%d").to_string());
        match d.succ_opt() {
            Some(next) => d = next,
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    #[test]
    fn lean_selection() {
        let names: Vec<String> = [
            "v1/width_evals/dt=2026-09-29/hour=10/keeper-100-1.jsonl.gz",
            "v1/width_evals/dt=2026-09-29/hour=10/keeper-160-2.jsonl.gz",
            "v1/width_evals/dt=2026-09-29/hour=11/keeper-3700-3.jsonl.gz",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let s = select("width_evals", &names, Mode::Lean);
        assert_eq!(s, vec![names[1].clone(), names[2].clone()], "100 and 160 share a 10-minute slot; 3700 is another");
        let decisions: Vec<String> = names.iter().map(|n| n.replace("width_evals", "keeper_decisions")).collect();
        assert_eq!(select("keeper_decisions", &decisions, Mode::Lean).len(), 2, "one per hour folder");
        let ten: Vec<String> =
            (0..6).map(|i| format!("v1/width_evals/dt=2026-09-29/hour=10/keeper-{}-{i}.jsonl.gz", 36_000 + i * 600)).collect();
        assert_eq!(select("width_evals", &ten, Mode::Lean).len(), 6, "one per 10 minutes");
        assert!(select("market_ticks", &names, Mode::Lean).is_empty());
        assert_eq!(select("position_events", &names, Mode::Lean).len(), 3);
        assert_eq!(select("market_ticks", &names, Mode::History).len(), 3);
        assert!(select("position_snapshots", &names, Mode::History).is_empty(), "never every snapshot file");
    }

    #[test]
    fn parses_real_keeper_rows() {
        let s = Store::memory().unwrap();
        for t in ["market_ticks", "width_evals", "keeper_decisions", "position_events", "position_snapshots"] {
            let line = std::fs::read_to_string(format!("tests/fixtures/{t}.json")).unwrap();
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            gz.write_all(format!("{}\n{}\n", line.trim(), line.trim()).as_bytes()).unwrap();
            let rows = parse_jsonl_gz(&gz.finish().unwrap()).unwrap();
            assert_eq!(rows.len(), 2, "{t}");
            assert!(rows[0]["ts"].as_i64().unwrap() > 1_790_000_000, "{t}");
            assert_eq!(store_rows(&s, t, &format!("obj-{t}"), &rows).unwrap(), 2);
            assert_eq!(store_rows(&s, t, &format!("obj-{t}"), &rows).unwrap(), 0, "re-ingest is a no-op");
        }
        let tick: f64 =
            s.db.query_row("SELECT json_extract(json, '$.tick') FROM keeper_rows WHERE tbl = 'market_ticks'", [], |r| r.get(0)).unwrap();
        assert_eq!(tick, -197254.0);
    }

    #[test]
    fn day_list() {
        let d = days_between("2026-09-27", 1_790_679_863); // 2026-09-29
        assert_eq!(d, vec!["2026-09-27", "2026-09-28", "2026-09-29"]);
    }
}
