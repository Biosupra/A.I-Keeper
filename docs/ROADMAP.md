# Roadmap

Owner's decisions (2026-09-28): shadow mode from day one, data 24/7, $0 budget (in practice about
$0.003/month of bucket reads), Rust, Google Cloud next to the bucket, daily brief by email, news
tagging later. Hands tied until the owner says otherwise (see GUARDRAILS.md).

## Built and running (2026-09-29)
- Guardrails in code and CI: no keys, read-only RPC allowlist, host allowlist, GET-only web,
  read-only mail
- Full capture (2026-09-30): the pool every 30 s (incl. staked liquidity and the AERO price),
  every pool and gauge event (archived; swaps decoded; LP and staking activity per minute), the
  liquidity map every 10 min, emission schedule and vote share hourly, every file of the keeper's
  bucket (archived and decoded), Coinbase minutes for ETH/BTC/AERO/ETH-BTC, the ETH order book,
  Deribit DVOL (ETH, BTC), perpetuals and the ETH option board, DefiLlama yields for every
  Aerodrome pool, Fear & Greed, crypto headlines, the website
- Six volatility models incl. HAR (nightly refit) and a self-weighting blend; range-exit odds on two
  price bases
- Prediction registry with automatic grading and a scorecard
- The past, replayed (2026-09-30): three years of ETH walked through hour by hour with no lookahead
  (every model forecast, graded and labelled calm/active/storm), every day replayed under the
  keeper's rules for every width and model; the champion is chosen per regime on paired hours
- Shadow mode: keeper vs every model under the keeper's exact rules, graded on real prices, with a
  fidelity check against the keeper's own history
- Alerts by email; replies to the brief read back as notes; daily brief
- Self-updating server: push to GitHub, restart the VM

## Deliberately not built yet
Only what the owner has deferred or must decide:
- **News / macro tagging** — owner deferred it (2026-09-28). The headlines themselves are captured
  since 2026-09-30, so the history exists when tagging is built.
- **Recommend mode** (the A.I proposing keeper setting changes for people to review) — owner's call
  once the scorecard has ~30 independent days behind it.
- **Any live trading, even a capped trial** — owner's call, and only with limits enforced on-chain by
  the upgradeable platform's bounded keeper.
