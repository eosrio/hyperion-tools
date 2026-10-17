//! The SHiP wire protocol as a node serves it: request decoding with fc semantics, the status
//! and result encoders, and the vendored Spring SHiP ABI.
//!
//! Variant indices follow Spring's append-only order (`state_history/types.hpp`), which is also
//! the order of the vendored ABI's `request` and `result` variants:
//!
//! | idx | request                     | result                 |
//! |-----|-----------------------------|------------------------|
//! | 0   | `get_status_request_v0`     | `get_status_result_v0` |
//! | 1   | `get_blocks_request_v0`     | `get_blocks_result_v0` |
//! | 2   | `get_blocks_ack_request_v0` | `get_blocks_result_v1` |
//! | 3   | `get_blocks_request_v1`     | `get_status_result_v1` |
//! | 4   | `get_status_request_v1`     |                        |
//!
//! [`decode_request`] is exactly as lenient as the `fc::raw::unpack` a node runs on every
//! message: a non-canonical varuint32 is accepted and trailing bytes are ignored. Only a short
//! message, an unknown variant, a bool other than 0 or 1, or more `have_positions` than fc
//! allows is an error.
//!
//! The encoders write canonical fc packing, so their bytes are the ones a Spring node sends.
//! Every variant index is below 128 and takes one byte, so `head` sits at `[1..37)` and
//! `last_irreversible` at `[37..73)` of every status and result message, where a SHiP-aware
//! router or proxy can patch them in place.
//!
//! A `get_blocks_result` message is [`result_variant`], then [`encode_result_base`], then for
//! `traces`, `deltas` and (v1 only) `finality_data` either [`encode_opt_len`]`(Some(len))`
//! followed by the `len` decompressed payload bytes, or [`encode_opt_len`]`(None)`. Spring
//! writes each of those pieces as its own WebSocket frame and closes the message with an empty
//! final frame.

use anyhow::{Context, Result, bail, ensure};

use crate::binio::FcReader;

/// The SHiP ABI text a Spring node sends as its first message: the raw literal
/// `state_history_plugin_abi` of Spring's `libraries/state_history/abi.cpp`, with LF line
/// endings, byte for byte (re-extract it with `scripts/extract_ship_abi.py`).
pub const SPRING_SHIP_ABI: &str = include_str!("ship_abi.json");

/// sha256 of [`SPRING_SHIP_ABI`], lowercase hex.
pub const SPRING_SHIP_ABI_SHA256: &str =
    "f17a3bc91db1baa017c78ed31ed82eb52a4f4f12b661d59d15815bc9fab0e12e";

/// Result variant index of `get_status_result_v0`.
pub const RESULT_STATUS_V0: u8 = 0;
/// Result variant index of `get_blocks_result_v0`.
pub const RESULT_BLOCKS_V0: u8 = 1;
/// Result variant index of `get_blocks_result_v1`.
pub const RESULT_BLOCKS_V1: u8 = 2;
/// Result variant index of `get_status_result_v1`.
pub const RESULT_STATUS_V1: u8 = 3;

/// Bytes in a `get_status_result_v0` message, variant index included.
pub const STATUS_V0_LEN: usize = 121;
/// Bytes in a `get_status_result_v1` message, variant index included.
pub const STATUS_V1_LEN: usize = 129;

/// fc `MAX_NUM_ARRAY_ELEMENTS`: the most `have_positions` a node unpacks.
pub const MAX_HAVE_POSITIONS: u32 = 1024 * 1024;

/// fc `MAX_SIZE_OF_BYTE_ARRAYS`: Spring refuses to pack a `block` longer than this (the
/// session then drops the connection), so a server checks a body against it before encoding.
pub const MAX_BLOCK_BYTES: usize = 20 * 1024 * 1024;

const REQ_STATUS_V0: u8 = 0;
const REQ_BLOCKS_V0: u8 = 1;
const REQ_ACK: u8 = 2;
const REQ_BLOCKS_V1: u8 = 3;
const REQ_STATUS_V1: u8 = 4;

/// Bytes in a packed `block_position`.
const POS_LEN: usize = 4 + 32;

/// `block_position`: a block number and its id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct BlockPos {
    pub num: u32,
    pub id: [u8; 32],
}

/// A `get_blocks_request_v0` (`v1 == false`) or `get_blocks_request_v1`. A v0 request has no
/// `fetch_finality_data` on the wire; it decodes as `false` and is not encoded.
///
/// The default is Spring's initial session state: `end == 0`, so nothing is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlocksReq {
    pub v1: bool,
    pub start: u32,
    /// Exclusive: blocks `[start, end)` are sent.
    pub end: u32,
    pub max_in_flight: u32,
    pub have_positions: Vec<BlockPos>,
    pub irreversible_only: bool,
    pub fetch_block: bool,
    pub fetch_traces: bool,
    pub fetch_deltas: bool,
    pub fetch_finality_data: bool,
}

/// One decoded client message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// `get_status_request_v0` or, with `v1`, `get_status_request_v1`.
    Status {
        v1: bool,
    },
    Blocks(BlocksReq),
    /// `get_blocks_ack_request_v0`: `num_messages` more results may be sent.
    Ack(u32),
}

/// Decode one whole client message with `fc::raw::unpack` semantics (Spring `session.hpp`
/// unpacks every message as a `state_request`). Anything after the request is ignored, as fc
/// ignores it.
pub fn decode_request(b: &[u8]) -> Result<Request> {
    let mut r = FcReader::new(b);
    let idx = r.varuint32().context("request variant index")?;
    Ok(match u8::try_from(idx) {
        Ok(REQ_STATUS_V0) => Request::Status { v1: false },
        Ok(REQ_STATUS_V1) => Request::Status { v1: true },
        Ok(REQ_BLOCKS_V0) => {
            Request::Blocks(decode_blocks(&mut r, false).context("get_blocks_request_v0")?)
        }
        Ok(REQ_BLOCKS_V1) => {
            Request::Blocks(decode_blocks(&mut r, true).context("get_blocks_request_v1")?)
        }
        Ok(REQ_ACK) => Request::Ack(r.u32().context("get_blocks_ack_request_v0")?),
        _ => bail!("request variant index {idx} out of range (0-4)"),
    })
}

fn decode_blocks(r: &mut FcReader, v1: bool) -> Result<BlocksReq> {
    let start = r.u32()?;
    let end = r.u32()?;
    let max_in_flight = r.u32()?;
    let count = r.varuint32()?;
    ensure!(
        count <= MAX_HAVE_POSITIONS,
        "{count} have_positions exceed fc's limit of {MAX_HAVE_POSITIONS}"
    );
    // Reserve only what the remaining bytes can hold, so a corrupt count cannot allocate.
    let mut have_positions = Vec::with_capacity((count as usize).min(r.remaining() / POS_LEN));
    for _ in 0..count {
        have_positions.push(BlockPos {
            num: r.u32()?,
            id: r.id()?,
        });
    }
    Ok(BlocksReq {
        v1,
        start,
        end,
        max_in_flight,
        have_positions,
        irreversible_only: r.bool()?,
        fetch_block: r.bool()?,
        fetch_traces: r.bool()?,
        fetch_deltas: r.bool()?,
        fetch_finality_data: v1 && r.bool()?,
    })
}

/// Canonical fc packing of `req`, as a SHiP client sends it: the inverse of
/// [`decode_request`]. The `fetch_finality_data` of a v0 request is not encoded.
pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    match req {
        Request::Status { v1 } => out.push(if *v1 { REQ_STATUS_V1 } else { REQ_STATUS_V0 }),
        Request::Ack(n) => {
            out.push(REQ_ACK);
            put_u32(&mut out, *n);
        }
        Request::Blocks(r) => {
            out.reserve(1 + 12 + 5 + r.have_positions.len() * POS_LEN + 5);
            out.push(if r.v1 { REQ_BLOCKS_V1 } else { REQ_BLOCKS_V0 });
            for v in [r.start, r.end, r.max_in_flight] {
                put_u32(&mut out, v);
            }
            put_varuint(&mut out, r.have_positions.len() as u64);
            for p in &r.have_positions {
                put_pos(&mut out, *p);
            }
            let flags = [
                r.irreversible_only,
                r.fetch_block,
                r.fetch_traces,
                r.fetch_deltas,
            ];
            out.extend(flags.map(u8::from));
            if r.v1 {
                out.push(r.fetch_finality_data.into());
            }
        }
    }
    out
}

/// What a status message reports. Each range is `[begin, end)`, or `(0, 0)` for a log the
/// server does not have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub head: BlockPos,
    pub lib: BlockPos,
    pub trace: (u32, u32),
    pub chain_state: (u32, u32),
    pub finality: (u32, u32),
    pub chain_id: [u8; 32],
}

/// A whole status message: `get_status_result_v0` ([`STATUS_V0_LEN`] bytes, the reply to
/// `get_status_request_v0`) or, with `v1`, `get_status_result_v1` ([`STATUS_V1_LEN`] bytes).
/// The v0 `chain_id` is an ABI binary extension; Spring always sends it, and so does this.
pub fn encode_status(v1: bool, s: &Status) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATUS_V1_LEN);
    out.push(if v1 {
        RESULT_STATUS_V1
    } else {
        RESULT_STATUS_V0
    });
    put_pos(&mut out, s.head);
    put_pos(&mut out, s.lib);
    for (begin, end) in [s.trace, s.chain_state] {
        put_u32(&mut out, begin);
        put_u32(&mut out, end);
    }
    out.extend_from_slice(&s.chain_id);
    if v1 {
        put_u32(&mut out, s.finality.0);
        put_u32(&mut out, s.finality.1);
    }
    out
}

/// `get_blocks_result_base` (`head`, `last_irreversible`, `this_block?`, `prev_block?`,
/// `block?`) without the variant index, as Spring packs it into the message's second frame.
/// `block` is the packed `signed_block` with a varuint32 length.
///
/// # Panics
///
/// If `block` is longer than `u32::MAX` bytes, which no varuint32 can describe. Spring refuses
/// anything above [`MAX_BLOCK_BYTES`]; callers check that bound first.
pub fn encode_result_base(
    head: BlockPos,
    lib: BlockPos,
    this: Option<BlockPos>,
    prev: Option<BlockPos>,
    block: Option<&[u8]>,
) -> Vec<u8> {
    let block_len = block.map_or(0, |b| 5 + b.len());
    let mut out = Vec::with_capacity(2 * POS_LEN + 2 * (1 + POS_LEN) + 1 + block_len);
    put_pos(&mut out, head);
    put_pos(&mut out, lib);
    for p in [this, prev] {
        match p {
            Some(p) => {
                out.push(1);
                put_pos(&mut out, p);
            }
            None => out.push(0),
        }
    }
    match block {
        Some(b) => {
            let len = u32::try_from(b.len())
                .expect("a block longer than u32::MAX bytes has no SHiP encoding");
            out.push(1);
            put_varuint(&mut out, len.into());
            out.extend_from_slice(b);
        }
        None => out.push(0),
    }
    out
}

/// The head of an optional `traces`, `deltas` or `finality_data` field: `00` when absent,
/// else `01` and the decompressed payload length as a varuint64 (Spring
/// `history_pack_varuint64`), which the payload bytes then follow. Below 2^32 the length bytes
/// equal a varuint32's.
pub fn encode_opt_len(len: Option<u64>) -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    match len {
        Some(len) => {
            out.push(1);
            put_varuint(&mut out, len);
        }
        None => out.push(0),
    }
    out
}

/// The result variant answering a `get_blocks` request: `get_blocks_result_v1` for a v1
/// request, even one with `fetch_finality_data == false`, else `get_blocks_result_v0`.
pub fn result_variant(v1: bool) -> u8 {
    if v1 {
        RESULT_BLOCKS_V1
    } else {
        RESULT_BLOCKS_V0
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_pos(out: &mut Vec<u8>, p: BlockPos) {
    put_u32(out, p.num);
    out.extend_from_slice(&p.id);
}

/// LEB128, least significant group first: fc `unsigned_int` packing for values that fit in
/// 32 bits, Spring `history_pack_varuint64` for any.
fn put_varuint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rs_abieos::Abieos;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    /// A recognisable id for block `num`: its number big-endian, as in a real id, then filler.
    fn id_of(num: u32) -> [u8; 32] {
        let mut id = [0; 32];
        id[..4].copy_from_slice(&num.to_be_bytes());
        for (i, b) in id[4..].iter_mut().enumerate() {
            *b = (num as u8).wrapping_mul(31).wrapping_add(i as u8);
        }
        id
    }

    fn pos(num: u32) -> BlockPos {
        BlockPos {
            num,
            id: id_of(num),
        }
    }

    /// A request with `positions` have_positions, whose block numbers cycle through the
    /// varuint boundaries.
    fn blocks(v1: bool, positions: usize) -> BlocksReq {
        const NUMS: [u32; 5] = [0, 1, 127, 128, 2_000];
        BlocksReq {
            v1,
            start: 2_001,
            end: 0xffff_ffff,
            max_in_flight: 50,
            have_positions: (0..positions).map(|i| pos(NUMS[i % NUMS.len()])).collect(),
            irreversible_only: false,
            fetch_block: true,
            fetch_traces: false,
            fetch_deltas: true,
            fetch_finality_data: v1,
        }
    }

    fn varuint(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_varuint(&mut out, v);
        out
    }

    fn ship_abieos() -> Abieos {
        let abieos = Abieos::new();
        abieos.set_abi_json("ship", SPRING_SHIP_ABI).unwrap();
        abieos
    }

    fn abi_decode(abieos: &Abieos, ty: &str, bin: &[u8]) -> Value {
        let text = abieos.bin_to_json("ship", ty, bin).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The ABI's JSON renders checksums and bytes as uppercase hex.
    fn pos_json(p: BlockPos) -> Value {
        json!({ "block_num": p.num, "block_id": hex::encode_upper(p.id) })
    }

    fn opt_pos_json(p: Option<BlockPos>) -> Value {
        p.map_or(Value::Null, pos_json)
    }

    fn opt_bytes_json(b: Option<&[u8]>) -> Value {
        b.map_or(Value::Null, |b| Value::String(hex::encode_upper(b)))
    }

    /// A whole, unfragmented result message: the variant, the base, then each optional field.
    fn result_msg(v1: bool, base: &[u8], fields: &[Option<&[u8]>]) -> Vec<u8> {
        let mut msg = vec![result_variant(v1)];
        msg.extend_from_slice(base);
        for f in fields {
            msg.extend(encode_opt_len(f.map(|b| b.len() as u64)));
            msg.extend_from_slice(f.unwrap_or_default());
        }
        msg
    }

    #[test]
    fn decodes_every_request_variant() {
        assert_eq!(decode_request(&[0]).unwrap(), Request::Status { v1: false });
        assert_eq!(decode_request(&[4]).unwrap(), Request::Status { v1: true });

        let mut ack = vec![2];
        ack.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        assert_eq!(decode_request(&ack).unwrap(), Request::Ack(0xdead_beef));

        // Packed by hand, field by field, in Spring's reflection order.
        let mut v0 = vec![1];
        for v in [10u32, 20, 5] {
            v0.extend_from_slice(&v.to_le_bytes());
        }
        v0.push(1);
        v0.extend_from_slice(&9u32.to_le_bytes());
        v0.extend_from_slice(&id_of(9));
        v0.extend_from_slice(&[1, 0, 1, 1]);
        let want = BlocksReq {
            v1: false,
            start: 10,
            end: 20,
            max_in_flight: 5,
            have_positions: vec![pos(9)],
            irreversible_only: true,
            fetch_block: false,
            fetch_traces: true,
            fetch_deltas: true,
            fetch_finality_data: false,
        };
        assert_eq!(decode_request(&v0).unwrap(), Request::Blocks(want.clone()));

        for fin in [false, true] {
            let mut v1 = v0.clone();
            v1[0] = 3;
            v1.push(fin.into());
            let want = BlocksReq {
                v1: true,
                fetch_finality_data: fin,
                ..want.clone()
            };
            assert_eq!(decode_request(&v1).unwrap(), Request::Blocks(want));
        }
    }

    #[test]
    fn requests_round_trip_with_multi_byte_position_counts() {
        for n in [0, 1, 127, 128, 2000] {
            for v1 in [false, true] {
                let req = Request::Blocks(blocks(v1, n));
                let enc = encode_request(&req);
                let count = varuint(n as u64);
                assert_eq!(&enc[13..13 + count.len()], count, "count {n}");
                let flags = if v1 { 5 } else { 4 };
                assert_eq!(enc.len(), 13 + count.len() + n * 36 + flags);
                assert_eq!(decode_request(&enc).unwrap(), req, "{n} positions, v1 {v1}");
            }
        }
        assert_eq!(varuint(128), [0x80, 0x01]);
        assert_eq!(varuint(2000), [0xd0, 0x0f]);
        for req in [
            Request::Status { v1: false },
            Request::Status { v1: true },
            Request::Ack(0),
            Request::Ack(u32::MAX),
        ] {
            assert_eq!(decode_request(&encode_request(&req)).unwrap(), req);
        }
    }

    #[test]
    fn v0_requests_carry_no_finality_flag() {
        let mut req = blocks(false, 1);
        req.fetch_finality_data = true;
        let enc = encode_request(&Request::Blocks(req.clone()));
        assert_eq!(enc.len(), 13 + 1 + 36 + 4);
        req.fetch_finality_data = false;
        assert_eq!(decode_request(&enc).unwrap(), Request::Blocks(req));
    }

    #[test]
    fn non_canonical_varuints_are_accepted() {
        assert_eq!(
            decode_request(&[0x80, 0x00]).unwrap(),
            Request::Status { v1: false }
        );
        assert_eq!(
            decode_request(&[0x84, 0x80, 0x80, 0x00]).unwrap(),
            Request::Status { v1: true }
        );
        // fc stops after the fifth byte even when it has the continuation bit set.
        assert_eq!(
            decode_request(&[0x80; 5]).unwrap(),
            Request::Status { v1: false }
        );
        assert_eq!(
            decode_request(&[0x84, 0x80, 0x80, 0x80, 0x80, 0xff]).unwrap(),
            Request::Status { v1: true }
        );

        // Variant 3 padded to two bytes, and a count of 1 padded to five.
        let req = blocks(true, 1);
        let canonical = encode_request(&Request::Blocks(req.clone()));
        let mut padded = vec![0x83, 0x00];
        padded.extend_from_slice(&canonical[1..13]);
        padded.extend_from_slice(&[0x81, 0x80, 0x80, 0x80, 0x00]);
        padded.extend_from_slice(&canonical[14..]);
        assert_eq!(decode_request(&padded).unwrap(), Request::Blocks(req));
    }

    #[test]
    fn bools_other_than_zero_or_one_are_rejected() {
        let enc = encode_request(&Request::Blocks(blocks(true, 0)));
        // The five bools are the last five bytes of a v1 request.
        for at in enc.len() - 5..enc.len() {
            let mut bad = enc.clone();
            bad[at] = 2;
            let err = format!("{:#}", decode_request(&bad).unwrap_err());
            assert!(err.contains("invalid bool 0x02"), "byte {at}: {err}");
            bad[at] = 0xff;
            assert!(decode_request(&bad).is_err(), "byte {at}");
        }
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        for req in [
            Request::Status { v1: false },
            Request::Status { v1: true },
            Request::Ack(7),
            Request::Blocks(blocks(false, 2)),
            Request::Blocks(blocks(true, 2)),
        ] {
            let mut enc = encode_request(&req);
            enc.extend_from_slice(&[0xff, 2, 0, 0x80]);
            assert_eq!(decode_request(&enc).unwrap(), req);
        }
    }

    #[test]
    fn short_messages_are_rejected() {
        assert!(decode_request(&[]).is_err());
        assert!(decode_request(&[0x80]).is_err());
        for req in [Request::Ack(7), Request::Blocks(blocks(true, 2))] {
            let enc = encode_request(&req);
            for len in 1..enc.len() {
                assert!(decode_request(&enc[..len]).is_err(), "{req:?} cut at {len}");
            }
        }
    }

    #[test]
    fn unknown_variants_are_rejected() {
        for bad in [
            &[5][..],
            &[9],
            &[0x7f],
            &[0x85, 0x00],
            &[0xff, 0xff, 0xff, 0xff, 0x0f],
        ] {
            let err = decode_request(bad).unwrap_err().to_string();
            assert!(err.contains("out of range"), "{bad:02x?}: {err}");
        }
    }

    #[test]
    fn position_count_is_bounded_like_fc() {
        let mut enc = vec![1];
        enc.extend_from_slice(&[0; 12]);
        let header = enc.clone();

        enc.extend_from_slice(&varuint(u64::from(MAX_HAVE_POSITIONS) + 1));
        let err = format!("{:#}", decode_request(&enc).unwrap_err());
        assert!(err.contains("exceed fc's limit"), "{err}");

        // A count at the limit with no positions behind it fails on the missing bytes, without
        // reserving room for a million entries first.
        let mut enc = header.clone();
        enc.extend_from_slice(&varuint(MAX_HAVE_POSITIONS.into()));
        enc.extend_from_slice(&[0; 40]);
        let err = format!("{:#}", decode_request(&enc).unwrap_err());
        assert!(err.contains("past end"), "{err}");
    }

    #[test]
    fn requests_match_the_abi_encoding() {
        let abieos = ship_abieos();
        let req = blocks(true, 3);
        let positions: Vec<Value> = req.have_positions.iter().map(|p| pos_json(*p)).collect();
        let fields = json!({
            "start_block_num": req.start,
            "end_block_num": req.end,
            "max_messages_in_flight": req.max_in_flight,
            "have_positions": positions,
            "irreversible_only": req.irreversible_only,
            "fetch_block": req.fetch_block,
            "fetch_traces": req.fetch_traces,
            "fetch_deltas": req.fetch_deltas,
        });
        let mut v1_fields = fields.clone();
        v1_fields["fetch_finality_data"] = json!(true);
        let v0 = BlocksReq {
            v1: false,
            fetch_finality_data: false,
            ..req.clone()
        };
        for (json, want) in [
            (
                json!(["get_status_request_v0", {}]),
                Request::Status { v1: false },
            ),
            (
                json!(["get_status_request_v1", {}]),
                Request::Status { v1: true },
            ),
            (
                json!(["get_blocks_ack_request_v0", { "num_messages": 4_000_000_000u32 }]),
                Request::Ack(4_000_000_000),
            ),
            (
                json!(["get_blocks_request_v0", fields]),
                Request::Blocks(v0),
            ),
            (
                json!(["get_blocks_request_v1", v1_fields]),
                Request::Blocks(req.clone()),
            ),
        ] {
            let bin = abieos
                .json_to_bin("ship", "request", &json.to_string())
                .unwrap();
            assert_eq!(bin, encode_request(&want), "{json}");
            assert_eq!(decode_request(&bin).unwrap(), want, "{json}");
        }
    }

    fn sample_status() -> Status {
        Status {
            head: pos(2_000),
            lib: pos(1_990),
            trace: (2, 2_001),
            chain_state: (3, 2_001),
            finality: (1_500, 2_001),
            chain_id: [0xc1; 32],
        }
    }

    #[test]
    fn status_messages_have_spring_sizes_and_offsets() {
        let s = sample_status();
        let u32_at = |b: &[u8], off: usize| u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        let v0 = encode_status(false, &s);
        let v1 = encode_status(true, &s);
        assert_eq!((v0.len(), v1.len()), (STATUS_V0_LEN, STATUS_V1_LEN));
        assert_eq!((v0[0], v1[0]), (RESULT_STATUS_V0, RESULT_STATUS_V1));
        for b in [&v0, &v1] {
            assert_eq!(u32_at(b, 1), 2_000);
            assert_eq!(b[5..37], id_of(2_000));
            assert_eq!(u32_at(b, 37), 1_990);
            assert_eq!(b[41..73], id_of(1_990));
            assert_eq!(
                [73, 77, 81, 85].map(|off| u32_at(b, off)),
                [2, 2_001, 3, 2_001]
            );
            assert_eq!(b[89..121], [0xc1; 32]);
        }
        assert_eq!((u32_at(&v1, 121), u32_at(&v1, 125)), (1_500, 2_001));
        assert_eq!(v0[1..], v1[1..STATUS_V0_LEN]);
    }

    #[test]
    fn status_messages_match_the_abi() {
        let abieos = ship_abieos();
        let s = sample_status();
        let base = json!({
            "head": pos_json(s.head),
            "last_irreversible": pos_json(s.lib),
            "trace_begin_block": 2,
            "trace_end_block": 2_001,
            "chain_state_begin_block": 3,
            "chain_state_end_block": 2_001,
            "chain_id": hex::encode_upper(s.chain_id),
        });
        let got = abi_decode(&abieos, "result", &encode_status(false, &s));
        assert_eq!(got, json!(["get_status_result_v0", base]));

        let mut v1 = base.clone();
        v1["finality_data_begin_block"] = json!(1_500);
        v1["finality_data_end_block"] = json!(2_001);
        let got = abi_decode(&abieos, "result", &encode_status(true, &s));
        assert_eq!(got, json!(["get_status_result_v1", v1]));
    }

    #[test]
    fn result_variant_follows_the_request_version() {
        assert_eq!(result_variant(false), RESULT_BLOCKS_V0);
        assert_eq!(result_variant(true), RESULT_BLOCKS_V1);
        assert_eq!((RESULT_BLOCKS_V0, RESULT_BLOCKS_V1), (1, 2));
    }

    #[test]
    fn opt_len_is_a_varuint64() {
        assert_eq!(encode_opt_len(None), [0]);
        assert_eq!(encode_opt_len(Some(0)), [1, 0]);
        assert_eq!(encode_opt_len(Some(127)), [1, 0x7f]);
        assert_eq!(encode_opt_len(Some(128)), [1, 0x80, 0x01]);
        assert_eq!(
            encode_opt_len(Some(u64::from(u32::MAX))),
            [1, 0xff, 0xff, 0xff, 0xff, 0x0f]
        );
        // Above 2^32 Spring keeps going in 7-bit groups; no varuint32 reader can follow.
        assert_eq!(
            encode_opt_len(Some(1 << 32)),
            [1, 0x80, 0x80, 0x80, 0x80, 0x10]
        );
        let max = encode_opt_len(Some(u64::MAX));
        assert_eq!((max.len(), max[10]), (11, 0x01));
    }

    #[test]
    fn result_base_layout() {
        let (head, lib) = (pos(2_000), pos(1_990));
        let empty = encode_result_base(head, lib, None, None, None);
        assert_eq!(empty.len(), 72 + 3);
        assert_eq!(empty[..4], 2_000u32.to_le_bytes());
        assert_eq!(empty[36..40], 1_990u32.to_le_bytes());
        assert_eq!(empty[72..], [0, 0, 0]);
        // An id-less result (head and LIB only) is 78 bytes as v0 and 79 as v1.
        assert_eq!(result_msg(false, &empty, &[None; 2]).len(), 78);
        assert_eq!(result_msg(true, &empty, &[None; 3]).len(), 79);

        let body = vec![0xab; 200];
        let full = encode_result_base(head, lib, Some(pos(7)), Some(pos(6)), Some(&body));
        assert_eq!(full[72], 1);
        assert_eq!(full[73..77], 7u32.to_le_bytes());
        assert_eq!(full[109], 1);
        assert_eq!(full[110..114], 6u32.to_le_bytes());
        // The block length is a varuint32: 200 takes two bytes.
        assert_eq!(full[146..149], [1, 0xc8, 0x01]);
        assert_eq!(full[149..], body[..]);
    }

    #[test]
    fn results_match_the_abi() {
        let abieos = ship_abieos();
        let (head, lib) = (pos(5_000), pos(4_000));
        let small = [0x5a; 1];
        let medium: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let large: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
        let payloads: [Option<&[u8]>; 5] =
            [None, Some(&[]), Some(&small), Some(&medium), Some(&large)];
        let links = [
            (None, None),
            (Some(pos(10)), None),
            (Some(pos(10)), Some(pos(9))),
        ];

        let mut checked = 0;
        for v1 in [false, true] {
            for (this, prev) in links {
                for (i, block) in payloads.iter().enumerate() {
                    // Rotate the payloads so every field sees every length.
                    let traces = payloads[(i + 1) % payloads.len()];
                    let deltas = payloads[(i + 2) % payloads.len()];
                    let finality = payloads[(i + 3) % payloads.len()];

                    let base = encode_result_base(head, lib, this, prev, *block);
                    let fields = [traces, deltas, finality];
                    let msg = result_msg(v1, &base, &fields[..if v1 { 3 } else { 2 }]);

                    let mut want = json!({
                        "head": pos_json(head),
                        "last_irreversible": pos_json(lib),
                        "this_block": opt_pos_json(this),
                        "prev_block": opt_pos_json(prev),
                        "block": opt_bytes_json(*block),
                        "traces": opt_bytes_json(traces),
                        "deltas": opt_bytes_json(deltas),
                    });
                    let name = if v1 {
                        want["finality_data"] = opt_bytes_json(finality);
                        "get_blocks_result_v1"
                    } else {
                        "get_blocks_result_v0"
                    };
                    let got = abi_decode(&abieos, "result", &msg);
                    assert_eq!(got, json!([name, want]), "v1 {v1}, block {i}");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 30);
    }

    #[test]
    fn vendored_abi_is_the_pinned_spring_literal() {
        let digest = hex::encode(Sha256::digest(SPRING_SHIP_ABI.as_bytes()));
        assert_eq!(digest, SPRING_SHIP_ABI_SHA256);
        assert_eq!(SPRING_SHIP_ABI.len(), 35_704);
        assert!(SPRING_SHIP_ABI.starts_with('{') && SPRING_SHIP_ABI.ends_with('}'));
        assert!(!SPRING_SHIP_ABI.contains('\r'));
    }

    #[test]
    fn vendored_abi_variant_order_matches_the_codec() {
        let abi: Value = serde_json::from_str(SPRING_SHIP_ABI).unwrap();
        assert_eq!(abi["version"], "eosio::abi/1.1");
        let variant = |name: &str| {
            abi["variants"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == name)
                .unwrap()["types"]
                .clone()
        };
        assert_eq!(
            variant("request"),
            json!([
                "get_status_request_v0",
                "get_blocks_request_v0",
                "get_blocks_ack_request_v0",
                "get_blocks_request_v1",
                "get_status_request_v1",
            ])
        );
        let results = variant("result");
        for (idx, name) in [
            (RESULT_STATUS_V0, "get_status_result_v0"),
            (RESULT_BLOCKS_V0, "get_blocks_result_v0"),
            (RESULT_BLOCKS_V1, "get_blocks_result_v1"),
            (RESULT_STATUS_V1, "get_status_result_v1"),
        ] {
            assert_eq!(results[usize::from(idx)], name);
        }
        assert_eq!(results.as_array().unwrap().len(), 4);
    }
}
