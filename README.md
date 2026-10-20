# Panda Financial — A.I Keeper

**Watch-only.** The A.I Keeper watches the live Panda keeper, the Panda pool on Base, the ETH market
and the Panda website around the clock. It forecasts, shadows every width decision the keeper makes,
grades itself on what really happened, and emails the owner a daily brief.

**It cannot move money, sign anything, or change any Panda system.** That is enforced in code and in
CI, not left to good behaviour — see [docs/GUARDRAILS.md](docs/GUARDRAILS.md).

## What it does, in plain words

| Every… | It… |
|---|---|
| 30 s | reads the Panda pool on Base: price, liquidity, **staked** liquidity, swap fee, gas, AERO emission rate, and the AERO price |
| 30 s | records **every event** the pool and its gauge emit: every swap (decoded), every other LP's range opened or closed, every NFT staked or unstaked, every reward claim (~150,000 events a day, all archived) |
| 1 min | stores one-minute prices with volume from Coinbase: ETH-USD, BTC-USD, AERO-USD, ETH-BTC |
| 5 min | the ETH-USD order book (spread, depth each side) and the ETH and BTC perpetuals on Deribit (funding, open interest) |
| 10 min | the **liquidity map** around the price: total and staked liquidity at every price level within ±20% |
| 15 min | crypto headlines from five news feeds (kept as published; tagging them comes later) |
| 30 min | reads **every file** the keeper writes to its history bucket (read-only), archives it byte for byte, and decodes each position's value and earnings minute by minute, every decision and every width table |
| 1 h | Deribit's DVOL for ETH and BTC, the whole ETH option board, the gauge's emission schedule, the pool's share of the Aerodrome vote, the keeper wallet's ETH |
| 1 h | checks the website and the keeper's deposit-width feed (GET only) |
| 6 h | the yields of every Aerodrome pool on Base (DefiLlama) and the Crypto Fear & Greed index |
| 1 h | **forecasts** ETH volatility (next hour, next day) with six models, and the chance the price leaves each range width (±1% … ±8%) within a day |
| 30 min | for every keeper width decision, makes **its own choice** — shadow mode |
| 10 min | **grades** every forecast and shadow decision whose time has come, against the real price path |
| 10 min | **alerts** you by email the moment something matters: keeper stopped, keeper gas low, site down, keeper stuck or hitting its runaway guard, the keeper's rules changed, or the A.I's own feeds failing |
| 10 min | reads **your replies** to the brief (read-only) and keeps them as notes with that day's decisions |
| night | refits its volatility model on all the history it holds, and adds yesterday to the replayed record |
| day | emails the brief: markets, the pool (flow, competing LPs, staking, emissions, the liquidity map), keeper activity, A.I vs keeper, scorecard, headlines, what it captured, health, your notes |

### How it learns
Every forecast goes into a registry with the time it can be checked. When that time comes, the A.I
grades itself (QLIKE for volatility, Brier for range exits). The `blend` model re-weights itself
toward whichever models have been right over the last 30 days, and shadow mode uses whichever model
is the current champion. The scorecard is the honest answer to "is it better than the keeper yet?"

It does not wait for bad days to happen live. `aikeeper replay` (run by `backfill` and nightly)
walks through **three years of ETH history hour by hour**: every model forecasts from what it could
have seen at that moment only (HAR refitted weekly on the past, the blend weighted only by grades
already known), is graded on the 24 hours that followed, and is labelled with the kind of market it
was made in (calm, active, storm). The champion is then chosen **for the kind of market it is now**,
on the same hours for every model, so a model that falls apart in storms is not followed into one.
Every day of those years is also replayed under the keeper's current rules for every width and for
each model's own choices, so the brief can say which widths survive storms and name the worst days.
Before the vault existed there are no emission figures, so those days are scored with the emission
rate, gas and pool fee measured since launch (stated in each brief).

### Shadow mode (from day one)
Every hour, the keeper's strategy and each A.I model's strategy are replayed over the next 24 hours
of real pool prices under the keeper's **exact** rules, ported from the live keeper and read from its
own start-up log: 5-minute TWAP trigger, 6 h confirmation, cooldown and daily cap, the fee/deviation
hold, the crash valve, in-range re-cuts, and its "±2% unless clearly better" width choice. The only
difference between the strategies is the volatility view each acts on. `aikeeper verify` checks the
copy against the keeper's own history; on 2026-09-29 it reproduced **185 of 185** width choices,
**14 of 14** out-of-range clocks and the one real rebalance to **11 seconds** with the same new width.
Details in [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md).

## Commands

```
aikeeper run                 # 24/7 daemon
aikeeper selftest            # prove the guardrails and read once from every source
aikeeper backfill            # 3 years of ETH minutes + DVOL, a year of the rest, the keeper's history, then replay
aikeeper replay              # replay the past walk-forward (only what is not replayed yet)
aikeeper predict             # this hour's forecasts and shadow rows now
aikeeper resolve             # grade what is due
aikeeper scorecard           # how every model is doing
aikeeper brief [--send]      # print (or email) today's brief
aikeeper note "too narrow in this chop"   # teach it (or just reply to the brief email)
aikeeper verify              # check the A.I's copy of the keeper's rules against the keeper's history
aikeeper check               # run every alert check once and print what is wrong (sends nothing)
aikeeper inbox               # read replies to the brief now (read-only)
```

## Running it for $0

Google Cloud's always-free tier: one e2-micro VM (1 GB RAM — which is why it is written in Rust)
and a 30 GB disk. The keeper's bucket is US multi-region, which the free Storage allowance does not
cover; reading every file it writes costs about **$0.20 a month** (see `src/collect/bucket.rs`),
and a $1 budget alert watches the project. Everything else it reads is free. The database grows by
roughly 10–15 GB a year, so the disk lasts about two years; an `ai-disk` alert fires at 80% of
`AIK_DISK_BUDGET_GB` (20 GB), long before it is full. Email goes out through Gmail with an app password. Step by step:
[docs/DEPLOY.md](docs/DEPLOY.md).

## Status

Deployed 2026-09-29 on Google Cloud (project `pf-ai-keeper`, VM `aikeeper`). Collectors, six volatility
models, range-exit models, exact-rules shadow mode, scorecard, alerts, reply reading, daily brief,
guardrails. What is deliberately not built yet, and why: [docs/ROADMAP.md](docs/ROADMAP.md).
