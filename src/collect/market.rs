//! Market data that costs nothing and needs no key:
//! * Coinbase Exchange one-minute candles with volume: ETH-USD (the volatility models train on a
//!   year of these; stored as source `coinbase`), and BTC-USD, AERO-USD, ETH-BTC (`cb:<product>`),
//! * Coinbase's ETH-USD order book, summarised every 5 minutes (spread, depth each side),
//! * Deribit: DVOL for ETH (table `dvol`) and BTC, the ETH and BTC perpetuals (funding, open
//!   interest, mark, index), and the whole ETH option board hourly,
//! * DefiLlama's yields for every Aerodrome pool on Base (the competition), every 6 hours,
//! * the Crypto Fear & Greed index, daily.

use crate::net::Web;
use crate::store::Store;
use anyhow::{bail, Result};
use rusqlite::params;
use serde_json::Value;
use std::time::Duration;

const COINBASE: &str = "https://api.exchange.coinbase.com";
const DERIBIT: &str = "https://www.deribit.com/api/v2/public";

/// Products read every minute besides ETH-USD.
pub const EXTRA_PRODUCTS: &[&str] = &["BTC-USD", "AERO-USD", "ETH-BTC"];

fn iso(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default()
}

/// The price_1m source name of a Coinbase product (ETH-USD keeps its historical name).
pub fn source_of(product: &str) -> String {
    if product == "ETH-USD" {
        "coinbase".into()
    } else {
        format!("cb:{product}")
    }
}

/// Up to 300 one-minute candles of `product` in [start, end). Returns how many were stored.
pub async fn candles(web: &Web, store: &Store, product: &str, start: i64, end: i64) -> Result<usize> {
    let url = format!("{COINBASE}/products/{product}/candles?granularity=60&start={}&end={}", iso(start), iso(end));
    let v = web.get_json(&url, &[]).await?;
    let Some(rows) = v.as_array() else { bail!("coinbase {product}: unexpected reply") };
    let source = source_of(product);
    let tx = store.write_tx()?;
    let mut n = 0;
    for r in rows {
        // [time, low, high, open, close, volume]
        let (Some(t), Some(lo), Some(hi), Some(close)) = (r[0].as_i64(), r[1].as_f64(), r[2].as_f64(), r[4].as_f64()) else { continue };
        tx.execute(
            "INSERT INTO price_1m (source, ts, close, high, low, volume) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source, ts) DO UPDATE SET close = excluded.close, high = excluded.high, low = excluded.low, volume = excluded.volume",
            params![source, t, close, hi, lo, r[5].as_f64()],
        )?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// Up to 300 one-minute ETH-USD candles in [start, end) (kept for the callers that predate the other products).
pub async fn coinbase_candles(web: &Web, store: &Store, start: i64, end: i64) -> Result<usize> {
    candles(web, store, "ETH-USD", start, end).await
}

/// The latest few minutes of every product (called every minute by the daemon).
pub async fn coinbase_latest(web: &Web, store: &Store, now: i64) -> Result<usize> {
    let mut n = candles(web, store, "ETH-USD", now - 10 * 60, now).await?;
    for p in EXTRA_PRODUCTS {
        match candles(web, store, p, now - 10 * 60, now).await {
            Ok(k) => n += k,
            Err(e) => tracing::warn!("coinbase {p}: {e:#}"),
        }
    }
    Ok(n)
}

/// Fill one-minute history of `product` back `days` days, skipping minutes already stored.
pub async fn backfill_product(web: &Web, store: &Store, product: &str, now: i64, days: i64) -> Result<usize> {
    let start = (now - days * 86_400) / 60 * 60;
    let source = source_of(product);
    let mut total = 0;
    let mut t = start;
    while t < now {
        let end = (t + 300 * 60).min(now);
        let have = store.count("SELECT COUNT(*) FROM price_1m WHERE source = ?1 AND ts >= ?2 AND ts < ?3", &[&source, &t, &end])?;
        // Thin products (AERO) trade in fewer minutes; ask again only when clearly incomplete.
        if have < (end - t) / 60 * 5 / 10 {
            match candles(web, store, product, t, end).await {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("coinbase backfill {product} {}: {e:#}", iso(t)),
            }
            tokio::time::sleep(Duration::from_millis(350)).await; // public limit is ~10 requests/s
        }
        t = end;
    }
    Ok(total)
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
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
        t = end;
    }
    Ok(total)
}

/// Spread and depth of an order book: (mid, spread_bps, [(pct, bid_usd, ask_usd)]).
pub type BookSummary = (f64, f64, Vec<(f64, f64, f64)>);

pub fn book_summary(book: &Value, pcts: &[f64]) -> Option<BookSummary> {
    let side = |k: &str| -> Vec<(f64, f64)> {
        book[k]
            .as_array()
            .map(|a| a.iter().filter_map(|l| Some((l[0].as_str()?.parse::<f64>().ok()?, l[1].as_str()?.parse::<f64>().ok()?))).collect())
            .unwrap_or_default()
    };
    let (bids, asks) = (side("bids"), side("asks"));
    let (bb, ba) = (bids.first()?.0, asks.first()?.0);
    let mid = (bb + ba) / 2.0;
    let depth = pcts
        .iter()
        .map(|p| {
            let b: f64 = bids.iter().filter(|(px, _)| *px >= mid * (1.0 - p / 100.0)).map(|(px, sz)| px * sz).sum();
            let a: f64 = asks.iter().filter(|(px, _)| *px <= mid * (1.0 + p / 100.0)).map(|(px, sz)| px * sz).sum();
            (*p, b, a)
        })
        .collect();
    Some((mid, (ba - bb) / mid * 1e4, depth))
}

/// ETH-USD order book summary (every 5 minutes).
pub async fn book(web: &Web, store: &Store, ts: i64) -> Result<()> {
    let v = web.get_json(&format!("{COINBASE}/products/ETH-USD/book?level=2"), &[]).await?;
    let Some((mid, spread, depth)) = book_summary(&v, &[0.1, 0.5, 1.0, 2.0]) else { bail!("coinbase book: empty") };
    store.put_series("coinbase_book", "ETH-USD.mid", ts, mid)?;
    store.put_series("coinbase_book", "ETH-USD.spread_bps", ts, spread)?;
    for (p, b, a) in depth {
        store.put_series("coinbase_book", &format!("ETH-USD.bid_usd_{p}pct"), ts, b)?;
        store.put_series("coinbase_book", &format!("ETH-USD.ask_usd_{p}pct"), ts, a)?;
        if b + a > 0.0 {
            store.put_series("coinbase_book", &format!("ETH-USD.imbalance_{p}pct"), ts, (b - a) / (b + a))?;
        }
    }
    Ok(())
}

/// DVOL (hourly) for [start, end], paging through Deribit's continuation token. ETH goes to the
/// `dvol` table (the models read it there); other currencies to the series table.
pub async fn deribit_dvol_for(web: &Web, store: &Store, currency: &str, start: i64, end: i64) -> Result<usize> {
    let mut total = 0;
    let mut until = end * 1000;
    // 1,000 hourly points per page, newest first: 60 pages cover almost 7 years.
    for _ in 0..60 {
        let url = format!(
            "{DERIBIT}/get_volatility_index_data?currency={currency}&start_timestamp={}&end_timestamp={until}&resolution=3600",
            start * 1000
        );
        let v = web.get_json(&url, &[]).await?;
        let Some(data) = v["result"]["data"].as_array() else { bail!("deribit: unexpected reply") };
        for r in data {
            // [ts_ms, open, high, low, close]
            if let (Some(ts), Some(close)) = (r[0].as_i64(), r[4].as_f64()) {
                if currency == "ETH" {
                    store.db.execute("INSERT OR REPLACE INTO dvol (ts, value) VALUES (?1, ?2)", params![ts / 1000, close])?;
                } else {
                    store.put_series("deribit", &format!("{currency}.dvol"), ts / 1000, close)?;
                }
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

/// ETH DVOL (kept for existing callers).
pub async fn deribit_dvol(web: &Web, store: &Store, start: i64, end: i64) -> Result<usize> {
    deribit_dvol_for(web, store, "ETH", start, end).await
}

/// Perpetual funding, open interest, mark and index for ETH and BTC (every 5 minutes).
pub async fn perps(web: &Web, store: &Store, ts: i64) -> Result<usize> {
    let mut n = 0;
    for inst in ["ETH-PERPETUAL", "BTC-PERPETUAL"] {
        let v = web.get_json(&format!("{DERIBIT}/ticker?instrument_name={inst}"), &[]).await?;
        let r = &v["result"];
        for k in ["current_funding", "funding_8h", "open_interest", "mark_price", "index_price", "best_bid_price", "best_ask_price"] {
            if let Some(x) = r[k].as_f64() {
                store.put_series("deribit", &format!("{inst}.{k}"), ts, x)?;
                n += 1;
            }
        }
        if let Some(x) = r["stats"]["volume_usd"].as_f64() {
            store.put_series("deribit", &format!("{inst}.volume_usd_24h"), ts, x)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The whole ETH option board (hourly): implied volatility, price and open interest per instrument.
pub async fn options(web: &Web, store: &Store, ts: i64) -> Result<usize> {
    let v = web.get_json(&format!("{DERIBIT}/get_book_summary_by_currency?currency=ETH&kind=option"), &[]).await?;
    let Some(rows) = v["result"].as_array() else { bail!("deribit options: unexpected reply") };
    let hour = ts / 3600 * 3600;
    let tx = store.write_tx()?;
    let mut n = 0;
    for r in rows {
        let Some(name) = r["instrument_name"].as_str() else { continue };
        tx.execute(
            "INSERT OR REPLACE INTO options_hourly (ts, instrument, underlying, mark_iv, mark_price, open_interest, volume, bid, ask)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                hour,
                name,
                r["underlying_price"].as_f64(),
                r["mark_iv"].as_f64(),
                r["mark_price"].as_f64(),
                r["open_interest"].as_f64(),
                r["volume"].as_f64(),
                r["bid_price"].as_f64(),
                r["ask_price"].as_f64()
            ],
        )?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// DefiLlama yields of every Aerodrome pool on Base (every 6 hours).
pub async fn pool_yields(web: &Web, store: &Store, ts: i64) -> Result<usize> {
    let v = web.get_json("https://yields.llama.fi/pools", &[]).await?;
    let Some(rows) = v["data"].as_array() else { bail!("defillama: unexpected reply") };
    let tx = store.write_tx()?;
    let mut n = 0;
    for p in rows {
        let project = p["project"].as_str().unwrap_or_default();
        if p["chain"].as_str() != Some("Base") || !project.starts_with("aerodrome") {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO pool_yields (ts, pool, project, symbol, meta, tvl_usd, apy, apy_base, apy_reward, volume_1d, volume_7d, apy_mean_30d)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                ts, p["pool"].as_str().unwrap_or_default(), project, p["symbol"].as_str(), p["poolMeta"].as_str(),
                p["tvlUsd"].as_f64(), p["apy"].as_f64(), p["apyBase"].as_f64(), p["apyReward"].as_f64(),
                p["volumeUsd1d"].as_f64(), p["volumeUsd7d"].as_f64(), p["apyMean30d"].as_f64()
            ],
        )?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// The Crypto Fear & Greed index (daily; up to `limit` past days).
pub async fn fear_greed(web: &Web, store: &Store, limit: u32) -> Result<usize> {
    let v = web.get_json(&format!("https://api.alternative.me/fng/?limit={limit}"), &[]).await?;
    let Some(rows) = v["data"].as_array() else { bail!("fear & greed: unexpected reply") };
    let mut n = 0;
    for r in rows {
        let (Some(t), Some(x)) =
            (r["timestamp"].as_str().and_then(|s| s.parse::<i64>().ok()), r["value"].as_str().and_then(|s| s.parse::<f64>().ok()))
        else {
            continue;
        };
        store.put_series("alternative", "fear_greed", t, x)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn book_depth() {
        let b = json!({"bids": [["100", "1"], ["99.6", "2"], ["98.5", "10"], ["90", "7"]], "asks": [["100.2", "1"], ["100.5", "3"], ["103", "5"]]});
        let (mid, spread, d) = book_summary(&b, &[0.5, 2.0]).unwrap();
        assert!((mid - 100.1).abs() < 1e-9);
        assert!((spread - 0.2 / 100.1 * 1e4).abs() < 1e-9);
        // Within 0.5% of 100.1: bids >= 99.5995 -> 100*1 + 99.6*2; asks <= 100.6005 -> 100.2 + 100.5*3.
        assert!((d[0].1 - 299.2).abs() < 1e-9 && (d[0].2 - 401.7).abs() < 1e-9, "{d:?}");
        // Within 2% (98.098 .. 102.102): adds the 98.5 bid, not the 90 bid nor the 103 ask.
        assert!((d[1].1 - 1284.2).abs() < 1e-9 && (d[1].2 - 401.7).abs() < 1e-9, "{d:?}");
    }

    #[test]
    fn source_names() {
        assert_eq!(source_of("ETH-USD"), "coinbase");
        assert_eq!(source_of("BTC-USD"), "cb:BTC-USD");
    }
}
