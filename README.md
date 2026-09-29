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
| night | refits its volatility model on all the history it holds |
| day | emails the brief: market, keeper activity, A.I vs keeper, scorecard, health, your notes |

### How it learns
Every forecast goes into a registry with the time it can be checked. When that time comes, the A.I
grades itself (QLIKE for volatility, Brier for range exits). The `blend` model re-weights itself
toward whichever models have been right over the last 30 days, and shadow mode uses whichever model
is the current champion. The scorecard is the honest answer to "is it better than the keeper yet?"

### Shadow mode (from day one)
The keeper scores every width each minute with its own simulator at its own volatility estimate
(`width_evals`). The A.I keeps that simulator's physics, swaps in its own volatility view, and picks
a width. Both choices are replayed on the pool's real next-24-hour prices with keeper-like rules
(rebalance after 6 h out of range, 2 h cooldown): reward earned in range, rebalance costs, and exact
concentrated-liquidity divergence. Details in [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md).

## Commands

```
aikeeper run                 # 24/7 daemon
aikeeper selftest            # prove the guardrails and read once from every source
aikeeper backfill --days 365 # a year of ETH minutes, DVOL, the keeper's history
aikeeper predict             # this hour's forecasts and shadow rows now
aikeeper resolve             # grade what is due
aikeeper scorecard           # how every model is doing
aikeeper brief [--send]      # print (or email) today's brief
aikeeper note "too narrow in this chop"   # teach it; shown in the brief with the day's decisions
```

## Running it for $0

Google Cloud's always-free tier: one e2-micro VM (1 GB RAM — which is why it is written in Rust),
30 GB disk, and the bucket's free read allowance (the collector is budgeted to stay inside it).
Email goes out through Gmail with an app password. Step by step: [docs/DEPLOY.md](docs/DEPLOY.md).

## Status

v0.1 — collectors, six volatility models, range-exit models, shadow mode with replay grading,
scorecard, daily brief, guardrails. Roadmap: [docs/ROADMAP.md](docs/ROADMAP.md).
