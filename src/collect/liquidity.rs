//! The competition for emissions, read straight from the chain (free public RPCs, read-only):
//!
//! * every 10 minutes, the liquidity map around the price: for each initialized tick within
//!   ±`TICK_SPAN` spacings (±20% on the tick-spacing-50 pool), its gross, net and STAKED net
//!   liquidity. Staked liquidity is what shares the gauge's AERO with the vault.
//! * every hour, the emission schedule and the vote: gauge.periodFinish/left/rewardRateByEpoch,
//!   the pool's rewardReserve and gaugeFees, the Voter's weight for this pool and the total weight,
//!   and the keeper wallet's ETH balance.
use crate::chainmath::{encode_i64_word, selector, word_i64, word_signed_dec, word_u128, word_unsigned_dec, words};
use crate::net::Rpc;
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use rusqlite::params;
use serde_json::{json, Value};

pub const TICK_SPACING: i64 = 50;
/// Spacings each side of the price that the map covers (40 x 50 ticks = ±2,000 ticks ≈ ±20%).
pub const TICK_SPAN: i64 = 40;
/// Aerodrome's Voter on Base (resolved from gauge.voter() and verified on Blockscout, 2026-09-30).
pub const VOTER: &str = "0x16613524e02ad97edfef371bc883f2f5d6c480a5";
/// The live keeper's wallet (public address; its ETH pays for rebalances).
pub const KEEPER_WALLET: &str = "0x940e46DBB87218C1dD9EeecF33DD9C36b0A6e8a8";

fn call(to: &str, data: String) -> (&'static str, Value) {
    ("eth_call", json!([{ "to": to, "data": data }, "latest"]))
}

fn arg_addr(a: &str) -> String {
    format!("{:0>64}", a.trim_start_matches("0x").to_ascii_lowercase())
}

fn ok_str(r: &Result<Value>) -> Option<&str> {
    r.as_ref().ok().and_then(Value::as_str)
}

/// Reads and stores the liquidity map around the current tick. Returns the number of initialized ticks.
pub async fn snapshot_ticks(rpc: &Rpc, store: &Store, pool: &str, ts: i64) -> Result<usize> {
    // The current tick first (one call), then every tick of the map in one batch.
    let head = rpc.batch(&[call(pool, selector("slot0()"))]).await?;
    let w = words(ok_str(&head[0]).ok_or_else(|| anyhow!("slot0 failed"))?);
    if w.len() < 2 {
        bail!("slot0 returned too little data");
    }
    let current = word_i64(&w[1]);
    let base = current.div_euclid(TICK_SPACING) * TICK_SPACING;
    let ticks: Vec<i64> = (-TICK_SPAN..=TICK_SPAN).map(|i| base + i * TICK_SPACING).collect();
    let sel = selector("ticks(int24)");
    let calls: Vec<(&str, Value)> = ticks.iter().map(|t| call(pool, format!("{sel}{}", encode_i64_word(*t)))).collect();
    let out = rpc.batch(&calls).await?;
    if out.iter().any(|r| r.is_err()) {
        // A partial map would read as missing liquidity; skip this round rather than store it.
        bail!("{} of {} tick reads failed", out.iter().filter(|r| r.is_err()).count(), out.len());
    }
    let tx = store.write_tx()?;
    let mut n = 0;
    for (t, r) in ticks.iter().zip(&out) {
        // ticks(): liquidityGross, liquidityNet, stakedLiquidityNet, feeGrowthOutside0/1, rewardGrowthOutside,
        // tickCumulativeOutside, secondsPerLiquidityOutside, secondsOutside, initialized.
        let w = words(ok_str(r).unwrap_or("0x"));
        if w.len() < 10 {
            continue;
        }
        if word_u128(&w[9]) == 0 {
            continue; // not initialized: nothing starts or ends here
        }
        tx.execute(
            "INSERT OR REPLACE INTO tick_liquidity (ts, tick, liquidity_gross, liquidity_net, staked_liquidity_net) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts, t, word_unsigned_dec(&w[0]), word_signed_dec(&w[1]), word_signed_dec(&w[2])],
        )?;
        n += 1;
    }
    tx.commit()?;
    store.put_series("pool", "map_current_tick", ts, current as f64)?;
    Ok(n)
}

/// Active and staked liquidity at each tick of a stored map, walking the nets outward from the
/// current tick where the pool's own `liquidity` and `stakedLiquidity` are known.
pub fn profile(store: &Store, ts: i64, current_tick: i64, active: f64, staked: f64) -> Result<Vec<(i64, f64, f64)>> {
    let mut st = store.db.prepare("SELECT tick, liquidity_net, staked_liquidity_net FROM tick_liquidity WHERE ts = ?1 ORDER BY tick")?;
    let rows: Vec<(i64, f64, f64)> = st
        .query_map([ts], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        .filter_map(|r| r.ok())
        .map(|(t, a, s)| (t, a.parse().unwrap_or(0.0), s.parse().unwrap_or(0.0)))
        .collect();
    // Above the current tick: crossing tick t upward adds its net. Below: crossing downward subtracts it.
    let mut out: Vec<(i64, f64, f64)> = Vec::new();
    let (mut a, mut s) = (active, staked);
    for (t, net, snet) in rows.iter().filter(|r| r.0 > current_tick) {
        a += net;
        s += snet;
        out.push((*t, a, s));
    }
    let (mut a, mut s) = (active, staked);
    for (t, net, snet) in rows.iter().rev().filter(|r| r.0 <= current_tick) {
        out.push((*t, a, s));
        a -= net;
        s -= snet;
    }
    out.sort_by_key(|r| r.0);
    Ok(out)
}

/// The emission schedule, the vote and the keeper wallet (hourly). Returns how many values were stored.
pub async fn snapshot_emissions(rpc: &Rpc, store: &Store, pool: &str, gauge: &str, ts: i64) -> Result<usize> {
    let epoch_start = ts - ts.rem_euclid(604_800); // Aerodrome epochs start Thursday 00:00 UTC (unix epoch was a Thursday)
    let calls = vec![
        call(gauge, selector("periodFinish()")),
        call(gauge, selector("left()")),
        call(gauge, format!("{}{:064x}", selector("rewardRateByEpoch(uint256)"), epoch_start)),
        call(gauge, selector("fees0()")),
        call(gauge, selector("fees1()")),
        call(pool, selector("rewardReserve()")),
        call(pool, selector("gaugeFees()")),
        call(pool, selector("stakedLiquidity()")),
        call(pool, selector("liquidity()")),
        call(VOTER, format!("{}{}", selector("weights(address)"), arg_addr(pool))),
        call(VOTER, selector("totalWeight()")),
        ("eth_getBalance", json!([KEEPER_WALLET, "latest"])),
    ];
    let out = rpc.batch(&calls).await?;
    let num = |i: usize, scale: f64| -> Option<f64> { ok_str(&out[i]).and_then(|s| words(s).first().map(|w| word_u128(w) as f64 / scale)) };
    let mut n = 0;
    let mut put = |key: &str, v: Option<f64>| -> Result<()> {
        if let Some(v) = v {
            store.put_series("gauge", key, ts, v)?;
            n += 1;
        }
        Ok(())
    };
    put("period_finish", num(0, 1.0))?;
    put("left_aero", num(1, 1e18))?;
    put("reward_rate_epoch_aero_s", num(2, 1e18))?;
    put("fees0_weth", num(3, 1e18))?;
    put("fees1_usdc", num(4, 1e6))?;
    put("pool_reward_reserve_aero", num(5, 1e18))?;
    if let Some(w) = ok_str(&out[6]).map(words) {
        if w.len() >= 2 {
            put("pool_gauge_fees0_weth", Some(word_u128(&w[0]) as f64 / 1e18))?;
            put("pool_gauge_fees1_usdc", Some(word_u128(&w[1]) as f64 / 1e6))?;
        }
    }
    put("pool_staked_liquidity", num(7, 1.0))?;
    put("pool_liquidity", num(8, 1.0))?;
    let (weight, total) = (num(9, 1e18), num(10, 1e18));
    put("voter_weight", weight)?;
    put("voter_total_weight", total)?;
    if let (Some(w), Some(t)) = (weight, total) {
        if t > 0.0 {
            put("voter_share", Some(w / t))?;
        }
    }
    let bal = out[11].as_ref().ok().and_then(Value::as_str).and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    if let Some(b) = bal {
        store.put_series("keeper", "wallet_eth", ts, b as f64 / 1e18)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_walks_nets_from_the_current_tick() {
        let s = Store::memory().unwrap();
        // A position [-100, 100) with liquidity 10 (5 staked) and another [0, 200) with 4 (unstaked).
        for (t, net, snet) in [(-100, "10", "5"), (0, "4", "0"), (100, "-10", "-5"), (200, "-4", "0")] {
            s.db.execute(
                "INSERT INTO tick_liquidity (ts, tick, liquidity_gross, liquidity_net, staked_liquidity_net) VALUES (1, ?1, '0', ?2, ?3)",
                params![t, net, snet],
            )
            .unwrap();
        }
        // At tick 50 both are active: 14 active, 5 staked.
        let p = profile(&s, 1, 50, 14.0, 5.0).unwrap();
        let at = |t: i64| p.iter().find(|r| r.0 == t).map(|r| (r.1, r.2)).unwrap();
        assert_eq!(at(0), (14.0, 5.0), "[0,100) has both");
        assert_eq!(at(-100), (10.0, 5.0), "[-100,0) only the first");
        assert_eq!(at(100), (4.0, 0.0), "[100,200) only the second");
        assert_eq!(at(200), (0.0, 0.0), "above 200 nothing");
    }

    #[test]
    fn tick_argument_encoding() {
        assert_eq!(format!("{}{}", selector("ticks(int24)"), encode_i64_word(-197250)).len(), 10 + 64);
        assert_eq!(arg_addr("0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A").len(), 64);
    }
}
