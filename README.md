# Panda Financial — A.I Keeper

**Watch-only.** The A.I Keeper watches the live Panda keeper, the Panda pool on Base, the ETH market
and the Panda website around the clock. It forecasts, shadows every width decision the keeper makes,
grades itself on what really happened, and emails the owner a daily brief.

**It cannot move money, sign anything, or change any Panda system.** That is enforced in code and in
CI, not left to good behaviour — see [docs/GUARDRAILS.md](docs/GUARDRAILS.md).

## What it does, in plain words

| Every… | It… |
|---|---|
| 30 s | reads the Panda pool on Base: price, liquidity, swap fee, gas, AERO emission rate |
| 1 min | stores ETH-USD's one-minute price from Coinbase |
| 1 h | stores Deribit's DVOL (what the options market expects ETH's volatility to be) |
| 1 h | checks the website and the keeper's deposit-width feed (GET only) |
| 1 h | **forecasts** ETH volatility (next hour, next day) with six models, and the chance the price leaves each range width (±1% … ±8%) within a day |
| 6 h | reads the keeper's history bucket (read-only) and, for every keeper width decision, makes **its own choice** — shadow mode |
| 10 min | **grades** every forecast and shadow decision whose time has come, against the real price path |
| 10 min | **alerts** you by email the moment something matters: keeper stopped, keeper gas low, site down, keeper stuck or hitting its runaway guard, the keeper's rules changed, or the A.I's own feeds failing |
| 10 min | reads **your replies** to the brief (read-only) and keeps them as notes with that day's decisions |
| night | refits its volatility model on all the history it holds |
| day | emails the brief: market, keeper activity, A.I vs keeper, scorecard, health, your notes |

### How it learns
Every forecast goes into a registry with the time it can be checked. When that time comes, the A.I
grades itself (QLIKE for volatility, Brier for range exits). The `blend` model re-weights itself
toward whichever models have been right over the last 30 days, and shadow mode uses whichever model
is the current champion. The scorecard is the honest answer to "is it better than the keeper yet?"

### Shadow mode (from day one)
Every hour, the keeper's strategy and each A.I model's strategy are replayed over the next 24 hours
of real pool prices under the keeper's **exact** rules, ported from the live keeper and read from its
own start-up log: 5-minute TWAP trigger, 6 h confirmation, cooldown and daily cap, the fee/deviation
hold, the crash valve, in-range re-cuts, and its "±2% unless clearly better" width choice. The only
difference between the strategies is the volatility view each acts on. `aikeeper verify` checks the
copy against the keeper's own history; on 2026-09-30 it reproduced **185 of 185** width choices,
**14 of 14** out-of-range clocks and the one real rebalance to **11 seconds** with the same new width.
Details in [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md).

## Commands

```
aikeeper run                 # 24/7 daemon
aikeeper selftest            # prove the guardrails and read once from every source
aikeeper backfill --days 365 # a year of ETH minutes, DVOL, the keeper's history
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
cover, so its reads cost about **$0.003 a month** (budgeted in `src/collect/bucket.rs`); a $1 budget
alert watches the project. Email goes out through Gmail with an app password. Step by step:
[docs/DEPLOY.md](docs/DEPLOY.md).

## Status

Deployed 2026-09-30 on Google Cloud (project `pf-ai-keeper`, VM `aikeeper`). Collectors, six volatility
models, range-exit models, exact-rules shadow mode, scorecard, alerts, reply reading, daily brief,
guardrails. What is deliberately not built yet, and why: [docs/ROADMAP.md](docs/ROADMAP.md).
