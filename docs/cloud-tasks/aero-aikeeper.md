# Cloud job: the A.I Keeper on Aero

You are working for **Panda Financial**, a live vault platform on Base. The owner gives specs; you build everything. They are
not a developer: your summary must be plain English, short sentences, no jargon.

This repo is the **A.I Keeper**: a Rust, watch-only service on a Google Cloud VM. It never sends transactions. It collects
the pool, the gauge and the vault, replays width rules, compares them, and writes a daily report. This copy is the code the VM
runs today (branch `claude-brief` ef3a180 on `wip-rules-compare`). You start from it on branch `wip-aero-aikeeper`. Your work
comes back as ONE patch (see "Deliver"). You cannot push and must not try.

## Why
Aerodrome becomes **Aero** at **2026-10-22 00:00 UTC**. Aerodrome's gauge keeps answering `rewardRate()` = 0.503 AERO/s forever
after that (it never resets), but nothing is paid: every comparison after launch that uses it counts rewards that never arrive.
Then Panda moves to a new vault on Aero's gauge, which has NO `rewardRate`, `periodFinish`, `left` or `rewardRateByEpoch`:
rewards stream per block through a reward-growth accumulator settled from Aero's LeafVoter. The A.I Keeper must (1) stop
counting phantom Aerodrome rewards from launch, and (2) be able to point at the Aero vault, pool and gauge.

Everything verified about Aero's deployed Base contracts is in `docs/cloud-tasks/aero-facts.md` (read fully) with the verified
sources in `docs/cloud-tasks/sourcify/*.json` (`python docs/cloud-tasks/sourcify/summarise.py <file> [regex]`). The live keeper's
finished Aero port is in `docs/cloud-tasks/reference/keeper-aero.rs` and `keeper-abi.rs` (ethers-rs; read-only reference for the
maths: measured earned-delta rate + on-chain rate from LeafVoter `gaugeStates` / `emissionsPerVP` / `emissionCap` /
`projectedCumulativeRewardShare`, block-based penalty window). The Aero vault's ABI: `docs/cloud-tasks/aero-vault-abi/PandaVault.json`.

## What to change
1. **Phantom-rate rule (Aerodrome gauge, from launch):** wherever the collector reads `rewardRate()` (src/collect/chain.rs:52) or
   `periodFinish` / `rewardRateByEpoch` (src/collect/liquidity.rs ~108), the rate counts as 0 once `periodFinish <= now`.
   Replays and comparisons after 2026-10-22 00:00 UTC must show zero Aerodrome rewards. Keep the stored raw values; add the
   effective rate next to them so history is not rewritten.
2. **Gauge mode:** `AIK_GAUGE_MODE=auto|aerodrome|aero` (auto probes once: `rewardRate()` answers = Aerodrome; array
   `earned(address,uint256[])` answers and `rewardRate()` reverts = Aero). In Aero mode: no rewardRate/periodFinish reads; the
   emission rate per unit of in-range liquidity is the on-chain instantaneous rate from the LeafVoter views, and, where the
   collector already samples `earned` per position, the measured earned-delta rate as the cross-check (same maths as the
   reference). `stakedLiquidity()` still exists on Aero's CLPool. Read the penalty window in blocks
   (`CLGaugeFactory.effectivePenaltyConfig(gauge)`, `gauge.depositBlock(id)`); never convert blocks to seconds with a constant.
3. **Config:** `AIK_POOL`, `AIK_GAUGE` and the vault address already exist; add what Aero needs (the LeafVoter and gauge factory
   are read from the gauge: `gauge.voter()`, `gauge.gaugeFactory()`; an optional `AIK_AERO_WETH_POOL` for the AERO price with a
   fallback to the Aerodrome AERO/WETH CL200 pool 0x4e506648d493C8870f55e870480f92f2F33ecE51, one old AERO = one new AERO).
   Replay feeds and the report must say which mode they ran in.
4. **The daily report / Claude brief:** one line in THE SHORT VERSION when the Aerodrome rate is phantom ("Aerodrome rewards ended
   on 2026-10-22; the gauge still reports 0.50 AERO/s but pays nothing") and, in Aero mode, the measured vs on-chain rate.
5. **Do not change** the width rules, the regime/return-wait logic (TEST-ONLY by the owner's rule), the Mamo shadow run, the
   Claude call budget, the guard allowlist beyond what the new reads need (public Base RPC hosts only), or the deploy scripts
   beyond the new variables. The VM update itself is the owner's step (manual path); write the exact variables and the
   `deploy/` changes they need in `docs/AERO-VM-UPDATE.md`.

## Tests
`cargo build`, `cargo test` (82 + 5 today), `cargo clippy` no new warnings; new unit tests: phantom rule at/after periodFinish,
mode probe, Aero decode (array earned sum, struct penalty config, gaugeStates layout), the on-chain rate maths on a mock that
follows Aero's maths, the AERO price fallback, blocks-only window. The selftest must pass in both modes with mocks. If cargo
cannot download crates in the cloud, stop and say exactly what was blocked. Logs under `test-logs/aero-aikeeper/`.

## Rules
No deploy, no keys, no funds, never Alchemy, no GCP calls. No Dromos source in the repo. Minimal changes; keep every log line.
Work on your own; choose the safer option and list it.

## Deliver
Commit on `wip-aero-aikeeper` (messages end with `Co-Authored-By: Claude <noreply@anthropic.com>`), then
`git format-patch <start-commit>..HEAD --stdout > cloud-aero-aikeeper.patch`. Publish with the Artifact tool, private: title
`PF Aero A.I Keeper cloud patch`, the patch and logs attached, plus a plain-English summary: what changed and why, every test
and its result, the VM variables and update steps, open questions. Print the link(s) at the very end.
