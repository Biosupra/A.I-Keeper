//! Which gauge AIK_GAUGE is (Aerodrome or Aero, `AIK_GAUGE_MODE`), probed once, and what the replays need to
//! know about it. Read-only by construction (see `guard`): every read here is an `eth_call` view.
//!
//! * The probe: `rewardRate()` answers = Aerodrome; `earned(address,uint256[])` answers and `rewardRate()`
//!   reverts = Aero (`aero::detect_gauge_mode`). An explicit `AIK_GAUGE_MODE` is checked against it.
//! * Aero mode: the LeafVoter and the gauge factory are read from the gauge (`voter()`, `gaugeFactory()`).
//! * The AERO price pool: `AIK_AERO_WETH_POOL` when it holds WETH and the gauge's AERO, else (Aero mode only)
//!   Aerodrome's AERO/WETH CL200 pool, old AERO = new AERO one for one (`aero::aero_price_source`).
//!
//! The result is kept in `meta` (gauge_mode, gauge_mode_vault, aero_price_source, gauge_mode_error) so the
//! replays, the alerts and the brief say which mode they ran in.

use crate::aero::{self, AeroPriceSource, GaugeMode, GaugeModeSetting};
use crate::chainmath::{selector, word_address, words};
use crate::collect::chain::AERO_WETH_POOL;
use crate::config::Config;
use crate::net::Rpc;
use crate::store::Store;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The pool that prices AERO and its token order.
#[derive(Clone, Debug, PartialEq)]
pub struct PricePool {
    pub pool: String,
    pub aero_is_token0: bool,
}

/// What the probe decided, fixed for the life of the process.
#[derive(Clone, Debug, PartialEq)]
pub struct GaugeCtx {
    pub mode: GaugeMode,
    /// Aero: the LeafVoter (`gauge.voter()`). Aerodrome: its Voter (unused here).
    pub voter: String,
    /// Aero: the CLGaugeFactory (`gauge.gaugeFactory()`).
    pub factory: String,
    pub price: Option<PricePool>,
    pub price_source: AeroPriceSource,
    /// The vault whose staked NFTs are read in Aero mode (AIK_PRIMARY_VAULT when it is an address).
    pub vault: Option<String>,
}

impl GaugeCtx {
    /// The price pool when no probe has succeeded yet: Aerodrome's AERO/WETH pool, WETH token0 (today's read).
    pub fn default_price() -> PricePool {
        PricePool { pool: AERO_WETH_POOL.into(), aero_is_token0: false }
    }
}

pub(crate) fn call(to: &str, data: String) -> (&'static str, Value) {
    ("eth_call", json!([{ "to": to, "data": data }, "latest"]))
}

pub(crate) fn arg_addr(a: &str) -> String {
    format!("{:0>64}", a.trim_start_matches("0x").to_ascii_lowercase())
}

/// Whether a call answered: Ok(Some(words)) with data, Ok(None) when the contract reverted or returned nothing
/// (no such function, or no contract), Err on a transport failure (to be retried, never read as "no").
pub(crate) fn answered(r: &Result<Value>) -> Result<Option<Vec<String>>> {
    match r {
        Ok(v) => {
            let w = v.as_str().map(words).unwrap_or_default();
            Ok((!w.is_empty() && w.iter().all(|x| x.len() == 64)).then_some(w))
        }
        Err(e) if format!("{e:#}").to_ascii_lowercase().contains("revert") => Ok(None),
        Err(e) => Err(anyhow!("{e:#}")),
    }
}

pub fn is_address(a: &str) -> bool {
    a.len() == 42 && a.starts_with("0x") && a[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Probes AIK_GAUGE and the AERO price pools. Outer Err: a transport failure (try again later). Inner Err: the
/// gauge answered but the mode cannot be used (no mode, or AIK_GAUGE_MODE disagrees with the gauge).
pub async fn resolve(rpc: &Rpc, cfg: &Config) -> Result<std::result::Result<GaugeCtx, String>> {
    let setting: GaugeModeSetting = match &cfg.gauge_mode {
        Ok(s) => *s,
        Err(e) => return Ok(Err(e.clone())),
    };
    let zero = "0x0000000000000000000000000000000000000000";
    let probe = rpc.batch(&[call(&cfg.gauge, selector("rewardRate()")), call(&cfg.gauge, aero::earned_array_call(zero, &[]))]).await?;
    let reward_rate = answered(&probe[0])?.is_some();
    let earned_array = answered(&probe[1])?.is_some();
    let mode = match aero::detect_gauge_mode(setting, reward_rate, earned_array) {
        Ok(m) => m,
        Err(e) => return Ok(Err(e)),
    };

    let (mut voter, mut factory) = (String::new(), String::new());
    if mode == GaugeMode::Aero {
        let r = rpc.batch(&[call(&cfg.gauge, selector("voter()")), call(&cfg.gauge, selector("gaugeFactory()"))]).await?;
        let addr = |i: usize, fallback: &str| -> Result<String> {
            Ok(match answered(&r[i])? {
                Some(w) => word_address(&w[0]),
                None => {
                    tracing::warn!(
                        "gauge: AIK_GAUGE did not answer {}; using the verified Aero address {fallback}",
                        ["voter()", "gaugeFactory()"][i]
                    );
                    fallback.to_ascii_lowercase()
                }
            })
        };
        voter = addr(0, aero::LEAF_VOTER)?;
        factory = addr(1, aero::CL_GAUGE_FACTORY)?;
        if voter != aero::LEAF_VOTER.to_ascii_lowercase() || factory != aero::CL_GAUGE_FACTORY.to_ascii_lowercase() {
            tracing::warn!("gauge: Aero gauge reports voter {voter} and factory {factory}, not the addresses verified on 2026-10-20");
        }
    }

    // The AERO price: the configured pool (Aerodrome mode: configured or Aerodrome's own), else the fallback.
    let configured = match mode {
        GaugeMode::Aerodrome => Some(cfg.aero_weth_pool.clone().unwrap_or_else(|| AERO_WETH_POOL.into())),
        GaugeMode::Aero => cfg.aero_weth_pool.clone(),
    };
    let tokens = |pool: &str| [call(pool, selector("token0()")), call(pool, selector("token1()"))];
    let mut calls = tokens(AERO_WETH_POOL).to_vec();
    if let Some(p) = &configured {
        calls.extend(tokens(p));
    }
    let r = rpc.batch(&calls).await?;
    let order = |i: usize, aero_token: &str| -> Result<Option<bool>> {
        Ok(match (answered(&r[i])?, answered(&r[i + 1])?) {
            (Some(a), Some(b)) => aero::aero_pool_order(&word_address(&a[0]), &word_address(&b[0]), aero_token),
            _ => None,
        })
    };
    let fallback_order = order(0, aero::OLD_AERO)?;
    let gauge_aero = if mode == GaugeMode::Aero { aero::NEW_AERO } else { aero::OLD_AERO };
    let configured_order = if configured.is_some() { order(2, gauge_aero)? } else { None };
    let price_source = aero::aero_price_source(mode, configured_order.is_some(), fallback_order.is_some());
    let price = match price_source {
        AeroPriceSource::Configured => configured.map(|pool| PricePool { pool, aero_is_token0: configured_order.unwrap_or(false) }),
        AeroPriceSource::AerodromeFallback => {
            Some(PricePool { pool: AERO_WETH_POOL.into(), aero_is_token0: fallback_order.unwrap_or(false) })
        }
        AeroPriceSource::Unavailable => None,
    };
    let vault = is_address(&cfg.primary_vault).then(|| cfg.primary_vault.to_ascii_lowercase());
    Ok(Ok(GaugeCtx { mode, voter, factory, price, price_source, vault }))
}

/// `resolve`, recorded in `meta` and logged. Returns the context (None while the mode is unknown: the gauge is
/// then not read) and whether the gauge refused (a configuration problem, kept in `meta` for the alert) rather
/// than Base being unreachable.
pub async fn resolve_and_record(rpc: &Rpc, cfg: &Config, store: &Store, now: i64) -> (Option<GaugeCtx>, bool) {
    match resolve(rpc, cfg).await {
        Ok(Ok(ctx)) => {
            tracing::info!(
                "gauge: {} mode ({}), AERO price from {} ({})",
                ctx.mode.name(),
                if matches!(cfg.gauge_mode, Ok(GaugeModeSetting::Auto)) { "probed" } else { "set and confirmed by the probe" },
                ctx.price.as_ref().map(|p| p.pool.as_str()).unwrap_or("nowhere"),
                ctx.price_source.name()
            );
            if let Err(e) = record(store, &ctx, &cfg.primary_vault, now) {
                tracing::warn!("gauge: could not record the mode: {e:#}");
            }
            (Some(ctx), false)
        }
        Ok(Err(e)) => {
            tracing::error!("gauge: {e}; the gauge is not read until this is fixed");
            let _ = store.meta_set("gauge_mode_error", &e);
            (None, true)
        }
        Err(e) => {
            tracing::warn!("gauge: the probe could not reach Base ({e:#}); trying again on the next poll");
            (None, false)
        }
    }
}

fn record(store: &Store, ctx: &GaugeCtx, vault: &str, now: i64) -> Result<()> {
    store.meta_set("gauge_mode", ctx.mode.name())?;
    store.meta_set("gauge_mode_vault", &vault.to_ascii_lowercase())?;
    store.meta_set("gauge_mode_ts", &now.to_string())?;
    store.meta_set("aero_price_source", ctx.price_source.name())?;
    store.meta_set("aero_price_pool", ctx.price.as_ref().map(|p| p.pool.as_str()).unwrap_or(""))?;
    store.meta_set("gauge_mode_error", "")?;
    Ok(())
}

/// The gauge mode last recorded (None before any probe succeeded).
pub fn recorded_mode(store: &Store) -> Option<GaugeMode> {
    store.meta_get("gauge_mode").ok().flatten().and_then(|m| GaugeMode::from_name(&m))
}

/// Whether `vault`'s keeper stakes in an Aero gauge: the A.I's gauge is Aero and the vault is the one it follows.
/// Every other vault (and every vault before a successful Aero probe) is on Aerodrome.
pub fn vault_on_aero(store: &Store, vault: &str) -> bool {
    recorded_mode(store) == Some(GaugeMode::Aero)
        && store.meta_get("gauge_mode_vault").ok().flatten().map(|v| v == vault.to_ascii_lowercase()).unwrap_or(false)
}

/// The latest Aerodrome `periodFinish` reading: (read at, periodFinish), from the 30-second pool reads and the
/// hourly emission snapshot, whichever is newer.
pub fn latest_period_finish(store: &Store) -> Option<(i64, i64)> {
    let obs: Option<(i64, i64)> = store
        .db
        .query_row("SELECT ts, period_finish FROM pool_obs WHERE period_finish IS NOT NULL ORDER BY ts DESC LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok();
    let series: Option<(i64, i64)> = store.series_at("gauge", "period_finish", i64::MAX).ok().flatten().map(|(t, v)| (t, v as i64));
    match (obs, series) {
        (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// For a replay of `vault`'s keeper: from when its rewards count as zero (Aerodrome's end), or None when the vault
/// is on Aero (its keeper's tables carry Aero's real rate).
pub fn rewards_end_for(store: &Store, vault: &str) -> Option<i64> {
    if vault_on_aero(store, vault) {
        None
    } else {
        Some(aero::aerodrome_rewards_end(latest_period_finish(store)))
    }
}

/// The selftest's chain lines: the probe, one pool read and one emission snapshot. Returns the lines and
/// whether every read worked.
pub async fn selftest_lines(rpc: &Rpc, cfg: &Config, store: &Store, t: i64) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut ok = true;
    let ctx = match resolve(rpc, cfg).await {
        Ok(Ok(ctx)) => {
            out.push(format!(
                "  Gauge mode     {} ({}) · AERO price from {} ({})",
                ctx.mode.name(),
                match &cfg.gauge_mode {
                    Ok(GaugeModeSetting::Auto) => "probed",
                    _ => "set, confirmed by the probe",
                },
                ctx.price.as_ref().map(|p| p.pool.as_str()).unwrap_or("nowhere"),
                ctx.price_source.name()
            ));
            if ctx.mode == GaugeMode::Aero {
                out.push(format!("  Aero voter     {} · gauge factory {}", ctx.voter, ctx.factory));
            }
            let _ = record(store, &ctx, &cfg.primary_vault, t);
            Some(ctx)
        }
        Ok(Err(e)) => {
            ok = false;
            out.push(format!("  Gauge mode     REFUSED: {e}"));
            None
        }
        Err(e) => {
            ok = false;
            out.push(format!("  Gauge mode     FAILED: {e:#}"));
            None
        }
    };
    match crate::collect::chain::poll(rpc, &cfg.pool, &cfg.gauge, ctx.as_ref(), store).await {
        Ok(o) => out.push(format!(
            "  Base pool      block {} tick {} (${:.2}) fee {:?} pips, base fee {:?} gwei, emissions {:?} AERO/s (raw {:?}, {})",
            o.block,
            o.tick,
            crate::chainmath::tick_to_price(o.tick as f64),
            o.fee_pips,
            o.base_fee_gwei,
            o.reward_rate_effective,
            o.reward_rate,
            o.gauge_mode.unwrap_or("gauge not read")
        )),
        Err(e) => {
            ok = false;
            out.push(format!("  Base pool      FAILED: {e:#}"));
        }
    }
    match crate::collect::liquidity::snapshot_emissions(rpc, store, &cfg.pool, &cfg.gauge, ctx.as_ref(), t).await {
        Ok(n) => out.push(format!("  Emissions/vote {n} values")),
        Err(e) => {
            ok = false;
            out.push(format!("  Emissions/vote FAILED: {e:#}"));
        }
    }
    (out, ok)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::net::MockChain;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    pub const POOL: &str = "0x3fe04a59ebd38cf06080a6f60a98d124eb59392a";
    pub const GAUGE: &str = "0xa0b61fdb9f1fb9b917fe38b49427fd4d87472d28";
    pub const VAULT: &str = "0x61986afae9b07314d855e7c54949e30a866c93a8";
    pub const NEW_POOL: &str = "0x1111111111111111111111111111111111111111";

    pub fn w(v: u128) -> String {
        format!("{v:064x}")
    }
    fn neg(v: i128) -> String {
        format!("{}{:032x}", "f".repeat(32), v as u128)
    }
    fn a(addr: &str) -> String {
        arg_addr(addr)
    }

    /// Base in miniature for one gauge mode: every view the A.I calls answers as the verified contracts would;
    /// anything else reverts, as a missing function does on chain. `calls` records every (to, selector).
    pub struct MockBase {
        pub answers: HashMap<(String, String), String>,
        pub calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockBase {
        fn put(&mut self, to: &str, sig: &str, data: String) {
            self.answers.insert((to.to_ascii_lowercase(), selector(sig)), format!("0x{data}"));
        }

        /// Aerodrome today: rewardRate 0.503 AERO/s, periodFinish at the launch, WETH/AERO price pool.
        pub fn aerodrome(period_finish: i64) -> MockBase {
            let mut m = MockBase { answers: HashMap::new(), calls: Arc::new(Mutex::new(Vec::new())) };
            m.common();
            m.put(GAUGE, "rewardRate()", w(503_000_000_000_000_000));
            m.put(GAUGE, "periodFinish()", w(period_finish as u128));
            m.put(GAUGE, "left()", w(0));
            m.put(GAUGE, "rewardRateByEpoch(uint256)", w(503_000_000_000_000_000));
            m.put(GAUGE, "fees0()", w(0));
            m.put(GAUGE, "fees1()", w(0));
            m.put(POOL, "rewardReserve()", w(0));
            m.put(crate::collect::liquidity::VOTER, "weights(address)", w(2_000_000_000_000_000_000));
            m.put(crate::collect::liquidity::VOTER, "totalWeight()", w(100_000_000_000_000_000_000));
            m
        }

        /// Aero: a permanent allocation of 1,000,000 AERO at emissionsPerVP 1e9 = 0.001 AERO/s gross, a 5%
        /// referral cut, 2e16 in-range staked liquidity, no penalty window (Base at launch), the vault staking
        /// NFTs 7 and 9, and a NEW AERO/WETH pool (AERO token0).
        pub fn aero() -> MockBase {
            let mut m = MockBase { answers: HashMap::new(), calls: Arc::new(Mutex::new(Vec::new())) };
            m.common();
            let voter = aero::LEAF_VOTER;
            let factory = aero::CL_GAUGE_FACTORY;
            // earned(account, ids) = the sum over the ids + the account's deferredEmissions (1 AERO): 3.5 AERO.
            m.put(GAUGE, "earned(address,uint256[])", w(3_500_000_000_000_000_000));
            m.put(GAUGE, "voter()", a(voter));
            m.put(GAUGE, "gaugeFactory()", a(factory));
            m.put(GAUGE, "stakedValues(address)", format!("{}{}{}{}", w(32), w(2), w(7), w(9)));
            m.put(GAUGE, "deferredEmissions(address)", w(1_000_000_000_000_000_000));
            m.put(GAUGE, "depositBlock(uint256)", w(52_900_000));
            m.put(
                voter,
                "gaugeStates(address)",
                format!(
                    "{}{}{}{}{}{}{}{}{}{}{}{}",
                    w(10_000_000_000_000_000_000),
                    w(0),
                    w(1_792_627_200),
                    w(1),
                    w(1),
                    w(0),
                    w(0),
                    w(0),
                    neg(0),
                    neg(0),
                    w(1_792_627_200),
                    w(1_000_000_000_000_000_000_000_000)
                ),
            );
            m.put(voter, "emissionsPerVP()", w(1_000_000_000));
            m.put(voter, "projectedCumulativeRewardShare(address)", w(12_345_000_000_000_000_000));
            m.put(factory, "effectivePenaltyConfig(address)", format!("{}{}", w(0), w(1_000_000)));
            m.put(factory, "emissionCap(address)", w(100_000_000_000_000_000));
            m.put(factory, "referralConfig(address)", format!("{}{}", a("0x00000000000000000000000000000000000000ee"), w(50_000)));
            m.put(NEW_POOL, "token0()", a(aero::NEW_AERO));
            m.put(NEW_POOL, "token1()", a(aero::WETH));
            // AERO token0: tick -81200 = AERO per WETH 1.0001^81200 (the same price as the Aerodrome pool's +81200).
            m.put(NEW_POOL, "slot0()", format!("{}{}", w(1), neg(-81_200)));
            m
        }

        fn common(&mut self) {
            // The WETH/USDC pool: tick -197254 (≈ $2,715), liquidity, 4 bps fee, 2e16 staked.
            self.put(POOL, "slot0()", format!("{}{}", w(4_115_564_623_999_126_475_116_757), neg(-197_254)));
            self.put(POOL, "liquidity()", w(50_000_000_000_000_000));
            self.put(POOL, "fee()", w(400));
            self.put(POOL, "stakedLiquidity()", w(20_000_000_000_000_000));
            self.put(POOL, "gaugeFees()", format!("{}{}", w(0), w(0)));
            // Aerodrome's AERO/WETH CL200 pool: WETH token0, old AERO token1, AERO per WETH 1.0001^81200.
            self.put(AERO_WETH_POOL, "token0()", a(aero::WETH));
            self.put(AERO_WETH_POOL, "token1()", a(aero::OLD_AERO));
            self.put(AERO_WETH_POOL, "slot0()", format!("{}{}", w(1), w(81_200)));
        }

        pub fn rpc(self) -> Rpc {
            let MockBase { answers, calls } = self;
            let chain: MockChain = Arc::new(move |method: &str, params: &Value| -> Result<Value> {
                match method {
                    "eth_getBlockByNumber" => Ok(
                        json!({"number": "0x3272a40", "timestamp": format!("0x{:x}", aero::LAUNCH_TS + 3600), "baseFeePerGas": "0x1000000"}),
                    ),
                    "eth_getBalance" => Ok(json!("0x2386f26fc10000")),
                    "eth_blockNumber" => Ok(json!("0x3272a40")),
                    "eth_call" => {
                        let to = params[0]["to"].as_str().unwrap_or("").to_ascii_lowercase();
                        let data = params[0]["data"].as_str().unwrap_or("");
                        let sel = data.get(..10).unwrap_or("").to_string();
                        calls.lock().unwrap().push((to.clone(), sel.clone()));
                        match answers.get(&(to, sel)) {
                            Some(v) => Ok(json!(v)),
                            None => Err(anyhow!("RPC error: {{\"code\":3,\"message\":\"execution reverted\"}}")),
                        }
                    }
                    other => Err(anyhow!("mock: {other} not served")),
                }
            });
            Rpc::mock(chain)
        }
    }

    pub fn cfg(mode: &str, aero_pool: Option<&str>) -> Config {
        let mut c = Config::from_env();
        c.pool = POOL.into();
        c.gauge = GAUGE.into();
        c.gauge_mode = mode.parse();
        c.aero_weth_pool = aero_pool.map(str::to_string);
        c.primary_vault = VAULT.into();
        c
    }

    #[tokio::test]
    async fn the_probe_resolves_each_gauge_and_refuses_a_wrong_setting() {
        let ctx = resolve(&MockBase::aerodrome(aero::LAUNCH_TS).rpc(), &cfg("auto", None)).await.unwrap().unwrap();
        assert_eq!(ctx.mode, GaugeMode::Aerodrome);
        assert_eq!(ctx.price, Some(GaugeCtx::default_price()), "Aerodrome mode keeps today's price pool");
        assert_eq!(ctx.price_source, AeroPriceSource::Configured);
        let ctx = resolve(&MockBase::aero().rpc(), &cfg("auto", Some(NEW_POOL))).await.unwrap().unwrap();
        assert_eq!(ctx.mode, GaugeMode::Aero);
        assert_eq!(
            (ctx.voter.as_str(), ctx.factory.as_str()),
            (aero::LEAF_VOTER.to_ascii_lowercase().as_str(), aero::CL_GAUGE_FACTORY.to_ascii_lowercase().as_str())
        );
        assert_eq!(ctx.price, Some(PricePool { pool: NEW_POOL.into(), aero_is_token0: true }));
        assert_eq!(ctx.vault.as_deref(), Some(VAULT));
        // Aero mode without a new AERO pool: the Aerodrome pool, old AERO = new AERO.
        let ctx = resolve(&MockBase::aero().rpc(), &cfg("aero", None)).await.unwrap().unwrap();
        assert_eq!(ctx.price_source, AeroPriceSource::AerodromeFallback);
        assert_eq!(ctx.price, Some(GaugeCtx::default_price()));
        // ...and a configured pool that does not hold the new AERO (the old pool) is not used as "configured".
        let ctx = resolve(&MockBase::aero().rpc(), &cfg("aero", Some(AERO_WETH_POOL))).await.unwrap().unwrap();
        assert_eq!(ctx.price_source, AeroPriceSource::AerodromeFallback);
        // Aerodrome mode pointed at a pool that is not WETH/AERO: no price rather than a wrong one.
        let ctx = resolve(&MockBase::aerodrome(aero::LAUNCH_TS).rpc(), &cfg("aerodrome", Some(NEW_POOL))).await.unwrap().unwrap();
        assert_eq!((ctx.price_source, ctx.price), (AeroPriceSource::Unavailable, None));
        // A setting the gauge contradicts, an unknown setting, and a gauge that answers neither.
        assert!(resolve(&MockBase::aero().rpc(), &cfg("aerodrome", None)).await.unwrap().unwrap_err().contains("AIK_GAUGE_MODE=aerodrome"));
        assert!(resolve(&MockBase::aerodrome(0).rpc(), &cfg("aero", None)).await.unwrap().unwrap_err().contains("AIK_GAUGE_MODE=aero"));
        assert!(resolve(&MockBase::aero().rpc(), &cfg("velodrome", None)).await.unwrap().unwrap_err().contains("not one of"));
        let mut empty = MockBase::aero();
        empty.answers.clear();
        assert!(resolve(&empty.rpc(), &cfg("auto", None)).await.unwrap().unwrap_err().contains("neither"));
        // A transport failure is not a "no": the probe reports it to be retried.
        let down: MockChain = Arc::new(|_: &str, _: &Value| -> Result<Value> { Err(anyhow!("connection reset")) });
        assert!(resolve(&Rpc::mock(down), &cfg("auto", None)).await.is_err());
    }

    #[tokio::test]
    async fn the_selftest_passes_in_both_modes_with_mocks() {
        // Aerodrome (after the launch: the phantom rule must show 0 next to the raw 0.503).
        let s = Store::memory().unwrap();
        let base = MockBase::aerodrome(aero::LAUNCH_TS);
        let calls = base.calls.clone();
        let (lines, ok) = selftest_lines(&base.rpc(), &cfg("auto", None), &s, aero::LAUNCH_TS + 3600).await;
        assert!(ok, "{lines:#?}");
        assert!(lines[0].contains("aerodrome (probed)"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("emissions Some(0.0) AERO/s (raw Some(0.503), aerodrome)")), "{lines:#?}");
        assert_eq!(recorded_mode(&s), Some(GaugeMode::Aerodrome));
        assert!(!calls.lock().unwrap().iter().any(|c| c.1 == selector("gaugeStates(address)")), "no Aero reads in Aerodrome mode");

        // Aero: no rewardRate/periodFinish reads after the probe; the voter's rate net of the referral cut.
        let s = Store::memory().unwrap();
        let base = MockBase::aero();
        let calls = base.calls.clone();
        let (lines, ok) = selftest_lines(&base.rpc(), &cfg("aero", Some(NEW_POOL)), &s, aero::LAUNCH_TS + 3600).await;
        assert!(ok, "{lines:#?}");
        assert!(lines[0].contains("aero (set, confirmed by the probe)"), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("emissions Some(0.00095) AERO/s (raw None, aero)")), "{lines:#?}");
        let calls = calls.lock().unwrap();
        let rr = selector("rewardRate()");
        assert_eq!(calls.iter().filter(|c| c.1 == rr).count(), 1, "rewardRate() only in the probe");
        for sig in ["periodFinish()", "left()", "rewardRateByEpoch(uint256)"] {
            assert!(!calls.iter().any(|c| c.1 == selector(sig)), "{sig} is never read in Aero mode");
        }
        assert!(vault_on_aero(&s, VAULT) && !vault_on_aero(&s, "old"));
        assert_eq!(s.meta_get("aero_price_source").unwrap().as_deref(), Some("configured"));
    }

    /// Every Aero read the A.I makes, checked against the verified Sourcify ABIs in docs/cloud-tasks/sourcify: the
    /// canonical signature (so the selector) and the shape of what comes back, and the Aerodrome-only functions
    /// that must NOT exist on Aero's gauge.
    #[test]
    fn every_aero_read_matches_the_verified_abis() {
        fn abi(file: &str) -> (String, Vec<Value>) {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(format!("docs/cloud-tasks/sourcify/{file}")).unwrap()).unwrap();
            (v["address"].as_str().unwrap().to_ascii_lowercase(), v["abi"].as_array().unwrap().clone())
        }
        fn ty(p: &Value) -> String {
            match p["type"].as_str().unwrap() {
                "tuple" => format!("({})", p["components"].as_array().unwrap().iter().map(ty).collect::<Vec<_>>().join(",")),
                t => t.to_string(),
            }
        }
        // (signature, output types) for each function of the ABI.
        fn functions(abi: &[Value]) -> Vec<(String, Vec<String>)> {
            abi.iter()
                .filter(|f| f["type"] == "function")
                .map(|f| {
                    let ins: Vec<String> = f["inputs"].as_array().unwrap().iter().map(ty).collect();
                    let outs: Vec<String> = f["outputs"].as_array().unwrap().iter().map(ty).collect();
                    (format!("{}({})", f["name"].as_str().unwrap(), ins.join(",")), outs)
                })
                .collect()
        }
        let check = |fns: &[(String, Vec<String>)], sig: &str, outs: &[&str]| {
            let f = fns.iter().find(|f| f.0 == sig).unwrap_or_else(|| panic!("{sig} is not in the verified ABI"));
            assert_eq!(f.1, outs, "{sig} returns");
            assert_eq!(selector(sig), selector(&f.0));
        };
        let (gauge_impl, g) = abi("gaugeimpl.json");
        let g = functions(&g);
        // The dump is the verified source of the first Base CLGauge implementation. Aero redeployed Slipstream on
        // 2026-10-21 (security page): the live implementation is 0x89E91A960154b1ed6c093039a8F0725ab67D14E0 and is
        // NOT verified on Sourcify. Its bytecode holds every selector below; the only differences found are the
        // dropped earned(address) aggregate (unused here) and the added emergencyWithdraw(uint256) +
        // emergencyWithdrawEnabled(). CL_GAUGE_FACTORY in aero.rs is the redeployed factory.
        assert_eq!(gauge_impl, "0x16f866688a306494c612149b48c2fe70ccbe3872");
        check(&g, "earned(address,uint256[])", &["uint256"]);
        check(&g, "voter()", &["address"]);
        check(&g, "gaugeFactory()", &["address"]);
        check(&g, "stakedValues(address)", &["uint256[]"]);
        check(&g, "deferredEmissions(address)", &["uint256"]);
        check(&g, "depositBlock(uint256)", &["uint256"]);
        for gone in
            ["rewardRate()", "periodFinish()", "left()", "rewardRateByEpoch(uint256)", "earned(address,uint256)", "fees0()", "fees1()"]
        {
            assert!(!g.iter().any(|f| f.0 == gone), "{gone} must not be on Aero's gauge (the probe relies on rewardRate() reverting)");
        }
        let (voter, v) = abi("leafvoter.json");
        let v = functions(&v);
        assert_eq!(voter, aero::LEAF_VOTER.to_ascii_lowercase());
        // gaugeStates: 8 static words then the Point inline: 12 words, as `aero::decode_gauge_state` reads them.
        check(
            &v,
            "gaugeStates(address)",
            &["uint128", "uint128", "uint48", "bool", "bool", "uint128", "uint256", "uint256", "(int128,int128,uint48,uint128)"],
        );
        check(&v, "emissionsPerVP()", &["uint256"]);
        check(&v, "projectedCumulativeRewardShare(address)", &["uint256"]);
        let (factory, f) = abi("gaugefactory.json");
        let f = functions(&f);
        // Verified dump = the first Base CLGaugeFactory; the redeployed one (2026-10-21) is aero::CL_GAUGE_FACTORY,
        // unverified, bytecode = all 50 verified functions + the four emergency-withdraw selectors.
        assert_eq!(factory, "0xa02d1f4e4243ff0d350f478c44b0fd37c7bf848c");
        assert_eq!(aero::CL_GAUGE_FACTORY.to_ascii_lowercase(), "0xf437f62053acb121665b8fdbfea8f214fb8e589d");
        check(&f, "effectivePenaltyConfig(address)", &["(uint256,uint256)"]);
        check(&f, "emissionCap(address)", &["uint128"]);
        check(&f, "referralConfig(address)", &["address", "uint256"]);
        let (token, _) = abi("token.json");
        assert_eq!(token, aero::NEW_AERO.to_ascii_lowercase());
        // The Aero CLPool is not verified itself; its interface is, inside the CLFactory bundle: the views the 30 s
        // read and the liquidity map use exist there with the same shapes.
        let bundle: Value = serde_json::from_str(&std::fs::read_to_string("docs/cloud-tasks/sourcify/clfactory.json").unwrap()).unwrap();
        let state = bundle["sources"]["contracts/core/interfaces/pool/ICLPoolState.sol"]["content"].as_str().unwrap();
        for f in [
            "function slot0()",
            "function liquidity()",
            "function fee()",
            "function stakedLiquidity()",
            "function gaugeFees()",
            "function ticks(int24 tick)",
        ] {
            assert!(state.contains(f), "{f}");
        }
        let constants = bundle["sources"]["contracts/core/interfaces/pool/ICLPoolConstants.sol"]["content"].as_str().unwrap();
        assert!(constants.contains("function token0()") && constants.contains("function token1()"));
    }

    #[test]
    fn replays_end_aerodrome_rewards_at_the_last_period_and_never_on_aero() {
        let s = Store::memory().unwrap();
        // Nothing read yet: the launch.
        assert_eq!(rewards_end_for(&s, VAULT), Some(aero::LAUNCH_TS));
        // An hourly reading showing a period that ended early and was never renewed.
        s.put_series("gauge", "period_finish", aero::LAUNCH_TS - 3000, (aero::LAUNCH_TS - 3600) as f64).unwrap();
        assert_eq!(rewards_end_for(&s, VAULT), Some(aero::LAUNCH_TS - 3600));
        // A newer 30 s reading shows it renewed: the launch again.
        s.db.execute("INSERT INTO pool_obs (ts, period_finish) VALUES (?1, ?2)", [aero::LAUNCH_TS - 60, aero::LAUNCH_TS]).unwrap();
        assert_eq!(latest_period_finish(&s), Some((aero::LAUNCH_TS - 60, aero::LAUNCH_TS)));
        assert_eq!(rewards_end_for(&s, VAULT), Some(aero::LAUNCH_TS));
        // The vault the A.I follows on Aero: its tables are Aero's real rate. Every other vault stays Aerodrome.
        s.meta_set("gauge_mode", "aero").unwrap();
        s.meta_set("gauge_mode_vault", VAULT).unwrap();
        assert_eq!(rewards_end_for(&s, VAULT), None);
        assert_eq!(rewards_end_for(&s, "old"), Some(aero::LAUNCH_TS));
    }
}
