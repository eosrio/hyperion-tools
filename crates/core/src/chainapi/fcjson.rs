//! fc's JSON writing, as nodeos formats every response (Spring `libfc/src/io/json.cpp`,
//! `libfc/src/utf8.cpp`, `libfc/src/variant.cpp`).
//!
//!   * Strings go through [`escape_string`]: `"`, `\`, TAB, LF and CR take their short escapes,
//!     every other byte below 0x20 and 0x7f become `\u00xx`, and then the result is cleaned
//!     with [`prune_invalid_utf8`] when it is not valid UTF-8 in fc's sense.
//!   * Integers above `0xffffffff` in magnitude are written as quoted decimal strings
//!     (`stringify_large_ints_and_doubles`, nodeos's default), smaller ones as numbers.
//!   * Byte arrays and checksums are lowercase hex strings; empty bytes are `""`.
//!
//! Log message arguments echoed into error details first pass through fc's `clean_append`,
//! which is [`escape_string`] without the short escapes ([`clean`]); the JSON writer then
//! escapes the message as any other string, so a backslash `clean` produced is doubled.

/// Lowercase hex digits, as `fc::to_hex` writes them.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// The largest integer magnitude fc writes as a JSON number.
const MAX_JSON_NUMBER: u64 = 0xffff_ffff;

/// fc `escape_string`. With `escape_control_chars`, `"`, `\`, TAB, LF and CR become `\"`, `\\`,
/// `\t`, `\n` and `\r`; without it they are kept as they are. Either way the other bytes below
/// 0x20 and 0x7f become `\u00xx` (lowercase), and the result is passed through
/// [`prune_invalid_utf8`] unless it is valid UTF-8 without C1 controls.
pub fn escape_string(s: &[u8], escape_control_chars: bool) -> Vec<u8> {
    let mut r = Vec::with_capacity(s.len() + 13);
    for &b in s {
        match b {
            b'\t' | b'\n' | b'\r' | b'\\' | b'"' if escape_control_chars => {
                r.push(b'\\');
                r.push(match b {
                    b'\t' => b't',
                    b'\n' => b'n',
                    b'\r' => b'r',
                    other => other,
                });
            }
            b'\t' | b'\n' | b'\r' => r.push(b),
            0x00..=0x1f | 0x7f => push_u00(&mut r, b),
            _ => r.push(b),
        }
    }
    if is_valid_utf8(&r) {
        r
    } else {
        prune_invalid_utf8(&r)
    }
}

/// fc `clean_append`: what a log message argument becomes inside the message text.
pub fn clean(s: &[u8]) -> Vec<u8> {
    escape_string(s, false)
}

/// fc `is_valid_utf8`: well-formed UTF-8 (utf8cpp's rules: no overlong forms, no surrogates,
/// nothing above U+10FFFF) that holds no C1 control (U+0080 to U+009F).
pub fn is_valid_utf8(s: &[u8]) -> bool {
    let mut at = 0;
    while at < s.len() {
        match next_code_point(&s[at..]) {
            Some((len, cp)) if !is_c1(cp) => at += len,
            _ => return false,
        }
    }
    true
}

/// fc `prune_invalid_utf8`: drop, one byte at a time, every byte that does not start a
/// well-formed sequence (so a bad sequence loses its lead byte, and then each stray
/// continuation byte), and write each C1 control as the six characters `\u00xx`.
pub fn prune_invalid_utf8(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut at = 0;
    while at < s.len() {
        match next_code_point(&s[at..]) {
            Some((len, cp)) if !is_c1(cp) => {
                out.extend_from_slice(&s[at..at + len]);
                at += len;
            }
            Some((len, cp)) => {
                // fc steps over the lead byte; its continuation byte is then an invalid lead
                // and is dropped too.
                push_u00(&mut out, cp as u8);
                at += len;
            }
            None => at += 1,
        }
    }
    out
}

/// A JSON string: `"`, the [`escape_string`] of `s` with the short escapes, `"`.
pub fn write_str(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    out.extend_from_slice(&escape_string(s, true));
    out.push(b'"');
}

/// An unsigned integer, quoted above `0xffffffff`.
pub fn write_u64(out: &mut Vec<u8>, v: u64) {
    if v > MAX_JSON_NUMBER {
        out.push(b'"');
        out.extend_from_slice(v.to_string().as_bytes());
        out.push(b'"');
    } else {
        out.extend_from_slice(v.to_string().as_bytes());
    }
}

/// A signed integer, quoted when its magnitude exceeds `0xffffffff`.
pub fn write_i64(out: &mut Vec<u8>, v: i64) {
    if v.unsigned_abs() > MAX_JSON_NUMBER {
        out.push(b'"');
        out.extend_from_slice(v.to_string().as_bytes());
        out.push(b'"');
    } else {
        out.extend_from_slice(v.to_string().as_bytes());
    }
}

/// Bytes or a checksum: a quoted lowercase hex string, `""` when empty.
pub fn write_hex(out: &mut Vec<u8>, b: &[u8]) {
    out.reserve(b.len() * 2 + 2);
    out.push(b'"');
    for &byte in b {
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 15)]);
    }
    out.push(b'"');
}

/// `\u00` and two lowercase hex digits.
fn push_u00(out: &mut Vec<u8>, b: u8) {
    out.extend_from_slice(b"\\u00");
    out.push(HEX[usize::from(b >> 4)]);
    out.push(HEX[usize::from(b & 15)]);
}

fn is_c1(cp: u32) -> bool {
    (0x80..=0x9f).contains(&cp)
}

/// The well-formed UTF-8 sequence at the start of `s` (non-empty), as utf8cpp's
/// `validate_next` decodes it: its length and code point, or `None`.
fn next_code_point(s: &[u8]) -> Option<(usize, u32)> {
    let lead = s[0];
    let len = match lead {
        0x00..=0x7f => return Some((1, u32::from(lead))),
        _ if lead >> 5 == 0b110 => 2,
        _ if lead >> 4 == 0b1110 => 3,
        _ if lead >> 3 == 0b11110 => 4,
        _ => return None,
    };
    let seq = s.get(..len)?;
    if !seq[1..].iter().all(|b| b >> 6 == 0b10) {
        return None;
    }
    let lead_bits = u32::from(lead) & (0x7f >> len);
    let cp = seq[1..]
        .iter()
        .fold(lead_bits, |cp, b| (cp << 6) | u32::from(b & 0x3f));
    let shortest = match cp {
        0..=0x7f => 1,
        0x80..=0x7ff => 2,
        0x800..=0xffff => 3,
        _ => 4,
    };
    let valid = cp <= 0x10_ffff && !(0xd800..=0xdfff).contains(&cp) && shortest == len;
    valid.then_some((len, cp))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(s: &[u8], short: bool) -> String {
        String::from_utf8(escape_string(s, short)).unwrap()
    }

    /// fc's escape table for one ASCII byte, written out from `json.cpp`.
    fn expected_ascii(b: u8, short: bool) -> String {
        match b {
            b'\t' if short => "\\t".into(),
            b'\n' if short => "\\n".into(),
            b'\r' if short => "\\r".into(),
            b'\\' if short => "\\\\".into(),
            b'"' if short => "\\\"".into(),
            b'\t' | b'\n' | b'\r' => char::from(b).to_string(),
            0x00..=0x1f | 0x7f => format!("\\u{b:04x}"),
            _ => char::from(b).to_string(),
        }
    }

    #[test]
    fn every_ascii_byte_escapes_as_fc_does_in_both_modes() {
        for short in [true, false] {
            for b in 0u8..=0x7f {
                assert_eq!(
                    esc(&[b], short),
                    expected_ascii(b, short),
                    "{b:#04x} {short}"
                );
            }
        }
        assert_eq!(
            esc(&[0x0b, 0x0c, 0x08, 0x1f], true),
            "\\u000b\\u000c\\u0008\\u001f"
        );
        assert_eq!(esc(b"a\"b\\c", true), "a\\\"b\\\\c");
        assert_eq!(esc(b"a\"b\\c", false), "a\"b\\c");
    }

    #[test]
    fn valid_utf8_passes_through() {
        for s in [
            "é",
            "€",
            "𝄞",
            "\u{7ff}",
            "\u{800}",
            "\u{ffff}",
            "\u{10000}",
            "\u{10ffff}",
        ] {
            assert!(is_valid_utf8(s.as_bytes()), "{s}");
            assert_eq!(esc(s.as_bytes(), true), s);
        }
        // U+00A0 is just above the C1 range.
        assert_eq!(esc("\u{a0}".as_bytes(), true), "\u{a0}");
    }

    #[test]
    fn invalid_utf8_is_pruned_byte_by_byte() {
        let cases: &[(&[u8], &[u8])] = &[
            // A lone continuation byte, a lone lead byte, bytes that never lead.
            (b"a\x80b", b"ab"),
            (b"a\xc3", b"a"),
            (b"\xff\xfe\xf8x", b"x"),
            // A truncated three-byte sequence: the lead goes, then its stray continuation.
            (b"\xe2\x82A", b"A"),
            // Overlong forms and surrogates.
            (b"\xc0\xafz", b"z"),
            (b"\xe0\x80\xafz", b"z"),
            (b"\xed\xa0\x80z", b"z"),
            // Above U+10FFFF.
            (b"\xf4\x90\x80\x80z", b"z"),
            (b"\xf5\x80\x80\x80z", b"z"),
            // Valid text around damage survives.
            ("é\u{80}".as_bytes(), "é\\u0080".as_bytes()),
            (b"\xc3\xa9\xa9\xc3\xa9", "éé".as_bytes()),
        ];
        for (input, want) in cases {
            assert!(!is_valid_utf8(input), "{input:?}");
            assert_eq!(escape_string(input, true), *want, "{input:?}");
        }
    }

    #[test]
    fn c1_controls_are_written_as_unicode_escapes() {
        assert!(!is_valid_utf8("\u{85}".as_bytes()));
        assert_eq!(esc("x\u{80}y\u{9f}".as_bytes(), true), "x\\u0080y\\u009f");
        // Without the short escapes too: the backslash is fc's own text.
        assert_eq!(esc("\u{9b}".as_bytes(), false), "\\u009b");
    }

    /// An echoed argument is cleaned, then escaped again as part of the message string.
    #[test]
    fn echoed_input_is_cleaned_then_escaped() {
        let echo = |s: &[u8]| {
            let mut out = Vec::new();
            write_str(&mut out, &clean(s));
            String::from_utf8(out).unwrap()
        };
        assert_eq!(echo(b"zz"), "\"zz\"");
        assert_eq!(echo(b"a\x01b"), "\"a\\\\u0001b\"");
        assert_eq!(echo(b"a\tb\"c\\d"), "\"a\\tb\\\"c\\\\d\"");
        assert_eq!(echo(b"x\xffy"), "\"xy\"");
        assert_eq!(echo("\u{85}".as_bytes()), "\"\\\\u0085\"");
        assert_eq!(echo(b"\x7f"), "\"\\\\u007f\"");
    }

    #[test]
    fn large_integers_are_quoted() {
        let w = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut out = Vec::new();
            f(&mut out);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(w(&|o| write_u64(o, 0)), "0");
        assert_eq!(w(&|o| write_u64(o, 0xffff_ffff)), "4294967295");
        assert_eq!(w(&|o| write_u64(o, 0x1_0000_0000)), "\"4294967296\"");
        assert_eq!(w(&|o| write_u64(o, u64::MAX)), "\"18446744073709551615\"");
        assert_eq!(w(&|o| write_i64(o, 0xffff_ffff)), "4294967295");
        assert_eq!(w(&|o| write_i64(o, -0xffff_ffff)), "-4294967295");
        assert_eq!(w(&|o| write_i64(o, 0x1_0000_0000)), "\"4294967296\"");
        assert_eq!(w(&|o| write_i64(o, -0x1_0000_0000)), "\"-4294967296\"");
        assert_eq!(w(&|o| write_i64(o, i64::MIN)), "\"-9223372036854775808\"");
        assert_eq!(w(&|o| write_hex(o, &[])), "\"\"");
        assert_eq!(w(&|o| write_hex(o, &[0x00, 0xab, 0xff])), "\"00abff\"");
    }
}
