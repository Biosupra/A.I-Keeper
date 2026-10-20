//! Reads the Panda pool straight from Base every 30 s: price (tick), liquidity, STAKED liquidity
//! (the emission denominator), swap fee, the gauge's emission rate, the block's base fee, and the
//! AERO price from the AERO/WETH pool the vault's reward swap uses. One JSON-RPC batch per poll.
//! Read-only by construction (see `guard`).
//!
//! The emission rate depends on the gauge (`collect::gauge`): Aerodrome's `rewardRate()` with its
//! `periodFinish()` (the rate counts as 0 once the period is over and from the Aero launch, the raw value is
//! kept next to it), or Aero's LeafVoter views (`gaugeStates`, `emissionsPerVP`, the gauge's `emissionCap` and
//! referral cut). Until the gauge has been probed, no gauge function is read at all.

use crate::aero::{self, GaugeMode};
use crate::chainmath::{selector, tick_to_price, word_address, word_i64, word_u128, word_unsigned_dec, words};
use crate::collect::gauge::{arg_addr, GaugeCtx};
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
    /// Aerodrome gauge `rewardRate()`, whole AERO per second, as read (None in Aero mode).
    pub reward_rate: Option<f64>,
    /// Aerodrome gauge `periodFinish()` as read (None in Aero mode).
    pub period_finish: Option<i64>,
    /// Whole AERO per second really paid to in-range stakers (see the module comment).
    pub reward_rate_effective: Option<f64>,
    /// "aerodrome" or "aero"; None before the gauge has been probed.
    pub gauge_mode: Option<&'static str>,
    /// Aero: the LeafVoter's gross capped rate (whole AERO per second), its projected cumulative reward share
    /// (wei, exact decimal) and whether the gauge is activated.
    pub aero_gross_rate: Option<f64>,
    pub aero_projected_share: Option<String>,
    pub aero_activated: Option<bool>,
    /// The AERO price pool's tick as AERO per WETH = 1.0001^tick, whichever way round the pool holds them.
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

pub async fn read_pool(rpc: &Rpc, pool: &str, gauge: &str, ctx: Option<&GaugeCtx>) -> Result<PoolObs> {
    let call = |to: &str, data: String| ("eth_call", json!([{ "to": to, "data": data }, "latest"]));
    let price = ctx.map(|c| c.price.clone()).unwrap_or_else(|| Some(GaugeCtx::default_price()));
    let mut calls = vec![
        ("eth_getBlockByNumber", json!(["latest", false])),
        call(pool, selector("slot0()")),
        call(pool, selector("liquidity()")),
        call(pool, selector("fee()")),
        call(pool, selector("stakedLiquidity()")),
        call(price.as_ref().map(|p| p.pool.as_str()).unwrap_or(AERO_WETH_POOL), selector("slot0()")),
    ];
    match ctx.map(|c| c.mode) {
        Some(GaugeMode::Aerodrome) => {
            calls.push(call(gauge, selector("rewardRate()")));
            calls.push(call(gauge, selector("periodFinish()")));
        }
        Some(GaugeMode::Aero) => {
            let c = ctx.expect("mode");
            let g = arg_addr(gauge);
            calls.push(call(&c.voter, format!("{}{g}", selector("gaugeStates(address)"))));
            calls.push(call(&c.voter, selector("emissionsPerVP()")));
            calls.push(call(&c.voter, format!("{}{g}", selector("projectedCumulativeRewardShare(address)"))));
            calls.push(call(&c.factory, format!("{}{g}", selector("emissionCap(address)"))));
            calls.push(call(&c.factory, format!("{}{g}", selector("referralConfig(address)"))));
        }
        None => {}
    }
    let out = rpc.batch(&calls).await?;
    let mut it = out.into_iter();
    let block = it.next().ok_or_else(|| anyhow!("no block"))??;
    let slot0 = it.next().ok_or_else(|| anyhow!("no slot0"))??;
    let liq = it.next().ok_or_else(|| anyhow!("no liquidity"))??;
    let fee = first_word(it.next());
    let staked = first_word(it.next());
    let aero_slot0 = it.next().and_then(|r| r.ok()).and_then(|v| v.as_str().map(words));
    let all_words = |r: Option<Result<Value>>| r.and_then(|r| r.ok()).and_then(|v| v.as_str().map(words)).filter(|w| !w.is_empty());

    let w = words(slot0.as_str().unwrap_or_default());
    if w.len() < 2 {
        return Err(anyhow!("slot0 returned too little data"));
    }
    let ts = hex_i64(&block["timestamp"]).ok_or_else(|| anyhow!("block without timestamp"))?;
    let staked_liquidity = staked.map(|s| word_u128(&s));
    let mut o = PoolObs {
        ts,
        block: hex_i64(&block["number"]).unwrap_or(0),
        sqrt_price: word_u128(&w[0]),
        tick: word_i64(&w[1]),
        liquidity: word_u128(words(liq.as_str().unwrap_or("0x0")).first().map(String::as_str).unwrap_or("0")),
        staked_liquidity,
        fee_pips: fee.map(|f| word_u128(&f) as i64),
        base_fee_gwei: hex_i64(&block["baseFeePerGas"]).map(|w| w as f64 / 1e9),
        reward_rate: None,
        period_finish: None,
        reward_rate_effective: None,
        gauge_mode: ctx.map(|c| c.mode.name()),
        aero_gross_rate: None,
        aero_projected_share: None,
        aero_activated: None,
        aero_weth_tick: match &price {
            Some(p) => aero_slot0.filter(|w| w.len() >= 2).map(|w| aero::aero_per_weth_tick(word_i64(&w[1]), p.aero_is_token0)),
            None => None,
        },
    };
    match ctx.map(|c| c.mode) {
        Some(GaugeMode::Aerodrome) => {
            o.reward_rate = first_word(it.next()).map(|r| word_u128(&r) as f64 / 1e18);
            o.period_finish = first_word(it.next()).map(|f| word_u128(&f) as i64);
            o.reward_rate_effective = o.reward_rate.map(|r| aero::aerodrome_rate_effective(r, o.period_finish, ts));
        }
        Some(GaugeMode::Aero) => {
            let state = all_words(it.next()).and_then(|w| aero::decode_gauge_state(&w));
            let per_vp = first_word(it.next()).map(|w| word_u128(&w) as f64);
            o.aero_projected_share = first_word(it.next()).map(|w| word_unsigned_dec(&w));
            let cap = first_word(it.next()).map(|w| word_u128(&w) as f64);
            let referral =
                all_words(it.next()).filter(|w| w.len() >= 2).map(|w| (word_address(&w[0]), word_u128(&w[1]).min(u64::MAX as u128) as u64));
            o.aero_activated = state.map(|s| s.is_registered && s.is_activated);
            if let (Some(s), Some(per_vp), Some(cap), Some((referrer, share))) = (state, per_vp, cap, referral) {
                // Not registered or not activated: the voter sends its weight to the zero gauge, nothing is paid.
                let gross =
                    if s.is_registered && s.is_activated { aero::voter_gauge_rate_wei_per_s(&s.point, per_vp, cap, ts) } else { 0.0 };
                // The gauge takes the referral cut only when a referral address is set (Gauge._applyReferral).
                let net =
                    if referrer.trim_start_matches("0x").chars().all(|c| c == '0') { gross } else { aero::net_of_referral(gross, share) };
                o.aero_gross_rate = Some(gross / 1e18);
                o.reward_rate_effective = Some(net / 1e18);
            }
        }
        None => {}
    }
    Ok(o)
}

pub async fn poll(rpc: &Rpc, pool: &str, gauge: &str, ctx: Option<&GaugeCtx>, store: &Store) -> Result<PoolObs> {
    let o = read_pool(rpc, pool, gauge, ctx).await?;
    let price = tick_to_price(o.tick as f64);
    let aero = o.aero_weth_tick.map(|t| aero_usd(price, t));
    store.db.execute(
        "INSERT OR REPLACE INTO pool_obs (ts, block, tick, sqrt_price, liquidity, fee_pips, base_fee_gwei, weth_usd, reward_rate, staked_liquidity, aero_weth_tick, aero_usd,
           period_finish, reward_rate_effective, gauge_mode, aero_gross_rate, aero_projected_share, aero_activated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            o.ts, o.block, o.tick, o.sqrt_price.to_string(), o.liquidity.to_string(), o.fee_pips, o.base_fee_gwei, price, o.reward_rate,
            o.staked_liquidity.map(|s| s.to_string()), o.aero_weth_tick, aero, o.period_finish, o.reward_rate_effective, o.gauge_mode,
            o.aero_gross_rate, o.aero_projected_share, o.aero_activated.map(i64::from)
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

    #[tokio::test]
    async fn the_pool_read_in_each_mode_and_the_aero_price_either_way_round() {
        use crate::collect::gauge::resolve;
        use crate::collect::gauge::tests::{cfg, MockBase, GAUGE, NEW_POOL, POOL};
        let t = aero::LAUNCH_TS + 3600; // the mock's block time
                                        // Aerodrome after its last period: rewardRate() still 0.503, nothing paid; both stored, raw untouched.
        let s = Store::memory().unwrap();
        let rpc = MockBase::aerodrome(aero::LAUNCH_TS).rpc();
        let ctx = resolve(&rpc, &cfg("auto", None)).await.unwrap().unwrap();
        let o = poll(&rpc, POOL, GAUGE, Some(&ctx), &s).await.unwrap();
        assert_eq!((o.ts, o.reward_rate, o.period_finish, o.reward_rate_effective), (t, Some(0.503), Some(aero::LAUNCH_TS), Some(0.0)));
        let (raw, eff, mode): (f64, f64, String) = s
            .db
            .query_row("SELECT reward_rate, reward_rate_effective, gauge_mode FROM pool_obs", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((raw, eff, mode.as_str()), (0.503, 0.0, "aerodrome"));
        // Still before periodFinish the rate is paid.
        let rpc = MockBase::aerodrome(aero::LAUNCH_TS + 7 * 86_400).rpc();
        let o = read_pool(&rpc, POOL, GAUGE, Some(&ctx)).await.unwrap();
        assert_eq!(o.reward_rate_effective, Some(0.0), "from the launch nothing is paid even if periodFinish moved on");
        // Aero: 0.001 AERO/s gross, 5% referral, the price from a pool holding AERO as token0.
        let s = Store::memory().unwrap();
        let rpc = MockBase::aero().rpc();
        let ctx = resolve(&rpc, &cfg("auto", Some(NEW_POOL))).await.unwrap().unwrap();
        let o = poll(&rpc, POOL, GAUGE, Some(&ctx), &s).await.unwrap();
        assert_eq!((o.reward_rate, o.period_finish, o.gauge_mode), (None, None, Some("aero")));
        assert!((o.aero_gross_rate.unwrap() - 0.001).abs() < 1e-15);
        assert!((o.reward_rate_effective.unwrap() - 0.00095).abs() < 1e-15);
        assert_eq!(o.aero_activated, Some(true));
        assert_eq!(o.aero_projected_share.as_deref(), Some("12345000000000000000"));
        assert_eq!(o.aero_weth_tick, Some(81_200), "AERO token0: the tick is turned round");
        let aerodrome_price = aero_usd(tick_to_price(-197_254.0), 81_200);
        let stored: f64 = s.db.query_row("SELECT aero_usd FROM pool_obs", [], |r| r.get(0)).unwrap();
        assert!((stored - aerodrome_price).abs() < 1e-12, "the same AERO price from either pool");
        // The fallback (no new pool configured) prices the new AERO from Aerodrome's pool, one for one.
        let ctx = resolve(&rpc, &cfg("auto", None)).await.unwrap().unwrap();
        let o = read_pool(&rpc, POOL, GAUGE, Some(&ctx)).await.unwrap();
        assert_eq!(o.aero_weth_tick, Some(81_200));
        // Not probed yet: no gauge function is called, and the pool and price are still read.
        let o = read_pool(&MockBase::aero().rpc(), POOL, GAUGE, None).await.unwrap();
        assert_eq!((o.reward_rate, o.reward_rate_effective, o.gauge_mode), (None, None, None));
        assert_eq!(o.aero_weth_tick, Some(81_200));
    }
}
