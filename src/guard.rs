//! The hands-tied rules, enforced in code.
//!
//! The A.I Keeper watches and learns. It must never be able to move money, sign anything, or change
//! any Panda system. Those limits are NOT left to good behaviour: they are enforced here, at the
//! only three doors to the outside world, and checked again by `tests/guardrails.rs` in CI.
//!
//! 1. Environment: the process refuses to start if anything that looks like a private key or seed
//!    phrase is present in its environment (by name or by value).
//! 2. Blockchain: every JSON-RPC call passes `check_rpc_method`, an allowlist of read-only methods.
//!    Sending, signing and wallet methods are rejected before any request is made.
//! 3. Web: every HTTP request passes `check_host`, an allowlist of hosts, and the web client only
//!    exposes GET (the JSON-RPC client is the one POST, and it carries only allowlisted methods).
//!
//! No wallet, signer or key-handling library is a dependency (the CI test reads Cargo.lock).

use anyhow::{bail, Result};

/// JSON-RPC methods the A.I may call. Every one of them only reads chain state.
pub const READ_ONLY_RPC_METHODS: &[&str] = &[
    "eth_chainId",
    "eth_blockNumber",
    "eth_call",
    "eth_getLogs",
    "eth_getBlockByNumber",
    "eth_getBalance",
    "eth_getCode",
    "eth_gasPrice",
    "eth_feeHistory",
    "eth_getTransactionByHash",
    "eth_getTransactionReceipt",
];

/// Methods `aikeeper selftest` tries, to prove they are refused.
pub const MUST_REFUSE: &[&str] =
    &["eth_sendRawTransaction", "eth_sendTransaction", "personal_sign", "eth_signTypedData_v4", "wallet_sendCalls"];

/// Hosts the A.I may talk to at all. Anything else is refused before a connection is opened.
pub const ALLOWED_HOSTS: &[&str] = &[
    // Base, public read-only endpoints. NOT the keeper's Alchemy account: its free quota is shared
    // with the live keeper, and the A.I must never be able to starve it.
    "mainnet.base.org",
    "base-rpc.publicnode.com",
    "base.drpc.org",
    // The only free endpoint found (2026-10-09) that serves eth_getLogs more than ~10,000 blocks back
    // (up to 1,000 blocks per call); the others refuse old ranges, so a reader that fell behind could
    // never catch up.
    "base.gateway.tenderly.co",
    // Market data (public, no key).
    "api.exchange.coinbase.com",
    "www.deribit.com",
    // Yields of every Aerodrome pool (DefiLlama) and the Crypto Fear & Greed index (public, no key).
    "yields.llama.fi",
    "api.alternative.me",
    // Crypto headlines (public RSS feeds, GET only).
    "www.coindesk.com",
    "cointelegraph.com",
    "decrypt.co",
    "www.theblock.co",
    "thedefiant.io",
    // The keeper's history bucket (read-only service account) and the VM's token endpoint.
    "storage.googleapis.com",
    "metadata.google.internal",
    // Panda's own website, GET only (the Worker's POST routes write to production).
    "pandafinancial.org",
    // The owner's mail: sending the brief (SMTP) and reading replies to it (IMAP, read-only EXAMINE).
    "smtp.gmail.com",
    "imap.gmail.com",
];

/// Hosts that are refused even if someone adds them above by mistake, with the reason.
const DENIED_HOSTS: &[(&str, &str)] = &[
    ("alchemy.com", "the live keeper's RPC quota lives here; the A.I must not share it"),
    ("railway.app", "the live keeper runs here; the A.I has no business near it"),
    ("api.cloudflare.com", "the website's control plane"),
    ("api.github.com", "code changes go through people, not the A.I"),
];

/// Environment variable names that mean a signing key is present.
const FORBIDDEN_ENV_NAME_PARTS: &[&str] = &["PRIVATE_KEY", "PRIVKEY", "MNEMONIC", "SEED_PHRASE", "SECRET_KEY", "SIGNER_KEY", "WALLET_KEY"];

pub fn check_rpc_method(method: &str) -> Result<()> {
    if READ_ONLY_RPC_METHODS.contains(&method) {
        Ok(())
    } else {
        bail!("guard: RPC method {method} is not on the read-only allowlist; the A.I Keeper cannot send, sign or change anything")
    }
}

pub fn check_host(url: &str) -> Result<()> {
    let host = host_of(url).ok_or_else(|| anyhow::anyhow!("guard: cannot read the host of {url}"))?;
    for (denied, why) in DENIED_HOSTS {
        if host == *denied || host.ends_with(&format!(".{denied}")) {
            bail!("guard: host {host} is denied ({why})");
        }
    }
    if ALLOWED_HOSTS.contains(&host.as_str()) {
        Ok(())
    } else {
        bail!("guard: host {host} is not on the allowlist")
    }
}

/// Lower-cased host of an http(s) URL, without port or credentials.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// True when a value looks like a raw 32-byte private key (64 hex characters, optional 0x).
fn looks_like_private_key(v: &str) -> bool {
    let h = v.trim().strip_prefix("0x").unwrap_or(v.trim());
    h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// True when a value looks like a BIP-39 seed phrase (12, 15, 18, 21 or 24 lowercase words).
fn looks_like_seed_phrase(v: &str) -> bool {
    let words: Vec<&str> = v.split_whitespace().collect();
    matches!(words.len(), 12 | 15 | 18 | 21 | 24) && words.iter().all(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_lowercase()))
}

/// Refuses to run when the environment carries anything that could sign a transaction.
pub fn check_environment<I: IntoIterator<Item = (String, String)>>(vars: I) -> Result<()> {
    for (name, value) in vars {
        let upper = name.to_ascii_uppercase();
        if FORBIDDEN_ENV_NAME_PARTS.iter().any(|p| upper.contains(p)) {
            bail!("guard: environment variable {name} looks like a signing key. The A.I Keeper refuses to start while any key is present");
        }
        if looks_like_private_key(&value) {
            bail!("guard: environment variable {name} holds a value shaped like a private key. The A.I Keeper refuses to start while any key is present");
        }
        if looks_like_seed_phrase(&value) {
            bail!("guard: environment variable {name} holds a value shaped like a seed phrase. The A.I Keeper refuses to start while any key is present");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sending_and_signing_are_refused() {
        for m in [
            "eth_sendRawTransaction",
            "eth_sendTransaction",
            "eth_sign",
            "eth_signTypedData_v4",
            "personal_sign",
            "wallet_sendCalls",
            "debug_traceCall",
            "anvil_setBalance",
        ] {
            assert!(check_rpc_method(m).is_err(), "{m} must be refused");
        }
    }

    #[test]
    fn reads_are_allowed() {
        for m in READ_ONLY_RPC_METHODS {
            assert!(check_rpc_method(m).is_ok());
        }
    }

    #[test]
    fn hosts() {
        assert!(check_host("https://mainnet.base.org").is_ok());
        assert!(check_host("https://api.exchange.coinbase.com/products/ETH-USD/candles").is_ok());
        assert!(check_host("http://metadata.google.internal/computeMetadata/v1/").is_ok());
        assert!(check_host("https://base-mainnet.g.alchemy.com/v2/xyz").is_err());
        assert!(check_host("https://backboard.railway.app/graphql").is_err());
        assert!(check_host("https://evil.example.com").is_err());
        assert!(check_host("https://pandafinancial.org.evil.com/").is_err());
        assert!(check_host("https://user:pw@mainnet.base.org:443/x").is_ok());
        assert!(check_host("ftp://mainnet.base.org").is_err());
        // Full capture sources, and nothing that merely resembles them.
        for u in [
            "https://yields.llama.fi/pools",
            "https://api.alternative.me/fng/",
            "https://www.coindesk.com/arc/outboundfeeds/rss/",
            "https://cointelegraph.com/rss",
            "https://decrypt.co/feed",
            "https://www.theblock.co/rss.xml",
            "https://thedefiant.io/api/feed",
        ] {
            assert!(check_host(u).is_ok(), "{u}");
        }
        assert!(check_host("https://api.llama.fi/protocols").is_err(), "only the yields host");
        assert!(check_host("https://evil.decrypt.co/feed").is_err());
    }

    #[test]
    fn every_collector_url_passes_the_guard() {
        for (_, u) in crate::collect::news::FEEDS {
            assert!(check_host(u).is_ok(), "{u}");
        }
    }

    #[test]
    fn environment() {
        let ok = vec![
            ("AIK_DATA_DIR".to_string(), "/var/lib/aikeeper".to_string()),
            ("AIK_SMTP_PASSWORD".to_string(), "abcd efgh ijkl mnop".to_string()),
        ];
        assert!(check_environment(ok).is_ok());
        assert!(check_environment(vec![("KEEPER_PRIVATE_KEY".into(), "x".into())]).is_err());
        assert!(check_environment(vec![("FOO".into(), format!("0x{}", "a".repeat(64)))]).is_err());
        assert!(check_environment(vec![(
            "BAR".into(),
            "abandon ability able about above absent absorb abstract absurd abuse access accident".into()
        )])
        .is_err());
        // A transaction hash would look like a key too; the A.I has no reason to hold one in its environment.
    }
}
