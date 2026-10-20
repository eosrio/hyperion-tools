//! Lossless JSON boundaries for uint64 sequence identities (ADR 0053).
//!
//! Binary readers retain `u64`. Only known sequence metadata uses these helpers; contract values,
//! block numbers, ordinals and uint32 ABI/code counters keep their own representations.

use std::fmt::Write;

/// Largest integer that a legacy JavaScript JSON-number caller can preserve exactly.
pub const MAX_SAFE_JSON_SEQUENCE: u64 = 9_007_199_254_740_991;

/// Parse canonical unsigned base-ten uint64 text. No trimming or numeric coercion is allowed.
pub fn parse_sequence(text: &str) -> Option<u64> {
    if text.is_empty()
        || text.len() > 20
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

/// Accept canonical text and explicitly bounded legacy integer JSON numbers.
///
/// A number above 2^53-1 is rejected even when serde can retain its original integer token: the
/// sender may already have rounded it. Floating point, exponent and signed representations are
/// not accepted. Responses always use strings, including responses to legacy numeric requests.
pub fn sequence_from_json(value: &serde_json::Value) -> Option<u64> {
    if let Some(text) = value.as_str() {
        parse_sequence(text)
    } else {
        value.as_u64().filter(|n| *n <= MAX_SAFE_JSON_SEQUENCE)
    }
}

/// Append the canonical decimal JSON string for an exact binary sequence.
pub fn push_sequence_json(out: &mut String, sequence: u64) {
    // Writing to a String cannot fail. Formatting an integer never needs JSON escaping.
    write!(out, "\"{sequence}\"").expect("writing to a String");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_boundaries_remain_exact_strings() {
        for (value, expected) in [
            (0, "0"),
            (1, "1"),
            (9_007_199_254_740_991, "9007199254740991"),
            (9_007_199_254_740_992, "9007199254740992"),
            (9_007_199_254_740_993, "9007199254740993"),
            (9_223_372_036_854_775_807, "9223372036854775807"),
            (9_223_372_036_854_775_808, "9223372036854775808"),
            (9_223_372_036_854_775_809, "9223372036854775809"),
            (18_446_744_073_709_551_615, "18446744073709551615"),
        ] {
            assert_eq!(parse_sequence(expected), Some(value));
            let mut output = String::new();
            push_sequence_json(&mut output, value);
            let decoded: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(decoded.as_str(), Some(expected));
        }
    }

    #[test]
    fn rejects_noncanonical_or_out_of_range_text() {
        for input in [
            "",
            "+1",
            "-1",
            "-0",
            " 1",
            "1 ",
            "\t1",
            "01",
            "00",
            "1.0",
            "1e3",
            "1E3",
            "18446744073709551616",
            "999999999999999999999",
            "１２",
            "1\n",
        ] {
            assert_eq!(parse_sequence(input), None, "accepted {input:?}");
            assert_eq!(sequence_from_json(&serde_json::Value::from(input)), None);
        }
    }

    #[test]
    fn legacy_numbers_are_safe_integers_only() {
        for (input, expected) in [
            ("0", 0),
            ("42", 42),
            ("9007199254740991", 9_007_199_254_740_991),
        ] {
            assert_eq!(
                sequence_from_json(&serde_json::from_str(input).unwrap()),
                Some(expected)
            );
        }
        for input in [
            "9007199254740992",
            "9007199254740993",
            "9223372036854775808",
            "18446744073709551615",
            "-1",
            "-0",
            "1.0",
            "1e3",
            "true",
            "null",
            "[]",
            "{}",
        ] {
            assert_eq!(
                sequence_from_json(&serde_json::from_str(input).unwrap()),
                None,
                "accepted {input}"
            );
        }
    }
}
