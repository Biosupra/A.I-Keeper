//! The competition for emissions, read straight from the chain (free public RPCs, read-only):
//!
//! * every 10 minutes, the liquidity map around the price: for each initialized tick within
//!   ±`TICK_SPAN` spacings (±20% on the tick-spacing-50 pool), its gross, net and STAKED net
//!   liquidity. Staked liquidity is what shares the gauge's AERO with the vault.
//! * every hour, the emission schedule and the vote: gauge.periodFinish/left/rewardRateByEpoch,
//!   the pool's rewardReserve and gaugeFees, the Voter's weight for this pool and the total weight,
//!   and the keeper wallet's ETH balance. On Aerodrome's gauge the epoch rate is also stored as it is
//!   really paid (0 once periodFinish has passed and from the Aero launch), next to the raw one.
//! * on Aero's gauge instead (no periodFinish/left/rewardRateByEpoch, no Aerodrome vote): the LeafVoter's
//!   gauge state and emissionsPerVP, the gauge factory's cap, referral cut and early-unstake window
//!   (`effectivePenaltyConfig`, in BLOCKS), and the followed vault's staked NFTs with each one's
//!   `depositBlock` and the gauge's `earned(vault, ids)` sum.
use crate::aero::{self, GaugeMode};
use crate::chainmath::{encode_i64_word, selector, word_address, word_i64, word_signed_dec, word_u128, word_unsigned_dec, words};
use crate::collect::gauge::GaugeCtx;
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
/// `ctx` None (the gauge not probed yet): only the pool and the keeper wallet are read.
pub async fn snapshot_emissions(rpc: &Rpc, store: &Store, pool: &str, gauge: &str, ctx: Option<&GaugeCtx>, ts: i64) -> Result<usize> {
    let mut calls = vec![
        call(pool, selector("gaugeFees()")),
        call(pool, selector("stakedLiquidity()")),
        call(pool, selector("liquidity()")),
        ("eth_getBalance", json!([KEEPER_WALLET, "latest"])),
    ];
    let mode = ctx.map(|c| c.mode);
    let g = arg_addr(gauge);
    match mode {
        Some(GaugeMode::Aerodrome) => {
            let epoch_start = ts - ts.rem_euclid(604_800); // Aerodrome epochs start Thursday 00:00 UTC (unix epoch was a Thursday)
            calls.extend([
                call(gauge, selector("periodFinish()")),
                call(gauge, selector("left()")),
                call(gauge, format!("{}{:064x}", selector("rewardRateByEpoch(uint256)"), epoch_start)),
                call(gauge, selector("fees0()")),
                call(gauge, selector("fees1()")),
                call(pool, selector("rewardReserve()")),
                call(VOTER, format!("{}{}", selector("weights(address)"), arg_addr(pool))),
                call(VOTER, selector("totalWeight()")),
            ]);
        }
        Some(GaugeMode::Aero) => {
            let c = ctx.expect("mode");
            calls.extend([
                call(&c.voter, format!("{}{g}", selector("gaugeStates(address)"))),
                call(&c.voter, selector("emissionsPerVP()")),
                call(&c.factory, format!("{}{g}", selector("effectivePenaltyConfig(address)"))),
                call(&c.factory, format!("{}{g}", selector("emissionCap(address)"))),
                call(&c.factory, format!("{}{g}", selector("referralConfig(address)"))),
            ]);
            if let Some(v) = &c.vault {
                calls.push(call(gauge, format!("{}{}", selector("stakedValues(address)"), arg_addr(v))));
                calls.push(call(gauge, format!("{}{}", selector("deferredEmissions(address)"), arg_addr(v))));
            }
        }
        None => {}
    }
    let out = rpc.batch(&calls).await?;
    let num = |i: usize, scale: f64| -> Option<f64> { ok_str(&out[i]).and_then(|s| words(s).first().map(|w| word_u128(w) as f64 / scale)) };
    let all = |i: usize| -> Option<Vec<String>> { ok_str(&out[i]).map(words).filter(|w| !w.is_empty()) };
    let mut n = 0;
    let mut put = |key: &str, v: Option<f64>| -> Result<()> {
        if let Some(v) = v {
            store.put_series("gauge", key, ts, v)?;
            n += 1;
        }
        Ok(())
    };
    if let Some(w) = all(0) {
        if w.len() >= 2 {
            put("pool_gauge_fees0_weth", Some(word_u128(&w[0]) as f64 / 1e18))?;
            put("pool_gauge_fees1_usdc", Some(word_u128(&w[1]) as f64 / 1e6))?;
        }
    }
    put("pool_staked_liquidity", num(1, 1.0))?;
    put("pool_liquidity", num(2, 1.0))?;
    let mut staked_ids: Vec<u128> = Vec::new();
    match mode {
        Some(GaugeMode::Aerodrome) => {
            let finish = num(4, 1.0).map(|f| f as i64);
            put("period_finish", finish.map(|f| f as f64))?;
            put("left_aero", num(5, 1e18))?;
            let epoch_rate = num(6, 1e18);
            put("reward_rate_epoch_aero_s", epoch_rate)?;
            // What that rate really pays: nothing once periodFinish has passed, and nothing from the Aero launch.
            put("reward_rate_epoch_effective_aero_s", epoch_rate.map(|r| aero::aerodrome_rate_effective(r, finish, ts)))?;
            put("aerodrome_paying", Some(if aero::aerodrome_paying(finish, ts) { 1.0 } else { 0.0 }))?;
            put("fees0_weth", num(7, 1e18))?;
            put("fees1_usdc", num(8, 1e6))?;
            put("pool_reward_reserve_aero", num(9, 1e18))?;
            let (weight, total) = (num(10, 1e18), num(11, 1e18));
            put("voter_weight", weight)?;
            put("voter_total_weight", total)?;
            if let (Some(w), Some(t)) = (weight, total) {
                if t > 0.0 {
                    put("voter_share", Some(w / t))?;
                }
            }
        }
        Some(GaugeMode::Aero) => {
            let state = all(4).and_then(|w| aero::decode_gauge_state(&w));
            let per_vp = num(5, 1.0);
            let window = all(6).and_then(|w| aero::decode_penalty_config(&w));
            let cap = num(7, 1.0);
            let referral = all(8).filter(|w| w.len() >= 2).map(|w| (word_address(&w[0]), word_u128(&w[1]) as f64));
            if let Some(s) = state {
                put("aero_registered", Some(f64::from(u8::from(s.is_registered))))?;
                put("aero_activated", Some(f64::from(u8::from(s.is_activated))))?;
                let weight = s.point.permanent_stake_balance as f64 + s.point.bias as f64
                    - s.point.slope as f64 * (ts - s.point.ts as i64).max(0) as f64;
                put("aero_gauge_weight", Some(weight.max(0.0) / 1e18))?;
            }
            put("aero_emissions_per_vp", per_vp)?;
            put("aero_emission_cap_aero_s", cap.map(|c| c / 1e18))?;
            if let Some((referrer, share)) = &referral {
                put(
                    "aero_referral_share_pips",
                    Some(if referrer.trim_start_matches("0x").chars().all(|c| c == '0') { 0.0 } else { *share }),
                )?;
            }
            // The early-unstake window, in blocks (0 = none). Never turned into seconds.
            if let Some(w) = window {
                put("aero_min_stake_blocks", Some(w.min_stake_blocks as f64))?;
                put("aero_penalty_rate_pips", Some(w.penalty_rate_pips as f64))?;
            }
            if ctx.and_then(|c| c.vault.as_ref()).is_some() {
                if let Some(ids) = all(9).and_then(|w| aero::decode_uint_array(&w)) {
                    put("aero_vault_staked_nfts", Some(ids.len() as f64))?;
                    staked_ids = ids;
                }
                put("aero_vault_deferred_aero", num(10, 1e18))?;
            }
        }
        None => {}
    }
    let bal = out[3].as_ref().ok().and_then(Value::as_str).and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    if let Some(b) = bal {
        store.put_series("keeper", "wallet_eth", ts, b as f64 / 1e18)?;
        n += 1;
    }

    // Aero: each staked NFT's deposit block (the window counts from it) and the gauge's earned SUM over them.
    if let (Some(c), false) = (ctx.filter(|c| c.mode == GaugeMode::Aero), staked_ids.is_empty()) {
        let vault = c.vault.as_deref().expect("ids come from the vault");
        let mut calls: Vec<(&str, Value)> =
            staked_ids.iter().map(|id| call(gauge, format!("{}{id:064x}", selector("depositBlock(uint256)")))).collect();
        calls.push(call(gauge, aero::earned_array_call(vault, &staked_ids)));
        let out = rpc.batch(&calls).await?;
        let tx = store.write_tx()?;
        for (id, r) in staked_ids.iter().zip(&out) {
            if let Some(b) = ok_str(r).and_then(|s| words(s).first().map(|w| word_u128(w) as i64)) {
                tx.execute(
                    "INSERT OR REPLACE INTO aero_stakes (token_id, deposit_block, ts) VALUES (?1, ?2, ?3)",
                    params![*id as i64, b, ts],
                )?;
            }
        }
        tx.commit()?;
        // earned(vault, ids) = the sum over the ids PLUS the vault's deferredEmissions: stored without the latter.
        let sum = ok_str(&out[staked_ids.len()]).and_then(|s| words(s).first().map(|w| word_u128(w) as f64 / 1e18));
        let deferred = store.series_at("gauge", "aero_vault_deferred_aero", ts)?.filter(|(t, _)| *t == ts).map(|(_, v)| v).unwrap_or(0.0);
        if let Some(sum) = sum {
            store.put_series("gauge", "aero_vault_earned_aero", ts, (sum - deferred).max(0.0))?;
            n += 1;
        }
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

    #[tokio::test]
    async fn the_emission_snapshot_in_each_mode() {
        use crate::collect::gauge::resolve;
        use crate::collect::gauge::tests::{cfg, MockBase, GAUGE, POOL};
        let t = aero::LAUNCH_TS + 3600;
        // Aerodrome after the launch: the raw epoch rate is kept, the effective one is 0.
        let s = Store::memory().unwrap();
        let rpc = MockBase::aerodrome(aero::LAUNCH_TS).rpc();
        let ctx = resolve(&rpc, &cfg("auto", None)).await.unwrap().unwrap();
        snapshot_emissions(&rpc, &s, POOL, GAUGE, Some(&ctx), t).await.unwrap();
        let v = |k: &str| s.series_at("gauge", k, t).unwrap().map(|x| x.1);
        assert_eq!(v("reward_rate_epoch_aero_s"), Some(0.503));
        assert_eq!(v("reward_rate_epoch_effective_aero_s"), Some(0.0));
        assert_eq!(v("aerodrome_paying"), Some(0.0));
        assert_eq!(v("period_finish"), Some(aero::LAUNCH_TS as f64));
        assert_eq!(v("voter_share"), Some(0.02));
        // Before periodFinish (and before the launch) the epoch rate is paid.
        let s2 = Store::memory().unwrap();
        snapshot_emissions(&MockBase::aerodrome(aero::LAUNCH_TS).rpc(), &s2, POOL, GAUGE, Some(&ctx), aero::LAUNCH_TS - 3600)
            .await
            .unwrap();
        assert_eq!(s2.series_at("gauge", "reward_rate_epoch_effective_aero_s", aero::LAUNCH_TS).unwrap().map(|x| x.1), Some(0.503));
        // Aero: the voter state, the window in blocks (0 on Base at launch), the referral cut, the staked NFTs with
        // their deposit blocks, and earned(vault, ids) without the vault's deferred emissions.
        let s = Store::memory().unwrap();
        let base = MockBase::aero();
        let calls = base.calls.clone();
        let rpc = base.rpc();
        let ctx = resolve(&rpc, &cfg("auto", None)).await.unwrap().unwrap();
        calls.lock().unwrap().clear();
        snapshot_emissions(&rpc, &s, POOL, GAUGE, Some(&ctx), t).await.unwrap();
        let v = |k: &str| s.series_at("gauge", k, t).unwrap().map(|x| x.1);
        assert_eq!(v("aero_activated"), Some(1.0));
        assert_eq!(v("aero_gauge_weight"), Some(1_000_000.0));
        assert_eq!(v("aero_min_stake_blocks"), Some(0.0));
        assert_eq!(v("aero_penalty_rate_pips"), Some(1_000_000.0));
        assert_eq!(v("aero_referral_share_pips"), Some(50_000.0));
        assert_eq!(v("aero_emission_cap_aero_s"), Some(0.1));
        assert_eq!(v("aero_vault_staked_nfts"), Some(2.0));
        assert_eq!(v("aero_vault_deferred_aero"), Some(1.0));
        assert_eq!(v("aero_vault_earned_aero"), Some(2.5), "earned(vault, [7, 9]) = 3.5: the sum, less the 1.0 deferred");
        assert_eq!(v("period_finish"), None);
        let blocks: Vec<(i64, i64)> = {
            let mut st = s.db.prepare("SELECT token_id, deposit_block FROM aero_stakes ORDER BY token_id").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(blocks, vec![(7, 52_900_000), (9, 52_900_000)]);
        {
            let calls = calls.lock().unwrap();
            for sig in ["periodFinish()", "left()", "rewardRateByEpoch(uint256)", "rewardRate()", "weights(address)"] {
                assert!(!calls.iter().any(|c| c.1 == selector(sig)), "{sig} is not read in Aero mode");
            }
        }
        // Not probed yet: only the pool and the wallet.
        let s = Store::memory().unwrap();
        assert_eq!(snapshot_emissions(&MockBase::aero().rpc(), &s, POOL, GAUGE, None, t).await.unwrap(), 5);
    }

    #[test]
    fn tick_argument_encoding() {
        assert_eq!(format!("{}{}", selector("ticks(int24)"), encode_i64_word(-197250)).len(), 10 + 64);
        assert_eq!(arg_addr("0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A").len(), 64);
    }
}
