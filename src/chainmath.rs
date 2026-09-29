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

/// Value in USD, at price `p`, of a concentrated position with liquidity `l` on range [pa, pb].
/// Standard Uniswap v3 amounts: below the range all WETH, above all USDC.
pub fn position_value(l: f64, pa: f64, pb: f64, p: f64) -> f64 {
    let (sa, sb) = (pa.sqrt(), pb.sqrt());
    let sp = p.sqrt().clamp(sa, sb);
    let x = l * (sb - sp) / (sp * sb); // WETH
    let y = l * (sp - sa); // USDC
    x * p + y
}

/// Liquidity that makes a position on [pa, pb] worth `value` USD at price `p`.
pub fn liquidity_for_value(value: f64, pa: f64, pb: f64, p: f64) -> f64 {
    let unit = position_value(1.0, pa, pb, p);
    if unit > 0.0 {
        value / unit
    } else {
        0.0
    }
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
    fn position_value_edges() {
        let (pa, pb, p) = (2500.0, 2900.0, 2700.0);
        let l = liquidity_for_value(1000.0, pa, pb, p);
        assert!((position_value(l, pa, pb, p) - 1000.0).abs() < 1e-6);
        // Far below: all WETH, worth less as price falls; far above: all USDC, flat.
        assert!(position_value(l, pa, pb, 2000.0) < position_value(l, pa, pb, 2400.0));
        let hi1 = position_value(l, pa, pb, 3000.0);
        let hi2 = position_value(l, pa, pb, 4000.0);
        assert!((hi1 - hi2).abs() < 1e-9);
    }
}
