//! Reads the Panda pool straight from Base: price (tick), liquidity, swap fee and the block's base
//! fee. One JSON-RPC batch per poll. Read-only by construction (see `guard`).

use crate::chainmath::{selector, tick_to_price, word_i64, word_u128, words};
use crate::net::Rpc;
use crate::store::Store;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::{json, Value};

pub struct PoolObs {
    pub ts: i64,
    pub block: i64,
    pub tick: i64,
    pub sqrt_price: u128,
    pub liquidity: u128,
    pub fee_pips: Option<i64>,
    pub base_fee_gwei: Option<f64>,
    /// Gauge emission rate, whole AERO per second.
    pub reward_rate: Option<f64>,
}

fn hex_i64(v: &Value) -> Option<i64> {
    v.as_str().and_then(|s| i64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

pub async fn read_pool(rpc: &Rpc, pool: &str, gauge: &str) -> Result<PoolObs> {
    let call = |sig: &str| json!([{ "to": pool, "data": selector(sig) }, "latest"]);
    let out = rpc
        .batch(&[
            ("eth_getBlockByNumber", json!(["latest", false])),
            ("eth_call", call("slot0()")),
            ("eth_call", call("liquidity()")),
            ("eth_call", call("fee()")),
            ("eth_call", json!([{ "to": gauge, "data": selector("rewardRate()") }, "latest"])),
        ])
        .await?;
    let mut it = out.into_iter();
    let block = it.next().ok_or_else(|| anyhow!("no block"))??;
    let slot0 = it.next().ok_or_else(|| anyhow!("no slot0"))??;
    let liq = it.next().ok_or_else(|| anyhow!("no liquidity"))??;
    let fee = it.next().and_then(|r| r.ok());
    let reward = it.next().and_then(|r| r.ok());

    let w = words(slot0.as_str().unwrap_or_default());
    if w.len() < 2 {
        return Err(anyhow!("slot0 returned too little data"));
    }
    Ok(PoolObs {
        ts: hex_i64(&block["timestamp"]).ok_or_else(|| anyhow!("block without timestamp"))?,
        block: hex_i64(&block["number"]).unwrap_or(0),
        sqrt_price: word_u128(&w[0]),
        tick: word_i64(&w[1]),
        liquidity: word_u128(words(liq.as_str().unwrap_or("0x0")).first().map(String::as_str).unwrap_or("0")),
        fee_pips: fee.and_then(|f| f.as_str().map(|s| word_u128(&words(s).first().cloned().unwrap_or_default()) as i64)),
        base_fee_gwei: hex_i64(&block["baseFeePerGas"]).map(|w| w as f64 / 1e9),
        reward_rate: reward.and_then(|r| r.as_str().map(|s| word_u128(&words(s).first().cloned().unwrap_or_default()) as f64 / 1e18)),
    })
}

pub async fn poll(rpc: &Rpc, pool: &str, gauge: &str, store: &Store) -> Result<PoolObs> {
    let o = read_pool(rpc, pool, gauge).await?;
    let price = tick_to_price(o.tick as f64);
    store.db.execute(
        "INSERT OR REPLACE INTO pool_obs (ts, block, tick, sqrt_price, liquidity, fee_pips, base_fee_gwei, weth_usd, reward_rate) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![o.ts, o.block, o.tick, o.sqrt_price.to_string(), o.liquidity.to_string(), o.fee_pips, o.base_fee_gwei, price, o.reward_rate],
    )?;
    store.put_price("pool", o.ts / 60 * 60, price, None, None, Some(o.tick as f64))?;
    Ok(o)
}
