//! Small Antelope trace fixture with independent uint64 receipt values.
//!
//! This encodes the documented v0 binary layout directly; it does not call the parser or JSON
//! sequence helpers under test. It establishes boundary behavior, not Wire decode qualification.

/// Independent literal values around JSON, signed-long and uint64 boundaries.
pub const SEQUENCE_BOUNDARIES: &[(u64, &str)] = &[
    (0, "0"),
    (1, "1"),
    (9_007_199_254_740_991, "9007199254740991"),
    (9_007_199_254_740_992, "9007199254740992"),
    (9_007_199_254_740_993, "9007199254740993"),
    (9_223_372_036_854_775_807, "9223372036854775807"),
    (9_223_372_036_854_775_808, "9223372036854775808"),
    (9_223_372_036_854_775_809, "9223372036854775809"),
    (18_446_744_073_709_551_615, "18446744073709551615"),
];

fn varuint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(byte | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

/// One transaction with the given `(global, receiver, authorization)` sequences, one action each.
/// Code/ABI counters are uint32::MAX, ordinals are numeric, and raw act.data is `DEAD`.
pub fn transaction(sequences: &[(u64, u64, u64)]) -> Vec<u8> {
    let mut out = vec![1, 0]; // one transaction_trace, v0
    out.extend_from_slice(&[0x12; 32]); // transaction id
    out.push(0); // executed
    out.extend_from_slice(&7u32.to_le_bytes()); // cpu
    out.push(2); // net words
    out.extend_from_slice(&0i64.to_le_bytes()); // elapsed
    out.extend_from_slice(&0u64.to_le_bytes()); // net usage
    out.push(0); // scheduled
    varuint(&mut out, sequences.len().try_into().unwrap());
    for (index, (global, receiver, auth)) in sequences.iter().enumerate() {
        out.push(0); // action_trace_v0
        varuint(&mut out, (index + 1).try_into().unwrap()); // action ordinal
        out.extend_from_slice(&[0, 1, 0]); // creator ordinal, receipt present, receipt v0
        out.extend_from_slice(&0u64.to_le_bytes()); // receiver name
        out.extend_from_slice(&[0xab; 32]); // action digest
        out.extend_from_slice(&global.to_le_bytes());
        out.extend_from_slice(&receiver.to_le_bytes());
        out.push(1); // one authorization sequence
        out.extend_from_slice(&0u64.to_le_bytes()); // auth account name
        out.extend_from_slice(&auth.to_le_bytes());
        varuint(&mut out, u32::MAX); // code sequence
        varuint(&mut out, u32::MAX); // ABI sequence
        out.extend_from_slice(&[0; 24]); // receiver, action account and action name
        out.push(0); // authorization vector
        out.extend_from_slice(&[2, 0xde, 0xad]); // act.data bytes
        out.push(0); // context free
        out.extend_from_slice(&0i64.to_le_bytes()); // elapsed
        out.extend_from_slice(&[0, 0, 0, 0]); // console, RAM deltas, except, error code
    }
    out.extend_from_slice(&[0; 5]); // transaction optionals
    out
}
