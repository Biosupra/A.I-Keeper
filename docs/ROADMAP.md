# Roadmap

Owner's decisions (2026-09-28): shadow mode from day one, data 24/7, $0 budget, Rust, Google Cloud
next to the bucket, daily brief by email, news tagging later. Hands tied until the owner says
otherwise (see GUARDRAILS.md).

## v0.1 — now
- [x] Guardrails in code and CI (no keys, read-only RPC allowlist, host allowlist, GET-only web)
- [x] Collectors: pool on Base (30 s), Coinbase minutes, Deribit DVOL, keeper bucket (lean), website
- [x] Six volatility models incl. HAR (nightly refit) and a self-weighting blend
- [x] Range-exit probabilities per width (formula on two bases + empirical)
- [x] Prediction registry with automatic grading and a scorecard
- [x] Shadow mode on every keeper width table, graded by replay on real ticks
- [x] Daily brief (email or file), owner notes
- [x] Deploy kit for the free e2-micro VM

## v0.2 — next
- [ ] Read owner replies to the brief (Gmail IMAP, same app password) into `notes`
- [ ] Replay the keeper's exact rules (TWAP trigger, 6 h confirmation on TWAP, crash valve, re-cut
      hysteresis) so shadow grades match the keeper's real accounting
- [ ] Emission forecasts: next-epoch AERO rate from `rewardRate`, staked liquidity and weekly votes
- [ ] Compare shadow grades with the keeper's REAL outcomes (position_events + snapshots)
- [ ] Weekly deep-review email (7-day scorecard, biggest misses, what changed)
- [ ] Alerts: pool/keeper anomalies (width feed stale > 30 min, no keeper rows for 2 h, site down)

## v0.3 — learning more
- [ ] Features beyond volatility: DVOL term structure, funding rates, time of day/week, gas regime
- [ ] A gradient-boosted exit-probability model trained walk-forward on the registry's own history
- [ ] Width recommendations with evidence ("over 30 days, ±X% in regime Y beat the keeper by Z")

## Later (owner's call)
- [ ] News / macro calendar tagging
- [ ] Recommend mode: proposals people review and apply through the normal keeper release process
- [ ] A small capped trial, with limits enforced on-chain by the upgradeable platform's bounded keeper
