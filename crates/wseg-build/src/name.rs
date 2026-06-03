//! Antelope `name` -> u64 encoder. Must match WormDB's `src/core/name.zig`
//! byte-for-byte so a segment keyed here looks up correctly there. Canonical
//! eosio packing: first 12 chars 5 bits each (bits 63..4), optional 13th in the
//! low 4 bits; invalid characters map to 0 ('.').

fn char_to_symbol(c: u8) -> u64 {
    if c.is_ascii_lowercase() {
        return (c - b'a') as u64 + 6;
    }
    if (b'1'..=b'5').contains(&c) {
        return (c - b'1') as u64 + 1;
    }
    0 // '.' and everything else
}

/// Encode a name string to its canonical u64 representation.
pub fn encode(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut value: u64 = 0;
    let mut i = 0usize;
    while i < b.len() && i < 12 {
        let shift = 64 - 5 * (i + 1);
        value |= (char_to_symbol(b[i]) & 0x1f) << shift;
        i += 1;
    }
    if b.len() > 12 {
        value |= char_to_symbol(b[12]) & 0x0f;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_eosio_vector() {
        // Matches WormDB name.zig and abieos string_to_name.
        assert_eq!(encode("eosio"), 6138663577826885632);
    }

    #[test]
    fn empty_is_zero() {
        assert_eq!(encode(""), 0);
    }

    #[test]
    fn distinct_names_distinct_keys() {
        assert_ne!(encode("waxupbitcold"), encode("eosio.token"));
        assert_ne!(encode("a"), encode("b"));
    }
}
