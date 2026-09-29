# How it works

## Data

| Source | What | How often | Where it goes |
|---|---|---|---|
| Panda pool on Base (`mainnet.base.org`, fallbacks) | tick, sqrtPrice, liquidity, `fee()`, base fee, gauge `rewardRate()` | 30 s | `pool_obs`, `price_1m` (source `pool`) |
| Coinbase Exchange | ETH-USD one-minute candles | 1 min (+ a year of backfill) | `price_1m` (source `coinbase`) |
| Deribit | ETH DVOL (options-implied volatility) | 1 h | `dvol` |
| Keeper bucket `gs://pfkeeperdata` | the keeper's six history tables, verbatim | 6 h (lean) | `keeper_rows` |
| pandafinancial.org | 8 pages / read-only API routes, status + latency, width-feed age | 1 h | `site_checks` |

`fee()` on the pool is the pool's own fee; the keeper's `fee_pips` is the dynamic fee it reads
elsewhere, so the two can differ.

Bucket budget: see the comment at the top of `src/collect/bucket.rs` — about 1,500 reads and 1,500
lists a month live, a one-time ~22,000 reads for the price history, all inside the free tier.

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

## Shadow mode (`src/model/shadow.rs`)

For each hour's newest keeper width table:

1. **A.I volatility** = keeper's volatility × (champion model ÷ keeper-style estimator), both
   computed on Coinbase *as of the table's time*.
2. **Rescore.** A random walk that must travel `a` at volatility `s` behaves like one travelling
   `a·s0/s` at `s0`. So under the A.I's volatility, width `h` behaves like the keeper's row at
   `h_eff = h · s_keeper / s_ai`: that row's time in range, rebalances per day and (× `s_ai/s_keeper`)
   divergence apply, while the reward per day in range is the real width's. Cost per rebalance is
   what the row implies (reward − score before divergence, per rebalance). With `s_ai = s_keeper`
   this reproduces the keeper's scores exactly (unit-tested).
3. **Choose** the best-scoring width. (The keeper's own `choice` adds hysteresis around its ±2% home
   width; the A.I's is a plain argmax. Both are recorded.)
4. **Grade** after 24 h: a $1,000 position at every width is replayed on the pool's real one-minute
   ticks — reward while in range (table rate × the row's emission capture, 80% share), rebalance after 6 h outside with a 2 h
   cooldown paying the row's cost, exact concentrated-liquidity divergence against holding each
   range's starting mix. Market direction is excluded, as in the keeper's score. The keeper's pick,
   the A.I's pick and the best-in-hindsight width are stored.

The replay is a simplification of the keeper (no TWAP trigger, no crash valve, no re-cuts); both
picks are graded by the same rules, so the comparison is fair even where the absolute numbers differ.

## Brief

Plain text, built by code (no paid AI), emailed through Gmail SMTP (port 465; Google Cloud blocks
port 25 only). Without email settings it is written to `<data dir>/briefs/`.
