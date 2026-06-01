//! Zero-copy hand-walk of a SHiP `transaction_trace[]` payload — the fast path for action-proto.
//!
//! Instead of `bin_to_json("transaction_trace[]", ..)` (which materializes the whole block as a
//! JSON string we then re-parse with serde, and renders `act.data` as hex we then re-decode), we
//! walk the binary directly and pull only the fields the Hyperion action doc needs — leaving
//! `act.data` as a raw byte range to hand straight to the contract decoder. Everything we don't
//! emit (console, the trailing optionals, `failed_dtrx_trace`, `partial_transaction`) is parsed
//! only enough to stay aligned.
//!
//! Layout (Antelope LE; verified against rs-abieos/abis/ship.abi.json + ship_protocol.hpp):
//!   transaction_trace[] = [varuint N]( [varuint variant=0] transaction_trace_v0 )*
//!   transaction_trace_v0 = id:checksum256 status:u8 cpu_usage_us:u32 net_usage_words:varuint
//!     elapsed:i64 net_usage:u64 scheduled:bool action_traces:action_trace[]
//!     account_ram_delta:account_delta? except:string? error_code:u64?
//!     failed_dtrx_trace:transaction_trace? partial:partial_transaction?
//!   action_trace = [varuint variant{0|1}] action_trace_v0 (+ return_value:bytes for v1)
//!   action_trace_v0 = action_ordinal:varuint creator_action_ordinal:varuint
//!     receipt:action_receipt? receiver:name act:action context_free:bool elapsed:i64
//!     console:string account_ram_deltas:account_delta[] except:string? error_code:u64?
//!   action = account:name name:name authorization:permission_level[] data:bytes
//!   action_receipt_v0 = receiver:name act_digest:checksum256 global_sequence:u64
//!     recv_sequence:u64 auth_sequence:(name,u64)[] code_sequence:varuint abi_sequence:varuint

/// A decoded `action_receipt_v0`.
pub struct Receipt {
    pub receiver: u64,
    pub act_digest: [u8; 32],
    pub global_sequence: u64,
    pub recv_sequence: u64,
    pub auth_sequence: Vec<(u64, u64)>,
    pub code_sequence: u32,
    pub abi_sequence: u32,
}

/// A decoded `action_trace_v0`/`v1`. `data`/`return_value` are `(offset, len)` ranges into the
/// inflated payload — sliced by the caller, never copied here.
pub struct Act {
    pub action_ordinal: u32,
    pub creator_action_ordinal: u32,
    pub receipt: Option<Receipt>,
    pub receiver: u64,
    pub account: u64,
    pub name: u64,
    pub authorization: Vec<(u64, u64)>,
    pub data: (usize, usize),
    pub context_free: bool,
    pub elapsed: i64,
    pub account_ram_deltas: Vec<(u64, i64)>,
    pub return_value: Option<(usize, usize)>,
    pub except: bool,
}

/// A decoded `transaction_trace_v0` (only the fields the action doc needs).
pub struct Tx {
    pub id: [u8; 32],
    pub cpu_usage_us: u32,
    pub net_usage_words: u32,
    pub actions: Vec<Act>,
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn varuint(&mut self) -> Option<u32> {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return None;
            }
        }
        Some(v as u32)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    /// A `bytes`/`string` field: varuint length + that many bytes; returns the (offset, len) range.
    fn bytes_range(&mut self) -> Option<(usize, usize)> {
        let len = self.varuint()? as usize;
        let start = self.p;
        self.take(len)?;
        Some((start, len))
    }
    fn skip_bytes(&mut self) -> Option<()> {
        let len = self.varuint()? as usize;
        self.skip(len)
    }
    fn checksum256(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }
    /// Optional-present byte: `1` => present, `0` => null.
    fn present(&mut self) -> Option<bool> {
        Some(self.u8()? == 1)
    }
}

/// Parse a whole block's `transaction_trace[]`. Returns `None` on any misalignment / truncation.
pub fn parse_block(payload: &[u8]) -> Option<Vec<Tx>> {
    let mut c = Cur::new(payload);
    let n = c.varuint()? as usize;
    let mut txs = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let _variant = c.varuint()?; // transaction_trace_v0
        txs.push(parse_tx_v0(&mut c)?);
    }
    Some(txs)
}

fn parse_tx_v0(c: &mut Cur) -> Option<Tx> {
    let id = c.checksum256()?;
    c.skip(1)?; // status
    let cpu_usage_us = c.u32()?;
    let net_usage_words = c.varuint()?;
    c.skip(8)?; // elapsed
    c.skip(8)?; // net_usage
    c.skip(1)?; // scheduled
    let m = c.varuint()? as usize;
    let mut actions = Vec::with_capacity(m.min(65536));
    for _ in 0..m {
        let v1 = c.varuint()? == 1; // action_trace_v0 | v1
        actions.push(parse_action_trace(c, v1)?);
    }
    if c.present()? {
        c.skip(16)?; // account_ram_delta: name + i64
    }
    if c.present()? {
        c.skip_bytes()?; // except: string
    }
    if c.present()? {
        c.skip(8)?; // error_code: u64
    }
    if c.present()? {
        c.varuint()?; // failed_dtrx_trace: transaction_trace (recursive)
        skip_tx_v0(c)?;
    }
    if c.present()? {
        c.varuint()?; // partial: partial_transaction_v0
        skip_partial_v0(c)?;
    }
    Some(Tx {
        id,
        cpu_usage_us,
        net_usage_words,
        actions,
    })
}

fn parse_action_trace(c: &mut Cur, v1: bool) -> Option<Act> {
    let action_ordinal = c.varuint()?;
    let creator_action_ordinal = c.varuint()?;
    let receipt = if c.present()? {
        c.varuint()?; // action_receipt_v0
        let receiver = c.u64()?;
        let act_digest = c.checksum256()?;
        let global_sequence = c.u64()?;
        let recv_sequence = c.u64()?;
        let asn = c.varuint()? as usize;
        let mut auth_sequence = Vec::with_capacity(asn.min(4096));
        for _ in 0..asn {
            let a = c.u64()?;
            let s = c.u64()?;
            auth_sequence.push((a, s));
        }
        let code_sequence = c.varuint()?;
        let abi_sequence = c.varuint()?;
        Some(Receipt {
            receiver,
            act_digest,
            global_sequence,
            recv_sequence,
            auth_sequence,
            code_sequence,
            abi_sequence,
        })
    } else {
        None
    };
    let receiver = c.u64()?;
    // act
    let account = c.u64()?;
    let name = c.u64()?;
    let an = c.varuint()? as usize;
    let mut authorization = Vec::with_capacity(an.min(4096));
    for _ in 0..an {
        let actor = c.u64()?;
        let perm = c.u64()?;
        authorization.push((actor, perm));
    }
    let data = c.bytes_range()?;
    let context_free = c.u8()? != 0;
    let elapsed = c.i64()?;
    c.skip_bytes()?; // console
    let rd = c.varuint()? as usize;
    let mut account_ram_deltas = Vec::with_capacity(rd.min(4096));
    for _ in 0..rd {
        let acc = c.u64()?;
        let d = c.i64()?;
        account_ram_deltas.push((acc, d));
    }
    let except = c.present()?;
    if except {
        c.skip_bytes()?; // except string body
    }
    if c.present()? {
        c.skip(8)?; // error_code
    }
    let return_value = if v1 { Some(c.bytes_range()?) } else { None };
    Some(Act {
        action_ordinal,
        creator_action_ordinal,
        receipt,
        receiver,
        account,
        name,
        authorization,
        data,
        context_free,
        elapsed,
        account_ram_deltas,
        return_value,
        except,
    })
}

// --- skip-only walkers (for the parts we never emit) -----------------------------------------

fn skip_tx_v0(c: &mut Cur) -> Option<()> {
    c.skip(32 + 1 + 4)?; // id, status, cpu
    c.varuint()?; // net_usage_words
    c.skip(8 + 8 + 1)?; // elapsed, net_usage, scheduled
    let m = c.varuint()?;
    for _ in 0..m {
        let v1 = c.varuint()? == 1;
        skip_action_trace(c, v1)?;
    }
    if c.present()? {
        c.skip(16)?;
    }
    if c.present()? {
        c.skip_bytes()?;
    }
    if c.present()? {
        c.skip(8)?;
    }
    if c.present()? {
        c.varuint()?;
        skip_tx_v0(c)?;
    }
    if c.present()? {
        c.varuint()?;
        skip_partial_v0(c)?;
    }
    Some(())
}

fn skip_action_trace(c: &mut Cur, v1: bool) -> Option<()> {
    c.varuint()?;
    c.varuint()?; // ordinals
    if c.present()? {
        c.varuint()?; // receipt variant
        c.skip(8 + 32 + 8 + 8)?; // receiver, act_digest, global, recv
        let asn = c.varuint()?;
        for _ in 0..asn {
            c.skip(16)?;
        }
        c.varuint()?;
        c.varuint()?; // code, abi
    }
    c.skip(8 + 8 + 8)?; // receiver, act.account, act.name
    let an = c.varuint()?;
    for _ in 0..an {
        c.skip(16)?;
    }
    c.skip_bytes()?; // data
    c.skip(1 + 8)?; // context_free, elapsed
    c.skip_bytes()?; // console
    let rd = c.varuint()?;
    for _ in 0..rd {
        c.skip(16)?;
    }
    if c.present()? {
        c.skip_bytes()?;
    }
    if c.present()? {
        c.skip(8)?;
    }
    if v1 {
        c.skip_bytes()?;
    }
    Some(())
}

fn skip_partial_v0(c: &mut Cur) -> Option<()> {
    c.skip(4 + 2 + 4)?; // expiration, ref_block_num, ref_block_prefix
    c.varuint()?; // max_net_usage_words
    c.skip(1)?; // max_cpu_usage_ms
    c.varuint()?; // delay_sec
    let ext = c.varuint()?;
    for _ in 0..ext {
        c.skip(2)?; // extension type u16
        c.skip_bytes()?;
    }
    let sigs = c.varuint()?;
    for _ in 0..sigs {
        skip_signature(c)?;
    }
    let cfd = c.varuint()?;
    for _ in 0..cfd {
        c.skip_bytes()?;
    }
    Some(())
}

fn skip_signature(c: &mut Cur) -> Option<()> {
    match c.u8()? {
        0 | 1 => c.skip(65)?, // K1, R1
        2 => {
            c.skip(65)?; // WA compact signature
            c.skip_bytes()?; // auth_data
            c.skip_bytes()?; // client_json
        }
        _ => return None,
    }
    Some(())
}

/// Standard Antelope `name` (u64) → string, trailing `.` trimmed.
pub fn name_to_string(value: u64) -> String {
    const CHARMAP: &[u8; 32] = b".12345abcdefghijklmnopqrstuvwxyz";
    let mut s = [b'.'; 13];
    let mut tmp = value;
    for i in 0..13 {
        let idx = (tmp & if i == 0 { 0x0f } else { 0x1f }) as usize;
        s[12 - i] = CHARMAP[idx];
        tmp >>= if i == 0 { 4 } else { 5 };
    }
    let end = s.iter().rposition(|&c| c != b'.').map_or(0, |p| p + 1);
    String::from_utf8_lossy(&s[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        assert_eq!(name_to_string(0), "");
        for n in [
            "eosio",
            "eosio.token",
            "m.federation",
            "guild.nefty",
            "mtlgg.wam",
            "a",
            "1xq.o.c.wam",
        ] {
            assert_eq!(name_to_string(name_val(n)), n);
        }
    }

    // test-only encoder to round-trip
    fn name_val(s: &str) -> u64 {
        const CHARMAP: &[u8; 32] = b".12345abcdefghijklmnopqrstuvwxyz";
        let idx = |c: u8| CHARMAP.iter().position(|&x| x == c).unwrap() as u64;
        let bytes = s.as_bytes();
        let mut value = 0u64;
        for i in 0..13 {
            let mut c = 0u64;
            if i < bytes.len() {
                c = idx(bytes[i]);
            }
            if i < 12 {
                c &= 0x1f;
                value |= c << (64 - 5 * (i + 1));
            } else {
                c &= 0x0f;
                value |= c;
            }
        }
        value
    }
}
