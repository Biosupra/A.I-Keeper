//! Every event the Panda pool and its gauge emit, read from Base's free public RPCs (never Alchemy).
//!
//! Why: the A.I should see everything that moves the pool, not just the price. In 66 minutes on
//! 2026-09-30 the pool emitted 536 swaps, 936 mints and 2,776 burns (other LPs re-cutting ranges
//! around ours), and the gauge ~2,700 stake/unstake/claim events: about 150,000 events a day.
//!
//! What is kept, per block range:
//! * `chain_log_archive`: every log exactly as the node returned it, gzipped (~20x smaller:
//!   ~8 MB a day). Nothing is lost; any event can be decoded again later.
//! * `swaps`: every swap, decoded exactly (amounts as signed decimal strings).
//! * `lp_minute` / `gauge_minute`: per-minute sums of the liquidity and staking events. Stored
//!   individually they would be ~13 GB a year; the archive keeps them anyway.
//!
//! Correctness:
//! * Each batch also asks the same node for the range's last block; a node that has not reached
//!   it yet answers null and the batch is retried instead of being stored as "no events".
//! * The archive row, the decoded rows and the cursor move in one SQLite transaction, and a range
//!   that overlaps one already archived is never aggregated again (no double counting).
use crate::chainmath::{topic, word_address, word_i64, word_signed_dec, word_u128, word_unsigned_dec, words};
use crate::net::Rpc;
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use flate2::{write::GzEncoder, Compression};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

/// Blocks per eth_getLogs (the public nodes' cap is 2,000).
pub const SPAN: i64 = 1999;
/// Blocks left unread at the head (Base reorgs are rare and shallow).
const CONFIRMATIONS: i64 = 3;

struct Topics {
    swap: String,
    mint: String,
    burn: String,
    collect: String,
    collect_fees: String,
    flash: String,
    g_deposit: String,
    g_withdraw: String,
    g_claim_rewards: String,
    g_claim_fees: String,
    g_notify: String,
    g_penalty: String,
}

fn topics() -> &'static Topics {
    static T: OnceLock<Topics> = OnceLock::new();
    // Signatures from the verified CLPool / CLGauge sources on Blockscout (implementations of the
    // Panda pool 0x3FE0... and gauge 0xA0B6..., checked 2026-09-30).
    T.get_or_init(|| Topics {
        swap: topic("Swap(address,address,int256,int256,uint160,uint128,int24)"),
        mint: topic("Mint(address,address,int24,int24,uint128,uint256,uint256)"),
        burn: topic("Burn(address,int24,int24,uint128,uint256,uint256)"),
        collect: topic("Collect(address,address,int24,int24,uint128,uint128)"),
        collect_fees: topic("CollectFees(address,uint128,uint128)"),
        flash: topic("Flash(address,address,uint256,uint256,uint256,uint256)"),
        g_deposit: topic("Deposit(address,uint256,uint128)"),
        g_withdraw: topic("Withdraw(address,uint256,uint128)"),
        g_claim_rewards: topic("ClaimRewards(address,uint256)"),
        g_claim_fees: topic("ClaimFees(address,uint256,uint256)"),
        g_notify: topic("NotifyReward(address,uint256)"),
        g_penalty: topic("EarlyWithdrawPenalty(address,uint256,uint256)"),
    })
}

fn hex_i64(v: &Value) -> Option<i64> {
    v.as_str().and_then(|s| i64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

fn f(dec: &str) -> f64 {
    dec.parse::<f64>().unwrap_or(0.0)
}

/// A decoded pool liquidity event: (kind, (tickLower, tickUpper), liquidity, amount0, amount1).
type LpEvent = (&'static str, Option<(i64, i64)>, f64, f64, f64);

#[derive(Default, Debug)]
struct LpAgg {
    events: i64,
    liquidity: f64,
    amount0: f64,
    amount1: f64,
    in_range_events: i64,
    in_range_liquidity: f64,
    width_sum: f64,
    width_n: i64,
}

#[derive(Default, Debug)]
struct GaugeAgg {
    events: i64,
    liquidity: f64,
    amount: f64,
    token_ids: HashSet<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Processed {
    pub logs: usize,
    pub swaps: usize,
    pub lp: usize,
    pub gauge: usize,
    pub skipped_overlap: bool,
}

/// The pool tick at or before `ts` from what the A.I already holds (its own reads, then swaps).
fn tick_before(store: &Store, ts: i64) -> Option<i64> {
    store
        .db
        .query_row(
            "SELECT tick FROM (SELECT ts, tick FROM pool_obs WHERE ts <= ?1 ORDER BY ts DESC LIMIT 1
                               UNION ALL SELECT ts, tick FROM swaps WHERE ts <= ?1 ORDER BY ts DESC LIMIT 1) ORDER BY ts DESC LIMIT 1",
            [ts],
            |r| r.get(0),
        )
        .ok()
}

/// Stores one block range's logs: archive + decoded swaps + per-minute sums, in one transaction.
/// `to_ts` is the timestamp of block `to` (used when a node omits `blockTimestamp`; Base blocks are 2 s).
pub fn process(store: &Store, pool: &str, gauge: &str, from: i64, to: i64, to_ts: i64, logs: &[Value]) -> Result<Processed> {
    let pool = pool.to_ascii_lowercase();
    let gauge = gauge.to_ascii_lowercase();
    let t = topics();
    let tx = store.db.unchecked_transaction()?;
    let overlap: i64 =
        tx.query_row("SELECT COUNT(*) FROM chain_log_archive WHERE from_block <= ?1 AND to_block >= ?2", params![to, from], |r| r.get(0))?;
    if overlap > 0 {
        return Ok(Processed { skipped_overlap: true, ..Default::default() });
    }
    let mut gz = GzEncoder::new(Vec::new(), Compression::new(9));
    gz.write_all(&serde_json::to_vec(logs)?)?;
    tx.execute(
        "INSERT INTO chain_log_archive (from_block, to_block, logs, gz) VALUES (?1, ?2, ?3, ?4)",
        params![from, to, logs.len() as i64, gz.finish()?],
    )?;

    // Chain order, so the running tick is the pool's tick at each event.
    let mut sorted: Vec<&Value> = logs.iter().collect();
    sorted.sort_by_key(|l| (hex_i64(&l["blockNumber"]).unwrap_or(0), hex_i64(&l["logIndex"]).unwrap_or(0)));
    let ts_of =
        |l: &Value| -> i64 { hex_i64(&l["blockTimestamp"]).unwrap_or_else(|| to_ts - 2 * (to - hex_i64(&l["blockNumber"]).unwrap_or(to))) };
    let mut tick: Option<i64> = sorted.first().and_then(|l| tick_before(store, ts_of(l)));
    let mut lp: BTreeMap<(i64, &'static str), LpAgg> = BTreeMap::new();
    let mut gg: BTreeMap<(i64, &'static str), GaugeAgg> = BTreeMap::new();
    let mut out = Processed { logs: logs.len(), ..Default::default() };

    for l in sorted {
        let addr = l["address"].as_str().unwrap_or_default().to_ascii_lowercase();
        let tp: Vec<&str> = l["topics"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let Some(t0) = tp.first().copied() else { continue };
        let w = words(l["data"].as_str().unwrap_or("0x"));
        let block = hex_i64(&l["blockNumber"]).unwrap_or(0);
        let index = hex_i64(&l["logIndex"]).unwrap_or(0);
        let ts = ts_of(l);
        let minute = ts / 60 * 60;
        let txh = l["transactionHash"].as_str().unwrap_or_default().to_ascii_lowercase();

        if addr == pool {
            if t0 == t.swap && tp.len() >= 3 && w.len() >= 5 {
                let (a0, a1) = (word_signed_dec(&w[0]), word_signed_dec(&w[1]));
                let new_tick = word_i64(&w[4]);
                tx.execute(
                    "INSERT OR IGNORE INTO swaps (block, log_index, ts, tx, sender, recipient, amount0, amount1, sqrt_price, liquidity, tick, usd)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        block, index, ts, txh, word_address(tp[1]), word_address(tp[2]), a0, a1,
                        word_unsigned_dec(&w[2]), word_unsigned_dec(&w[3]), new_tick, f(&a1).abs() / 1e6
                    ],
                )?;
                tick = Some(new_tick);
                out.swaps += 1;
                continue;
            }
            // (kind, tickLower, tickUpper, liquidity, amount0, amount1)
            let ev: Option<LpEvent> = if t0 == t.mint && tp.len() >= 4 && w.len() >= 4 {
                Some((
                    "mint",
                    Some((word_i64(tp[2]), word_i64(tp[3]))),
                    word_u128(&w[1]) as f64,
                    f(&word_unsigned_dec(&w[2])),
                    f(&word_unsigned_dec(&w[3])),
                ))
            } else if t0 == t.burn && tp.len() >= 4 && w.len() >= 3 {
                Some((
                    "burn",
                    Some((word_i64(tp[2]), word_i64(tp[3]))),
                    word_u128(&w[0]) as f64,
                    f(&word_unsigned_dec(&w[1])),
                    f(&word_unsigned_dec(&w[2])),
                ))
            } else if t0 == t.collect && tp.len() >= 4 && w.len() >= 3 {
                Some(("collect", Some((word_i64(tp[2]), word_i64(tp[3]))), 0.0, f(&word_unsigned_dec(&w[1])), f(&word_unsigned_dec(&w[2]))))
            } else if t0 == t.collect_fees && w.len() >= 2 {
                Some(("collect_fees", None, 0.0, f(&word_unsigned_dec(&w[0])), f(&word_unsigned_dec(&w[1]))))
            } else if t0 == t.flash && w.len() >= 4 {
                Some(("flash", None, 0.0, f(&word_unsigned_dec(&w[0])), f(&word_unsigned_dec(&w[1]))))
            } else {
                None
            };
            if let Some((kind, range, liq, a0, a1)) = ev {
                let a = lp.entry((minute, kind)).or_default();
                a.events += 1;
                a.liquidity += liq;
                a.amount0 += a0 / 1e18;
                a.amount1 += a1 / 1e6;
                if let Some((lo, hi)) = range {
                    a.width_sum += (hi - lo) as f64;
                    a.width_n += 1;
                    if tick.map(|c| lo <= c && c < hi).unwrap_or(false) {
                        a.in_range_events += 1;
                        a.in_range_liquidity += liq;
                    }
                }
                out.lp += 1;
            }
        } else if addr == gauge {
            // (kind, token id, liquidity, amount)
            let ev: Option<(&'static str, Option<String>, f64, f64)> = if (t0 == t.g_deposit || t0 == t.g_withdraw) && tp.len() >= 4 {
                let kind = if t0 == t.g_deposit { "stake" } else { "unstake" };
                Some((kind, Some(word_unsigned_dec(tp[2])), word_u128(tp[3]) as f64, 0.0))
            } else if t0 == t.g_claim_rewards && !w.is_empty() {
                Some(("claim_rewards", None, 0.0, f(&word_unsigned_dec(&w[0])) / 1e18))
            } else if t0 == t.g_notify && !w.is_empty() {
                Some(("notify_reward", None, 0.0, f(&word_unsigned_dec(&w[0])) / 1e18))
            } else if t0 == t.g_penalty && tp.len() >= 3 && !w.is_empty() {
                Some(("early_withdraw_penalty", Some(word_unsigned_dec(tp[2])), 0.0, f(&word_unsigned_dec(&w[0])) / 1e18))
            } else if t0 == t.g_claim_fees && w.len() >= 2 {
                // liquidity column = token0 (WETH), amount column = token1 (USDC), for this kind only.
                Some(("claim_fees", None, f(&word_unsigned_dec(&w[0])) / 1e18, f(&word_unsigned_dec(&w[1])) / 1e6))
            } else {
                None
            };
            if let Some((kind, id, liq, amount)) = ev {
                let a = gg.entry((minute, kind)).or_default();
                a.events += 1;
                a.liquidity += liq;
                a.amount += amount;
                if let Some(id) = id {
                    a.token_ids.insert(id);
                }
                out.gauge += 1;
            }
        }
    }

    for ((ts, kind), a) in &lp {
        tx.execute(
            "INSERT INTO lp_minute (ts, kind, events, liquidity, amount0, amount1, in_range_events, in_range_liquidity, width_ticks_mean)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(ts, kind) DO UPDATE SET
               width_ticks_mean = CASE WHEN excluded.width_ticks_mean IS NULL THEN lp_minute.width_ticks_mean
                                       WHEN lp_minute.width_ticks_mean IS NULL THEN excluded.width_ticks_mean
                                       ELSE (lp_minute.width_ticks_mean * lp_minute.events + excluded.width_ticks_mean * excluded.events) / (lp_minute.events + excluded.events) END,
               events = lp_minute.events + excluded.events, liquidity = lp_minute.liquidity + excluded.liquidity,
               amount0 = lp_minute.amount0 + excluded.amount0, amount1 = lp_minute.amount1 + excluded.amount1,
               in_range_events = lp_minute.in_range_events + excluded.in_range_events,
               in_range_liquidity = lp_minute.in_range_liquidity + excluded.in_range_liquidity",
            params![
                ts, kind, a.events, a.liquidity, a.amount0, a.amount1, a.in_range_events, a.in_range_liquidity,
                if a.width_n > 0 { Some(a.width_sum / a.width_n as f64) } else { None }
            ],
        )?;
    }
    for ((ts, kind), a) in &gg {
        tx.execute(
            "INSERT INTO gauge_minute (ts, kind, events, liquidity, amount, token_ids) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(ts, kind) DO UPDATE SET events = gauge_minute.events + excluded.events,
               liquidity = gauge_minute.liquidity + excluded.liquidity, amount = gauge_minute.amount + excluded.amount,
               token_ids = gauge_minute.token_ids + excluded.token_ids",
            params![ts, kind, a.events, a.liquidity, a.amount, a.token_ids.len() as i64],
        )?;
    }
    tx.commit()?;
    Ok(out)
}

/// One range from the node, proven complete (the node has block `to`). Retries rate limits.
async fn fetch(rpc: &Rpc, pool: &str, gauge: &str, from: i64, to: i64) -> Result<(Vec<Value>, i64)> {
    let mut last = anyhow!("not tried");
    for attempt in 0..4u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(1500 * attempt)).await;
        }
        let out = match rpc
            .batch(&[
                ("eth_getLogs", json!([{ "address": [pool, gauge], "fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{to:x}") }])),
                ("eth_getBlockByNumber", json!([format!("0x{to:x}"), false])),
            ])
            .await
        {
            Ok(o) => o,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let mut it = out.into_iter();
        let logs = it.next().ok_or_else(|| anyhow!("no logs reply"))?;
        let block = it.next().ok_or_else(|| anyhow!("no block reply"))?;
        match (logs, block) {
            (Ok(Value::Array(l)), Ok(b)) if !b.is_null() => {
                let ts = hex_i64(&b["timestamp"]).ok_or_else(|| anyhow!("block {to} without timestamp"))?;
                return Ok((l, ts));
            }
            (Ok(_), Ok(_)) => last = anyhow!("node has not reached block {to} yet"),
            (Err(e), _) | (_, Err(e)) => last = e,
        }
    }
    Err(last)
}

pub async fn head(rpc: &Rpc) -> Result<i64> {
    let v = rpc.call("eth_blockNumber", json!([])).await?;
    hex_i64(&v).ok_or_else(|| anyhow!("bad block number"))
}

#[derive(Debug, Default)]
pub struct Summary {
    pub to: i64,
    pub logs: usize,
    pub swaps: usize,
    pub behind: i64,
}

/// Live: reads (cursor, head - 3] in up to `max_chunks` ranges. A fresh database starts at the head.
pub async fn poll(rpc: &Rpc, store: &Store, pool: &str, gauge: &str, max_chunks: usize) -> Result<Summary> {
    let safe = head(rpc).await? - CONFIRMATIONS;
    let mut cursor: i64 = match store.meta_get("flow_block")?.and_then(|v| v.parse().ok()) {
        Some(c) => c,
        None => {
            store.meta_set("flow_block", &(safe - 1).to_string())?;
            safe - 1
        }
    };
    let mut s = Summary { to: cursor, ..Default::default() };
    for _ in 0..max_chunks {
        if cursor >= safe {
            break;
        }
        let (from, to) = (cursor + 1, (cursor + SPAN).min(safe));
        let (logs, to_ts) = fetch(rpc, pool, gauge, from, to).await?;
        let p = process(store, pool, gauge, from, to, to_ts, &logs)?;
        s.logs += p.logs;
        s.swaps += p.swaps;
        cursor = to;
        store.meta_set("flow_block", &cursor.to_string())?;
        s.to = to;
    }
    s.behind = safe - cursor;
    Ok(s)
}

/// A node refusing the range because it is too wide for it ("eth_getLogs is limited to a 500
/// range", "block range too large", "query returned more than N results"), as against a node that
/// is down or behind.
fn range_too_wide(err: &anyhow::Error) -> bool {
    let m = format!("{err:#}").to_lowercase();
    m.contains("limited to") || m.contains("block range") || m.contains("too many") || m.contains("-32614") || m.contains("exceed")
}

/// One-time history: every range in [from, to] not archived yet, paced for the public node. A node
/// that caps eth_getLogs below SPAN narrows the range for the rest of the run (until 2026-10-07 the
/// backfill stopped at the first such refusal, at block 51,207,242, on every run).
pub async fn backfill(rpc: &Rpc, store: &Store, pool: &str, gauge: &str, from: i64, to: i64) -> Result<Summary> {
    let mut s = Summary { to: from - 1, ..Default::default() };
    let mut b = from;
    let mut span = SPAN;
    while b <= to {
        let mut e = (b + span - 1).min(to);
        // Stop short of anything already archived (the live cursor's ranges), then skip past it.
        let next: Option<(i64, i64)> = store
            .db
            .query_row(
                "SELECT from_block, to_block FROM chain_log_archive WHERE to_block >= ?1 AND from_block <= ?2 ORDER BY from_block LIMIT 1",
                params![b, e],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some((af, at)) = next {
            if af <= b {
                b = at + 1;
                continue;
            }
            e = af - 1;
        }
        let (logs, to_ts) = match fetch(rpc, pool, gauge, b, e).await {
            Ok(x) => x,
            Err(err) if range_too_wide(&err) && e > b => {
                span = ((e - b + 1) / 2).max(1);
                tracing::info!("flow backfill: the node refused {} blocks ({err:#}); ranges narrowed to {span} blocks", e - b + 1);
                continue;
            }
            Err(err) => bail!("backfill stopped at block {b} ({err:#}); run it again to continue"),
        };
        let p = process(store, pool, gauge, b, e, to_ts, &logs)?;
        if p.skipped_overlap {
            // The live reader archived part of this range meanwhile: plan the range again (the
            // check above now shortens or skips it), so nothing outside its range is lost.
            continue;
        }
        s.logs += p.logs;
        s.swaps += p.swaps;
        s.to = e;
        b = e + 1;
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "0x3fe04a59ebd38cf06080a6f60a98d124eb59392a";
    const GAUGE: &str = "0xa0b61fdb9f1fb9b917fe38b49427fd4d87472d28";

    fn w(v: &str) -> String {
        format!("{v:0>64}")
    }
    fn neg(v: i128) -> String {
        format!("{}{:032x}", "f".repeat(32), v as u128)
    }
    fn t_i(v: i64) -> String {
        format!("0x{}", crate::chainmath::encode_i64_word(v))
    }

    fn logs() -> Vec<Value> {
        let t = topics();
        let addr = |a: &str| format!("0x{}{}", "0".repeat(24), a.trim_start_matches("0x"));
        vec![
            // Swap: 0.5 WETH in, 1,350 USDC out, to tick -197250.
            json!({"address": POOL, "blockNumber": "0x64", "logIndex": "0x1", "blockTimestamp": "0x6000", "transactionHash": "0xAA",
                   "topics": [t.swap, addr("0x1111111111111111111111111111111111111111"), addr("0x2222222222222222222222222222222222222222")],
                   "data": format!("0x{}{}{}{}{}", w(&format!("{:x}", 500_000_000_000_000_000u128)), neg(-1_350_000_000), w("abc"), w("def"), crate::chainmath::encode_i64_word(-197250))}),
            // Mint of an in-range position [-197500, -197000) with liquidity 1000.
            json!({"address": POOL, "blockNumber": "0x64", "logIndex": "0x2", "blockTimestamp": "0x6000", "transactionHash": "0xbb",
                   "topics": [t.mint, addr("0xe1f8cd9ac4e4a65f54f38a5cdafca44f6dd68b53"), t_i(-197500), t_i(-197000)],
                   "data": format!("0x{}{}{}{}", w("00"), w(&format!("{:x}", 1000)), w(&format!("{:x}", 2_000_000_000_000_000_000u128)), w(&format!("{:x}", 3_000_000)))}),
            // Burn of an out-of-range position.
            json!({"address": POOL, "blockNumber": "0x65", "logIndex": "0x0", "blockTimestamp": "0x6002", "transactionHash": "0xcc",
                   "topics": [t.burn, addr("0xe1f8cd9ac4e4a65f54f38a5cdafca44f6dd68b53"), t_i(-190000), t_i(-189000)],
                   "data": format!("0x{}{}{}", w(&format!("{:x}", 400)), w("0"), w("0"))}),
            // Gauge stake of token 7 with liquidity 1000.
            json!({"address": GAUGE, "blockNumber": "0x65", "logIndex": "0x1", "blockTimestamp": "0x6002", "transactionHash": "0xdd",
                   "topics": [t.g_deposit, addr("0x3333333333333333333333333333333333333333"), format!("0x{}", w("7")), format!("0x{}", w(&format!("{:x}", 1000)))],
                   "data": "0x"}),
            // Something else from the pool (kept in the archive only).
            json!({"address": POOL, "blockNumber": "0x65", "logIndex": "0x2", "blockTimestamp": "0x6002", "transactionHash": "0xee",
                   "topics": ["0x1234"], "data": "0x"}),
        ]
    }

    /// Network (opt-in, free public RPC): `cargo test --release old_ranges -- --ignored --nocapture`.
    /// Old blocks need Base's own endpoint (PublicNode refuses archive reads).
    #[tokio::test]
    #[ignore]
    async fn backfills_old_ranges_contiguously() {
        let rpc = Rpc::new(vec!["https://mainnet.base.org".into(), "https://base-rpc.publicnode.com".into()]).unwrap();
        let s = Store::memory().unwrap();
        let from = 51_189_251; // the vault's deploy block
        let to = from + 3 * SPAN - 1;
        let sum = backfill(&rpc, &s, POOL, GAUGE, from, to).await.unwrap();
        let ranges: Vec<(i64, i64, i64)> = {
            let mut st = s.db.prepare("SELECT from_block, to_block, logs FROM chain_log_archive ORDER BY from_block").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect()
        };
        assert_eq!(ranges.first().map(|r| r.0), Some(from));
        assert_eq!(ranges.last().map(|r| r.1), Some(to));
        for w in ranges.windows(2) {
            assert_eq!(w[1].0, w[0].1 + 1, "no gap and no overlap between ranges");
        }
        let logs: i64 = ranges.iter().map(|r| r.2).sum();
        assert_eq!(logs as usize, sum.logs);
        assert!(sum.swaps > 0, "the pool traded in its first ~3 hours on record");
        let swaps: i64 = s.db.query_row("SELECT COUNT(*) FROM swaps", [], |r| r.get(0)).unwrap();
        assert_eq!(swaps as usize, sum.swaps);
        // Running it again changes nothing.
        let again = backfill(&rpc, &s, POOL, GAUGE, from, to).await.unwrap();
        assert_eq!(again.logs, 0);
        println!("{} ranges, {} logs, {} swaps from block {from}", ranges.len(), logs, sum.swaps);
    }

    #[test]
    fn decodes_archives_and_never_double_counts() {
        let s = Store::memory().unwrap();
        let p = process(&s, POOL, GAUGE, 100, 101, 0x6002, &logs()).unwrap();
        assert_eq!(p, Processed { logs: 5, swaps: 1, lp: 2, gauge: 1, skipped_overlap: false });
        let (a0, a1, tick, usd): (String, String, i64, f64) =
            s.db.query_row("SELECT amount0, amount1, tick, usd FROM swaps", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap();
        assert_eq!((a0.as_str(), a1.as_str(), tick), ("500000000000000000", "-1350000000", -197250));
        assert!((usd - 1350.0).abs() < 1e-9);
        // The mint is in range at the swap's tick (-197250); the burn is not.
        let (ev, inr, liq, amt0, amt1): (i64, i64, f64, f64, f64) = s
            .db
            .query_row("SELECT events, in_range_events, in_range_liquidity, amount0, amount1 FROM lp_minute WHERE kind = 'mint'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap();
        assert_eq!((ev, inr, liq), (1, 1, 1000.0));
        assert!((amt0 - 2.0).abs() < 1e-12 && (amt1 - 3.0).abs() < 1e-12);
        let burn_in: i64 = s.db.query_row("SELECT in_range_events FROM lp_minute WHERE kind = 'burn'", [], |r| r.get(0)).unwrap();
        assert_eq!(burn_in, 0);
        let (g_ev, g_liq, g_ids): (i64, f64, i64) =
            s.db.query_row("SELECT events, liquidity, token_ids FROM gauge_minute WHERE kind = 'stake'", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((g_ev, g_liq, g_ids), (1, 1000.0, 1));
        // The archive holds all five logs, and decompresses to exactly what was given.
        let gz: Vec<u8> = s.db.query_row("SELECT gz FROM chain_log_archive", [], |r| r.get(0)).unwrap();
        let mut back = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut back).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Value>>(&back).unwrap(), logs());
        // The same range again changes nothing.
        let again = process(&s, POOL, GAUGE, 100, 101, 0x6002, &logs()).unwrap();
        assert!(again.skipped_overlap);
        let mints: i64 = s.db.query_row("SELECT events FROM lp_minute WHERE kind = 'mint'", [], |r| r.get(0)).unwrap();
        assert_eq!(mints, 1);
    }
}
