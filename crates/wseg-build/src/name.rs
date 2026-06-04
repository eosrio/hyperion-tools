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

const CHARMAP: &[u8; 32] = b".12345abcdefghijklmnopqrstuvwxyz";

/// Decode a canonical u64 `name` back to its string (inverse of `encode`, trailing '.' trimmed).
/// Must match WormDB `core/name.zig` `decode` so owner names emitted into the segment's ranking /
/// reverse-index tables read back identically.
pub fn decode(mut value: u64) -> String {
    let mut buf = [b'.'; 13];
    let mut i = 0usize;
    while i <= 12 {
        let mask: u64 = if i == 0 { 0x0f } else { 0x1f };
        buf[12 - i] = CHARMAP[(value & mask) as usize];
        value >>= if i == 0 { 4 } else { 5 };
        i += 1;
    }
    let end = buf.iter().rposition(|&b| b != b'.').map_or(0, |p| p + 1);
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_round_trips() {
        for s in ["eosio", "genesis.wax", "eosio.token", "a", "waxupbitcold", "eosio.saving"] {
            assert_eq!(decode(encode(s)), s);
        }
        assert_eq!(decode(0), "");
    }

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

    #[test]
    fn token_key_matches_zig() {
        // FNV1a-64 of "eosio.token:WAX" — cross-checked against WormDB's name.zig tokenKey.
        assert_eq!(crate::builder::token_key("eosio.token", "WAX"), 13053440730298864435);
        assert_ne!(
            crate::builder::token_key("eosio.token", "WAX"),
            crate::builder::token_key("eosio.token", "EOS")
        );
    }
}
