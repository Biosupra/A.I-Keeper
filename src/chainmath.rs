//! Small pieces of Ethereum and concentrated-liquidity arithmetic the A.I needs to READ the chain
//! and to value a position on a price path. Nothing here can build or sign a transaction.

use tiny_keccak::{Hasher, Keccak};

/// 4-byte function selector from a signature such as `slot0()`.
pub fn selector(signature: &str) -> String {
    let mut k = Keccak::v256();
    k.update(signature.as_bytes());
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    format!("0x{}", hex(&out[..4]))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The 32-byte words of an ABI-encoded return value.
pub fn words(data: &str) -> Vec<String> {
    let h = data.strip_prefix("0x").unwrap_or(data);
    h.as_bytes().chunks(64).map(|c| String::from_utf8_lossy(c).to_string()).collect()
}

/// A word as an unsigned integer (as f64 for large values; exact enough for prices and sizes).
pub fn word_u128(w: &str) -> u128 {
    let w = w.trim_start_matches('0');
    if w.is_empty() {
        return 0;
    }
    let tail = if w.len() > 32 { &w[w.len() - 32..] } else { w };
    u128::from_str_radix(tail, 16).unwrap_or(0)
}

/// A word as a signed 256-bit integer that fits in i64 (ticks).
pub fn word_i64(w: &str) -> i64 {
    // Two's complement: take the low 16 hex digits.
    let low = &w[w.len().saturating_sub(16)..];
    u64::from_str_radix(low, 16).map(|v| v as i64).unwrap_or(0)
}

/// Pool price in USDC per WETH from a tick (token0 = WETH, 18 dp; token1 = USDC, 6 dp).
pub fn tick_to_price(tick: f64) -> f64 {
    1.0001f64.powf(tick) * 1e12
}

#[cfg(test)]
pub fn price_to_tick(price: f64) -> f64 {
    (price / 1e12).ln() / 1.0001f64.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors() {
        assert_eq!(selector("slot0()"), "0x3850c7bd");
        assert_eq!(selector("liquidity()"), "0x1a686502");
        assert_eq!(selector("approve(address,uint256)"), "0x095ea7b3");
    }

    #[test]
    fn price_from_tick_matches_live_pool() {
        // Keeper market_ticks row: tick -197254, weth_usd 2715.22.
        let p = tick_to_price(-197254.0);
        assert!((p - 2715.2).abs() < 1.0, "{p}");
        assert!((price_to_tick(p) + 197254.0).abs() < 1e-6);
    }

    #[test]
    fn negative_tick_word() {
        let w = format!("{}{:016x}", "f".repeat(48), (-197254i64) as u64);
        assert_eq!(word_i64(&w), -197254);
    }
}
