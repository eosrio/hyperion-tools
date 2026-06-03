//! Public-key handling for `/key/PUBKEY`.
//!
//! cc32d9 accepts either the legacy `EOS…` form or the modern `PUB_K1_…`/`PUB_R1_…`/`PUB_WA_…` form.
//! Rather than convert between encodings in the hot path (which needs base58 + ripemd160), the loader
//! stores BOTH forms on each `pub_keys` / permission key entry (`key` = legacy `EOS…`,
//! `key_pub` = `PUB_…`), and the server matches the raw input against either field. This module just
//! validates and trims the input.

/// Trim + sanity-check a public key from the request path. Returns the canonical query string, or
/// `None` if it does not look like an Antelope public key.
pub fn normalize(input: &str) -> Option<String> {
    let k = input.trim();
    if k.len() < 8 || k.len() > 128 {
        return None;
    }
    let ok = k.starts_with("EOS")
        || k.starts_with("PUB_K1_")
        || k.starts_with("PUB_R1_")
        || k.starts_with("PUB_WA_");
    if !ok {
        return None;
    }
    if !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some(k.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_both_forms() {
        assert!(normalize("EOS7epPHs9z9zrngVSXmUyuVLzCDE1SexW2tqTorJhugoQCErtqiU").is_some());
        assert!(normalize("PUB_K1_7epPHs9z9zrngVSXmUyuVLzCDE1SexW2tqTorJhugoQCAmyxZL").is_some());
    }

    #[test]
    fn rejects_junk() {
        assert!(normalize("").is_none());
        assert!(normalize("hello").is_none());
        assert!(normalize("EOS$$$bad chars").is_none());
    }
}
