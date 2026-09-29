//! Repository-level guardrails, run in CI on every push. They fail the build if anything that could
//! sign or send a transaction ever enters the project.

use std::fs;
use std::path::Path;

/// Crates that exist to hold keys or sign transactions. None may appear in Cargo.lock.
const FORBIDDEN_CRATES: &[&str] = &[
    "k256",
    "secp256k1",
    "libsecp256k1",
    "ethers",
    "ethers-signers",
    "ethers-middleware",
    "alloy",
    "alloy-signer",
    "alloy-signer-local",
    "alloy-network",
    "coins-bip32",
    "coins-bip39",
    "bip39",
    "eth-keystore",
    "web3",
];

/// Strings that only appear in code that sends or signs.
const FORBIDDEN_STRINGS: &[&str] = &[
    "eth_sendRawTransaction",
    "eth_sendTransaction",
    "personal_sign",
    "eth_signTypedData",
    "signTransaction",
    "sign_transaction",
    "LocalWallet",
    "PrivateKeySigner",
];

#[test]
fn no_signing_crates_in_lockfile() {
    let lock = fs::read_to_string("Cargo.lock").expect("Cargo.lock is committed");
    for krate in FORBIDDEN_CRATES {
        let needle = format!("name = \"{krate}\"");
        assert!(!lock.contains(&needle), "forbidden crate {krate} is in Cargo.lock: the A.I Keeper must not be able to sign");
    }
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

#[test]
fn no_send_or_sign_code_outside_the_guard() {
    let mut files = Vec::new();
    walk(Path::new("src"), &mut files);
    for f in files {
        // guard.rs names these methods in order to refuse them.
        if f.ends_with("guard.rs") {
            continue;
        }
        let text = fs::read_to_string(&f).unwrap();
        for s in FORBIDDEN_STRINGS {
            assert!(!text.contains(s), "{} contains {s:?}; sending/signing code is not allowed here", f.display());
        }
    }
}

#[test]
fn web_client_has_no_write_verbs() {
    let net = fs::read_to_string("src/net.rs").unwrap();
    for verb in [".put(", ".delete(", ".patch("] {
        assert!(!net.contains(verb), "src/net.rs must not expose {verb}");
    }
    // The single POST is the JSON-RPC client, and every method it carries passes the allowlist.
    assert_eq!(net.matches(".post(").count(), 1, "exactly one POST (JSON-RPC) is allowed");
    assert!(net.contains("guard::check_rpc_method"), "the JSON-RPC POST must check the method allowlist");
    // Nothing outside net.rs may build its own HTTP client.
    let mut files = Vec::new();
    walk(Path::new("src"), &mut files);
    for f in files.iter().filter(|f| !f.ends_with("net.rs")) {
        let t = fs::read_to_string(f).unwrap();
        assert!(!t.contains("reqwest::Client"), "{} builds its own HTTP client; use net::Web / net::Rpc", f.display());
    }
}

#[test]
fn no_secrets_committed() {
    for f in [".env", "data/aikeeper.sqlite"] {
        assert!(
            !Path::new(f).exists() || fs::read_to_string(".gitignore").unwrap_or_default().contains(f.split('/').next().unwrap()),
            "{f} must be git-ignored"
        );
    }
}
