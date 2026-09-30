//! Reads the live keeper's history bucket (gs://pfkeeperdata), read-only.
//!
//! Access: on the Google Cloud VM, a token for the VM's own service account comes from the metadata
//! server; that account holds only `roles/storage.objectViewer` on this one bucket, so the A.I can
//! list and read and nothing else. On a laptop, `AIK_GCS_TOKEN` can carry a short-lived token.
//!
//! Everything (2026-09-30, the owner: "capture every single bit of data it possibly can"): EVERY
//! file of every table is read once and
//!
//! * archived byte for byte in `bucket_archive` (gzipped, as stored: ~15.7 MB a day), and
//! * decoded into compact per-minute tables: `position_min` (each position every minute),
//!   `decision_min` (every keeper decision), `width_min` (every width table with its scores) and
//!   `keeper_market_min` (the keeper's market row).
//!
//! In addition the rows the shadow replay reads keep going to `keeper_rows` as before (`select`):
//! position_events and keeper_runs in full, width_evals one per 10 minutes, keeper_decisions one per
//! hour, market_ticks in `History` mode only. A file already archived is re-read from the archive,
//! never downloaded twice.
//!
//! Cost: the bucket is US multi-region, outside the free tier. ~4,000 reads a day ($0.004 per
//! 10,000) and ~500 lists a day ($0.10 per 10,000) come to about $0.20 a month; the one-time
//! backfill of the first fortnight (~64,000 reads) about $0.03.

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

fn f(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}
fn i(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|x| x as i64))
}
fn s(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(x) => Some(x.clone()),
        other => Some(other.to_string()),
    }
}

/// Archives one object byte for byte and decodes its rows into the compact per-minute tables.
/// Returns the number of compact rows written.
pub fn archive(store: &Store, table: &str, obj: &str, gz: &[u8], rows: &[Value]) -> Result<usize> {
    let tx = store.db.unchecked_transaction()?;
    let ts0 = rows.first().and_then(|r| i(&r["ts"])).unwrap_or(0);
    tx.execute(
        "INSERT OR IGNORE INTO bucket_archive (obj, tbl, ts, bytes, gz) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![obj, table, ts0, gz.len() as i64, gz],
    )?;
    let mut n = 0;
    for r in rows {
        let Some(ts) = i(&r["ts"]) else { continue };
        n += match table {
            "position_snapshots" => tx.execute(
                "INSERT OR REPLACE INTO position_min (token_id, ts, origin_token_id, position_usd, hodl_usd, value_minus_hodl_usd,
                   value_plus_returns_minus_hodl_usd, earned_aero_user, paid_aero_cum, paid_usdc_cum, returned_usd, in_range, tick_lower,
                   tick_upper, half_spacings, decision, reason, outside_secs, hold_secs, recut_wait, width_best_hs, width_best_net,
                   width_current_net, liquidity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
                params![
                    i(&r["token_id"]), ts, i(&r["origin_token_id"]), f(&r["position_usd"]), f(&r["hodl_usd"]),
                    f(&r["value_minus_hodl_usd"]), f(&r["value_plus_returns_minus_hodl_usd"]), f(&r["earned_aero_user"]),
                    f(&r["paid_aero_cumulative"]), f(&r["paid_usdc_cumulative"]), f(&r["returned_to_owner_usd"]),
                    r["in_range"].as_bool().map(i64::from), i(&r["tick_lower"]), i(&r["tick_upper"]), i(&r["half_spacings"]),
                    s(&r["decision"]), s(&r["reason"]), i(&r["outside_secs"]), i(&r["hold_secs"]), s(&r["recut_wait"]),
                    i(&r["width_best_hs"]), f(&r["width_best_net_per_day"]), f(&r["width_current_net_per_day"]), s(&r["liquidity"])
                ],
            )?,
            "keeper_decisions" => tx.execute(
                "INSERT OR REPLACE INTO decision_min (token_id, ts, decision, reason, outside_secs, hold_secs, recut_wait, recut_candidate,
                   half_spacings, width_best_hs, position_usd, spot_tick, twap_tick, fee_pips, tick_lower, tick_upper)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    i(&r["token_id"]), ts, s(&r["decision"]), s(&r["reason"]), i(&r["outside_secs"]), i(&r["hold_secs"]),
                    s(&r["recut_wait"]), i(&r["recut_candidate"]), i(&r["half_spacings"]), i(&r["width_best_hs"]),
                    f(&r["position_usd"]), i(&r["spot_tick"]), f(&r["twap_tick"]), i(&r["fee_pips"]), i(&r["tick_lower"]), i(&r["tick_upper"])
                ],
            )?,
            "width_evals" => tx.execute(
                "INSERT OR REPLACE INTO width_min (ts, choice, vol, vol_short, vol_long, rate_per_l, gas_usd, weth_usd, current_tick, coverage_secs, table_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    ts, i(&r["choice"]), f(&r["realised_vol_hourly"]), f(&r["realised_vol_hourly_short"]), f(&r["realised_vol_hourly_long"]),
                    f(&r["rate_per_l"]), f(&r["gas_usd"]), f(&r["weth_usd"]), i(&r["current_tick"]), i(&r["coverage_secs"]),
                    r.get("table").map(|t| t.to_string())
                ],
            )?,
            "market_ticks" => tx.execute(
                "INSERT OR REPLACE INTO keeper_market_min (ts, tick, twap_tick, fee_pips, weth_usd, aero_usd, gas_usd, rate_per_l, avg_rate_per_l,
                   vol, vol_short, vol_long, staked_liquidity, reward_rate, tracked_positions)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    ts, i(&r["tick"]), f(&r["twap_tick"]), i(&r["fee_pips"]), f(&r["weth_usd"]), f(&r["aero_usd"]), f(&r["gas_usd"]),
                    f(&r["rate_per_l"]), f(&r["avg_rate_per_l"]), f(&r["realised_vol_hourly_blended"]).or(f(&r["realised_vol_hourly"])),
                    f(&r["realised_vol_hourly_short"]), f(&r["realised_vol_hourly_long"]), s(&r["staked_liquidity"]),
                    f(&r["reward_rate_aero_per_s"]), i(&r["tracked_positions"])
                ],
            )?,
            _ => 0,
        };
    }
    tx.commit()?;
    Ok(n)
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

/// Syncs the given days (YYYY-MM-DD) of every table: every file is archived and decoded (once),
/// and the files `select` picks also go to `keeper_rows`. Returns (objects read, rows stored).
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
            let lean: std::collections::HashSet<String> = select(table, &names, mode).into_iter().collect();
            for name in &names {
                let archived = store.count("SELECT COUNT(*) FROM bucket_archive WHERE obj = ?1", &[name])? > 0;
                let wants_rows = lean.contains(name) && store.count("SELECT COUNT(*) FROM ingested WHERE obj = ?1", &[name])? == 0;
                if archived && !wants_rows {
                    continue;
                }
                let bytes = if archived {
                    store.db.query_row("SELECT gz FROM bucket_archive WHERE obj = ?1", [name], |r| r.get::<_, Vec<u8>>(0))?
                } else {
                    match bucket.read(name).await {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!("bucket read {name}: {e:#}");
                            continue;
                        }
                    }
                };
                let parsed = match parse_jsonl_gz(&bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("bucket parse {name}: {e:#}");
                        continue;
                    }
                };
                if !archived {
                    rows += archive(store, table, name, &bytes, &parsed)?;
                    objs += 1;
                }
                if wants_rows {
                    rows += store_rows(store, table, name, &parsed)?;
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
    fn archives_and_decodes_every_table() {
        let s = Store::memory().unwrap();
        for t in ["market_ticks", "width_evals", "keeper_decisions", "position_snapshots", "position_events"] {
            let line = std::fs::read_to_string(format!("tests/fixtures/{t}.json")).unwrap();
            let mut gz = GzEncoder::new(Vec::new(), Compression::default());
            gz.write_all(format!("{}\n", line.trim()).as_bytes()).unwrap();
            let bytes = gz.finish().unwrap();
            let rows = parse_jsonl_gz(&bytes).unwrap();
            let n = archive(&s, t, &format!("v1/{t}/obj"), &bytes, &rows).unwrap();
            assert_eq!(n, if t == "position_events" { 0 } else { 1 }, "{t}");
            let back: Vec<u8> =
                s.db.query_row("SELECT gz FROM bucket_archive WHERE obj = ?1", [format!("v1/{t}/obj")], |r| r.get(0)).unwrap();
            assert_eq!(back, bytes, "{t} archived byte for byte");
        }
        let (usd, earned, inr, lo): (f64, f64, i64, i64) = s
            .db
            .query_row("SELECT position_usd, earned_aero_user, in_range, tick_lower FROM position_min WHERE token_id = 6456381", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert!((usd - 14.991916841368223).abs() < 1e-12 && (earned - 0.38506248356060196).abs() < 1e-15);
        assert_eq!((inr, lo), (1, -198200));
        let (choice, table): (i64, String) =
            s.db.query_row("SELECT choice, table_json FROM width_min", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(choice, 16);
        assert_eq!(serde_json::from_str::<Value>(&table).unwrap().as_array().unwrap().len(), 15);
        let (dec, hi): (String, i64) =
            s.db.query_row("SELECT decision, tick_upper FROM decision_min", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((dec.as_str(), hi), ("in_range", -196500));
        let (fee, reward): (i64, f64) =
            s.db.query_row("SELECT fee_pips, reward_rate FROM keeper_market_min", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(fee, 475);
        assert!((reward - 0.5108040683799872).abs() < 1e-15);
    }

    #[test]
    fn day_list() {
        let d = days_between("2026-09-27", 1_790_679_863); // 2026-09-29
        assert_eq!(d, vec!["2026-09-27", "2026-09-28", "2026-09-29"]);
    }
}
