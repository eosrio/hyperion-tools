//! The block endpoint bodies, as nodeos's reflection writes them (Spring `chain_plugin.cpp`
//! `get_raw_block`, `get_block_header`, `get_block_info`; `block.hpp`, `block_header.hpp`,
//! `transaction.hpp`, `producer_schedule.hpp`). Members come in reflection order, absent
//! optionals are left out, a `std::variant` is `[index, value]` and a `std::pair` is
//! `[first, second]`. `SBH` is the reflection of `signed_block_header`:
//!
//! ```text
//! "timestamp":T,"producer":N,"confirmed":C,"previous":ID,"transaction_mroot":H,
//! "action_mroot":H,"schedule_version":V,
//! ["new_producers":{"version":V,"producers":[{"producer_name":N,"block_signing_key":K},...]},]
//! "header_extensions":[[k,"hex"],...],"producer_signature":S
//! ```
//!
//!   * `get_block_info`: `{"block_num","ref_block_num","id"`, the header fields without
//!     `new_producers` and the extensions, `"producer_signature","ref_block_prefix"}`.
//!   * `get_block_header`: `{"id":ID,"signed_block_header":{SBH}}`, and with
//!     `include_extensions` also `"block_extensions":[...]` (present when empty).
//!   * `get_raw_block`: `{SBH,"transactions":[...],"block_extensions":[...]}`, each receipt
//!     `{"status","cpu_usage_us","net_usage_words","trx":[0,ID] | [1,{"signatures",
//!     "compression","packed_context_free_data","packed_trx"}]}` with the bytes as stored.

use anyhow::{Result, ensure};

use super::Endpoint;
use super::block::{Extension, List, Receipt, ReceiptTrx, SignedBlock};
use super::fcjson::{write_hex, write_str, write_u64};
use super::text::{block_timestamp, name, public_key, signature};

/// Append `endpoint`'s body for block `b` to `out`. `include_extensions` is
/// `get_block_header`'s parameter and is ignored by the other endpoints. The body may take at
/// most `limit` bytes: past that, rendering stops with an error and `out` is left as it was.
/// Either way `out` never grows by more than `limit` bytes, so with that much spare capacity
/// it is never reallocated.
pub fn render(
    endpoint: Endpoint,
    b: &SignedBlock<'_>,
    include_extensions: bool,
    limit: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    let start = out.len();
    let mut w = Writer {
        out,
        start,
        limit,
        over: false,
        staged: Vec::new(),
    };
    let done = match endpoint {
        Endpoint::GetBlockInfo => block_info(&mut w, b),
        Endpoint::GetBlockHeader => block_header(&mut w, b, include_extensions),
        Endpoint::GetRawBlock => raw_block(&mut w, b),
    }
    .and_then(|()| w.check());
    if done.is_err() {
        w.out.truncate(start);
    }
    done
}

/// `get_supported_apis`'s body for `endpoints`: `{"apis":[...]}`, the paths sorted by bytes.
pub fn supported_apis(endpoints: &[Endpoint], out: &mut Vec<u8>) {
    let mut paths: Vec<_> = endpoints.iter().map(|ep| ep.path()).collect();
    paths.sort_unstable();
    paths.dedup();
    out.extend_from_slice(b"{\"apis\":[");
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_str(out, path.as_bytes());
    }
    out.extend_from_slice(b"]}");
}

/// The output and its budget. Each write is measured before it is made, and one that would
/// take the body past its limit is not made, nor any after it: the body never outgrows its
/// limit, and the next [`Writer::check`] fails.
struct Writer<'o> {
    out: &'o mut Vec<u8>,
    start: usize,
    limit: usize,
    /// A write did not fit.
    over: bool,
    /// A short write (a key, a number, a text) is made here first, to be measured.
    staged: Vec<u8>,
}

impl Writer<'_> {
    /// Whether `n` more bytes fit in the limit; once a write does not, none does.
    fn fits(&mut self, n: usize) -> bool {
        self.over = self.over || self.out.len() - self.start + n > self.limit;
        !self.over
    }

    fn raw(&mut self, s: &[u8]) {
        if self.fits(s.len()) {
            self.out.extend_from_slice(s);
        }
    }

    /// What `write` writes, written if it fits.
    fn staged(&mut self, write: impl FnOnce(&mut Vec<u8>)) {
        let mut staged = std::mem::take(&mut self.staged);
        staged.clear();
        write(&mut staged);
        self.raw(&staged);
        self.staged = staged;
    }

    /// `"key":`, the first member of an object.
    fn first_key(&mut self, key: &str) {
        self.staged(|s| {
            write_str(s, key.as_bytes());
            s.push(b':');
        });
    }

    /// `,"key":`, any later member.
    fn key(&mut self, key: &str) {
        self.raw(b",");
        self.first_key(key);
    }

    fn num(&mut self, v: impl Into<u64>) {
        let v = v.into();
        self.staged(|s| write_u64(s, v));
    }

    fn text(&mut self, t: &str) {
        self.staged(|s| write_str(s, t.as_bytes()));
    }

    /// Hex is written in place: its length is known, two digits a byte between quotes.
    fn hex(&mut self, b: &[u8]) {
        if self.fits(2 + 2 * b.len()) {
            write_hex(self.out, b);
        }
    }

    /// Called after each element of an unbounded list, so a body stops near its limit.
    fn check(&self) -> Result<()> {
        ensure!(
            !self.over,
            "the rendered body passed its limit of {} bytes",
            self.limit
        );
        Ok(())
    }
}

fn block_info(w: &mut Writer, b: &SignedBlock<'_>) -> Result<()> {
    w.raw(b"{");
    w.first_key("block_num");
    w.num(b.number);
    w.key("ref_block_num");
    w.num(b.number & 0xffff);
    w.key("id");
    w.hex(&b.id);
    w.key("timestamp");
    w.text(&block_timestamp(b.timestamp));
    w.key("producer");
    w.text(&name(b.producer));
    w.key("confirmed");
    w.num(b.confirmed);
    w.key("previous");
    w.hex(b.previous);
    w.key("transaction_mroot");
    w.hex(b.transaction_mroot);
    w.key("action_mroot");
    w.hex(b.action_mroot);
    w.key("schedule_version");
    w.num(b.schedule_version);
    w.key("producer_signature");
    w.text(&signature(&b.producer_signature));
    w.key("ref_block_prefix");
    w.num(ref_block_prefix(&b.id));
    w.raw(b"}");
    Ok(())
}

fn block_header(w: &mut Writer, b: &SignedBlock<'_>, include_extensions: bool) -> Result<()> {
    w.raw(b"{");
    w.first_key("id");
    w.hex(&b.id);
    w.key("signed_block_header");
    w.raw(b"{");
    signed_block_header(w, b)?;
    w.raw(b"}");
    if include_extensions {
        w.key("block_extensions");
        extensions(w, &b.block_extensions)?;
    }
    w.raw(b"}");
    Ok(())
}

fn raw_block(w: &mut Writer, b: &SignedBlock<'_>) -> Result<()> {
    w.raw(b"{");
    signed_block_header(w, b)?;
    w.key("transactions");
    w.raw(b"[");
    for (i, r) in b.transactions.iter().enumerate() {
        if i > 0 {
            w.raw(b",");
        }
        receipt(w, &r?)?;
        w.check()?;
    }
    w.raw(b"]");
    w.key("block_extensions");
    extensions(w, &b.block_extensions)?;
    w.raw(b"}");
    Ok(())
}

/// The members of `signed_block_header`, without braces.
fn signed_block_header(w: &mut Writer, b: &SignedBlock<'_>) -> Result<()> {
    w.first_key("timestamp");
    w.text(&block_timestamp(b.timestamp));
    w.key("producer");
    w.text(&name(b.producer));
    w.key("confirmed");
    w.num(b.confirmed);
    w.key("previous");
    w.hex(b.previous);
    w.key("transaction_mroot");
    w.hex(b.transaction_mroot);
    w.key("action_mroot");
    w.hex(b.action_mroot);
    w.key("schedule_version");
    w.num(b.schedule_version);
    if let Some(schedule) = &b.new_producers {
        w.key("new_producers");
        w.raw(b"{");
        w.first_key("version");
        w.num(schedule.version);
        w.key("producers");
        w.raw(b"[");
        for (i, p) in schedule.producers.iter().enumerate() {
            let p = p?;
            if i > 0 {
                w.raw(b",");
            }
            w.raw(b"{");
            w.first_key("producer_name");
            w.text(&name(p.producer_name));
            w.key("block_signing_key");
            w.text(&public_key(&p.block_signing_key));
            w.raw(b"}");
            w.check()?;
        }
        w.raw(b"]}");
    }
    w.key("header_extensions");
    extensions(w, &b.header_extensions)?;
    w.key("producer_signature");
    w.text(&signature(&b.producer_signature));
    Ok(())
}

/// `extensions_type`: `[[type,"hex"],...]`.
fn extensions(w: &mut Writer, list: &List<'_, Extension<'_>>) -> Result<()> {
    w.raw(b"[");
    for (i, e) in list.iter().enumerate() {
        let e = e?;
        if i > 0 {
            w.raw(b",");
        }
        w.raw(b"[");
        w.num(e.kind);
        w.raw(b",");
        w.hex(e.data);
        w.raw(b"]");
        w.check()?;
    }
    w.raw(b"]");
    Ok(())
}

fn receipt(w: &mut Writer, r: &Receipt<'_>) -> Result<()> {
    w.raw(b"{");
    w.first_key("status");
    w.text(r.status.name());
    w.key("cpu_usage_us");
    w.num(r.cpu_usage_us);
    w.key("net_usage_words");
    w.num(r.net_usage_words);
    w.key("trx");
    match &r.trx {
        ReceiptTrx::Id(id) => {
            w.raw(b"[0,");
            w.hex(*id);
        }
        ReceiptTrx::Packed(p) => {
            w.raw(b"[1,{");
            w.first_key("signatures");
            w.raw(b"[");
            for (i, s) in p.signatures.iter().enumerate() {
                if i > 0 {
                    w.raw(b",");
                }
                w.text(&signature(&s?));
            }
            w.raw(b"]");
            w.key("compression");
            w.text(p.compression.name());
            w.key("packed_context_free_data");
            w.hex(p.packed_context_free_data);
            w.key("packed_trx");
            w.hex(p.packed_trx);
            w.raw(b"}");
        }
    }
    w.raw(b"]}");
    Ok(())
}

/// The transaction header's TaPoS prefix: the id's second 64-bit word, cut to 32 bits
/// (`id._hash[1]`), which is bytes 8 to 12 read little-endian.
fn ref_block_prefix(id: &[u8; 32]) -> u32 {
    u32::from_le_bytes([id[8], id[9], id[10], id[11]])
}

/// The fixtures come from the testkit writers.
#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::*;
    use crate::chainapi::block::{PublicKey, Signature, parse_signed_block};
    use crate::testkit::blocks::{
        self, Block, BlockSpec, Key, Receipt as R, ReceiptTrx as T, Signature as S, synthetic_id,
    };

    const EOSIO_TOKEN: u64 = 0x5530_ea03_3482_a600;

    fn body(ep: Endpoint, block: &[u8], include_extensions: bool) -> String {
        let b = parse_signed_block(block).unwrap();
        let mut out = Vec::new();
        render(ep, &b, include_extensions, usize::MAX, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Block 70000 with every field this module renders.
    fn rich_block() -> Block {
        let id_receipt = |status, cpu, net, byte| R {
            status,
            cpu_usage_us: cpu,
            net_usage_words: net,
            trx: T::Id([byte; 32]),
        };
        let spec = BlockSpec {
            timestamp: 3,
            producer: blocks::EOSIO,
            confirmed: 240,
            transaction_mroot: [0x11; 32],
            action_mroot: [0x22; 32],
            schedule_version: 7,
            new_producers: Some((
                8,
                vec![
                    (blocks::EOSIO, Key::K1([0x02; 33])),
                    (EOSIO_TOKEN, Key::R1([0x03; 33])),
                    (
                        0,
                        Key::WebAuthn {
                            point: [0x02; 33],
                            user_presence: 1,
                            rpid: "example.com".into(),
                        },
                    ),
                ],
            )),
            header_extensions: vec![(1, vec![0xab, 0x01, 0x00])],
            block_extensions: vec![(2, vec![0xcd, 0xef]), (3, vec![])],
            producer_signature: S::K1([0x05; 65]),
            transactions: vec![
                id_receipt(0, 1, 2, 0x33),
                R {
                    status: 1,
                    cpu_usage_us: 3,
                    net_usage_words: 4,
                    trx: T::Packed {
                        signatures: vec![S::K1([0x06; 65]), S::R1([0x07; 65])],
                        compression: 0,
                        packed_context_free_data: vec![],
                        packed_trx: vec![0xde, 0xad],
                    },
                },
                R {
                    status: 2,
                    cpu_usage_us: 4_294_967_295,
                    net_usage_words: 200_000,
                    trx: T::Packed {
                        signatures: vec![],
                        compression: 1,
                        packed_context_free_data: vec![0x78, 0x9c, 0x03, 0x00],
                        packed_trx: vec![0x78, 0x9c, 0x01],
                    },
                },
                id_receipt(3, 0, 0, 0x44),
                id_receipt(4, 9, 9, 0x55),
            ],
        };
        Block::new(&synthetic_id(69_999), &spec).unwrap()
    }

    fn k1_sig(byte: u8) -> String {
        signature(&Signature::K1(&[byte; 65]))
    }

    /// The `signed_block_header` members of [`rich_block`], written out by hand. Key and
    /// signature text comes from `text`, which its own tests check against other decoders.
    fn rich_header(block: &Block) -> String {
        let keys = [
            public_key(&PublicKey::K1(&[0x02; 33])),
            public_key(&PublicKey::R1(&[0x03; 33])),
            public_key(&PublicKey::WebAuthn {
                point: &[0x02; 33],
                user_presence: 1,
                rpid: b"example.com",
            }),
        ];
        assert!(keys[0].starts_with("EOS") && keys[2].starts_with("PUB_WA_"));
        format!(
            concat!(
                r#""timestamp":"2000-01-01T00:00:01.500","producer":"eosio","confirmed":240,"#,
                r#""previous":"{prev}","transaction_mroot":"{m1}","action_mroot":"{m2}","#,
                r#""schedule_version":7,"new_producers":{{"version":8,"producers":["#,
                r#"{{"producer_name":"eosio","block_signing_key":"{k0}"}},"#,
                r#"{{"producer_name":"eosio.token","block_signing_key":"{k1}"}},"#,
                r#"{{"producer_name":"","block_signing_key":"{k2}"}}]}},"#,
                r#""header_extensions":[[1,"ab0100"]],"producer_signature":"{sig}""#
            ),
            prev = hex::encode(block.previous),
            m1 = "11".repeat(32),
            m2 = "22".repeat(32),
            k0 = keys[0],
            k1 = keys[1],
            k2 = keys[2],
            sig = k1_sig(0x05),
        )
    }

    #[test]
    fn get_raw_block_renders_every_receipt_form() {
        let block = rich_block();
        assert_eq!(block.num, 70_000);
        let receipts = format!(
            concat!(
                r#"{{"status":"executed","cpu_usage_us":1,"net_usage_words":2,"#,
                r#""trx":[0,"{id3}"]}},"#,
                r#"{{"status":"soft_fail","cpu_usage_us":3,"net_usage_words":4,"trx":[1,{{"#,
                r#""signatures":["{s6}","{s7}"],"compression":"none","#,
                r#""packed_context_free_data":"","packed_trx":"dead"}}]}},"#,
                r#"{{"status":"hard_fail","cpu_usage_us":4294967295,"net_usage_words":200000,"#,
                r#""trx":[1,{{"signatures":[],"compression":"zlib","#,
                r#""packed_context_free_data":"789c0300","packed_trx":"789c01"}}]}},"#,
                r#"{{"status":"delayed","cpu_usage_us":0,"net_usage_words":0,"#,
                r#""trx":[0,"{id4}"]}},"#,
                r#"{{"status":"expired","cpu_usage_us":9,"net_usage_words":9,"#,
                r#""trx":[0,"{id5}"]}}"#
            ),
            id3 = "33".repeat(32),
            id4 = "44".repeat(32),
            id5 = "55".repeat(32),
            s6 = k1_sig(0x06),
            s7 = signature(&Signature::R1(&[0x07; 65])),
        );
        let want = format!(
            r#"{{{},"transactions":[{receipts}],"block_extensions":[[2,"cdef"],[3,""]]}}"#,
            rich_header(&block)
        );
        assert_eq!(body(Endpoint::GetRawBlock, &block.body, false), want);
        // The parameter belongs to get_block_header only.
        assert_eq!(body(Endpoint::GetRawBlock, &block.body, true), want);
    }

    #[test]
    fn get_block_header_renders_both_modes() {
        let block = rich_block();
        let id = hex::encode(block.id);
        let header = rich_header(&block);
        assert_eq!(
            body(Endpoint::GetBlockHeader, &block.body, false),
            format!(r#"{{"id":"{id}","signed_block_header":{{{header}}}}}"#)
        );
        assert_eq!(
            body(Endpoint::GetBlockHeader, &block.body, true),
            format!(
                r#"{{"id":"{id}","signed_block_header":{{{header}}},"block_extensions":[[2,"cdef"],[3,""]]}}"#
            )
        );
        // No new_producers and no extensions: the optional is left out, the lists are empty
        // and block_extensions is still there.
        let plain = Block::new(&synthetic_id(1), &BlockSpec::default()).unwrap();
        let want = format!(
            concat!(
                r#"{{"id":"{id}","signed_block_header":{{"timestamp":"2000-01-01T00:00:00.000","#,
                r#""producer":"","confirmed":0,"previous":"{prev}","transaction_mroot":"{z}","#,
                r#""action_mroot":"{z}","schedule_version":0,"header_extensions":[],"#,
                r#""producer_signature":"{sig}"}},"block_extensions":[]}}"#
            ),
            id = hex::encode(plain.id),
            prev = hex::encode(plain.previous),
            z = "00".repeat(32),
            sig = k1_sig(0),
        );
        assert_eq!(body(Endpoint::GetBlockHeader, &plain.body, true), want);
    }

    #[test]
    fn get_block_info_renders_its_own_member_list() {
        let block = rich_block();
        let prefix = u32::from_le_bytes(block.id[8..12].try_into().unwrap());
        let want = format!(
            concat!(
                r#"{{"block_num":70000,"ref_block_num":4464,"id":"{id}","#,
                r#""timestamp":"2000-01-01T00:00:01.500","producer":"eosio","confirmed":240,"#,
                r#""previous":"{prev}","transaction_mroot":"{m1}","action_mroot":"{m2}","#,
                r#""schedule_version":7,"producer_signature":"{sig}","#,
                r#""ref_block_prefix":{prefix}}}"#
            ),
            id = hex::encode(block.id),
            prev = hex::encode(block.previous),
            m1 = "11".repeat(32),
            m2 = "22".repeat(32),
            sig = k1_sig(0x05),
            prefix = prefix,
        );
        assert_eq!(body(Endpoint::GetBlockInfo, &block.body, false), want);
        assert_eq!(body(Endpoint::GetBlockInfo, &block.body, true), want);
    }

    #[test]
    fn webauthn_producer_signatures_render() {
        let spec = BlockSpec {
            producer_signature: S::WebAuthn {
                compact: [0x09; 65],
                auth_data: vec![1; 37],
                client_json: "{\"type\":\"webauthn.get\"}".into(),
            },
            ..BlockSpec::default()
        };
        let block = Block::new(&synthetic_id(1), &spec).unwrap();
        let info = body(Endpoint::GetBlockInfo, &block.body, false);
        let sig = signature(&Signature::WebAuthn {
            compact: &[0x09; 65],
            auth_data: &[1; 37],
            client_json: b"{\"type\":\"webauthn.get\"}",
        });
        assert!(sig.starts_with("SIG_WA_"));
        assert!(
            info.contains(&format!(r#""producer_signature":"{sig}""#)),
            "{info}"
        );
    }

    #[test]
    fn rendering_stops_at_its_limit() {
        let block = rich_block();
        let b = parse_signed_block(&block.body).unwrap();
        for ep in Endpoint::ALL {
            let mut full = Vec::new();
            render(ep, &b, true, usize::MAX, &mut full).unwrap();
            let mut out = b"prefix".to_vec();
            render(ep, &b, true, full.len(), &mut out).unwrap();
            assert_eq!(&out[6..], &full[..]);
            let mut out = b"prefix".to_vec();
            let e = render(ep, &b, true, full.len() - 1, &mut out).unwrap_err();
            assert!(e.to_string().contains("limit"), "{e}");
            assert_eq!(out, b"prefix", "{ep:?} leaves the output as it was");
        }
    }

    #[test]
    fn rendering_never_grows_its_output_past_the_limit() {
        // A buffer with exactly the limit to spare is never reallocated, whether the body fits
        // or a write in the middle of a long field would pass the limit.
        let block = rich_block();
        let b = parse_signed_block(&block.body).unwrap();
        for ep in Endpoint::ALL {
            let mut full = Vec::new();
            render(ep, &b, true, usize::MAX, &mut full).unwrap();
            for limit in (0..=full.len()).step_by(7).chain([full.len()]) {
                let mut out = Vec::with_capacity(3 + limit);
                out.extend_from_slice(b"pre");
                let (at, room) = (out.as_ptr(), out.capacity());
                let done = render(ep, &b, true, limit, &mut out);
                assert_eq!(done.is_ok(), limit == full.len(), "{ep:?} {limit}");
                assert_eq!((out.as_ptr(), out.capacity()), (at, room), "{ep:?} {limit}");
            }
        }
    }

    #[test]
    fn supported_apis_are_sorted_and_unique() {
        let mut out = Vec::new();
        let eps = [
            Endpoint::GetRawBlock,
            Endpoint::GetBlockInfo,
            Endpoint::GetBlockHeader,
            Endpoint::GetRawBlock,
        ];
        supported_apis(&eps, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"{"apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","#,
                r#""/v1/chain/get_raw_block"]}"#
            )
        );
        let mut out = Vec::new();
        supported_apis(&[], &mut out);
        assert_eq!(out, br#"{"apis":[]}"#);
    }

    /// `signed_block` with its transactions, as Spring declares it, for rs_abieos.
    const SIGNED_BLOCK_ABI: &str = r#"{
        "version": "eosio::abi/1.1",
        "types": [], "actions": [], "tables": [], "ricardian_clauses": [],
        "variants": [
            {"name": "transaction_variant", "types": ["checksum256", "packed_transaction"]}],
        "structs": [
            {"name": "extension", "base": "", "fields": [
                {"name": "type", "type": "uint16"}, {"name": "data", "type": "bytes"}]},
            {"name": "producer_key", "base": "", "fields": [
                {"name": "producer_name", "type": "name"},
                {"name": "block_signing_key", "type": "public_key"}]},
            {"name": "producer_schedule", "base": "", "fields": [
                {"name": "version", "type": "uint32"},
                {"name": "producers", "type": "producer_key[]"}]},
            {"name": "packed_transaction", "base": "", "fields": [
                {"name": "signatures", "type": "signature[]"},
                {"name": "compression", "type": "uint8"},
                {"name": "packed_context_free_data", "type": "bytes"},
                {"name": "packed_trx", "type": "bytes"}]},
            {"name": "transaction_receipt", "base": "", "fields": [
                {"name": "status", "type": "uint8"},
                {"name": "cpu_usage_us", "type": "uint32"},
                {"name": "net_usage_words", "type": "varuint32"},
                {"name": "trx", "type": "transaction_variant"}]},
            {"name": "signed_block", "base": "", "fields": [
                {"name": "timestamp", "type": "block_timestamp_type"},
                {"name": "producer", "type": "name"},
                {"name": "confirmed", "type": "uint16"},
                {"name": "previous", "type": "checksum256"},
                {"name": "transaction_mroot", "type": "checksum256"},
                {"name": "action_mroot", "type": "checksum256"},
                {"name": "schedule_version", "type": "uint32"},
                {"name": "new_producers", "type": "producer_schedule?"},
                {"name": "header_extensions", "type": "extension[]"},
                {"name": "producer_signature", "type": "signature"},
                {"name": "transactions", "type": "transaction_receipt[]"},
                {"name": "block_extensions", "type": "extension[]"}]}
        ]
    }"#;

    /// What `get_raw_block` says about `body` (which has no legacy schedule), built from
    /// rs_abieos's decode: names, timestamps and signatures as it writes them, hex lowercased,
    /// extensions as pairs, and variants and enums as nodeos writes them.
    fn raw_block_from_abieos(body: &[u8]) -> serde_json::Value {
        use serde_json::{Value, json};
        let abieos = rs_abieos::Abieos::new();
        abieos.set_abi_json("rendertest", SIGNED_BLOCK_ABI).unwrap();
        let json = abieos
            .bin_to_json("rendertest", "signed_block", body)
            .unwrap();
        let mut v: Value = serde_json::from_str(&json).unwrap();
        let lower = |v: &Value| json!(v.as_str().unwrap().to_ascii_lowercase());
        let pairs = |v: &Value| {
            let list = v.as_array().unwrap().iter();
            Value::Array(
                list.map(|e| json!([e["type"], lower(&e["data"])]))
                    .collect(),
            )
        };
        for k in ["previous", "transaction_mroot", "action_mroot"] {
            v[k] = lower(&v[k]);
        }
        v["header_extensions"] = pairs(&v["header_extensions"]);
        v["block_extensions"] = pairs(&v["block_extensions"]);
        assert!(v["new_producers"].is_null(), "{json}");
        v.as_object_mut().unwrap().remove("new_producers");
        let statuses = ["executed", "soft_fail", "hard_fail", "delayed", "expired"];
        for r in v["transactions"].as_array_mut().unwrap() {
            r["status"] = json!(statuses[r["status"].as_u64().unwrap() as usize]);
            let trx = r["trx"].as_array().unwrap().clone();
            r["trx"] = if trx[0] == "checksum256" {
                json!([0, lower(&trx[1])])
            } else {
                let mut p = trx[1].clone();
                let compression = ["none", "zlib"][p["compression"].as_u64().unwrap() as usize];
                p["compression"] = json!(compression);
                p["packed_context_free_data"] = lower(&p["packed_context_free_data"]);
                p["packed_trx"] = lower(&p["packed_trx"]);
                json!([1, p])
            };
        }
        v
    }

    /// EOS mainnet block 480385480, a Savanna block with transactions, whose id nodeos gave.
    #[test]
    fn a_public_block_renders_all_three_endpoints() {
        let hex = include_str!("../../testdata/blocks/eos-mainnet-480385480.hex");
        let bytes = hex::decode(hex.trim()).unwrap();
        let b = parse_signed_block(&bytes).unwrap();
        let id = "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4";
        assert_eq!((b.number, hex::encode(b.id).as_str()), (480_385_480, id));
        assert!(!b.transactions.is_empty());

        let text = body(Endpoint::GetRawBlock, &bytes, false);
        let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(raw, raw_block_from_abieos(&bytes));
        // Members come in nodeos's order, which comparing JSON values does not see.
        let order = [
            "timestamp",
            "producer",
            "confirmed",
            "previous",
            "transaction_mroot",
            "action_mroot",
            "schedule_version",
            "header_extensions",
            "producer_signature",
            "transactions",
            "block_extensions",
        ];
        let at: Vec<_> = order
            .iter()
            .map(|k| text.find(&format!("\"{k}\":")).unwrap())
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{text}");

        let info = body(Endpoint::GetBlockInfo, &bytes, false);
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert_eq!(info["id"], id);
        assert_eq!(info["block_num"], 480_385_480);
        assert_eq!(info["ref_block_num"], 480_385_480 & 0xffff);
        assert_eq!(info["ref_block_prefix"], 525_514_455);
        for k in ["timestamp", "producer", "previous", "producer_signature"] {
            assert_eq!(info[k], raw[k], "{k}");
        }
        assert_eq!(info.as_object().unwrap().len(), 12);

        let mut sbh = raw.clone();
        let members = sbh.as_object_mut().unwrap();
        members.remove("transactions");
        let block_extensions = members.remove("block_extensions").unwrap();
        for ext in [false, true] {
            let hdr = body(Endpoint::GetBlockHeader, &bytes, ext);
            let hdr: serde_json::Value = serde_json::from_str(&hdr).unwrap();
            assert_eq!(hdr["id"], id);
            assert_eq!(hdr["signed_block_header"], sbh);
            assert_eq!(
                hdr.get("block_extensions"),
                ext.then_some(&block_extensions)
            );
        }
    }
}
