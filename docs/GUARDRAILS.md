# Guardrails: why the A.I Keeper cannot touch money

Owner's rule (2026-09-28): *for now its hands are tied — it only watches and learns.* An instruction
is not a guardrail; a model can be talked out of an instruction. So every limit below is enforced by
what the program **can** do, and CI fails the build if any of it erodes.

## 1. It holds no key

* No wallet, signer or key library is a dependency. `tests/guardrails.rs` reads `Cargo.lock` and
  fails on `k256`, `secp256k1`, `ethers`, `alloy-signer`, `bip39` and friends.
* At start-up (and again before sending any email) `guard::check_environment` refuses to run if any
  environment variable is **named** like a key (`PRIVATE_KEY`, `MNEMONIC`, `SEED_PHRASE`, …) or
  **holds a value shaped** like a private key (64 hex characters) or a seed phrase.

## 2. It can only read the chain

* All blockchain access goes through `net::Rpc`, which passes every method through
  `guard::check_rpc_method` — an allowlist of read-only methods (`eth_call`, `eth_getLogs`,
  `eth_getBlockByNumber`, …). Sending, signing and wallet methods are refused **before** any request.
* CI fails if the words `eth_sendRawTransaction`, `eth_sendTransaction`, `personal_sign`,
  `eth_signTypedData` or signer types appear anywhere outside `guard.rs` (which names them to refuse
  them). `aikeeper selftest` shows each one being refused.
* It uses public Base endpoints, **never the keeper's Alchemy account**: Alchemy's free quota is per
  account, and the A.I must not be able to starve the live keeper. `alchemy.com` is on a deny list.

## 3. It can only talk to known places, and only to read

* Every web request passes `guard::check_host`: Base public RPCs, Coinbase, Deribit, Google Cloud
  Storage (+ the VM token endpoint) and pandafinancial.org. Railway, Cloudflare's API, GitHub's API
  and Alchemy are explicitly denied.
* The web client exposes **GET only**. The one POST in the program is the JSON-RPC client above,
  and CI checks that `src/net.rs` has exactly one `.post(` and that it calls the allowlist. No other
  file may build its own HTTP client.
* The website is checked with GET only: the Worker's POST routes write to production (a test once
  created a real referral row).

## 4. Its cloud identity can only read one bucket

On Google Cloud the VM runs as a dedicated service account with exactly one permission:
`roles/storage.objectViewer` on `gs://pfkeeperdata`. It can list and read the keeper's history;
it cannot write, delete, or see anything else in the project. The live keeper's own key (which can
create objects) is never given to it. See `docs/DEPLOY.md`.

## 5. What it can change

Only its own SQLite file and its own brief files on its own disk. Its only outward action is one
email a day to the owner.

## Moving past watch-only

Not before the owner decides, and not by editing this file. The ladder agreed on 2026-09-28:
observe + shadow (now) → recommend changes that people review and apply → only after months of
out-of-sample wins, a small capped trial whose limits are enforced **on-chain** (a bounded keeper
contract), never direct control of the vault.
