# Roadmap

Owner's decisions (2026-09-28): shadow mode from day one, data 24/7, $0 budget (in practice about
$0.003/month of bucket reads), Rust, Google Cloud next to the bucket, daily brief by email, news
tagging later. Hands tied until the owner says otherwise (see GUARDRAILS.md).

## Built and running (2026-09-29)
- Guardrails in code and CI: no keys, read-only RPC allowlist, host allowlist, GET-only web,
  read-only mail
- Collectors: pool on Base every 30 s (price, liquidity, fee, gas, AERO emission rate), Coinbase
  minutes, Deribit DVOL, the keeper's history bucket, the website
- Six volatility models incl. HAR (nightly refit) and a self-weighting blend; range-exit odds on two
  price bases
- Prediction registry with automatic grading and a scorecard
- Shadow mode: keeper vs every model under the keeper's exact rules, graded on real prices, with a
  fidelity check against the keeper's own history
- Alerts by email; replies to the brief read back as notes; daily brief
- Self-updating server: push to GitHub, restart the VM

## Deliberately not built yet
Only what the owner has deferred or must decide:
- **News / macro tagging** — owner deferred it (2026-09-28).
- **Recommend mode** (the A.I proposing keeper setting changes for people to review) — owner's call
  once the scorecard has ~30 independent days behind it.
- **Any live trading, even a capped trial** — owner's call, and only with limits enforced on-chain by
  the upgradeable platform's bounded keeper.
