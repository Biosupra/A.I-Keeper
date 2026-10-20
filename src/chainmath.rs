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

/// A 256-bit two's-complement word as an exact decimal string (swap amounts, liquidityNet).
/// Values that fit in 128 bits are exact; anything larger (never seen in this pool) is reported in hex.
pub fn word_signed_dec(w: &str) -> String {
    let w = w.trim_start_matches("0x");
    let w = if w.len() < 64 { format!("{w:0>64}") } else { w[w.len() - 64..].to_string() };
    let (high, low) = w.split_at(32);
    let low_v = u128::from_str_radix(low, 16).unwrap_or(0);
    if high.chars().all(|c| c == '0') {
        low_v.to_string()
    } else if high.chars().all(|c| c == 'f' || c == 'F') {
        // Negative: the low 128 bits read as i128 are the value when it fits.
        (low_v as i128).to_string()
    } else {
        format!("0x{w}")
    }
}

/// An unsigned word as an exact decimal string when it fits in 128 bits (else hex).
pub fn word_unsigned_dec(w: &str) -> String {
    let w = w.trim_start_matches("0x");
    let w = if w.len() < 64 { format!("{w:0>64}") } else { w[w.len() - 64..].to_string() };
    let (high, low) = w.split_at(32);
    if high.chars().all(|c| c == '0') {
        u128::from_str_radix(low, 16).unwrap_or(0).to_string()
    } else {
        format!("0x{w}")
    }
}

/// An address from a 32-byte topic or word (the low 20 bytes), lowercase with 0x.
pub fn word_address(w: &str) -> String {
    let w = w.trim_start_matches("0x");
    format!("0x{}", &w[w.len().saturating_sub(40)..]).to_ascii_lowercase()
}

/// keccak256 of an event signature, as a 0x topic.
pub fn topic(signature: &str) -> String {
    let mut k = Keccak::v256();
    k.update(signature.as_bytes());
    let mut out = [0u8; 32];
    k.finalize(&mut out);
    format!("0x{}", hex(&out))
}

/// ABI-encodes a single int24 / int256 argument as one 32-byte word (two's complement).
pub fn encode_i64_word(v: i64) -> String {
    if v < 0 {
        format!("{}{:016x}", "f".repeat(48), v as u64)
    } else {
        format!("{v:064x}")
    }
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

    #[test]
    fn signed_words() {
        let neg = format!("{}{:032x}", "f".repeat(32), (-1_234_567_890_123i128) as u128);
        assert_eq!(word_signed_dec(&neg), "-1234567890123");
        assert_eq!(word_signed_dec(&format!("{:064x}", 987_654_321u64)), "987654321");
        assert_eq!(word_unsigned_dec(&format!("{:064x}", u128::MAX)), u128::MAX.to_string());
        assert_eq!(
            word_address(&format!("{}{}", "0".repeat(24), "3FE04A59Ebd38cF06080a6F60a98D124eb59392A")),
            "0x3fe04a59ebd38cf06080a6f60a98d124eb59392a"
        );
        assert_eq!(
            topic("Swap(address,address,int256,int256,uint160,uint128,int24)"),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
        );
        assert_eq!(word_i64(&encode_i64_word(-197250)), -197250);
        assert_eq!(word_i64(&encode_i64_word(123)), 123);
    }
}
