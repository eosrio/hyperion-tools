//! One binary walk of a Wire `signed_block` from the block log.
//!
//! Hyperion 4.5 block documents need each block's header fields, its id and its receipts' CPU
//! vectors, and action documents need each transaction's id to find its signatures. All of these
//! come from the block-log entry. The walk follows the Wire SHiP ABI's `signed_block` (embedded
//! `wire_ship_abi.json`) with the Wire codec's crypto layouts and canonical varuints, and must
//! consume the entry exactly:
//!
//! ```text
//! signed_block        = block_header producer_signatures:signature[]
//!                       transactions:transaction_receipt[] qc:qc_t? block_extensions:extension[]
//! block_header        = timestamp:u32 producer:name previous:checksum256
//!                       transaction_mroot:checksum256 finality_mroot:checksum256
//!                       qc_claim:{block_num:u32 is_strong_qc:bool}
//!                       new_finalizer_policy_diff:finalizer_policy_diff?
//!                       new_proposer_policy_diff:proposer_policy_diff? header_extensions:extension[]
//! transaction_receipt = cpu_usage_us:varuint32[] trx:packed_transaction
//! packed_transaction  = signatures:signature[] compression:u8 packed_context_free_data:bytes
//!                       packed_trx:bytes
//! qc_t                = block_num:u32 active_policy_sig:qc_sig_t pending_policy_sig:qc_sig_t?
//! qc_sig_t            = strong_votes:bitset? weak_votes:bitset? sig:bytes
//! ```
//!
//! A block id is the sha256 of the packed `block_header` with its first four bytes replaced by the
//! big-endian block number. A transaction id is the sha256 of the serialized transaction:
//! `packed_trx`, inflated first when it is zlib-compressed.

use std::io::Read;

use sha2::{Digest, Sha256};

/// The Wire codec's element bound.
const MAX_ELEMENTS: usize = 100_000;
/// The Wire codec's bound on one crypto frame.
const MAX_CRYPTO_FRAME: usize = 4096;
/// Bound on an inflated transaction. The codec bounds a whole value at 16 MiB.
const MAX_TRANSACTION_BYTES: u64 = 16 << 20;

/// One transaction receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Per-input-action CPU, as Wire receipts carry it.
    pub cpu_usage_us: Vec<u32>,
    /// The transaction id: sha256 of the serialized transaction.
    pub trx_id: [u8; 32],
}

/// The parts of a block that block and action documents use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// `block_timestamp_type` slot.
    pub slot: u32,
    pub producer: u64,
    pub previous: [u8; 32],
    /// Derived from the header bytes and the block number.
    pub id: [u8; 32],
    pub receipts: Vec<Receipt>,
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

type Walk<T> = Result<T, String>;

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Walk<&'a [u8]> {
        let end = self
            .p
            .checked_add(n)
            .filter(|end| *end <= self.b.len())
            .ok_or_else(|| format!("truncated: {n} bytes needed at offset {}", self.p))?;
        let slice = &self.b[self.p..end];
        self.p = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Walk<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Walk<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Walk<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn id(&mut self) -> Walk<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    /// A canonical varuint32, as the Wire codec reads it.
    fn varuint(&mut self) -> Walk<u32> {
        let mut value = 0u32;
        for n in 0..5 {
            let byte = self.u8()?;
            if n == 4 && byte > 15 {
                return Err(format!("overflowing varuint32 at offset {}", self.p - 1));
            }
            value |= u32::from(byte & 127) << (7 * n);
            if byte < 128 {
                if n > 0 && byte == 0 {
                    return Err(format!("noncanonical varuint32 at offset {}", self.p - 1));
                }
                return Ok(value);
            }
        }
        Err(format!("overflowing varuint32 at offset {}", self.p))
    }
    fn count(&mut self) -> Walk<usize> {
        let count = self.varuint()? as usize;
        if count > MAX_ELEMENTS {
            return Err(format!("{count} elements exceed the codec bound"));
        }
        Ok(count)
    }
    fn blob(&mut self) -> Walk<&'a [u8]> {
        let len = self.varuint()? as usize;
        self.take(len)
    }
    fn optional(&mut self) -> Walk<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(format!(
                "invalid optional presence {b:#04x} at offset {}",
                self.p - 1
            )),
        }
    }
    /// A Wire `public_key` (`public == true`) or `signature`: K1, R1, WA, EM, ED or BLS.
    fn crypto(&mut self, public: bool) -> Walk<()> {
        let tag = self.varuint()?;
        let start = self.p;
        match (tag, public) {
            (0 | 1 | 3, true) => {
                self.take(33)?;
            }
            (0 | 1 | 3, false) => {
                self.take(65)?;
            }
            (2, true) => {
                self.take(33)?;
                self.u8()?; // user presence
                if self.blob()?.is_empty() {
                    return Err("empty WebAuthn rpid".into());
                }
            }
            (2, false) => {
                self.take(65)?;
                self.blob()?; // auth_data
                self.blob()?; // client_json
            }
            (4, true) => {
                self.take(32)?;
            }
            (4, false) => {
                self.take(96)?;
            }
            (5, _) => {
                // The ABI variant mirrors the BLS shim's shared pointer: a presence byte, then
                // the fixed-size key or signature.
                if self.u8()? != 1 {
                    return Err("absent BLS pointer".into());
                }
                self.take(if public { 96 } else { 192 })?;
            }
            _ => return Err(format!("unknown crypto variant {tag}")),
        }
        if self.p - start > MAX_CRYPTO_FRAME {
            return Err("crypto frame exceeds its bound".into());
        }
        Ok(())
    }
    fn extensions(&mut self) -> Walk<()> {
        for _ in 0..self.count()? {
            self.take(2)?;
            self.blob()?;
        }
        Ok(())
    }
    /// A `bitset`: its bit count, then that many bits in whole bytes.
    fn bitset(&mut self) -> Walk<()> {
        let bits = self.count()?;
        self.take(bits.div_ceil(8))?;
        Ok(())
    }
    fn qc_sig(&mut self) -> Walk<()> {
        if self.optional()? {
            self.bitset()?; // strong_votes
        }
        if self.optional()? {
            self.bitset()?; // weak_votes
        }
        self.blob()?; // sig
        Ok(())
    }
}

/// The serialized transaction behind `packed_trx`.
fn transaction_id(compression: u8, packed: &[u8]) -> Walk<[u8; 32]> {
    match compression {
        0 => Ok(Sha256::digest(packed).into()),
        1 => {
            let mut inflated = Vec::new();
            flate2::read::ZlibDecoder::new(packed)
                .take(MAX_TRANSACTION_BYTES + 1)
                .read_to_end(&mut inflated)
                .map_err(|e| format!("invalid zlib transaction: {e}"))?;
            if inflated.len() as u64 > MAX_TRANSACTION_BYTES {
                return Err("inflated transaction exceeds its bound".into());
            }
            Ok(Sha256::digest(&inflated).into())
        }
        other => Err(format!("unknown transaction compression {other}")),
    }
}

/// The id a header derives: its sha256 with the block number in the first four bytes.
pub fn block_id(header: &[u8], block_num: u32) -> [u8; 32] {
    let mut id: [u8; 32] = Sha256::digest(header).into();
    id[..4].copy_from_slice(&block_num.to_be_bytes());
    id
}

/// Walk block `block_num`'s `signed_block` bytes (a block-log entry less its position trailer).
pub fn parse(bytes: &[u8], block_num: u32) -> Walk<Block> {
    let mut c = Cursor { b: bytes, p: 0 };
    let slot = c.u32()?;
    let producer = c.u64()?;
    let previous = c.id()?;
    c.take(32 + 32)?; // transaction_mroot, finality_mroot
    c.take(4 + 1)?; // qc_claim: block_num, and is_strong_qc, which the codec reads as any byte
    if c.optional()? {
        // new_finalizer_policy_diff
        c.take(4 + 8)?; // generation, threshold
        let removed = c.count()?;
        c.take(2 * removed)?; // remove_indexes
        for _ in 0..c.count()? {
            c.take(2)?; // index
            c.blob()?; // description
            c.take(8)?; // weight
            c.blob()?; // public_key bytes
        }
    }
    if c.optional()? {
        // new_proposer_policy_diff
        c.take(4 + 4)?; // version, proposal_time
        let removed = c.count()?;
        c.take(2 * removed)?; // remove_indexes
        for _ in 0..c.count()? {
            c.take(2 + 8)?; // index, producer_name
            if c.varuint()? != 0 {
                return Err("unknown block_signing_authority variant".into());
            }
            c.take(4)?; // threshold
            for _ in 0..c.count()? {
                c.crypto(true)?;
                c.take(2)?; // weight
            }
        }
    }
    c.extensions()?; // header_extensions
    let id = block_id(&bytes[..c.p], block_num);
    for _ in 0..c.count()? {
        c.crypto(false)?; // producer_signatures
    }
    let count = c.count()?;
    let mut receipts = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let cpu = c.count()?;
        let mut cpu_usage_us = Vec::with_capacity(cpu.min(256));
        for _ in 0..cpu {
            cpu_usage_us.push(c.varuint()?);
        }
        for _ in 0..c.count()? {
            c.crypto(false)?;
        }
        let compression = c.u8()?;
        c.blob()?; // packed_context_free_data
        let trx_id = transaction_id(compression, c.blob()?)?;
        receipts.push(Receipt {
            cpu_usage_us,
            trx_id,
        });
    }
    if c.optional()? {
        c.take(4)?; // qc.block_num
        c.qc_sig()?; // active_policy_sig
        if c.optional()? {
            c.qc_sig()?; // pending_policy_sig
        }
    }
    c.extensions()?; // block_extensions
    if c.p != bytes.len() {
        return Err(format!(
            "{} bytes follow the signed_block",
            bytes.len() - c.p
        ));
    }
    Ok(Block {
        slot,
        producer,
        previous,
        id,
        receipts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::Write;

    fn testdata(path: &str) -> Value {
        let file = format!("{}/../core/testdata/{path}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap()
    }

    fn profile() -> hyperion_wire_protocol::codec::CodecProfile {
        hyperion_wire_protocol::codec::CodecProfile::for_id("wire-sysio-6508c9e8-engineering-v1")
            .unwrap()
    }

    fn ship_abi() -> Value {
        serde_json::from_str(include_str!("wire_ship_abi.json")).unwrap()
    }

    /// Every crypto variant's packed form, from the Wire codec's own vectors.
    fn crypto(kind: &str) -> Vec<Vec<u8>> {
        let vectors = testdata("wire/crypto-vectors.json");
        let picked: Vec<Vec<u8>> = vectors["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["operation"] == "decodeValue" && v["typeName"] == kind)
            .filter(|v| v.get("error").is_none() && v.get("expectedError").is_none())
            .map(|v| hex::decode(v["hex"].as_str().unwrap()).unwrap())
            .collect();
        assert!(picked.len() >= 6, "{kind} vectors: {}", picked.len());
        picked
    }

    fn varuint(out: &mut Vec<u8>, mut v: u32) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
        varuint(out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// Each receipt's CPU vector and serialized transaction.
    type Receipts = Vec<(Vec<u32>, Vec<u8>)>;

    /// A block with both policy diffs (every key variant), header extensions, every signature
    /// variant, a plain and a compressed transaction, and a qc with both signatures. Returns the
    /// block, its header length and its receipts.
    fn synthetic() -> (Vec<u8>, usize, Receipts) {
        let keys = crypto("public_key");
        let sigs = crypto("signature");
        let mut b = Vec::new();
        b.extend_from_slice(&1_234_567u32.to_le_bytes());
        b.extend_from_slice(&0x5530_ea00_0000_0000u64.to_le_bytes());
        b.extend_from_slice(&[0x11; 32]);
        b.extend_from_slice(&[0x22; 64]);
        b.extend_from_slice(&41u32.to_le_bytes());
        b.push(1);
        b.push(1); // new_finalizer_policy_diff
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&7u64.to_le_bytes());
        varuint(&mut b, 2);
        b.extend_from_slice(&[0, 0, 1, 0]);
        varuint(&mut b, 1);
        b.extend_from_slice(&[2, 0]);
        blob(&mut b, b"finalizer-1");
        b.extend_from_slice(&5u64.to_le_bytes());
        blob(&mut b, &[0x33; 96]);
        b.push(1); // new_proposer_policy_diff
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&1_234_000u32.to_le_bytes());
        varuint(&mut b, 1);
        b.extend_from_slice(&[4, 0]);
        varuint(&mut b, 2);
        for (i, chunk) in keys.chunks(keys.len().div_ceil(2)).enumerate() {
            b.extend_from_slice(&(i as u16).to_le_bytes());
            b.extend_from_slice(&(0x1000 + i as u64).to_le_bytes());
            varuint(&mut b, 0); // block_signing_authority_v0
            b.extend_from_slice(&1u32.to_le_bytes());
            varuint(&mut b, chunk.len() as u32);
            for key in chunk {
                b.extend_from_slice(key);
                b.extend_from_slice(&1u16.to_le_bytes());
            }
        }
        varuint(&mut b, 1); // header_extensions
        b.extend_from_slice(&2u16.to_le_bytes());
        blob(&mut b, &[1, 2, 3]);
        let header = b.len();
        varuint(&mut b, sigs.len() as u32);
        for sig in &sigs {
            b.extend_from_slice(sig);
        }
        let plain = b"a serialized transaction".to_vec();
        let deflated = vec![0x5a; 300];
        let expected = vec![
            (vec![23, 456], plain.clone()),
            (vec![200_000], deflated.clone()),
        ];
        varuint(&mut b, 2);
        varuint(&mut b, 2);
        varuint(&mut b, 23);
        varuint(&mut b, 456);
        varuint(&mut b, 1);
        b.extend_from_slice(&sigs[0]);
        b.push(0);
        blob(&mut b, &[]);
        blob(&mut b, &plain);
        varuint(&mut b, 1);
        varuint(&mut b, 200_000);
        varuint(&mut b, 0);
        b.push(1);
        blob(&mut b, &[9, 9]);
        blob(&mut b, &zlib(&deflated));
        b.push(1); // qc
        b.extend_from_slice(&41u32.to_le_bytes());
        b.push(1);
        varuint(&mut b, 21);
        b.extend_from_slice(&[0xff, 0xff, 0x1f]);
        b.push(0);
        blob(&mut b, &[0x44; 192]);
        b.push(1);
        b.push(0);
        b.push(1);
        varuint(&mut b, 3);
        b.push(0b101);
        blob(&mut b, &[0x55; 192]);
        varuint(&mut b, 1); // block_extensions
        b.extend_from_slice(&3u16.to_le_bytes());
        blob(&mut b, &[7; 5]);
        (b, header, expected)
    }

    #[test]
    fn synthetic_block_matches_the_codec_and_its_own_layout() {
        let (bytes, header, expected) = synthetic();
        let decoded = hyperion_wire_protocol::codec::decode_value(
            &ship_abi(),
            "signed_block",
            &bytes,
            &profile(),
        )
        .expect("the Wire codec accepts the synthetic block");
        let block = parse(&bytes, 0x0012_d687).unwrap();
        assert_eq!(block.id, block_id(&bytes[..header], 0x0012_d687));
        assert_eq!(&block.id[..4], &0x0012_d687u32.to_be_bytes());
        assert_eq!(block.slot, 1_234_567);
        assert_eq!(
            hyperion_ship::trace::name_to_string(block.producer),
            decoded["producer"]
        );
        assert_eq!(hex::encode(block.previous), decoded["previous"]);
        let receipts = decoded["transactions"].as_array().unwrap();
        assert_eq!(block.receipts.len(), receipts.len());
        for ((walked, value), (cpu, serialized)) in
            block.receipts.iter().zip(receipts).zip(&expected)
        {
            assert_eq!(&walked.cpu_usage_us, cpu);
            assert_eq!(value["cpu_usage_us"], serde_json::json!(cpu));
            let id: [u8; 32] = Sha256::digest(serialized).into();
            assert_eq!(walked.trx_id, id);
        }
    }

    #[test]
    fn captured_receipt_block_walks_like_the_codec_decodes_it() {
        let fixture = testdata("wire-host-receipts.json");
        let bytes = hex::decode(fixture["blockHex"].as_str().unwrap()).unwrap();
        let block = parse(&bytes, 0x18ce).unwrap();
        let expected = &fixture["expected"]["block"];
        assert_eq!(hex::encode(block.previous), expected["previous"]);
        assert_eq!(
            hyperion_ship::trace::name_to_string(block.producer),
            expected["producer"]
        );
        let receipts = expected["transactions"].as_array().unwrap();
        assert_eq!(block.receipts.len(), receipts.len());
        assert_eq!(block.receipts[0].cpu_usage_us, vec![23, 456]);
        let packed = hex::decode(receipts[0]["trx"]["packed_trx"].as_str().unwrap()).unwrap();
        let id: [u8; 32] = Sha256::digest(&packed).into();
        assert_eq!(block.receipts[0].trx_id, id);
    }

    #[test]
    fn damaged_blocks_are_refused() {
        let (bytes, header, _) = synthetic();
        // Every strict prefix is truncated somewhere.
        for cut in [1, header - 1, header, bytes.len() - 1] {
            assert!(parse(&bytes[..cut], 1).is_err(), "prefix {cut}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(parse(&trailing, 1).unwrap_err().contains("follow"));
        // An optional presence byte other than 0 or 1.
        let mut presence = bytes.clone();
        presence[4 + 8 + 32 + 64 + 5] = 2;
        assert!(parse(&presence, 1).unwrap_err().contains("presence"));
        // A noncanonical varuint where the header extension count sits.
        let (mut b, _, _) = synthetic();
        let mut tail = b.split_off(header - 7);
        assert_eq!(tail[0], 1, "header extension count");
        tail.splice(0..1, [0x81, 0x00]);
        b.extend_from_slice(&tail);
        assert!(parse(&b, 1).unwrap_err().contains("noncanonical"));
    }

    #[test]
    fn unknown_crypto_and_compression_are_refused() {
        let mut c = Cursor {
            b: &[6, 0, 0],
            p: 0,
        };
        assert!(c.crypto(false).unwrap_err().contains("unknown crypto"));
        let mut c = Cursor { b: &[5, 0], p: 0 };
        assert!(c.crypto(true).unwrap_err().contains("BLS"));
        assert!(transaction_id(2, b"x").unwrap_err().contains("compression"));
        assert!(transaction_id(1, b"not zlib").unwrap_err().contains("zlib"));
    }
}
