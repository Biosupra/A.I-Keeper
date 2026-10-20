//! Reads the Panda pool straight from Base every 30 s: price (tick), liquidity, STAKED liquidity
//! (the emission denominator), swap fee, the gauge's emission rate, the block's base fee, and the
//! AERO price from the AERO/WETH pool the vault's reward swap uses. One JSON-RPC batch per poll.
//! Read-only by construction (see `guard`).

use crate::chainmath::{selector, tick_to_price, word_i64, word_u128, words};
use crate::net::Rpc;
use crate::store::Store;
use anyhow::{anyhow, Result};
use rusqlite::params;
use serde_json::{json, Value};

/// Aerodrome AERO/WETH pool (token0 WETH, token1 AERO, both 18 decimals), the first hop of the
/// vault's AERO -> USDC reward swap (same address the keeper logs at startup).
pub const AERO_WETH_POOL: &str = "0x4e506648d493C8870f55e870480f92f2F33ecE51";

pub struct PoolObs {
    pub ts: i64,
    pub block: i64,
    pub tick: i64,
    pub sqrt_price: u128,
    pub liquidity: u128,
    pub staked_liquidity: Option<u128>,
    pub fee_pips: Option<i64>,
    pub base_fee_gwei: Option<f64>,
    /// Gauge emission rate, whole AERO per second.
    pub reward_rate: Option<f64>,
    pub aero_weth_tick: Option<i64>,
}

fn hex_i64(v: &Value) -> Option<i64> {
    v.as_str().and_then(|s| i64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

fn first_word(r: Option<Result<Value>>) -> Option<String> {
    r.and_then(|r| r.ok()).and_then(|v| v.as_str().map(str::to_string)).and_then(|s| words(&s).first().cloned())
}

/// USD per AERO from the pool ETH price and the AERO/WETH tick (AERO per WETH = 1.0001^tick).
pub fn aero_usd(weth_usd: f64, aero_weth_tick: i64) -> f64 {
    weth_usd / 1.0001f64.powf(aero_weth_tick as f64)
}

pub async fn read_pool(rpc: &Rpc, pool: &str, gauge: &str) -> Result<PoolObs> {
    let call = |to: &str, sig: &str| json!([{ "to": to, "data": selector(sig) }, "latest"]);
    let out = rpc
        .batch(&[
            ("eth_getBlockByNumber", json!(["latest", false])),
            ("eth_call", call(pool, "slot0()")),
            ("eth_call", call(pool, "liquidity()")),
            ("eth_call", call(pool, "fee()")),
            ("eth_call", call(gauge, "rewardRate()")),
            ("eth_call", call(pool, "stakedLiquidity()")),
            ("eth_call", call(AERO_WETH_POOL, "slot0()")),
        ])
        .await?;
    let mut it = out.into_iter();
    let block = it.next().ok_or_else(|| anyhow!("no block"))??;
    let slot0 = it.next().ok_or_else(|| anyhow!("no slot0"))??;
    let liq = it.next().ok_or_else(|| anyhow!("no liquidity"))??;
    let fee = first_word(it.next());
    let reward = first_word(it.next());
    let staked = first_word(it.next());
    let aero_slot0 = it.next().and_then(|r| r.ok()).and_then(|v| v.as_str().map(words));

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
        staked_liquidity: staked.map(|s| word_u128(&s)),
        fee_pips: fee.map(|f| word_u128(&f) as i64),
        base_fee_gwei: hex_i64(&block["baseFeePerGas"]).map(|w| w as f64 / 1e9),
        reward_rate: reward.map(|r| word_u128(&r) as f64 / 1e18),
        aero_weth_tick: aero_slot0.filter(|w| w.len() >= 2).map(|w| word_i64(&w[1])),
    })
}

pub async fn poll(rpc: &Rpc, pool: &str, gauge: &str, store: &Store) -> Result<PoolObs> {
    let o = read_pool(rpc, pool, gauge).await?;
    let price = tick_to_price(o.tick as f64);
    let aero = o.aero_weth_tick.map(|t| aero_usd(price, t));
    store.db.execute(
        "INSERT OR REPLACE INTO pool_obs (ts, block, tick, sqrt_price, liquidity, fee_pips, base_fee_gwei, weth_usd, reward_rate, staked_liquidity, aero_weth_tick, aero_usd)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            o.ts, o.block, o.tick, o.sqrt_price.to_string(), o.liquidity.to_string(), o.fee_pips, o.base_fee_gwei, price, o.reward_rate,
            o.staked_liquidity.map(|s| s.to_string()), o.aero_weth_tick, aero
        ],
    )?;
    store.put_price("pool", o.ts / 60 * 60, price, None, None, Some(o.tick as f64))?;
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aero_price_from_the_two_pools() {
        // Keeper market row: weth_usd 2715.22, aero_per_weth 3263.75 -> aero_usd 0.8319.
        let t = (3263.7507957622743f64).ln() / 1.0001f64.ln();
        let p = aero_usd(2715.2181805145665, t.round() as i64);
        assert!((p - 0.8319).abs() < 0.0005, "{p}");
    }
}
