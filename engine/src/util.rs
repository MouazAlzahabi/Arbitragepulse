use alloy::primitives::{Address, U256}; // U256 used by u256_to_f64

/// Convert a U256 to f64 (lossy for very large values).
pub fn u256_to_f64(value: U256) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(0.0)
}

/// Lowercase hex string for an address — used as pool cache key components.
pub fn addr_key(addr: Address) -> String {
    format!("{:?}", addr).to_lowercase()
}

