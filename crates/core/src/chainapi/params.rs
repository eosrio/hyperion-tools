//! Request bodies, read as nodeos reads them (Spring `http_plugin.hpp` `parse_params`, fc's
//! legacy JSON parser and `from_variant` conversions, `chain_plugin.cpp`'s block lookups).
//!
//! **Parser.** fc's legacy parser accepts much that JSON does not, and gives some inputs
//! meanings no other parser shares. This one accepts exactly a subset that both read the same
//! way, and answers anything outside it as unparsable:
//!
//!   * Leading space, TAB, LF and CR are skipped; the first value must be an object; bytes after
//!     it are ignored.
//!   * Object members are separated by any number of commas, or none, as fc allows.
//!   * Strings keep every byte raw, control bytes included, except 0x04, which fc reads as the
//!     end of input. `\t`, `\n`, `\r` and `\\` decode as usual and `\X` is `X` for any other
//!     byte, so `"\u0031"` is the five bytes `u0031`, as in fc.
//!   * Numbers are `-?[0-9]+` not followed by a letter, digit or `.`; negative ones are i64,
//!     others u64, and overflow is an error. fc reads fractions as doubles and exponents as
//!     strings; both are refused here.
//!   * `true`, `false` and `null` must not run into a letter or digit.
//!   * Arrays are comma-separated values.
//!   * Nesting is limited as fc limits it: a budget of 200 that each container and each of its
//!     members spends one of.
//!   * Of duplicate keys, the first counts (fc appends members and its field lookup finds the
//!     first).
//!
//! **Fields.** Each endpoint's parameters convert as fc's `from_variant` does, a missing field
//! taking its default, and `block_num_or_id` resolves to a block number or id as
//! `get_raw_block` and `get_block_header` resolve it.

use super::errors::ApiError;
use super::{BlockRef, Endpoint, Request};

/// fc `DEFAULT_MAX_RECURSION_DEPTH`.
const MAX_DEPTH: u32 = 200;

/// nodeos's `is_empty_content`: after trimming spaces (spaces only) from both ends, nothing is
/// left, or `{`, then only spaces and TABs, then `}`.
pub fn is_empty_content(body: &[u8]) -> bool {
    let Some(start) = body.iter().position(|&b| b != b' ') else {
        return true;
    };
    let end = body
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(start, |i| i + 1);
    match &body[start..end] {
        [b'{', inner @ .., b'}'] => inner.iter().all(|&b| b == b' ' || b == b'\t'),
        _ => false,
    }
}

/// fc `to_uint64` (boost `lexical_cast<uint64_t>`): an optional `+` or `-`, then one or more
/// ASCII digits and nothing else. The value must fit in 64 bits; a `-` negates it modulo 2^64.
pub fn to_uint64(s: &[u8]) -> Option<u64> {
    let (negative, digits) = match s {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        _ => (false, s),
    };
    let v = decimal(digits)?;
    Some(if negative { v.wrapping_neg() } else { v })
}

/// Parse a request body for `endpoint`: nodeos's `params_required` rule, the fields, and for
/// the endpoints that take `block_num_or_id`, its resolution.
pub fn parse(endpoint: Endpoint, body: &[u8]) -> Result<Request, ApiError> {
    if is_empty_content(body) {
        return Err(ApiError::BodyRequired);
    }
    let members = Parser::new(body).top().ok_or(ApiError::Unparsable)?;
    let field = |key: &[u8]| members.iter().find(|(k, _)| k == key).map(|(_, v)| v);

    if endpoint == Endpoint::GetBlockInfo {
        let num = field(b"block_num").map_or(Some(0), Value::as_u32);
        let num = num.ok_or(ApiError::Unparsable)?;
        return Ok(Request {
            endpoint,
            block: BlockRef::Num(num),
            echo: num.to_string().into_bytes(),
            include_extensions: false,
        });
    }

    // fc converts every field before the handler runs, so a bad include_extensions is
    // unparsable even when block_num_or_id is also bad.
    let s = field(b"block_num_or_id").map_or(Some(Vec::new()), Value::as_string);
    let include_extensions = match endpoint {
        Endpoint::GetBlockHeader => {
            field(b"include_extensions").map_or(Some(false), Value::as_bool)
        }
        _ => Some(false),
    };
    let (Some(s), Some(include_extensions)) = (s, include_extensions) else {
        return Err(ApiError::Unparsable);
    };
    let block = resolve(&s).map_err(|bad| match bad {
        Bad::Length => ApiError::BadLength { endpoint },
        Bad::Id => ApiError::BadId {
            endpoint,
            include_extensions,
            echo: s.clone(),
        },
    })?;
    Ok(Request {
        endpoint,
        block,
        echo: s,
        include_extensions,
    })
}

/// Why `block_num_or_id` names no block.
enum Bad {
    Length,
    Id,
}

/// `block_num_or_id`: empty or longer than 64 bytes is refused; anything `to_uint64` reads is
/// a number, cut to 32 bits as `fetch_block_by_number` takes it; anything else must be
/// even-length hex (either case), copied into a zeroed 32-byte id.
fn resolve(s: &[u8]) -> Result<BlockRef, Bad> {
    if s.is_empty() || s.len() > 64 {
        return Err(Bad::Length);
    }
    if let Some(v) = to_uint64(s) {
        return Ok(BlockRef::Num(v as u32));
    }
    let (pairs, []) = s.as_chunks::<2>() else {
        return Err(Bad::Id);
    };
    let nibble = |c: u8| char::from(c).to_digit(16).ok_or(Bad::Id);
    let mut id = [0; 32];
    for (byte, [hi, lo]) in id.iter_mut().zip(pairs) {
        *byte = (nibble(*hi)? << 4 | nibble(*lo)?) as u8;
    }
    Ok(BlockRef::Id(id))
}

/// One or more ASCII digits as a u64, or `None` on anything else or overflow.
fn decimal(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u64, |v, &c| {
        let d = char::from(c).to_digit(10)?;
        v.checked_mul(10)?.checked_add(u64::from(d))
    })
}

/// A parsed value, as far as these endpoints read it: containers only have to be well-formed.
#[derive(Debug, PartialEq, Eq)]
enum Value {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    Str(Vec<u8>),
    Array,
    Object,
}

impl Value {
    /// fc `variant::as_string`, for a `std::string` field.
    fn as_string(&self) -> Option<Vec<u8>> {
        match self {
            Self::Null => Some(Vec::new()),
            Self::Bool(b) => Some(b.to_string().into_bytes()),
            Self::U64(v) => Some(v.to_string().into_bytes()),
            Self::I64(v) => Some(v.to_string().into_bytes()),
            Self::Str(s) => Some(s.clone()),
            Self::Array | Self::Object => None,
        }
    }

    /// fc `variant::as_uint64` cut to 32 bits, for a `uint32_t` field.
    fn as_u32(&self) -> Option<u32> {
        let v = match self {
            Self::Null => 0,
            Self::Bool(b) => u64::from(*b),
            Self::U64(v) => *v,
            Self::I64(v) => *v as u64,
            Self::Str(s) => to_uint64(s)?,
            Self::Array | Self::Object => return None,
        };
        Some(v as u32)
    }

    /// fc `variant::as_bool`.
    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Null => Some(false),
            Self::Bool(b) => Some(*b),
            Self::U64(v) => Some(*v != 0),
            Self::I64(v) => Some(*v != 0),
            Self::Str(s) => match s.as_slice() {
                b"true" => Some(true),
                b"false" => Some(false),
                _ => None,
            },
            Self::Array | Self::Object => None,
        }
    }
}

/// The parser of the module docs. `None` is "unparsable".
struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    /// The members of the top-level object, in order.
    fn top(&mut self) -> Option<Vec<(Vec<u8>, Value)>> {
        self.ws();
        if self.peek()? != b'{' {
            return None;
        }
        // fc `variant_from_stream(200)` hands the object 199.
        self.object(MAX_DEPTH - 1)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// fc `variant_from_stream`. Its depth is a u32 that containers decrement unchecked, so the
    /// subtractions wrap as fc's do (from 200 they never reach it).
    fn value(&mut self, depth: u32) -> Option<Value> {
        if depth == 0 {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'"' => self.string().map(Value::Str),
            b'{' => self.object(depth.wrapping_sub(1)).map(|_| Value::Object),
            b'[' => self.array(depth.wrapping_sub(1)).map(|()| Value::Array),
            b'-' | b'0'..=b'9' => self.number(),
            b't' | b'f' | b'n' => self.literal(),
            _ => None,
        }
    }

    fn object(&mut self, depth: u32) -> Option<Vec<(Vec<u8>, Value)>> {
        if !self.eat(b'{') {
            return None;
        }
        let mut members = Vec::new();
        loop {
            while matches!(self.peek()?, b' ' | b'\t' | b'\n' | b'\r' | b',') {
                self.pos += 1;
            }
            if self.eat(b'}') {
                return Some(members);
            }
            let key = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return None;
            }
            let value = self.value(depth.wrapping_sub(1))?;
            members.push((key, value));
        }
    }

    fn array(&mut self, depth: u32) -> Option<()> {
        if !self.eat(b'[') {
            return None;
        }
        self.ws();
        if self.eat(b']') {
            return Some(());
        }
        loop {
            self.value(depth.wrapping_sub(1))?;
            self.ws();
            match self.next()? {
                b',' => {}
                b']' => return Some(()),
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<Vec<u8>> {
        if !self.eat(b'"') {
            return None;
        }
        let mut s = Vec::new();
        loop {
            match self.next()? {
                b'"' => return Some(s),
                b'\\' => s.push(match self.next()? {
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'r' => b'\r',
                    other => other,
                }),
                0x04 => return None,
                c => s.push(c),
            }
        }
    }

    fn number(&mut self) -> Option<Value> {
        let negative = self.eat(b'-');
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'.') {
            return None;
        }
        let magnitude = decimal(&self.b[start..self.pos])?;
        if !negative {
            return Some(Value::U64(magnitude));
        }
        // i64::MIN's magnitude is one more than i64::MAX.
        let v = 0i64.checked_sub_unsigned(magnitude)?;
        Some(Value::I64(v))
    }

    fn literal(&mut self) -> Option<Value> {
        let rest = &self.b[self.pos..];
        let (len, value) = if rest.starts_with(b"true") {
            (4, Value::Bool(true))
        } else if rest.starts_with(b"false") {
            (5, Value::Bool(false))
        } else if rest.starts_with(b"null") {
            (4, Value::Null)
        } else {
            return None;
        };
        if rest.get(len).is_some_and(u8::is_ascii_alphanumeric) {
            return None;
        }
        self.pos += len;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(body: &str) -> Result<Request, ApiError> {
        parse(Endpoint::GetRawBlock, body.as_bytes())
    }

    fn num(body: &str) -> (u32, String) {
        let r = raw(body).unwrap_or_else(|e| panic!("{body}: {e}"));
        let BlockRef::Num(n) = r.block else {
            panic!("{body} is not a number: {r:?}")
        };
        (n, String::from_utf8(r.echo).unwrap())
    }

    fn bad_id(body: &str) -> Vec<u8> {
        match raw(body) {
            Err(ApiError::BadId { echo, .. }) => echo,
            other => panic!("{body}: {other:?}"),
        }
    }

    fn id_of(s: &str) -> [u8; 32] {
        let mut id = [0; 32];
        let b = hex::decode(s).unwrap();
        id[..b.len()].copy_from_slice(&b);
        id
    }

    #[test]
    fn empty_bodies_follow_nodeos() {
        for body in ["", " ", "   ", "{}", "{ \t }", "  {}  ", "{\t\t}"] {
            assert!(is_empty_content(body.as_bytes()), "{body:?}");
            assert_eq!(raw(body), Err(ApiError::BodyRequired), "{body:?}");
        }
        // Only spaces are trimmed, and only spaces and TABs may sit inside the braces.
        for body in ["{}\n", "\t{}", "\n", "{\n}", "{", "}", "x", "{ x }"] {
            assert!(!is_empty_content(body.as_bytes()), "{body:?}");
        }
        assert_eq!(
            raw("{}\n"),
            Err(ApiError::BadLength {
                endpoint: Endpoint::GetRawBlock
            })
        );
        assert_eq!(raw("\n"), Err(ApiError::Unparsable));
    }

    #[test]
    fn to_uint64_is_lexical_cast() {
        let cases: &[(&str, Option<u64>)] = &[
            ("0", Some(0)),
            ("0001", Some(1)),
            ("+5", Some(5)),
            ("-0", Some(0)),
            ("-1", Some(u64::MAX)),
            ("18446744073709551615", Some(u64::MAX)),
            ("-18446744073709551615", Some(1)),
            (
                "00000000000000000000000000018446744073709551615",
                Some(u64::MAX),
            ),
            ("18446744073709551616", None),
            ("-18446744073709551616", None),
            ("", None),
            ("+", None),
            ("-", None),
            ("+-1", None),
            (" 1", None),
            ("1 ", None),
            ("1a", None),
            ("0x10", None),
        ];
        for (s, want) in cases {
            assert_eq!(to_uint64(s.as_bytes()), *want, "{s:?}");
        }
    }

    #[test]
    fn block_num_or_id_converts_and_resolves_as_nodeos() {
        assert_eq!(num(r#"{"block_num_or_id":1}"#), (1, "1".into()));
        assert_eq!(num(r#"{"block_num_or_id":"1"}"#), (1, "1".into()));
        assert_eq!(num(r#"{"block_num_or_id":"+1"}"#), (1, "+1".into()));
        assert_eq!(num(r#"{"block_num_or_id":"0001"}"#), (1, "0001".into()));
        assert_eq!(num(r#"{"block_num_or_id":"-1"}"#), (u32::MAX, "-1".into()));
        assert_eq!(num(r#"{"block_num_or_id":-1}"#), (u32::MAX, "-1".into()));
        assert_eq!(num(r#"{"block_num_or_id":-0}"#), (0, "0".into()));
        assert_eq!(
            num(r#"{"block_num_or_id":"4294967297"}"#),
            (1, "4294967297".into())
        );
        assert_eq!(num(r#"{"block_num_or_id":"00"}"#), (0, "00".into()));

        assert_eq!(bad_id(r#"{"block_num_or_id":" 1"}"#), b" 1");
        assert_eq!(bad_id(r#"{"block_num_or_id":true}"#), b"true");
        assert_eq!(bad_id(r#"{"block_num_or_id":false}"#), b"false");
        assert_eq!(bad_id(r#"{"block_num_or_id":"abc"}"#), b"abc");
        assert_eq!(bad_id(r#"{"block_num_or_id":"zz"}"#), b"zz");
        assert_eq!(bad_id(r#"{"block_num_or_id":"\u0031"}"#), b"u0031");

        let bad_len = Err(ApiError::BadLength {
            endpoint: Endpoint::GetRawBlock,
        });
        for body in [
            r#"{"block_num_or_id":""}"#,
            r#"{"block_num_or_id":null}"#,
            r#"{"other":1}"#,
            r#"{"block_num_or_id":"0123456789012345678901234567890123456789012345678901234567890123a"}"#,
        ] {
            assert_eq!(raw(body), bad_len, "{body}");
        }

        // Ids: even-length hex of either case, zero-padded; 64 characters at most.
        let full = "0000002a".to_owned() + &"Ab".repeat(28);
        let r = raw(&format!(r#"{{"block_num_or_id":"{full}"}}"#)).unwrap();
        assert_eq!(r.block, BlockRef::Id(id_of(&full)));
        assert_eq!((r.block.num(), r.echo), (42, full.into_bytes()));
        let r = raw(r#"{"block_num_or_id":"0a"}"#).unwrap();
        assert_eq!(r.block, BlockRef::Id(id_of("0a")));
        assert_eq!(r.block.num(), 0x0a00_0000);
        assert_eq!(bad_id(r#"{"block_num_or_id":"0a1"}"#), b"0a1");
        assert_eq!(bad_id(r#"{"block_num_or_id":"0g"}"#), b"0g");
    }

    #[test]
    fn strings_keep_fc_escapes_and_raw_bytes() {
        // `\b` and `\f` decode to the letters; `\"` to a quote; control bytes stay raw.
        assert_eq!(bad_id(r#"{"block_num_or_id":"a\b\f\"x"}"#), b"abf\"x");
        assert_eq!(
            bad_id("{\"block_num_or_id\":\"a\\tb\\n\\r\\\\\"}"),
            b"a\tb\n\r\\"
        );
        assert_eq!(bad_id("{\"block_num_or_id\":\"\x01\x7f\"}"), b"\x01\x7f");
        assert_eq!(
            parse(Endpoint::GetRawBlock, b"{\"block_num_or_id\":\"\xff\xfe\"}"),
            Err(ApiError::BadId {
                endpoint: Endpoint::GetRawBlock,
                include_extensions: false,
                echo: b"\xff\xfe".to_vec()
            })
        );
        // An escaped 0x04 is data; a raw one ends the input for fc.
        assert_eq!(bad_id("{\"block_num_or_id\":\"x\\\x04\"}"), b"x\x04");
        assert_eq!(
            raw("{\"block_num_or_id\":\"x\x04\"}"),
            Err(ApiError::Unparsable)
        );
        // Escapes in keys count too.
        assert_eq!(num(r#"{"block_num_or_i\d":7}"#).0, 7);
    }

    #[test]
    fn objects_are_read_as_fc_reads_them() {
        // The first of duplicate keys counts.
        assert_eq!(num(r#"{"block_num_or_id":5,"block_num_or_id":6}"#).0, 5);
        // Commas are optional and may repeat.
        assert_eq!(num(r#"{"a":1 "block_num_or_id":5}"#).0, 5);
        assert_eq!(num(r#"{,,"a":1,,,"block_num_or_id":5,}"#).0, 5);
        assert_eq!(num(r#"{"a":1"block_num_or_id":5}"#).0, 5);
        // Whitespace anywhere between tokens; bytes after the object are ignored.
        assert_eq!(
            num(" \r\n\t{ \"block_num_or_id\" \n:\t 5 } trailing junk {").0,
            5
        );
        assert_eq!(num(r#"{"block_num_or_id":5}}}"#).0, 5);
        // Unknown members may hold anything well-formed.
        assert_eq!(
            num(r#"{"x":{"y":[1,-2,"s",true,false,null,{},[]]},"block_num_or_id":5}"#).0,
            5
        );
    }

    #[test]
    fn bodies_outside_the_subset_are_unparsable() {
        for body in [
            r#"{"block_num_or_id":1e5}"#,
            r#"{"block_num_or_id":1.5}"#,
            r#"{"block_num_or_id":.5}"#,
            r#"{"block_num_or_id":1x}"#,
            r#"{"block_num_or_id":-}"#,
            r#"{"block_num_or_id":--1}"#,
            r#"{"block_num_or_id":18446744073709551616}"#,
            r#"{"block_num_or_id":-9223372036854775809}"#,
            r#"{"block_num_or_id":tru}"#,
            r#"{"block_num_or_id":truex}"#,
            r#"{"block_num_or_id":nulls}"#,
            r#"{"block_num_or_id":abc}"#,
            r#"{"block_num_or_id":[1]}"#,
            r#"{"block_num_or_id":{}}"#,
            r#"{"x":[1,,2],"block_num_or_id":5}"#,
            r#"{"x":[1,],"block_num_or_id":5}"#,
            r#"{"x":[,1],"block_num_or_id":5}"#,
            r#"{"x":[1 2],"block_num_or_id":5}"#,
            r#"{"block_num_or_id":5"#,
            r#"{"block_num_or_id" 5}"#,
            r#"{block_num_or_id:5}"#,
            r#"{"block_num_or_id":"5}"#,
            "[1]",
            "\"x\"",
            "5",
            "null",
            "\n",
            "{\0}",
        ] {
            assert_eq!(raw(body), Err(ApiError::Unparsable), "{body}");
        }
        // The extreme integers are fine.
        assert_eq!(
            num(r#"{"block_num_or_id":18446744073709551615}"#).0,
            u32::MAX
        );
        let (n, echo) = num(r#"{"block_num_or_id":-9223372036854775808}"#);
        assert_eq!((n, echo.as_str()), (0, "-9223372036854775808"));
    }

    #[test]
    fn nesting_is_limited_as_in_fc() {
        // fc spends one unit of its 200 on each container and one on each member, so 100
        // nested containers parse when the innermost is empty, and no deeper ones do.
        let nest = |depth: usize, inner: &str| {
            let open = "{\"a\":".repeat(depth - 1);
            format!("{open}{inner}{}", "}".repeat(depth - 1))
        };
        // Parsed bodies without the field answer E-LEN; refused ones are unparsable.
        let parsed = Err(ApiError::BadLength {
            endpoint: Endpoint::GetRawBlock,
        });
        assert_eq!(raw(&nest(99, r#"{"block_num_or_id":5}"#)), parsed);
        assert_eq!(raw(&nest(100, "{}")), parsed);
        assert_eq!(raw(&nest(100, "{\"x\":1}")), Err(ApiError::Unparsable));
        assert_eq!(raw(&nest(101, "{}")), Err(ApiError::Unparsable));
        let arrays = format!("{{\"x\":{}{}}}", "[".repeat(99), "]".repeat(99));
        assert_eq!(raw(&arrays), parsed);
        let arrays = format!("{{\"x\":{}{}}}", "[".repeat(100), "]".repeat(100));
        assert_eq!(raw(&arrays), Err(ApiError::Unparsable));
    }

    #[test]
    fn get_block_info_takes_a_32_bit_number() {
        let info = |body: &str| parse(Endpoint::GetBlockInfo, body.as_bytes());
        let n = |body: &str| {
            let r = info(body).unwrap();
            assert!(!r.include_extensions);
            let BlockRef::Num(n) = r.block else {
                panic!("{body}")
            };
            assert_eq!(r.echo, n.to_string().into_bytes());
            n
        };
        assert_eq!(n(r#"{"block_num":7}"#), 7);
        assert_eq!(n(r#"{"block_num":"7"}"#), 7);
        assert_eq!(n(r#"{"block_num":"+7"}"#), 7);
        assert_eq!(n(r#"{"block_num":"-1"}"#), u32::MAX);
        assert_eq!(n(r#"{"block_num":-1}"#), u32::MAX);
        assert_eq!(n(r#"{"block_num":4294967297}"#), 1);
        assert_eq!(n(r#"{"block_num":true}"#), 1);
        assert_eq!(n(r#"{"block_num":null}"#), 0);
        assert_eq!(n(r#"{"other":1}"#), 0);
        // The id field means nothing here.
        assert_eq!(n(r#"{"block_num_or_id":"zz","block_num":3}"#), 3);
        for body in [
            r#"{"block_num":"x"}"#,
            r#"{"block_num":" 7"}"#,
            r#"{"block_num":""}"#,
            r#"{"block_num":[7]}"#,
            r#"{"block_num":{}}"#,
            r#"{"block_num":7.0}"#,
        ] {
            assert_eq!(info(body), Err(ApiError::Unparsable), "{body}");
        }
        assert_eq!(info("{ }"), Err(ApiError::BodyRequired));
        assert_eq!(
            info(r#"{"block_num":9}"#).unwrap().unknown_block().body(),
            {
                let e = ApiError::UnknownBlock {
                    endpoint: Endpoint::GetBlockInfo,
                    include_extensions: false,
                    echo: b"9".to_vec(),
                };
                e.body()
            }
        );
    }

    #[test]
    fn include_extensions_converts_as_a_bool() {
        let hdr = |body: &str| parse(Endpoint::GetBlockHeader, body.as_bytes());
        let ext = |body: &str| hdr(body).unwrap().include_extensions;
        assert!(!ext(r#"{"block_num_or_id":1}"#));
        assert!(ext(r#"{"block_num_or_id":1,"include_extensions":true}"#));
        assert!(ext(r#"{"block_num_or_id":1,"include_extensions":1}"#));
        assert!(ext(r#"{"block_num_or_id":1,"include_extensions":-3}"#));
        assert!(ext(r#"{"block_num_or_id":1,"include_extensions":"true"}"#));
        assert!(!ext(r#"{"block_num_or_id":1,"include_extensions":0}"#));
        assert!(!ext(
            r#"{"block_num_or_id":1,"include_extensions":"false"}"#
        ));
        assert!(!ext(r#"{"block_num_or_id":1,"include_extensions":null}"#));
        for body in [
            r#"{"block_num_or_id":1,"include_extensions":"yes"}"#,
            r#"{"block_num_or_id":1,"include_extensions":""}"#,
            r#"{"block_num_or_id":1,"include_extensions":[]}"#,
            // Every field converts before the handler checks the id.
            r#"{"block_num_or_id":"","include_extensions":"yes"}"#,
        ] {
            assert_eq!(hdr(body), Err(ApiError::Unparsable), "{body}");
        }
        // The mode picks get_block_header's error lines.
        assert_eq!(
            hdr(r#"{"block_num_or_id":"zz","include_extensions":true}"#),
            Err(ApiError::BadId {
                endpoint: Endpoint::GetBlockHeader,
                include_extensions: true,
                echo: b"zz".to_vec()
            })
        );
        // Other endpoints ignore the field, whatever it holds.
        assert!(
            !raw(r#"{"block_num_or_id":1,"include_extensions":true}"#)
                .unwrap()
                .include_extensions
        );
        assert!(raw(r#"{"block_num_or_id":1,"include_extensions":"yes"}"#).is_ok());
    }
}
