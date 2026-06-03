//! Public-key encoding helper for the Light-API `pub_keys` reverse index.
//!
//! abieos renders authority keys in the modern `PUB_K1_…` form, but cc32d9 `/key` callers typically
//! use the legacy `EOS…` form. We store BOTH on each `pub_keys` / permission key entry so a query in
//! either encoding matches. This module converts a `PUB_K1_…` string to its legacy `EOS…` equivalent.
//!
//! Encodings (Antelope):
//!   legacy : `"EOS" + base58( point33 || ripemd160(point33)[..4] )`
//!   K1     : `"PUB_K1_" + base58( point33 || ripemd160(point33 || "K1")[..4] )`
//! Only K1 keys have a legacy form; R1/WA keys return `None` (callers store only the modern form).

use ripemd::{Digest, Ripemd160};

/// Convert a `PUB_K1_…` key to its legacy `EOS…` form. Returns `None` for non-K1 keys or malformed
/// input.
pub fn k1_to_legacy(pub_k1: &str) -> Option<String> {
    let b58 = pub_k1.strip_prefix("PUB_K1_")?;
    let raw = bs58::decode(b58).into_vec().ok()?;
    // raw = 33-byte compressed point + 4-byte checksum (ripemd160(point || "K1")[..4]).
    if raw.len() != 37 {
        return None;
    }
    let point = &raw[..33];
    let want = &raw[33..37];
    if &ripemd160_suffix(point, b"K1")[..4] != want {
        return None; // checksum mismatch — not a valid K1 key
    }
    let mut buf = point.to_vec();
    buf.extend_from_slice(&ripemd160_suffix(point, b"")[..4]);
    Some(format!("EOS{}", bs58::encode(buf).into_string()))
}

/// `ripemd160(data || suffix)` (suffix is the curve tag for the modern checksum, empty for legacy).
fn ripemd160_suffix(data: &[u8], suffix: &[u8]) -> [u8; 20] {
    let mut h = Ripemd160::new();
    h.update(data);
    if !suffix.is_empty() {
        h.update(suffix);
    }
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_known_vectors() {
        // Captured from the live cc32d9 instance (bp.boid permissions).
        assert_eq!(
            k1_to_legacy("PUB_K1_7epPHs9z9zrngVSXmUyuVLzCDE1SexW2tqTorJhugoQCAmyxZL").as_deref(),
            Some("EOS7epPHs9z9zrngVSXmUyuVLzCDE1SexW2tqTorJhugoQCErtqiU")
        );
        assert_eq!(
            k1_to_legacy("PUB_K1_7bQBpDGoX5qdYXWgwxF4LDKfGmDUFEmdYW9cZgvThHBj5L3Qz5").as_deref(),
            Some("EOS7bQBpDGoX5qdYXWgwxF4LDKfGmDUFEmdYW9cZgvThHBj4tENcy")
        );
    }

    #[test]
    fn rejects_non_k1() {
        assert!(k1_to_legacy("EOS7epPHs9z9zrngVSXmUyuVLzCDE1SexW2tqTorJhugoQCErtqiU").is_none());
        assert!(k1_to_legacy("PUB_R1_xyz").is_none());
        assert!(k1_to_legacy("garbage").is_none());
    }
}
