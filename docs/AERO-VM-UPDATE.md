# Updating the A.I Keeper VM for Aero

Aerodrome becomes Aero at **2026-10-22 00:00 UTC**. This update does two things:

1. From the launch, the A.I counts **zero** Aerodrome rewards. Aerodrome's gauge keeps saying 0.503 AERO a second
   after that, but it pays nothing. The raw number is still stored; the real one is stored next to it.
2. The A.I can watch the new Aero vault, pool and gauge once Panda moves there.

The A.I stays watch-only. Nothing here gives it a key or lets it send anything. No new web host is needed: every
new read goes to the same public Base RPCs it already uses.

## Step 1: put the new code on the VM (before the launch if you can)

No setting needs to change for this step. The A.I probes the gauge by itself (`AIK_GAUGE_MODE=auto`). It finds
Aerodrome's gauge and applies the "no rewards after the end" rule on its own.

1. Merge the patch into `main` of the A.I-Keeper repository on GitHub.
2. Reset the VM (Cloud console -> Compute Engine -> VM instances -> `aikeeper` -> Reset). At every boot
   `deploy/gce-startup.sh` pulls `main`, builds, installs and restarts the service (the build takes 15-30 minutes
   on the e2-micro; the old version keeps running if the build fails).
3. Check it:
   ```
   sudo -u aikeeper /usr/local/bin/aikeeper-env selftest
   ```
   You should see a line `Gauge mode     aerodrome (probed)`. After the launch the pool line shows
   `emissions Some(0.0) AERO/s (raw Some(0.503...), aerodrome)`.
4. **Only if you update after 2026-10-22:** some hours were already scored with the old rule. Score them again:
   ```
   sudo -u aikeeper /usr/local/bin/aikeeper-env regrade --days 7
   sudo -u aikeeper /usr/local/bin/aikeeper-env replay --days 30
   ```
   (The nightly replay removes its own post-launch days once by itself; these two commands do the rest now.)

## Step 2: point the A.I at the Aero vault (when Panda moves)

The Aero WETH/USDC pool and its gauge did not exist yet on 2026-10-20. When they do, find the two addresses:

- pool: `CLFactory.getPool(WETH, USDC, 50)` on CLFactory `0x9CB220581b91b0C83223012FE69688b92aD07a76`
  (WETH `0x4200000000000000000000000000000000000006`, USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`);
- gauge: `pool.gauge()` on that pool.

Then edit `/etc/aikeeper.env` on the VM (`sudo nano /etc/aikeeper.env`) and set:

| Variable | Value | Notes |
|---|---|---|
| `AIK_POOL` | the Aero WETH/USDC pool | already exists |
| `AIK_GAUGE` | that pool's Aero gauge | already exists |
| `AIK_PRIMARY_VAULT` | the Aero vault address, **lower case** | already exists; its rewards are then read as Aero's |
| `AIK_GAUGE_MODE` | `aero` (or leave `auto`) | **new**. `aero` is checked against the gauge: a wrong address is refused, not used |
| `AIK_AERO_WETH_POOL` | Aero's AERO/WETH pool, once it exists | **new**, optional. Unset or unusable: Aerodrome's AERO/WETH CL200 pool `0x4e506648d493C8870f55e870480f92f2F33ecE51` prices AERO (one old AERO = one new AERO) |
| `AIK_CHAIN_START_BLOCK` | the Aero vault's deploy block | already exists; only used by `backfill` for the pool's event history |

The two new variables can also be set from the VM's metadata, like the email settings: keys `aik-gauge-mode`
and `aik-aero-weth-pool` (Cloud console -> VM -> Edit -> Metadata). `deploy/gce-startup.sh` copies them into
`/etc/aikeeper.env` at every boot. An empty key leaves the file alone.

Then:
```
sudo systemctl restart aikeeper
sudo -u aikeeper /usr/local/bin/aikeeper-env selftest
```
You should see `Gauge mode     aero (set, confirmed by the probe)`, the Aero voter and gauge factory addresses,
and a pool line with `aero` at the end. Nothing else to do: the LeafVoter and the gauge factory are read from the
gauge itself.

## What changed under `deploy/`

- `deploy/gce-startup.sh`: two more `set_var` lines (`AIK_GAUGE_MODE` from metadata `aik-gauge-mode`,
  `AIK_AERO_WETH_POOL` from `aik-aero-weth-pool`). Nothing else.
- `deploy/install.sh`, `deploy/aikeeper.service`: unchanged.
- `.env.example`: the two new variables, and the existing pool/gauge/vault ones, with their defaults.

## If something is wrong

- The brief and the alert email say `The A.I is not reading the gauge: ...`: `AIK_GAUGE_MODE` and `AIK_GAUGE`
  disagree (for example `aero` with Aerodrome's gauge). Fix the variable and restart. Meanwhile the pool, the
  market and everything else are still read; only the gauge is not.
- To go back to Aerodrome: set `AIK_POOL`, `AIK_GAUGE`, `AIK_PRIMARY_VAULT` back to their old values (the
  defaults in `.env.example`) and `AIK_GAUGE_MODE=auto`, then restart.
