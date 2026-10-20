# How it works

## Data

Everything is read-only, free public sources except the keeper's bucket (read-only, ~$0.20 a month).
Since 2026-09-30 it captures everything it can ("watch EVERYTHING"):

| Source | What | How often | Where it goes |
|---|---|---|---|
| Panda pool on Base (`mainnet.base.org`, fallbacks) | tick, sqrtPrice, liquidity, **stakedLiquidity**, `fee()`, base fee, gauge `rewardRate()`, AERO/WETH pool tick (AERO price) | 30 s | `pool_obs`, `price_1m` (source `pool`) |
| Pool + gauge event logs (`src/collect/flow.rs`) | every log, gzipped as received (~8 MB a day); swaps decoded; Mint/Burn/Collect/CollectFees/Flash and gauge stake/unstake/claim/notify/penalty summed per minute | 30 s (cursor; up to 3 × 1,999 blocks per turn; full history since the vault's deploy block by `backfill`) | `chain_log_archive`, `swaps`, `lp_minute`, `gauge_minute` |
| Pool `ticks()` (`src/collect/liquidity.rs`) | gross, net and **staked** net liquidity of every initialized tick within ±40 spacings (±20%) | 10 min | `tick_liquidity` |
| Gauge, pool, Voter | periodFinish, left, rewardRateByEpoch, fees, rewardReserve, gaugeFees, the pool's vote weight and the total; the keeper wallet's ETH | 1 h | `series` (sources `gauge`, `keeper`) |
| Coinbase Exchange | one-minute candles with volume: ETH-USD, BTC-USD, AERO-USD, ETH-BTC | 1 min (+ three years of ETH-USD backfill, a year of the others) | `price_1m` (sources `coinbase`, `cb:<product>`) |
| Coinbase Exchange | ETH-USD order book: mid, spread, depth and imbalance within 0.1/0.5/1/2% | 5 min | `series` (`coinbase_book`) |
| Deribit | ETH DVOL; BTC DVOL; ETH and BTC perpetual funding, open interest, mark, index, 24 h volume; every ETH option's IV, price, open interest | 1 h / 5 min / 1 h | `dvol`, `series` (`deribit`), `options_hourly` |
| DefiLlama yields | TVL, APY (base, reward), volume of every Aerodrome pool on Base | 6 h | `pool_yields` |
| alternative.me | Crypto Fear & Greed index | 6 h | `series` (`alternative`) |
| News RSS (CoinDesk, Cointelegraph, Decrypt, The Block, The Defiant) | headline, link, time, summary | 15 min | `news` |
| Keeper bucket `gs://pfkeeperdata` | **every file** of its six tables, byte for byte; each position every minute, every decision, every width table, the keeper's market row, decoded | 30 min (own thread) | `bucket_archive`, `position_min`, `decision_min`, `width_min`, `keeper_market_min`, and `keeper_rows` for the replay |
| pandafinancial.org | 8 pages / read-only API routes, status + latency, width-feed age | 1 h | `site_checks` |

Every network job runs under a hard time limit, and the bucket sync on its own thread, so no source
can stall the others (`src/run.rs`).

`fee()` on the pool is the pool's own fee; the keeper's `fee_pips` is the dynamic fee it reads
elsewhere, so the two can differ.

Bucket cost: see the comment at the top of `src/collect/bucket.rs` — about 4,000 reads and 500
lists a day, roughly $0.20 a month, and ~$0.03 once for the first fortnight's files.

## Forecasts (`src/model`, `src/registry.rs`)

Volatility is in the keeper's unit: fraction per sqrt(hour) (0.005 = a typical one-hour move of
0.5%).

| Model | Idea |
|---|---|
| `keeper` | the keeper's estimator: max(realised 24 h, realised 48 h) — the baseline to beat |
| `persist` | the last `horizon` repeated |
| `ewma` | exponentially weighted hourly variance, lambda 0.97 |
| `har` | HAR-RV: tomorrow from today, last week, last month; OLS refitted nightly |
| `dvol` | Deribit DVOL (annualised) converted to per-sqrt-hour |
| `blend` | variance-average of the above, weighted by 1 / their 30-day QLIKE |

Range exits: probability that the pool price leaves ±h spacings (50 ticks each) within 24 h —
`barrier:<model>` (exact double-barrier probability for a driftless random walk at that model's
volatility), `barrier:<model>@pool` (the same, rescaled to the pool's own volatility basis), and
`empirical` (how often it happened in the last 90 days).

**Basis.** On 2026-09-29 the keeper's estimator read 0.43% on its pool while the same estimator on
Coinbase read 0.54% — at 1, 5 and 15-minute sampling alike, so a basis difference, not noise.
Volatility forecasts are made and graded on Coinbase (consistent), shadow mode applies the A.I's
*relative* view to the keeper's own number, and range exits are issued on both bases so the
scorecard can settle which is right over 24 h.

Grading: volatility with QLIKE (0 perfect; punishes under-forecasting risk harder), exits with the
Brier score (0 perfect, 0.25 = coin flip). Ungradable forecasts (data never arrived) are closed after
two days.

## Shadow mode (`src/model/shadow.rs`, `src/registry.rs`)

For each hour's keeper width table the A.I records a decision. 24 hours later (once the prices and
the keeper's tables for that day have arrived) it replays, on the pool's real one-minute ticks:

* **the keeper's strategy** — its own tables and choices;
* **each model's strategy** — the same tables re-scored at that model's volatility view
  (volatility ratio model/keeper-style, applied to the keeper's own number; see the basis note);
* **every fixed width** — the ceiling a perfect width-picker could have reached.

The replay is a port of the live keeper 0.8.0 (`decide`, `choose_width`, `best_for_position`,
`should_hold_out`, `assess_recut`, `confirm_recut`, `recut_gate`, `divergence_fraction`), with the
settings read from the keeper's own `keeper_runs` log so a policy change is followed automatically:

* trigger on the 5-minute TWAP; 6 h outside without re-entering to confirm; 2 h cooldown; at most
  3 rebalances in 24 h;
* hold while the pool fee is above 1000 pips or spot is more than 50 ticks from the TWAP, up to 2 h;
* crash valve: when no width pays and even the widest range would exit ≥ 0.65 times a day, sit out,
  capped at 24 h or 400 ticks past the edge;
* in-range re-cuts: width ratio ≥ 2, gain ≥ 4 × the one-off cost over 24 h, same direction for 3 h,
  narrowing only 12 h after a known rebalance/deposit and never below ±3%, widening a fresh
  deposit only 12 h after it (once the deposit time is known, as in the keeper);
* new ranges: the table's hysteresis choice (±2% home width unless a narrower one is 10% better or a
  wider one 25% better), snapped to the 50-tick grid;
* money: reward while spot is in range at the table's rate × emission capture × the 80% share;
  each rebalance pays half a swap at the pool's actual fee plus the table's gas; divergence against
  each range's mint-time mix is realised at every close and marked to market at the end.

Tables are read every 10 minutes (the replay uses the one in force at each moment). With only hourly
tables a real rebalance's new width came out wrong (±6% vs the keeper's ±4.5%); with the minute
tables it is exact. The replay's position is the keeper's reference $1,000; `verify` also re-scores
at each real position's size (gas weighs more on small positions).

**Fidelity check (`aikeeper verify`, and in every brief):** width choices reproduced, out-of-range
clocks within 10 minutes of the keeper's own log, real rebalance times within 10 minutes, and their
new widths at the real position size. 2026-09-29: 185/185, 14/14, 1/1 (11 s), 1/1. If the keeper's
rules change, the rule-drift alert fires.

## The past, replayed (`src/replay.rs`)

Owner, 2026-09-30: "if it doesn't see the bad times it won't know what not to do." So the A.I does
not only learn from the days since it started.

**Forecasts, walk-forward.** For every hour of the ETH history it holds (three years by default,
starting 30 days in), every model forecasts the next 24 h and the next hour with exactly the live
code (`vol::base_forecasts`), from prices before that hour only; HAR is refitted every 7 days on the
year before that moment; DVOL is the reading then; the blend is weighted by grades whose outcome was
already known then (a test proves this equals the live blend, and another rewrites the future into a
storm and proves no forecast changes). Each forecast is graded like a live one and stored in
`predictions` with `replay = 1` and its **regime**: calm (< 0.5%/sqrt h over the previous 24 h),
active (< 1%) or storm. Live forecasts carry the regime too. Hours already forecast live are left
alone; hours without a complete outcome are skipped and retried.

**Which model to follow.** `registry::champion` picks, for the current regime, the model with the
lowest mean QLIKE over all history held, **paired**: only hours in which every model forecast count
(HAR needs 90 days of history, DVOL can be missing, and an unpaired mean would compare easy hours
with hard ones). At least 50 paired hours; otherwise the 30-day rule. The live scorecard excludes
replayed forecasts, so it stays the record of what the A.I did live.

**Days, under the keeper's rules.** Every UTC day is replayed with the shadow engine: every width of
the keeper's table held fixed, and each model's own hourly choices (its forecast scaled to the
pool's basis, then the keeper's width rule), on the day's real one-minute ETH path, starting a fresh
$1,000 position at the day's first price. Stored in `replay_day` with the day's realised volatility.
Assumed for days before the vault (recorded in `replay_runs`): the emission rate, gas and position
size of the keeper's width table at the median volatility, the pool fee at its median since launch,
and the pool/Coinbase volatility basis at its median since launch.

The brief's THE PAST, REPLAYED section: the best forecaster per regime next to the keeper's
estimator, the best and worst fixed width on calm, active and storm days, each model's strategy with
its worst day, and the three worst days for the keeper-style strategy.

## Alerts (`src/alerts.rs`)

Every 10 minutes; email on start, a reminder every 6 h while it lasts, "resolved" when it clears,
at most 12 alert emails a day:

| Alert | Fires when |
|---|---|
| keeper-heartbeat | the keeper's signed deposit-width feed is > 30 min old (it posts every 5) |
| keeper-gas | the keeper wallet holds < 0.001 ETH on Base |
| site | `/` or `/api/health` fails twice in a row |
| keeper-history | the keeper's newest history row is > 3 h older than the A.I's last good bucket read |
| keeper-runaway / keeper-stuck | a runaway-guard skip, or a position out of range > 26 h (valve cap is 24 h) |
| rule-drift | the keeper chose widths the A.I's copy of its rules would not |
| ai-chain / ai-market / ai-bucket | the A.I's own Base reads, Coinbase prices or bucket access stalled |

## Replies (`src/inbox.rs`)

Every 10 minutes the inbox of the sending account is opened read-only; messages from the owner's
address that Gmail authenticates are kept as notes (quoted brief text stripped) and shown in the next
brief. `aikeeper note "..."` does the same from the command line.

## Brief

Plain text, built by code (no paid AI), emailed through Gmail SMTP (port 465; Google Cloud blocks
port 25 only). Without email settings it is written to `<data dir>/briefs/`.
