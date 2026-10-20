//! The text forms nodeos gives names, block timestamps, public keys and signatures in JSON.
//!
//!   * `name` (Spring `libraries/chain/name.cpp`): 13 base-32 characters, trailing dots
//!     trimmed, so the name 0 is `""`.
//!   * `block_timestamp_type`: the time point `946684800000 + slot × 500` ms, written by fc as
//!     `YYYY-MM-DDTHH:MM:SS.mmm` (always three millisecond digits, no zone).
//!   * Public keys and signatures (fc `crypto/common.hpp`, `public_key.cpp`, `signature.cpp`):
//!     base58 of the packed key data followed by the first four bytes of its ripemd160, where
//!     the hash also covers the curve tag (`K1`, `R1`, `WA`) for every form but the legacy K1
//!     key. A K1 key is `EOS…`; the others are `PUB_R1_…`, `PUB_WA_…`, `SIG_K1_…`,
//!     `SIG_R1_…` and `SIG_WA_…`. WebAuthn data is re-packed as fc packs it, with canonical
//!     varuints.

use ripemd::{Digest, Ripemd160};

use super::block::{PublicKey, Signature};

/// An account or action name.
pub fn name(value: u64) -> String {
    const CHARMAP: &[u8; 32] = b".12345abcdefghijklmnopqrstuvwxyz";
    let mut s = [b'.'; 13];
    let mut tmp = value;
    for (i, c) in s.iter_mut().rev().enumerate() {
        let (mask, shift) = if i == 0 { (0x0f, 4) } else { (0x1f, 5) };
        *c = CHARMAP[(tmp & mask) as usize];
        tmp >>= shift;
    }
    let len = s.iter().rposition(|&c| c != b'.').map_or(0, |i| i + 1);
    String::from_utf8_lossy(&s[..len]).into_owned()
}

/// A `block_timestamp_type` slot as nodeos writes it.
pub fn block_timestamp(slot: u32) -> String {
    crate::provider::TimestampProfile::ANTELOPE.slot_to_iso(slot)
}

/// Protocol-aware timestamp rendering; other Wire block-layout changes still need
/// the selected provider's block decoder, rather than the Antelope fast parser.
pub fn block_timestamp_with_provider(
    slot: u32,
    provider: &crate::provider::ProtocolProvider,
) -> anyhow::Result<String> {
    Ok(provider.timestamp_profile()?.slot_to_iso(slot))
}

/// A public key as nodeos writes it.
pub fn public_key(key: &PublicKey<'_>) -> String {
    match key {
        PublicKey::K1(point) => format!("EOS{}", checked_base58(*point, b"")),
        PublicKey::R1(point) => format!("PUB_R1_{}", checked_base58(*point, b"R1")),
        PublicKey::WebAuthn {
            point,
            user_presence,
            rpid,
        } => {
            let mut d = point.to_vec();
            d.push(*user_presence);
            bytes_into(&mut d, rpid);
            format!("PUB_WA_{}", checked_base58(&d, b"WA"))
        }
    }
}

/// A signature as nodeos writes it.
pub fn signature(sig: &Signature<'_>) -> String {
    match sig {
        Signature::K1(compact) => format!("SIG_K1_{}", checked_base58(*compact, b"K1")),
        Signature::R1(compact) => format!("SIG_R1_{}", checked_base58(*compact, b"R1")),
        Signature::WebAuthn {
            compact,
            auth_data,
            client_json,
        } => {
            let mut d = compact.to_vec();
            bytes_into(&mut d, auth_data);
            bytes_into(&mut d, client_json);
            format!("SIG_WA_{}", checked_base58(&d, b"WA"))
        }
    }
}

/// `base58(data || ripemd160(data || tag)[..4])`.
fn checked_base58(data: &[u8], tag: &[u8]) -> String {
    let digest = Ripemd160::new()
        .chain_update(data)
        .chain_update(tag)
        .finalize();
    let mut buf = Vec::with_capacity(data.len() + 4);
    buf.extend_from_slice(data);
    buf.extend_from_slice(&digest[..4]);
    bs58::encode(buf).into_string()
}

/// fc `bytes`/`string` packing: a canonical varuint32 length, then the data.
fn bytes_into(out: &mut Vec<u8>, data: &[u8]) {
    let mut v = data.len() as u64;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JSON value through rs_abieos, an independent decoder: `type_name` packed as `bin`.
    fn abieos(type_name: &str, bin: &[u8]) -> String {
        let abi = format!(
            r#"{{"version": "eosio::abi/1.1", "types": [], "actions": [], "tables": [],
                "ricardian_clauses": [], "variants": [],
                "structs": [{{"name": "s", "base": "", "fields": [{{"name": "v", "type": "{type_name}"}}]}}]}}"#
        );
        let a = rs_abieos::Abieos::new();
        a.set_abi_json("texttest", &abi).unwrap();
        let json = a.bin_to_json("texttest", "s", bin).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v["v"].as_str().unwrap().to_owned()
    }

    #[test]
    fn names_match_an_independent_decoder() {
        let a = rs_abieos::Abieos::new();
        assert_eq!(name(0), "");
        assert_eq!(name(0x5530_ea00_0000_0000), "eosio");
        assert_eq!(name(0x5530_ea03_3482_a600), "eosio.token");
        assert_eq!(name(u64::MAX), "zzzzzzzzzzzzj");
        for v in [
            1,
            0x0f,
            0x10,
            0x5530_ea00_0000_0000,
            0x5530_ea03_3482_a600,
            0x8000_0000_0000_0000,
            0x0123_4567_89ab_cdef,
            u64::MAX,
        ] {
            assert_eq!(name(v), a.name_to_string(v).unwrap(), "{v:#x}");
        }
    }

    #[test]
    fn block_timestamps_have_three_millisecond_digits() {
        assert_eq!(block_timestamp(0), "2000-01-01T00:00:00.000");
        assert_eq!(block_timestamp(1), "2000-01-01T00:00:00.500");
        assert_eq!(block_timestamp(2 * 86_400), "2000-01-02T00:00:00.000");
        // 2000 is a leap year; 2100 is not.
        assert_eq!(block_timestamp(2 * 86_400 * 59), "2000-02-29T00:00:00.000");
        assert_eq!(block_timestamp(u32::MAX), "2068-01-19T03:14:07.500");
        for slot in [
            0,
            1,
            3,
            1_645_576_422,
            1_645_576_423,
            4_000_000_001,
            u32::MAX,
        ] {
            let t = block_timestamp(slot);
            assert_eq!(
                t,
                abieos("block_timestamp_type", &slot.to_le_bytes()),
                "{slot}"
            );
            assert_eq!(t.len(), 23, "{t}");
        }
    }

    #[test]
    fn legacy_k1_keys() {
        // The EOS mainnet genesis key, from its 33 bytes (as the testkit genesis state holds
        // them).
        let point = [
            0x03, 0x35, 0x05, 0x29, 0xef, 0xca, 0x8c, 0x60, 0x74, 0x21, 0xe9, 0x58, 0x46, 0xcc,
            0x2a, 0x3d, 0x2e, 0xfa, 0xa8, 0x45, 0x40, 0x18, 0xde, 0xb7, 0x5a, 0x20, 0x4e, 0x27,
            0xac, 0xf2, 0x9e, 0xe5, 0xcc,
        ];
        assert_eq!(
            public_key(&PublicKey::K1(&point)),
            "EOS7EarnUhcyYqmdnPon8rm7mBCTnBoot6o7fE2WzjvEX2TdggbL3"
        );
        // And back from a published key: its checksum is what this computes.
        let legacy = "EOS8Znrtgwt8TfpmbVpTKvA2oB8Nqey625CLN8bCN3TEbgx86Dsvr";
        let raw = bs58::decode(&legacy[3..]).into_vec().unwrap();
        let point: [u8; 33] = raw[..33].try_into().unwrap();
        assert_eq!(public_key(&PublicKey::K1(&point)), legacy);
        // rs_abieos writes K1 keys in the modern form only.
        let mut bin = vec![0];
        bin.extend_from_slice(&point);
        let modern = abieos("public_key", &bin);
        assert!(modern.starts_with("PUB_K1_"), "{modern}");
        assert_ne!(&modern[7..], &legacy[3..]);
    }

    #[test]
    fn r1_and_webauthn_keys_match_an_independent_decoder() {
        let mut point = [0u8; 33];
        point[0] = 0x02;
        for (i, b) in point[1..].iter_mut().enumerate() {
            *b = i as u8 * 7 + 1;
        }
        let mut bin = vec![1];
        bin.extend_from_slice(&point);
        assert_eq!(
            public_key(&PublicKey::R1(&point)),
            abieos("public_key", &bin)
        );

        for rpid in [&b""[..], b"example.com", &[b'r'; 200]] {
            let key = PublicKey::WebAuthn {
                point: &point,
                user_presence: 2,
                rpid,
            };
            let mut bin = vec![2];
            bin.extend_from_slice(&point);
            bin.push(2);
            bytes_into(&mut bin, rpid);
            let text = public_key(&key);
            assert!(text.starts_with("PUB_WA_"));
            assert_eq!(text, abieos("public_key", &bin), "rpid of {}", rpid.len());
        }
    }

    #[test]
    fn signatures_match_an_independent_decoder() {
        let mut compact = [0u8; 65];
        for (i, b) in compact.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13).wrapping_add(31);
        }
        for (k, sig) in [(0, Signature::K1(&compact)), (1, Signature::R1(&compact))] {
            let mut bin = vec![k];
            bin.extend_from_slice(&compact);
            assert_eq!(signature(&sig), abieos("signature", &bin));
        }
        assert!(signature(&Signature::K1(&compact)).starts_with("SIG_K1_"));
        assert!(signature(&Signature::R1(&compact)).starts_with("SIG_R1_"));

        let auth_data = [0x49u8; 37];
        let client_json =
            br#"{"type":"webauthn.get","challenge":"x","origin":"https://example.com"}"#;
        let sig = Signature::WebAuthn {
            compact: &compact,
            auth_data: &auth_data,
            client_json,
        };
        let mut bin = vec![2];
        bin.extend_from_slice(&compact);
        bytes_into(&mut bin, &auth_data);
        bytes_into(&mut bin, client_json);
        let text = signature(&sig);
        assert!(text.starts_with("SIG_WA_"));
        assert_eq!(text, abieos("signature", &bin));
    }
}
