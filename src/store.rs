//! The A.I's own memory: one SQLite file on its own disk. Nothing here is shared with the live
//! keeper or the website; it only ever reads from them.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    pub db: Connection,
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

-- One-minute prices. source = 'coinbase' (ETH-USD candles) or 'pool' (tick from the Panda pool).
CREATE TABLE IF NOT EXISTS price_1m (
  source TEXT NOT NULL, ts INTEGER NOT NULL, close REAL NOT NULL, high REAL, low REAL, tick REAL,
  PRIMARY KEY (source, ts)
);

-- Direct reads of the Panda pool and the chain.
CREATE TABLE IF NOT EXISTS pool_obs (
  ts INTEGER PRIMARY KEY, block INTEGER, tick INTEGER, sqrt_price TEXT, liquidity TEXT,
  fee_pips INTEGER, base_fee_gwei REAL, weth_usd REAL,
  reward_rate REAL -- gauge AERO emitted per second (whole AERO)
);

-- Deribit's ETH volatility index (the options market's expectation of volatility), hourly.
CREATE TABLE IF NOT EXISTS dvol (ts INTEGER PRIMARY KEY, value REAL NOT NULL);

-- Every row of the keeper's history bucket, verbatim.
CREATE TABLE IF NOT EXISTS keeper_rows (
  obj TEXT NOT NULL, line INTEGER NOT NULL, tbl TEXT NOT NULL, ts INTEGER NOT NULL, json TEXT NOT NULL,
  PRIMARY KEY (obj, line)
);
CREATE INDEX IF NOT EXISTS keeper_rows_tbl_ts ON keeper_rows (tbl, ts);
CREATE TABLE IF NOT EXISTS ingested (obj TEXT PRIMARY KEY, ts INTEGER NOT NULL, rows INTEGER NOT NULL);

-- Website health (GET only).
CREATE TABLE IF NOT EXISTS site_checks (ts INTEGER NOT NULL, url TEXT NOT NULL, status INTEGER, ms INTEGER, note TEXT);

-- The prediction registry: every forecast the A.I makes, and later what actually happened.
CREATE TABLE IF NOT EXISTS predictions (
  id INTEGER PRIMARY KEY, made_ts INTEGER NOT NULL, target_ts INTEGER NOT NULL,
  kind TEXT NOT NULL, model TEXT NOT NULL, key TEXT NOT NULL, value REAL NOT NULL, inputs TEXT,
  outcome REAL, score REAL, resolved_ts INTEGER
);
CREATE INDEX IF NOT EXISTS predictions_open ON predictions (resolved_ts, target_ts);
CREATE INDEX IF NOT EXISTS predictions_kind ON predictions (kind, model, made_ts);

-- Shadow mode: the keeper's width choice next to the A.I's, graded on the price path that followed.
CREATE TABLE IF NOT EXISTS shadow (
  made_ts INTEGER PRIMARY KEY, start_tick REAL NOT NULL, position_usd REAL,
  keeper_choice INTEGER NOT NULL, ai_choice INTEGER NOT NULL, keeper_vol REAL, ai_vol REAL, ai_model TEXT,
  detail TEXT, keeper_net_per_day REAL, ai_net_per_day REAL, best_hindsight INTEGER, best_net REAL, replay TEXT, resolved_ts INTEGER
);

-- Every model's own width choice for each shadow decision, graded like the champion's, so shadow
-- mode is informative from day one (before a champion has emerged every model still competes).
CREATE TABLE IF NOT EXISTS shadow_model (
  made_ts INTEGER NOT NULL, model TEXT NOT NULL, vol REAL NOT NULL, choice INTEGER NOT NULL, net_per_day REAL, replay TEXT,
  PRIMARY KEY (made_ts, model)
);

-- What the owner tells it (CLI `note`, and replies to the brief read from the inbox).
CREATE TABLE IF NOT EXISTS notes (ts INTEGER NOT NULL, text TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS inbox_seen (message_id TEXT PRIMARY KEY, ts INTEGER NOT NULL);

-- Every alert raised and cleared (see alerts.rs).
CREATE TABLE IF NOT EXISTS alerts (ts INTEGER NOT NULL, key TEXT NOT NULL, state TEXT NOT NULL, message TEXT NOT NULL);

-- Fitted model parameters and small bits of state.
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);

-- ===== Full capture (2026-09-30: "watch EVERYTHING") =====

-- Every file the keeper writes to its history bucket, byte for byte (gzipped JSONL, as stored).
CREATE TABLE IF NOT EXISTS bucket_archive (obj TEXT PRIMARY KEY, tbl TEXT NOT NULL, ts INTEGER NOT NULL, bytes INTEGER NOT NULL, gz BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS bucket_archive_tbl_ts ON bucket_archive (tbl, ts);

-- One row per position per keeper snapshot (every minute): the numbers that say how each position is doing.
CREATE TABLE IF NOT EXISTS position_min (
  token_id INTEGER NOT NULL, ts INTEGER NOT NULL, origin_token_id INTEGER, position_usd REAL, hodl_usd REAL,
  value_minus_hodl_usd REAL, value_plus_returns_minus_hodl_usd REAL, earned_aero_user REAL, paid_aero_cum REAL,
  paid_usdc_cum REAL, returned_usd REAL, in_range INTEGER, tick_lower INTEGER, tick_upper INTEGER, half_spacings INTEGER,
  decision TEXT, reason TEXT, outside_secs INTEGER, hold_secs INTEGER, recut_wait TEXT, width_best_hs INTEGER,
  width_best_net REAL, width_current_net REAL, liquidity TEXT,
  PRIMARY KEY (token_id, ts)
);
CREATE INDEX IF NOT EXISTS position_min_ts ON position_min (ts);

-- One row per keeper width table (every minute): what it chose and the whole scored table.
CREATE TABLE IF NOT EXISTS width_min (
  ts INTEGER PRIMARY KEY, choice INTEGER, vol REAL, vol_short REAL, vol_long REAL, rate_per_l REAL, gas_usd REAL,
  weth_usd REAL, current_tick INTEGER, coverage_secs INTEGER, table_json TEXT
);

-- Every keeper decision row, compact (the keeper logs one per position when something changes).
CREATE TABLE IF NOT EXISTS decision_min (
  token_id INTEGER NOT NULL, ts INTEGER NOT NULL, decision TEXT, reason TEXT, outside_secs INTEGER, hold_secs INTEGER,
  recut_wait TEXT, recut_candidate INTEGER, half_spacings INTEGER, width_best_hs INTEGER, position_usd REAL,
  spot_tick INTEGER, twap_tick REAL, fee_pips INTEGER, tick_lower INTEGER, tick_upper INTEGER,
  PRIMARY KEY (token_id, ts)
);

-- The keeper's own market row (every minute).
CREATE TABLE IF NOT EXISTS keeper_market_min (
  ts INTEGER PRIMARY KEY, tick INTEGER, twap_tick REAL, fee_pips INTEGER, weth_usd REAL, aero_usd REAL, gas_usd REAL,
  rate_per_l REAL, avg_rate_per_l REAL, vol REAL, vol_short REAL, vol_long REAL, staked_liquidity TEXT,
  reward_rate REAL, tracked_positions INTEGER
);

-- Every log the Panda pool and its gauge emit, byte for byte (one gzipped JSON array per block range).
CREATE TABLE IF NOT EXISTS chain_log_archive (
  from_block INTEGER PRIMARY KEY, to_block INTEGER NOT NULL, logs INTEGER NOT NULL, gz BLOB NOT NULL
);

-- Every swap in the Panda pool, decoded (amounts as signed decimal strings; token0 WETH, token1 USDC).
CREATE TABLE IF NOT EXISTS swaps (
  block INTEGER NOT NULL, log_index INTEGER NOT NULL, ts INTEGER, tx TEXT NOT NULL, sender TEXT, recipient TEXT,
  amount0 TEXT NOT NULL, amount1 TEXT NOT NULL, sqrt_price TEXT, liquidity TEXT, tick INTEGER, usd REAL,
  PRIMARY KEY (block, log_index)
);
CREATE INDEX IF NOT EXISTS swaps_ts ON swaps (ts);

-- Liquidity changes by the other LPs (pool Mint/Burn/Collect/CollectFees/Flash), per minute and kind.
-- ~80,000 a day, so the individual events live in chain_log_archive and these are their sums.
-- in_range_* counts/sums only ranges that contain the price at that event (the ones competing right now).
CREATE TABLE IF NOT EXISTS lp_minute (
  ts INTEGER NOT NULL, kind TEXT NOT NULL, events INTEGER NOT NULL, liquidity REAL NOT NULL, amount0 REAL NOT NULL,
  amount1 REAL NOT NULL, in_range_events INTEGER NOT NULL, in_range_liquidity REAL NOT NULL, width_ticks_mean REAL,
  PRIMARY KEY (ts, kind)
);

-- Gauge activity (NFTs staked/unstaked, rewards claimed, emissions notified, early-withdraw penalties), per minute and kind.
CREATE TABLE IF NOT EXISTS gauge_minute (
  ts INTEGER NOT NULL, kind TEXT NOT NULL, events INTEGER NOT NULL, liquidity REAL NOT NULL, amount REAL NOT NULL,
  token_ids INTEGER NOT NULL,
  PRIMARY KEY (ts, kind)
);

-- The liquidity map around the price (every 10 min): per initialized tick, total and STAKED liquidity.
CREATE TABLE IF NOT EXISTS tick_liquidity (
  ts INTEGER NOT NULL, tick INTEGER NOT NULL, liquidity_gross TEXT, liquidity_net TEXT, staked_liquidity_net TEXT,
  PRIMARY KEY (ts, tick)
);

-- Any scalar time series (funding, open interest, order-book depth, emission schedule, vote weights,
-- fear & greed, keeper wallet balance, ...). source/key name it, e.g. ('deribit', 'ETH-PERPETUAL.funding_8h').
CREATE TABLE IF NOT EXISTS series (source TEXT NOT NULL, key TEXT NOT NULL, ts INTEGER NOT NULL, value REAL NOT NULL,
  PRIMARY KEY (source, key, ts));

-- Deribit's whole ETH (and BTC) option board, hourly.
CREATE TABLE IF NOT EXISTS options_hourly (
  ts INTEGER NOT NULL, instrument TEXT NOT NULL, underlying REAL, mark_iv REAL, mark_price REAL, open_interest REAL,
  volume REAL, bid REAL, ask REAL,
  PRIMARY KEY (ts, instrument)
);

-- DefiLlama's yield data for every Aerodrome pool on Base (competition), every 6 hours.
CREATE TABLE IF NOT EXISTS pool_yields (
  ts INTEGER NOT NULL, pool TEXT NOT NULL, project TEXT, symbol TEXT, meta TEXT, tvl_usd REAL, apy REAL, apy_base REAL,
  apy_reward REAL, volume_1d REAL, volume_7d REAL, apy_mean_30d REAL,
  PRIMARY KEY (ts, pool)
);

-- Crypto headlines (RSS), stored as published. Tagging them comes later.
CREATE TABLE IF NOT EXISTS news (
  id TEXT PRIMARY KEY, source TEXT NOT NULL, ts INTEGER NOT NULL, title TEXT NOT NULL, link TEXT, summary TEXT, seen_ts INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS news_ts ON news (ts);

-- The past replayed (src/replay.rs): for every day of history, every fixed width and every model's
-- own width choices run under the keeper's current rules on the real ETH path. strategy is
-- 'fixed:<half spacings>' or a model name. Emissions and the pool fee before the vault existed
-- are ASSUMED at the levels measured since (see `assumptions`, one JSON row per replay run).
CREATE TABLE IF NOT EXISTS replay_day (
  day INTEGER NOT NULL, strategy TEXT NOT NULL, net_per_day REAL NOT NULL, revenue REAL, costs REAL,
  divergence REAL, rebalances INTEGER, recuts INTEGER, held_out_minutes INTEGER, in_range REAL,
  day_vol REAL, prior_vol REAL,
  PRIMARY KEY (day, strategy)
);
CREATE TABLE IF NOT EXISTS replay_runs (ts INTEGER PRIMARY KEY, kind TEXT NOT NULL, assumptions TEXT NOT NULL);
"#;

/// Columns added after a database may already exist on disk (CREATE TABLE IF NOT EXISTS does not
/// add columns). Each is added once if missing.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("pool_obs", "reward_rate", "REAL"),
    ("shadow", "best_net", "REAL"),
    ("shadow", "replay", "TEXT"),
    ("shadow_model", "replay", "TEXT"),
    // Full capture: traded volume on every Coinbase minute.
    ("price_1m", "volume", "REAL"),
    // Full capture: more of the pool and the AERO price in the 30 s read.
    ("pool_obs", "staked_liquidity", "TEXT"),
    ("pool_obs", "aero_weth_tick", "INTEGER"),
    ("pool_obs", "aero_usd", "REAL"),
    ("pool_obs", "reward_reserve", "REAL"),
    ("pool_obs", "period_finish", "INTEGER"),
    ("pool_obs", "gauge_fees0", "TEXT"),
    ("pool_obs", "gauge_fees1", "TEXT"),
    // The historical replay: the regime each forecast was made in (calm / active / storm, by the
    // previous 24 h of Coinbase volatility) and whether it was replayed (1) or made live (NULL).
    ("predictions", "regime", "TEXT"),
    ("predictions", "replay", "INTEGER"),
];

pub fn migrate(db: &Connection) -> Result<()> {
    for (table, column, ty) in MIGRATIONS {
        let mut st = db.prepare(&format!("PRAGMA table_info({table})"))?;
        let have: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.collect::<std::result::Result<_, _>>()?;
        if !have.iter().any(|c| c == column) {
            db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ty}"))?;
        }
    }
    // Indexes on migrated columns (the columns must exist first).
    db.execute_batch("CREATE INDEX IF NOT EXISTS predictions_regime ON predictions (kind, regime, made_ts)")?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        // A manual command (backfill, brief) may run while the service writes: wait, don't fail.
        db.busy_timeout(std::time::Duration::from_secs(60))?;
        db.execute_batch(SCHEMA)?;
        migrate(&db)?;
        Ok(Store { db })
    }

    #[cfg(test)]
    pub fn memory() -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        migrate(&db)?;
        Ok(Store { db })
    }

    pub fn meta_get(&self, k: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT v FROM meta WHERE k = ?1", params![k], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&self, k: &str, v: &str) -> Result<()> {
        self.db.execute("INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", params![k, v])?;
        Ok(())
    }

    pub fn put_price(&self, source: &str, ts: i64, close: f64, high: Option<f64>, low: Option<f64>, tick: Option<f64>) -> Result<()> {
        self.db.execute(
            "INSERT INTO price_1m (source, ts, close, high, low, tick) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source, ts) DO UPDATE SET close = excluded.close, high = excluded.high, low = excluded.low, tick = COALESCE(excluded.tick, price_1m.tick)",
            params![source, ts, close, high, low, tick],
        )?;
        Ok(())
    }

    /// One-minute closes for a source in [from, to), oldest first.
    pub fn prices(&self, source: &str, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let mut st = self.db.prepare_cached("SELECT ts, close FROM price_1m WHERE source = ?1 AND ts >= ?2 AND ts < ?3 ORDER BY ts")?;
        let rows = st.query_map(params![source, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Pool ticks in [from, to), one per minute, from the A.I's own reads and the keeper's
    /// market_ticks (both are the same pool; the A.I's own read wins when both exist).
    pub fn pool_ticks(&self, from: i64, to: i64) -> Result<Vec<(i64, f64)>> {
        let mut st = self.db.prepare_cached(
            "SELECT ts / 60 * 60 AS m, tick, 0 AS pri FROM price_1m WHERE source = 'pool' AND ts >= ?1 AND ts < ?2 AND tick IS NOT NULL
             UNION ALL
             SELECT ts / 60 * 60, CAST(json_extract(json, '$.tick') AS REAL), 1 FROM keeper_rows
               WHERE tbl = 'market_ticks' AND ts >= ?1 AND ts < ?2
             UNION ALL
             SELECT ts / 60 * 60, CAST(tick AS REAL), 2 FROM keeper_market_min WHERE tick IS NOT NULL AND ts >= ?1 AND ts < ?2
             ORDER BY 1, 3",
        )?;
        let rows = st.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let mut out: Vec<(i64, f64)> = Vec::new();
        for row in rows {
            let (m, t) = row?;
            match out.last() {
                Some((lm, _)) if *lm == m => {}
                _ => out.push((m, t)),
            }
        }
        Ok(out)
    }

    /// One-minute pool path in [from, to) as (ts, tick, fee_pips): the A.I's own reads (with the
    /// pool's fee), the keeper's market_ticks (with its fee reading), and last the price series.
    pub fn pool_path(&self, from: i64, to: i64) -> Result<Vec<(i64, f64, Option<f64>)>> {
        let mut st = self.db.prepare_cached(
            "SELECT ts / 60 * 60 AS m, CAST(tick AS REAL), CAST(fee_pips AS REAL), 0 AS pri FROM pool_obs WHERE ts >= ?1 AND ts < ?2
             UNION ALL
             SELECT ts / 60 * 60, CAST(json_extract(json, '$.tick') AS REAL), CAST(json_extract(json, '$.fee_pips') AS REAL), 1
               FROM keeper_rows WHERE tbl = 'market_ticks' AND ts >= ?1 AND ts < ?2
             UNION ALL
             SELECT ts / 60 * 60, CAST(tick AS REAL), CAST(fee_pips AS REAL), 2 FROM keeper_market_min WHERE tick IS NOT NULL AND ts >= ?1 AND ts < ?2
             UNION ALL
             SELECT ts / 60 * 60, tick, NULL, 3 FROM price_1m WHERE source = 'pool' AND tick IS NOT NULL AND ts >= ?1 AND ts < ?2
             ORDER BY 1, 4",
        )?;
        let rows = st.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, Option<f64>>(2)?)))?;
        let mut out: Vec<(i64, f64, Option<f64>)> = Vec::new();
        for row in rows {
            let (m, t, f) = row?;
            match out.last_mut() {
                Some(last) if last.0 == m => {
                    if last.2.is_none() {
                        last.2 = f;
                    }
                }
                _ => out.push((m, t, f)),
            }
        }
        Ok(out)
    }

    pub fn count(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<i64> {
        Ok(self.db.query_row(sql, args, |r| r.get(0))?)
    }

    /// One point of a named scalar series (see the `series` table).
    pub fn put_series(&self, source: &str, key: &str, ts: i64, value: f64) -> Result<()> {
        if !value.is_finite() {
            return Ok(());
        }
        self.db.execute(
            "INSERT INTO series (source, key, ts, value) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source, key, ts) DO UPDATE SET value = excluded.value",
            params![source, key, ts, value],
        )?;
        Ok(())
    }

    /// The newest value of a series at or before `ts`.
    pub fn series_at(&self, source: &str, key: &str, ts: i64) -> Result<Option<(i64, f64)>> {
        Ok(self
            .db
            .query_row(
                "SELECT ts, value FROM series WHERE source = ?1 AND key = ?2 AND ts <= ?3 ORDER BY ts DESC LIMIT 1",
                params![source, key, ts],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_ticks_merge_and_dedup() {
        let s = Store::memory().unwrap();
        s.put_price("pool", 120, 2700.0, None, None, Some(-197000.0)).unwrap();
        s.db.execute(
            "INSERT INTO keeper_rows (obj, line, tbl, ts, json) VALUES ('a', 0, 'market_ticks', 125, '{\"tick\": -197001}'), ('a', 1, 'market_ticks', 185, '{\"tick\": -197002}')",
            [],
        )
        .unwrap();
        let t = s.pool_ticks(0, 1000).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[1], (180, -197002.0));
    }

    #[test]
    fn meta_roundtrip() {
        let s = Store::memory().unwrap();
        s.meta_set("x", "1").unwrap();
        s.meta_set("x", "2").unwrap();
        assert_eq!(s.meta_get("x").unwrap().as_deref(), Some("2"));
    }
}
