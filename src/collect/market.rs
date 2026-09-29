//! Market data that costs nothing and needs no key:
//! * Coinbase Exchange ETH-USD one-minute candles (the deepest USD market; the volatility models
//!   train on a year of these),
//! * Deribit's DVOL, the options market's own 30-day forward volatility for ETH (hourly).

use crate::net::Web;
use crate::store::Store;
use anyhow::{bail, Result};
use rusqlite::params;
use std::time::Duration;

const COINBASE: &str = "https://api.exchange.coinbase.com";
const DERIBIT: &str = "https://www.deribit.com/api/v2/public";

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

/// Up to 300 one-minute candles in [start, end). Returns how many were stored.
pub async fn coinbase_candles(web: &Web, store: &Store, start: i64, end: i64) -> Result<usize> {
    let url = format!("{COINBASE}/products/ETH-USD/candles?granularity=60&start={}&end={}", iso(start), iso(end));
    let v = web.get_json(&url, &[]).await?;
    let Some(rows) = v.as_array() else { bail!("coinbase: unexpected reply") };
    let tx = store.db.unchecked_transaction()?;
    let mut n = 0;
    for r in rows {
        // [time, low, high, open, close, volume]
        let (Some(t), Some(lo), Some(hi), Some(close)) = (r[0].as_i64(), r[1].as_f64(), r[2].as_f64(), r[4].as_f64()) else { continue };
        tx.execute(
            "INSERT INTO price_1m (source, ts, close, high, low) VALUES ('coinbase', ?1, ?2, ?3, ?4)
             ON CONFLICT(source, ts) DO UPDATE SET close = excluded.close, high = excluded.high, low = excluded.low",
            params![t, close, hi, lo],
        )?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// The latest few minutes (called every minute by the daemon).
pub async fn coinbase_latest(web: &Web, store: &Store, now: i64) -> Result<usize> {
    coinbase_candles(web, store, now - 10 * 60, now).await
}

/// Fill one-minute ETH-USD history back `days` days, skipping minutes already stored.
pub async fn coinbase_backfill(web: &Web, store: &Store, now: i64, days: i64) -> Result<usize> {
    let start = (now - days * 86_400) / 60 * 60;
    let mut total = 0;
    let mut t = start;
    while t < now {
        let end = (t + 300 * 60).min(now);
        let have = store.count("SELECT COUNT(*) FROM price_1m WHERE source = 'coinbase' AND ts >= ?1 AND ts < ?2", &[&t, &end])?;
        if have < (end - t) / 60 * 9 / 10 {
            match coinbase_candles(web, store, t, end).await {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("coinbase backfill {}: {e:#}", iso(t)),
            }
            tokio::time::sleep(Duration::from_millis(350)).await; // public limit is ~10 requests/s
        }
        t = end;
    }
    Ok(total)
}

/// DVOL (hourly) for [start, end], paging through Deribit's continuation token.
pub async fn deribit_dvol(web: &Web, store: &Store, start: i64, end: i64) -> Result<usize> {
    let mut total = 0;
    let mut until = end * 1000;
    for _ in 0..20 {
        let url = format!(
            "{DERIBIT}/get_volatility_index_data?currency=ETH&start_timestamp={}&end_timestamp={until}&resolution=3600",
            start * 1000
        );
        let v = web.get_json(&url, &[]).await?;
        let Some(data) = v["result"]["data"].as_array() else { bail!("deribit: unexpected reply") };
        for r in data {
            // [ts_ms, open, high, low, close]
            if let (Some(ts), Some(close)) = (r[0].as_i64(), r[4].as_f64()) {
                store.db.execute("INSERT OR REPLACE INTO dvol (ts, value) VALUES (?1, ?2)", params![ts / 1000, close])?;
                total += 1;
            }
        }
        match v["result"]["continuation"].as_i64() {
            Some(c) if c > start * 1000 && c < until => until = c,
            _ => break,
        }
    }
    Ok(total)
}
