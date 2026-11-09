//! Strict binary walks of the Wire SHiP records that ABI history reads.
//!
//! Each walk accepts and rejects the same bytes as the pinned Wire codec
//! (`hyperion_wire_protocol::codec`) decoding the same type, and reports the same error classes:
//! `invalid_value` for malformed bytes, `limits_exceeded` for bounds and `unsupported_feature` for
//! an unknown crypto variant. A walk renders nothing. It borrows byte ranges and reads fixed-width
//! integers, so a block costs one pass over its bytes instead of a JSON rendering per call.
//!
//! Two codec bounds are deliberately not applied: one million decode nodes and 64 decode levels
//! per call (with 16 MiB of rendered text). They bound the work of building a JSON value. A walk
//! builds none, keeps the codec's 16 MiB input and per-array bounds, and so does work linear in
//! its input. Well-formed data past the codec's traversal bounds, such as a block with tens of
//! thousands of actions, is walked instead of refused. The walks' only recursion, a
//! `failed_dtrx_trace` nested in another, stops at [`MAX_TRACE_NESTING`] levels.
//!
//! The layouts are fixed in code. [`check_ship_schema`] refuses a SHiP schema whose definitions
//! of the walked types differ from them, so a walk never reads bytes under the wrong layout.

use std::fmt;

use serde_json::Value;

pub const MAX_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ELEMENTS: usize = 100_000;
/// Nested `failed_dtrx_trace` levels a walk follows: the most the codec's 64-level bound allows
/// when every nested trace carries a receipted action with an auth sequence. The codec follows up
/// to 20 levels of traces without actions. ABI history refuses any `failed_dtrx_trace`, so this
/// bound only decides which error reports one.
pub const MAX_TRACE_NESTING: usize = 17;
const CRYPTO_FRAME_LIMIT: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkError {
    code: &'static str,
    message: &'static str,
}
impl WalkError {
    const fn value(message: &'static str) -> Self {
        Self {
            code: "invalid_value",
            message,
        }
    }
    const fn limits(message: &'static str) -> Self {
        Self {
            code: "limits_exceeded",
            message,
        }
    }
    const fn unsupported(message: &'static str) -> Self {
        Self {
            code: "unsupported_feature",
            message,
        }
    }
    pub const fn code(&self) -> &'static str {
        self.code
    }
}
impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for WalkError {}

type Result<T> = std::result::Result<T, WalkError>;

/// A bounds-checked cursor with the codec reader's acceptance rules.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn at(bytes: &'a [u8], pos: usize) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            return Err(WalkError::limits("binary byte limit"));
        }
        Ok(Self { bytes, pos })
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }
    fn read(&mut self, count: usize) -> Result<&'a [u8]> {
        if count > self.remaining() {
            return Err(WalkError::value("truncated binary"));
        }
        let start = self.pos;
        self.pos += count;
        Ok(&self.bytes[start..self.pos])
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.read(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.read(4)?.try_into().expect("4 bytes"),
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.read(8)?.try_into().expect("8 bytes"),
        ))
    }
    fn varuint(&mut self) -> Result<u32> {
        let mut value = 0u32;
        for n in 0..5 {
            let byte = self.byte()?;
            if n == 4 && byte > 15 {
                return Err(WalkError::value("overflowing varuint32"));
            }
            value |= u32::from(byte & 127) << (7 * n);
            if byte < 128 {
                if n > 0 && byte == 0 {
                    return Err(WalkError::value("noncanonical varuint32"));
                }
                return Ok(value);
            }
        }
        Err(WalkError::value("overflowing varuint32"))
    }
    /// An array length, bounded like the codec's arrays.
    fn count(&mut self) -> Result<usize> {
        let count = self.varuint()? as usize;
        if count > MAX_ELEMENTS {
            return Err(WalkError::limits("binary element limit"));
        }
        Ok(count)
    }
    /// `count` fixed-size elements. One bounds check covers them all; a truncated array fails the
    /// same way as reading its elements one by one.
    fn elements(&mut self, width: usize) -> Result<()> {
        let count = self.count()?;
        self.read(count * width)?;
        Ok(())
    }
    fn blob(&mut self) -> Result<&'a [u8]> {
        let count = self.varuint()? as usize;
        self.read(count)
    }
    fn string(&mut self) -> Result<&'a str> {
        std::str::from_utf8(self.blob()?).map_err(|_| WalkError::value("invalid UTF-8"))
    }
    fn optional(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WalkError::value("invalid optional presence byte")),
        }
    }
    fn variant(&mut self, arms: u32) -> Result<u32> {
        let index = self.varuint()?;
        if index >= arms {
            return Err(WalkError::value("variant selector out of range"));
        }
        Ok(index)
    }
    fn end(&self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(WalkError::value("trailing binary bytes"));
        }
        Ok(())
    }
}

/// One `account` stream row of a block's `table_delta[]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccountRow<'a> {
    /// The raw `bool` byte; the codec renders it as a number and does not range-check it.
    pub present: u8,
    pub data: &'a [u8],
    /// The row's position among all rows of all streams, from 0.
    pub ordinal: u32,
}

/// How a block's delta count was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaFraming {
    Declared,
    /// The pinned producer counts the duplicated resource index stream once and writes it twice.
    ResourceIndexCount,
}

/// Stream order the codec requires of a corrected payload.
const CORRECTED_ORDER: [&str; 13] = [
    "account",
    "account_metadata",
    "code",
    "contract_row_kv",
    "contract_index_kv",
    "global_property",
    "protocol_state",
    "permission",
    "permission_link",
    "resource_limits",
    "resource_usage",
    "resource_limits_state",
    "resource_limits_config",
];

/// The `account` rows of a chain-state payload, with the codec's delta framing correction.
///
/// Mirrors `codec::ship::decode_deltas`: the declared count is tried first; only an
/// `invalid_value` failure retries with one more delta, which must then name known streams in
/// order and include both resource streams.
pub fn account_rows<'a>(bytes: &'a [u8], rows: &mut Vec<AccountRow<'a>>) -> Result<DeltaFraming> {
    if bytes.len() > MAX_BYTES {
        return Err(WalkError::limits("delta byte limit"));
    }
    let (count, prefix) = declared_count(bytes)?;
    if count > MAX_ELEMENTS {
        return Err(WalkError::limits("delta count limit"));
    }
    rows.clear();
    match walk_deltas(bytes, prefix, count, rows, false) {
        Ok(()) => return Ok(DeltaFraming::Declared),
        Err(error) if error.code() == "invalid_value" => {}
        Err(error) => return Err(error),
    }
    if count >= MAX_ELEMENTS {
        return Err(WalkError::limits("delta count limit"));
    }
    // The codec re-decodes the corrected count followed by the original remainder; that buffer
    // is a byte longer when the new count needs another varuint byte.
    if bytes.len() - prefix + varuint_len(count + 1) > MAX_BYTES {
        return Err(WalkError::limits("binary byte limit"));
    }
    rows.clear();
    walk_deltas(bytes, prefix, count + 1, rows, true)?;
    Ok(DeltaFraming::ResourceIndexCount)
}

fn varuint_len(mut value: usize) -> usize {
    let mut length = 1;
    while value >= 128 {
        value >>= 7;
        length += 1;
    }
    length
}

fn declared_count(bytes: &[u8]) -> Result<(usize, usize)> {
    let mut count = 0usize;
    for i in 0..5 {
        let b = *bytes
            .get(i)
            .ok_or(WalkError::value("missing delta count"))?;
        if i == 4 && b > 15 {
            return Err(WalkError::value("delta count overflow"));
        }
        count |= ((b & 127) as usize) << (7 * i);
        if b & 128 == 0 {
            if i > 0 && b == 0 {
                return Err(WalkError::value("noncanonical delta count"));
            }
            return Ok((count, i + 1));
        }
    }
    Err(WalkError::value("unterminated delta count"))
}

fn walk_deltas<'a>(
    bytes: &'a [u8],
    start: usize,
    deltas: usize,
    rows: &mut Vec<AccountRow<'a>>,
    corrected: bool,
) -> Result<()> {
    let mut c = Cursor::at(bytes, start)?;
    let mut ordinal = 0u32;
    let mut previous: Option<usize> = None;
    // The codec checks stream names only after the whole payload decodes, so a name error
    // waits until the walk has seen every byte.
    let mut name_error: Option<WalkError> = None;
    let (mut limits, mut usage) = (false, false);
    for _ in 0..deltas {
        c.variant(1)?; // table_delta: [table_delta_v0]
        let name = c.string()?;
        let account = name == "account";
        for _ in 0..c.count()? {
            let present = c.byte()?;
            let data = c.blob()?;
            if account {
                rows.push(AccountRow {
                    present,
                    data,
                    ordinal,
                });
            }
            ordinal = ordinal
                .checked_add(1)
                .ok_or(WalkError::limits("delta ordinal overflow"))?;
        }
        if corrected {
            if name_error.is_none() {
                match CORRECTED_ORDER.iter().position(|entry| *entry == name) {
                    None => name_error = Some(WalkError::value("unknown corrected stream")),
                    Some(index) if previous.is_some_and(|p| index <= p) => {
                        name_error = Some(WalkError::value("incorrect corrected stream ordering"));
                    }
                    Some(index) => previous = Some(index),
                }
            }
            limits |= name == "resource_limits";
            usage |= name == "resource_usage";
        }
    }
    c.end()?;
    if let Some(error) = name_error {
        return Err(error);
    }
    if corrected && !(limits && usage) {
        return Err(WalkError::value("missing duplicate resource streams"));
    }
    Ok(())
}

/// `account` variant: `account_v0 {name, creation_date, abi}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Account<'a> {
    pub name: u64,
    pub abi: &'a [u8],
}
pub fn account(data: &[u8]) -> Result<Account<'_>> {
    let mut c = Cursor::at(data, 0)?;
    c.variant(1)?; // account: [account_v0]
    let name = c.u64()?;
    c.u32()?; // creation_date: block_timestamp_type, valid for every u32 slot
    let abi = c.blob()?;
    c.end()?;
    Ok(Account { name, abi })
}

/// One transaction of a `transaction_trace[]` payload, with the fields ABI history checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction<'a> {
    pub id: &'a [u8],
    pub status: u8,
    /// The raw `bool` byte; the codec renders it as a number.
    pub scheduled: u8,
    pub except: bool,
    pub error_code: bool,
    pub failed_dtrx_trace: bool,
    /// Index range of this transaction's actions in the shared action list.
    pub actions: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub receiver: u64,
    pub global_sequence: u64,
    pub abi_sequence: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action<'a> {
    pub ordinal: u32,
    pub receipt: Option<Receipt>,
    pub receiver: u64,
    pub account: u64,
    pub name: u64,
    pub data: &'a [u8],
    pub except: bool,
    pub error_code: bool,
}

/// Walk a complete `transaction_trace[]` payload. Nested `failed_dtrx_trace` values are walked
/// for well-formedness, and only their presence is reported.
pub fn transactions<'a>(
    bytes: &'a [u8],
    out: &mut Vec<Transaction<'a>>,
    actions: &mut Vec<Action<'a>>,
) -> Result<()> {
    out.clear();
    actions.clear();
    let mut c = Cursor::at(bytes, 0)?;
    for _ in 0..c.count()? {
        let tx = transaction(&mut c, actions, 0)?;
        out.push(tx);
    }
    c.end()
}

fn transaction<'a>(
    c: &mut Cursor<'a>,
    actions: &mut Vec<Action<'a>>,
    nesting: usize,
) -> Result<Transaction<'a>> {
    c.variant(1)?; // transaction_trace: [transaction_trace_v0]
    let id = c.read(32)?;
    let status = c.byte()?;
    c.u32()?; // cpu_usage_us
    c.varuint()?; // net_usage_words
    c.read(8)?; // elapsed
    c.read(8)?; // net_usage
    let scheduled = c.byte()?;
    let first = actions.len();
    for _ in 0..c.count()? {
        let next = action(c)?;
        actions.push(next);
    }
    let range = first..actions.len();
    if c.optional()? {
        c.read(16)?; // account_ram_delta: account_delta {name, int64}
    }
    let except = c.optional()?;
    if except {
        c.string()?;
    }
    let error_code = c.optional()?;
    if error_code {
        c.read(8)?;
    }
    let failed_dtrx_trace = c.optional()?;
    if failed_dtrx_trace {
        if nesting + 1 > MAX_TRACE_NESTING {
            return Err(WalkError::limits("value recursion limit"));
        }
        let mut nested = Vec::new();
        transaction(c, &mut nested, nesting + 1)?;
    }
    if c.optional()? {
        partial_transaction(c)?;
    }
    Ok(Transaction {
        id,
        status,
        scheduled,
        except,
        error_code,
        failed_dtrx_trace,
        actions: range,
    })
}

fn action<'a>(c: &mut Cursor<'a>) -> Result<Action<'a>> {
    let version = c.variant(2)?; // action_trace: [action_trace_v0, action_trace_v1]
    let ordinal = c.varuint()?;
    c.varuint()?; // creator_action_ordinal
    let receipt = if c.optional()? {
        c.variant(1)?; // action_receipt: [action_receipt_v0]
        let receiver = c.u64()?;
        c.read(32)?; // act_digest
        let global_sequence = c.u64()?;
        c.u64()?; // recv_sequence
        c.elements(16)?; // auth_sequence: account_auth_sequence[] {name, uint64}
        c.varuint()?; // code_sequence
        let abi_sequence = c.varuint()?;
        Some(Receipt {
            receiver,
            global_sequence,
            abi_sequence,
        })
    } else {
        None
    };
    let receiver = c.u64()?;
    let account = c.u64()?;
    let name = c.u64()?;
    c.elements(16)?; // authorization: permission_level[] {name, name}
    let data = c.blob()?;
    c.byte()?; // context_free
    c.read(8)?; // elapsed
    c.string()?; // console
    c.elements(16)?; // account_ram_deltas: account_delta[] {name, int64}
    let except = c.optional()?;
    if except {
        c.string()?;
    }
    let error_code = c.optional()?;
    if error_code {
        c.read(8)?;
    }
    if version == 1 {
        c.blob()?; // return_value
    }
    Ok(Action {
        ordinal,
        receipt,
        receiver,
        account,
        name,
        data,
        except,
        error_code,
    })
}

fn partial_transaction(c: &mut Cursor<'_>) -> Result<()> {
    c.variant(1)?; // partial_transaction: [partial_transaction_v0]
    c.u32()?; // expiration: time_point_sec, valid for every u32
    c.read(2)?; // ref_block_num
    c.u32()?; // ref_block_prefix
    c.varuint()?; // max_net_usage_words
    c.byte()?; // max_cpu_usage_ms
    c.varuint()?; // delay_sec
    for _ in 0..c.count()? {
        c.read(2)?; // extension.type
        c.blob()?; // extension.data
    }
    for _ in 0..c.count()? {
        signature(c)?;
    }
    for _ in 0..c.count()? {
        c.blob()?; // context_free_data
    }
    Ok(())
}

/// A Wire `signature`: the codec's crypto variants and frame bounds, without rendering.
fn signature(c: &mut Cursor<'_>) -> Result<()> {
    let tag = c.varuint()?;
    let start = c.pos;
    match tag {
        0 | 1 | 3 => {
            c.read(65)?;
        }
        2 => {
            c.read(65)?;
            crypto_blob(c)?;
            let client = crypto_blob(c)?;
            std::str::from_utf8(client)
                .map_err(|_| WalkError::value("invalid WebAuthn client JSON UTF-8"))?;
        }
        4 => {
            c.read(96)?;
        }
        5 => {
            if c.byte()? != 1 {
                return Err(WalkError::value("invalid BLS pointer presence"));
            }
            c.read(192)?;
        }
        _ => return Err(WalkError::unsupported("unknown Wire crypto variant")),
    }
    if c.pos - start > CRYPTO_FRAME_LIMIT {
        return Err(WalkError::limits("crypto raw frame limit"));
    }
    Ok(())
}
fn crypto_blob<'a>(c: &mut Cursor<'a>) -> Result<&'a [u8]> {
    let length = c.varuint()? as usize;
    if length > CRYPTO_FRAME_LIMIT {
        return Err(WalkError::limits("crypto variable frame limit"));
    }
    c.read(length)
}

/// Every type the walks read, with the exact definition they assume.
const STRUCTS: &[(&str, &[(&str, &str)])] = &[
    ("row", &[("present", "bool"), ("data", "bytes")]),
    ("table_delta_v0", &[("name", "string"), ("rows", "row[]")]),
    (
        "account_v0",
        &[
            ("name", "name"),
            ("creation_date", "block_timestamp_type"),
            ("abi", "bytes"),
        ],
    ),
    (
        "transaction_trace_v0",
        &[
            ("id", "checksum256"),
            ("status", "uint8"),
            ("cpu_usage_us", "uint32"),
            ("net_usage_words", "varuint32"),
            ("elapsed", "int64"),
            ("net_usage", "uint64"),
            ("scheduled", "bool"),
            ("action_traces", "action_trace[]"),
            ("account_ram_delta", "account_delta?"),
            ("except", "string?"),
            ("error_code", "uint64?"),
            ("failed_dtrx_trace", "transaction_trace?"),
            ("partial", "partial_transaction?"),
        ],
    ),
    (
        "action_trace_v0",
        &[
            ("action_ordinal", "varuint32"),
            ("creator_action_ordinal", "varuint32"),
            ("receipt", "action_receipt?"),
            ("receiver", "name"),
            ("act", "action"),
            ("context_free", "bool"),
            ("elapsed", "int64"),
            ("console", "string"),
            ("account_ram_deltas", "account_delta[]"),
            ("except", "string?"),
            ("error_code", "uint64?"),
        ],
    ),
    (
        "action_trace_v1",
        &[
            ("action_ordinal", "varuint32"),
            ("creator_action_ordinal", "varuint32"),
            ("receipt", "action_receipt?"),
            ("receiver", "name"),
            ("act", "action"),
            ("context_free", "bool"),
            ("elapsed", "int64"),
            ("console", "string"),
            ("account_ram_deltas", "account_delta[]"),
            ("except", "string?"),
            ("error_code", "uint64?"),
            ("return_value", "bytes"),
        ],
    ),
    (
        "action_receipt_v0",
        &[
            ("receiver", "name"),
            ("act_digest", "checksum256"),
            ("global_sequence", "uint64"),
            ("recv_sequence", "uint64"),
            ("auth_sequence", "account_auth_sequence[]"),
            ("code_sequence", "varuint32"),
            ("abi_sequence", "varuint32"),
        ],
    ),
    (
        "action",
        &[
            ("account", "name"),
            ("name", "name"),
            ("authorization", "permission_level[]"),
            ("data", "bytes"),
        ],
    ),
    (
        "permission_level",
        &[("actor", "name"), ("permission", "name")],
    ),
    ("account_delta", &[("account", "name"), ("delta", "int64")]),
    (
        "account_auth_sequence",
        &[("account", "name"), ("sequence", "uint64")],
    ),
    (
        "partial_transaction_v0",
        &[
            ("expiration", "time_point_sec"),
            ("ref_block_num", "uint16"),
            ("ref_block_prefix", "uint32"),
            ("max_net_usage_words", "varuint32"),
            ("max_cpu_usage_ms", "uint8"),
            ("delay_sec", "varuint32"),
            ("transaction_extensions", "extension[]"),
            ("signatures", "signature[]"),
            ("context_free_data", "bytes[]"),
        ],
    ),
    ("extension", &[("type", "uint16"), ("data", "bytes")]),
];
const VARIANTS: &[(&str, &[&str])] = &[
    ("table_delta", &["table_delta_v0"]),
    ("account", &["account_v0"]),
    ("transaction_trace", &["transaction_trace_v0"]),
    ("action_trace", &["action_trace_v0", "action_trace_v1"]),
    ("action_receipt", &["action_receipt_v0"]),
    ("partial_transaction", &["partial_transaction_v0"]),
];

/// Refuse a SHiP schema whose definition of any walked type differs from the walks' layouts.
pub fn check_ship_schema(schema: &Value) -> std::result::Result<(), String> {
    let find = |list: &str, name: &str| {
        schema[list]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["name"] == name))
    };
    for &(name, fields) in STRUCTS {
        let found = find("structs", name).ok_or_else(|| format!("SHiP schema lacks {name}"))?;
        let base = found["base"].as_str().unwrap_or("");
        let actual = found["fields"].as_array().map(|items| {
            items
                .iter()
                .map(|f| (f["name"].as_str(), f["type"].as_str()))
                .collect::<Vec<_>>()
        });
        let expected = fields
            .iter()
            .map(|(n, t)| (Some(*n), Some(*t)))
            .collect::<Vec<_>>();
        if !base.is_empty() || actual.as_ref() != Some(&expected) {
            return Err(format!(
                "SHiP schema defines {name} differently than the binary walk"
            ));
        }
    }
    for &(name, arms) in VARIANTS {
        let found = find("variants", name).ok_or_else(|| format!("SHiP schema lacks {name}"))?;
        let actual = found["types"]
            .as_array()
            .map(|items| items.iter().map(Value::as_str).collect::<Vec<_>>());
        if actual != Some(arms.iter().map(|a| Some(*a)).collect()) {
            return Err(format!(
                "SHiP schema defines {name} differently than the binary walk"
            ));
        }
    }
    // An alias would redirect a walked type name to another definition.
    let walked = STRUCTS
        .iter()
        .map(|(n, _)| *n)
        .chain(VARIANTS.iter().map(|(n, _)| *n));
    for name in walked {
        if schema["types"]
            .as_array()
            .is_some_and(|items| items.iter().any(|t| t["new_type_name"] == name))
        {
            return Err(format!("SHiP schema aliases walked type {name}"));
        }
    }
    Ok(())
}

/// The codec's `name` rendering.
pub fn name_string(value: u64) -> String {
    const ALPHABET: &[u8] = b".12345abcdefghijklmnopqrstuvwxyz";
    let mut text = String::with_capacity(13);
    for i in 0..13 {
        let shift = if i == 12 { 0 } else { 64 - 5 * (i + 1) };
        let mask = if i == 12 { 15 } else { 31 };
        text.push(ALPHABET[((value >> shift) & mask) as usize] as char);
    }
    text.trim_end_matches('.').into()
}

/// A name's u64 value. Only for the fixed names the scan compares against.
pub const fn name_value(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let mut value = 0u64;
    let mut i = 0;
    while i < bytes.len() && i < 13 {
        let c = bytes[i];
        let symbol = match c {
            b'.' => 0,
            b'1'..=b'5' => (c - b'1' + 1) as u64,
            b'a'..=b'z' => (c - b'a' + 6) as u64,
            _ => panic!("invalid name character"),
        };
        value |= if i == 12 {
            symbol & 15
        } else {
            (symbol & 31) << (64 - 5 * (i + 1))
        };
        i += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    //! The pinned codec is the oracle. On every input here a walk must reach the codec's
    //! verdict: the same values where both accept, the same error class where both refuse.
    use super::*;
    use crate::provider::ProtocolProvider;
    use serde_json::json;

    type Verdict<T> = std::result::Result<T, String>;
    type Rows = (Vec<(u8, Vec<u8>, u32)>, DeltaFraming);

    #[derive(Debug, PartialEq)]
    struct TxView {
        id: String,
        status: u64,
        scheduled: u64,
        except: bool,
        error_code: bool,
        failed: bool,
        actions: Vec<ActionView>,
    }
    #[derive(Debug, PartialEq)]
    struct ActionView {
        ordinal: u64,
        receipt: Option<(String, u64, u64)>,
        receiver: String,
        account: String,
        name: String,
        data: String,
    }

    fn class(error: impl std::fmt::Display) -> String {
        error
            .to_string()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_owned()
    }
    fn number(value: &Value) -> u64 {
        value
            .as_u64()
            .or_else(|| value.as_bool().map(u64::from))
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or_else(|| panic!("not a number: {value}"))
    }
    fn text(value: &Value) -> String {
        value.as_str().unwrap().to_ascii_lowercase()
    }
    fn tx_view(tx: &Value) -> TxView {
        TxView {
            id: text(&tx["id"]),
            status: number(&tx["status"]),
            scheduled: number(&tx["scheduled"]),
            except: !tx["except"].is_null(),
            error_code: !tx["error_code"].is_null(),
            failed: !tx["failed_dtrx_trace"].is_null(),
            actions: tx["action_traces"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| {
                    let a = &a[1];
                    ActionView {
                        ordinal: number(&a["action_ordinal"]),
                        receipt: (!a["receipt"].is_null()).then(|| {
                            let r = &a["receipt"][1];
                            (
                                text(&r["receiver"]),
                                number(&r["global_sequence"]),
                                number(&r["abi_sequence"]),
                            )
                        }),
                        receiver: text(&a["receiver"]),
                        account: text(&a["act"]["account"]),
                        name: text(&a["act"]["name"]),
                        data: text(&a["act"]["data"]),
                    }
                })
                .collect(),
        }
    }

    struct Oracle {
        provider: ProtocolProvider,
        schema: Value,
        capture: Value,
    }
    impl Oracle {
        fn new() -> Self {
            let capture: Value =
                serde_json::from_str(include_str!("../testdata/wire/ship-capture.json")).unwrap();
            let schema = serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
            Self {
                provider: "wire-sysio-6508c9e8-engineering-v1".parse().unwrap(),
                schema,
                capture,
            }
        }
        /// The captured block's chain-state and trace payloads.
        fn captured(&self) -> (Vec<u8>, Vec<u8>) {
            let frame = self.capture["vectors"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["id"] == "captured-ship-block-result")
                .unwrap();
            let envelope = self
                .provider
                .decode_value(
                    &self.schema,
                    "result",
                    &hex::decode(frame["hex"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
            let field = |name: &str| hex::decode(envelope[1][name].as_str().unwrap()).unwrap();
            (field("deltas"), field("traces"))
        }
        fn encode(&self, ty: &str, value: &Value) -> Vec<u8> {
            self.provider.encode_value(&self.schema, ty, value).unwrap()
        }
        fn decode(&self, ty: &str, bytes: &[u8]) -> Verdict<Value> {
            self.provider
                .decode_value(&self.schema, ty, bytes)
                .map_err(class)
        }
        fn deltas(&self, bytes: &[u8]) -> Verdict<Rows> {
            let payload = self
                .provider
                .decode_delta_payload(&self.schema, bytes)
                .map_err(class)?;
            let mut rows = Vec::new();
            let mut ordinal = 0;
            for delta in payload["deltas"].as_array().unwrap() {
                for row in delta[1]["rows"].as_array().unwrap() {
                    if delta[1]["name"] == "account" {
                        let data = hex::decode(row["data"].as_str().unwrap()).unwrap();
                        rows.push((number(&row["present"]) as u8, data, ordinal));
                    }
                    ordinal += 1;
                }
            }
            let framing = if payload["framing"]["compatibility"].is_null() {
                DeltaFraming::Declared
            } else {
                DeltaFraming::ResourceIndexCount
            };
            Ok((rows, framing))
        }
        fn account(&self, data: &[u8]) -> Verdict<(String, Vec<u8>)> {
            let value = self.decode("account", data)?;
            let abi = hex::decode(value[1]["abi"].as_str().unwrap()).unwrap();
            Ok((text(&value[1]["name"]), abi))
        }
        fn traces(&self, bytes: &[u8]) -> Verdict<Vec<TxView>> {
            let value = self.decode("transaction_trace[]", bytes)?;
            Ok(value
                .as_array()
                .unwrap()
                .iter()
                .map(|tx| tx_view(&tx[1]))
                .collect())
        }
    }

    fn walk_deltas(bytes: &[u8]) -> Verdict<Rows> {
        let mut rows = Vec::new();
        let framing = account_rows(bytes, &mut rows).map_err(class)?;
        let rows = rows
            .iter()
            .map(|r| (r.present, r.data.to_vec(), r.ordinal))
            .collect();
        Ok((rows, framing))
    }
    fn walk_account(data: &[u8]) -> Verdict<(String, Vec<u8>)> {
        let account = account(data).map_err(class)?;
        Ok((name_string(account.name), account.abi.to_vec()))
    }
    fn walk_traces(bytes: &[u8]) -> Verdict<Vec<TxView>> {
        let (mut txs, mut actions) = (Vec::new(), Vec::new());
        transactions(bytes, &mut txs, &mut actions).map_err(class)?;
        Ok(txs
            .iter()
            .map(|tx| TxView {
                id: hex::encode(tx.id),
                status: tx.status.into(),
                scheduled: tx.scheduled.into(),
                except: tx.except,
                error_code: tx.error_code,
                failed: tx.failed_dtrx_trace,
                actions: actions[tx.actions.clone()]
                    .iter()
                    .map(|a| ActionView {
                        ordinal: a.ordinal.into(),
                        receipt: a.receipt.map(|r| {
                            (
                                name_string(r.receiver),
                                r.global_sequence,
                                r.abi_sequence.into(),
                            )
                        }),
                        receiver: name_string(a.receiver),
                        account: name_string(a.account),
                        name: name_string(a.name),
                        data: hex::encode(a.data),
                    })
                    .collect(),
            })
            .collect())
    }

    /// The input, its prefixes and five single-byte mutations at each of `positions`, and the
    /// input plus a trailing byte, must each get the same verdict from the walk and the codec.
    /// Returns how many of those inputs were accepted.
    fn differential<T: PartialEq + std::fmt::Debug>(
        input: &[u8],
        positions: impl Iterator<Item = usize> + Clone,
        walk: impl Fn(&[u8]) -> Verdict<T>,
        codec: impl Fn(&[u8]) -> Verdict<T>,
    ) -> usize {
        let mut accepted = 0;
        let mut check = |bytes: &[u8], what: &dyn Fn() -> String| {
            let verdict = walk(bytes);
            assert_eq!(verdict, codec(bytes), "{}", what());
            accepted += usize::from(verdict.is_ok());
        };
        check(input, &|| "the input".into());
        for end in positions.clone().filter(|p| *p < input.len()) {
            check(&input[..end], &|| format!("the first {end} bytes"));
        }
        let mut extended = input.to_vec();
        extended.push(0);
        check(&extended, &|| "a trailing byte".into());
        let mut mutated = input.to_vec();
        for i in positions.filter(|p| *p < input.len()) {
            let original = input[i];
            for value in [original ^ 1, original ^ 0x80, 0, 0xff, 2] {
                if value != original {
                    mutated[i] = value;
                    check(&mutated, &|| format!("byte {i} set to {value:#04x}"));
                }
            }
            mutated[i] = original;
        }
        accepted
    }
    /// Every position of a short input; the ends and a stride of a long one.
    fn sampled(length: usize) -> impl Iterator<Item = usize> + Clone {
        (0..length)
            .filter(move |i| length <= 1024 || *i < 160 || *i + 160 >= length || i % 211 == 0)
    }

    fn account_bytes(oracle: &Oracle, abi: &str) -> Vec<u8> {
        oracle.encode(
            "account",
            &json!(["account_v0", {"name":"target","creation_date":"2025-01-01T00:00:00.000","abi":abi}]),
        )
    }
    /// One executed transaction with one receipted action, shaped like the captured trace.
    fn small_trace(oracle: &Oracle) -> Value {
        let (_, traces) = oracle.captured();
        let mut tx = oracle.decode("transaction_trace[]", &traces).unwrap()[0].clone();
        let mut action = tx[1]["action_traces"][0].clone();
        action[1]["act"] = json!({"account":"sysio","name":"setabi","authorization":[{"actor":"sysio","permission":"active"}],"data":"0a0b"});
        action[1]["console"] = json!("");
        action[1]["account_ram_deltas"] = json!([]);
        action[1]["receipt"][1]["auth_sequence"] = json!([{"account":"sysio","sequence":"7"}]);
        tx[1]["action_traces"] = json!([action]);
        tx[1]["account_ram_delta"] = Value::Null;
        tx[1]["failed_dtrx_trace"] = Value::Null;
        tx[1]["partial"] = Value::Null;
        tx
    }
    /// `small_trace` encoded with a partial transaction that carries one signature.
    fn signed_trace(oracle: &Oracle, signature: &[u8]) -> Vec<u8> {
        let mut bytes = oracle.encode("transaction_trace[]", &json!([small_trace(oracle)]));
        assert_eq!(bytes.pop(), Some(0), "the absent partial is the last byte");
        bytes.extend_from_slice(&[1, 0]); // present, partial_transaction_v0
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]); // expiration, ref block
        bytes.extend_from_slice(&[0, 30, 0]); // net words, cpu ms, delay
        bytes.extend_from_slice(&[1, 0x34, 0x12, 2, 0xaa, 0xbb]); // one extension
        bytes.push(1);
        bytes.extend_from_slice(signature);
        bytes.extend_from_slice(&[1, 1, 0xcc]); // context-free data
        bytes
    }

    #[test]
    fn captured_block_walks_like_the_codec() {
        let oracle = Oracle::new();
        let (deltas, traces) = oracle.captured();
        let rows = walk_deltas(&deltas);
        assert_eq!(rows, oracle.deltas(&deltas));
        let (rows, framing) = rows.unwrap();
        assert_eq!(framing, DeltaFraming::ResourceIndexCount);
        assert!(!rows.is_empty());
        for (_, data, _) in &rows {
            assert_eq!(walk_account(data), oracle.account(data));
        }
        let txs = walk_traces(&traces);
        assert_eq!(txs, oracle.traces(&traces));
        assert!(!txs.unwrap().is_empty());
    }

    #[test]
    fn captured_block_mutations_get_the_codec_verdict() {
        let oracle = Oracle::new();
        let (deltas, traces) = oracle.captured();
        let positions = sampled(deltas.len());
        assert!(differential(&deltas, positions, walk_deltas, |b| oracle.deltas(b)) > 0);
        let positions = sampled(traces.len());
        assert!(differential(&traces, positions, walk_traces, |b| oracle.traces(b)) > 0);
    }

    #[test]
    fn small_delta_and_account_mutations_get_the_codec_verdict() {
        let oracle = Oracle::new();
        let account = hex::encode(account_bytes(&oracle, "0e"));
        let deltas = oracle.encode(
            "table_delta[]",
            &json!([
                ["table_delta_v0", {"name":"resource_limits","rows":[{"present":1,"data":"00"}]}],
                ["table_delta_v0", {"name":"account","rows":[{"present":1,"data":account},{"present":0,"data":account}]}],
            ]),
        );
        assert_eq!(walk_deltas(&deltas).unwrap().0.len(), 2);
        differential(&deltas, 0..deltas.len(), walk_deltas, |b| oracle.deltas(b));
        let data = account_bytes(&oracle, "0e0f");
        differential(&data, 0..data.len(), walk_account, |b| oracle.account(b));
    }

    #[test]
    fn small_trace_mutations_get_the_codec_verdict() {
        let oracle = Oracle::new();
        let tx = small_trace(&oracle);
        let traces = oracle.encode("transaction_trace[]", &json!([tx]));
        assert_eq!(walk_traces(&traces).unwrap()[0].actions.len(), 1);
        differential(&traces, 0..traces.len(), walk_traces, |b| oracle.traces(b));
        let mut failed = tx.clone();
        failed[1]["except"] = json!("failed");
        failed[1]["error_code"] = json!("3");
        failed[1]["failed_dtrx_trace"] = tx;
        let traces = oracle.encode("transaction_trace[]", &json!([failed]));
        let view = walk_traces(&traces).unwrap();
        assert!(view[0].failed && view[0].except && view[0].error_code);
        differential(&traces, sampled(traces.len()), walk_traces, |b| {
            oracle.traces(b)
        });
    }

    #[test]
    fn every_signature_variant_gets_the_codec_verdict() {
        let oracle = Oracle::new();
        let fixed = |tag: u8, length: usize| [vec![tag], vec![0x1f; length]].concat();
        let mut webauthn = fixed(2, 65);
        webauthn.extend_from_slice(&[3, 1, 2, 3, 7]);
        webauthn.extend_from_slice(br#"{"t":1}"#);
        let mut bls = vec![5, 1];
        bls.extend_from_slice(&[0x2e; 192]);
        let accepted = [
            fixed(0, 65),
            fixed(1, 65),
            webauthn,
            fixed(3, 65),
            fixed(4, 96),
            bls,
        ];
        for signature in &accepted {
            let traces = signed_trace(&oracle, signature);
            assert!(
                walk_traces(&traces).is_ok(),
                "signature tag {}",
                signature[0]
            );
            // Mutate the whole partial transaction: header, extension, signature and tail.
            let start = traces.len() - signature.len() - 27;
            differential(&traces, start..traces.len(), walk_traces, |b| {
                oracle.traces(b)
            });
        }
        let unknown = signed_trace(&oracle, &fixed(6, 65));
        assert_eq!(walk_traces(&unknown), Err("unsupported_feature".into()));
        assert_eq!(oracle.traces(&unknown), Err("unsupported_feature".into()));
    }

    fn varuint(mut value: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 127) as u8;
            value >>= 7;
            out.push(if value == 0 { byte } else { byte | 128 });
            if value == 0 {
                return out;
            }
        }
    }

    #[test]
    fn size_and_count_bounds_get_the_codec_verdict() {
        let oracle = Oracle::new();
        let limits = Err::<(), String>("limits_exceeded".into());
        for bytes in [
            varuint(MAX_ELEMENTS + 1),
            vec![0; MAX_BYTES + 1],
            // One delta, named "", whose row count is past the bound.
            [vec![1, 0, 0], varuint(MAX_ELEMENTS + 1)].concat(),
        ] {
            let walked = walk_deltas(&bytes).map(|_| ());
            assert_eq!(walked, oracle.deltas(&bytes).map(|_| ()));
            let walked_traces = walk_traces(&bytes).map(|_| ());
            assert_eq!(walked_traces, oracle.traces(&bytes).map(|_| ()));
            assert!(walked == limits || walked_traces == limits);
        }
        // The corrected count of a payload of exactly MAX_BYTES needs another varuint byte.
        let mut full = varuint(127);
        full.resize(MAX_BYTES, 0);
        assert_eq!(
            walk_deltas(&full).map(|_| ()),
            oracle.deltas(&full).map(|_| ())
        );
        assert_eq!(walk_deltas(&full).map(|_| ()), limits);
        // A corrected count of MAX_ELEMENTS + 1 would exceed the element bound.
        let at_bound = varuint(MAX_ELEMENTS);
        assert_eq!(
            walk_deltas(&at_bound).map(|_| ()),
            oracle.deltas(&at_bound).map(|_| ())
        );
    }

    /// A trace whose `failed_dtrx_trace` chain is `depth` levels deep, or empty when the
    /// encoder refuses it.
    fn nested(oracle: &Oracle, depth: usize, with_actions: bool) -> Vec<u8> {
        let mut leaf = small_trace(oracle);
        if !with_actions {
            leaf[1]["action_traces"] = json!([]);
        }
        let mut tx = leaf.clone();
        for _ in 0..depth {
            let mut outer = leaf.clone();
            outer[1]["failed_dtrx_trace"] = tx;
            tx = outer;
        }
        oracle
            .provider
            .encode_value(&oracle.schema, "transaction_trace[]", &json!([tx]))
            .unwrap_or_default()
    }

    #[test]
    fn nested_failed_traces_stop_where_documented() {
        let oracle = Oracle::new();
        let at_bound = nested(&oracle, MAX_TRACE_NESTING, true);
        assert!(!at_bound.is_empty());
        assert!(walk_traces(&at_bound).is_ok());
        assert_eq!(walk_traces(&at_bound), oracle.traces(&at_bound));
        // One level deeper the encoder refuses too, so wrap the at-bound trace by hand: a copy
        // of the leaf whose failed_dtrx_trace holds it.
        let leaf = oracle.encode("transaction_trace[]", &json!([small_trace(&oracle)]));
        let tail = [0u8, 0, 0, 0]; // except, error_code, failed_dtrx_trace, partial: all absent
        assert!(leaf.ends_with(&tail));
        let mut deeper = leaf[..leaf.len() - 2].to_vec();
        deeper.push(1);
        deeper.extend_from_slice(&at_bound[1..]); // the at-bound trace, without its array count
        deeper.push(0); // partial
        assert_eq!(walk_traces(&deeper), Err("limits_exceeded".into()));
        assert_eq!(oracle.traces(&deeper), Err("limits_exceeded".into()));
        // Traces without actions are shallower: the codec follows them up to 20 levels.
        for depth in [MAX_TRACE_NESTING + 1, 20] {
            let shallow = nested(&oracle, depth, false);
            assert!(oracle.traces(&shallow).is_ok(), "codec at {depth}");
            assert_eq!(walk_traces(&shallow), Err("limits_exceeded".into()));
        }
        assert!(nested(&oracle, 21, false).is_empty());
    }

    #[test]
    fn traversal_bounds_are_not_applied() {
        let oracle = Oracle::new();
        // One transaction with 60,000 receipt-less actions: over a million decode nodes.
        let mut tx = small_trace(&oracle);
        let mut action = tx[1]["action_traces"][0].clone();
        action[1]["receipt"] = Value::Null;
        tx[1]["action_traces"] = json!([action]);
        let one = oracle.encode("transaction_trace[]", &json!([tx]));
        let single = oracle.encode("action_trace", &action);
        // The action array count (1) sits right before the one action's bytes.
        let at = one
            .windows(single.len() + 1)
            .position(|w| w[0] == 1 && w[1..] == single[..])
            .unwrap();
        let mut bytes = one[..at].to_vec();
        bytes.extend(varuint(60_000));
        for _ in 0..60_000 {
            bytes.extend_from_slice(&single);
        }
        bytes.extend_from_slice(&one[at + 1 + single.len()..]);
        assert_eq!(walk_traces(&bytes).unwrap()[0].actions.len(), 60_000);
        assert_eq!(oracle.traces(&bytes), Err("limits_exceeded".into()));
    }

    #[test]
    fn schema_check_refuses_other_layouts() {
        let oracle = Oracle::new();
        let schema = &oracle.schema;
        assert_eq!(check_ship_schema(schema), Ok(()));
        let edited = |list: &str, name: &str, change: &dyn Fn(&mut Value)| {
            let mut schema = schema.clone();
            let item = schema[list]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|item| item["name"] == name)
                .unwrap();
            change(item);
            check_ship_schema(&schema)
        };
        assert!(
            edited("structs", "account_v0", &|i| i["fields"][2]["type"] =
                json!("string"))
            .is_err()
        );
        assert!(
            edited("structs", "action_trace_v1", &|i| {
                i["fields"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"name":"extra","type":"uint8"}))
            })
            .is_err()
        );
        assert!(edited("structs", "row", &|i| i["base"] = json!("extension")).is_err());
        assert!(
            edited("variants", "action_trace", &|i| {
                i["types"].as_array_mut().unwrap().reverse()
            })
            .is_err()
        );
        let mut aliased = schema.clone();
        let alias = json!({"new_type_name":"action","type":"bytes"});
        match aliased["types"].as_array_mut() {
            Some(types) => types.push(alias),
            None => aliased["types"] = json!([alias]),
        }
        assert!(check_ship_schema(&aliased).is_err());
    }

    #[test]
    fn names_read_like_the_codec() {
        let oracle = Oracle::new();
        let codec = |value: u64| text(&oracle.decode("name", &value.to_le_bytes()).unwrap());
        for value in [
            0,
            1,
            15,
            u64::MAX,
            1 << 63,
            name_value("sysio"),
            name_value("setabi"),
            name_value("a.b"),
            name_value("zzzzzzzzzzzzj"),
            0x5530_ea03_3482_a600,
        ] {
            assert_eq!(name_string(value), codec(value), "{value:#x}");
        }
        for text in [
            "sysio",
            "setabi",
            "a.b",
            "eosio.token",
            "zzzzzzzzzzzzj",
            "1",
            "",
        ] {
            assert_eq!(name_string(name_value(text)), text);
        }
    }
}
