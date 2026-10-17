//! Request bodies, read as nodeos reads them (Spring `http_plugin.hpp` `parse_params`, fc's
//! legacy JSON parser in `json.cpp` and its `from_variant` conversions, `chain_plugin.cpp`'s
//! block lookups).
//!
//! **Parser.** fc's legacy parser, rule for rule. It accepts much that JSON does not, and
//! gives some inputs meanings no other parser shares:
//!
//!   * Leading space, TAB, LF and CR are skipped; the first value must be an object (fc parses
//!     any value, and then fails to convert one that is not an object); bytes after it are
//!     ignored.
//!   * Object members, and array elements, are separated by any number of commas, or none.
//!   * Strings keep every byte raw, control bytes included, except 0x04, which fc reads as the
//!     end of input. `\t`, `\n`, `\r` and `\\` decode as usual and `\X` is `X` for any other
//!     byte, so `"\u0031"` is the five bytes `u0031`.
//!   * A number is an optional `-`, then digits and at most one `.`: `.5`, `5.` and `-.5` are
//!     numbers, and `-`, `.` and `-.` alone are refused. With a `.` it is a double, the nearest
//!     to the text, refused when `strtod` finds it out of range; otherwise it is an i64 when
//!     negative and a u64 when not, refused on overflow. A number running into a letter or a
//!     digit is instead the start of a bare token, so `1e5`, `1.5e-3` and `12ab` are strings.
//!   * A bare token starts with `n`, `t` or `f`. Its longest run of the letters of `null`,
//!     `true` and `false` is that literal when it spells one (`truex` is `true` followed by a
//!     stray `x`); otherwise the token is a string, continued as fc continues every token:
//!     letters, digits and `_-.:/`, with the string escapes, up to a space, TAB or LF (taken
//!     with it) or any other byte.
//!   * A NUL byte outside a string is the end of input for fc: whatever expects more fails.
//!   * Nesting is limited as fc limits it: a budget of 200 that each container and each of its
//!     members spends one of.
//!   * Of duplicate keys, the first counts (fc appends members and its field lookup finds the
//!     first).
//!
//! Only the fields the endpoint reads are kept; every other value is checked and dropped. A
//! string is borrowed from the body, and only one holding escapes is decoded, into at most the
//! bytes its field reads. So a body costs next to no memory besides itself, however many
//! members it holds.
//!
//! **Fields.** Each endpoint's parameters convert as fc's `from_variant` does, a missing field
//! taking its default, and `block_num_or_id` resolves to a block number or id as
//! `get_raw_block` and `get_block_header` resolve it. A double converts as Spring's release
//! build (clang and libc++, for x86-64) converts it: to a string with 17 fixed decimals, to an
//! integer through `static_cast<uint64_t>`, to a bool when it is not zero.

use std::borrow::Cow;

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
    let wanted: &[Field] = match endpoint {
        Endpoint::GetBlockInfo => &[Field::BlockNum],
        Endpoint::GetBlockHeader => &[Field::BlockNumOrId, Field::IncludeExtensions],
        Endpoint::GetRawBlock => &[Field::BlockNumOrId],
    };
    let found = Parser::new(body).top(wanted).ok_or(ApiError::Unparsable)?;
    let field = |f: Field| found[f as usize].as_ref();

    if endpoint == Endpoint::GetBlockInfo {
        let num = field(Field::BlockNum).map_or(Some(0), Value::as_u32);
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
    let s = field(Field::BlockNumOrId).map_or(Some(Cow::Borrowed(&b""[..])), Value::as_string);
    let include_extensions = field(Field::IncludeExtensions).map_or(Some(false), Value::as_bool);
    let (Some(s), Some(include_extensions)) = (s, include_extensions) else {
        return Err(ApiError::Unparsable);
    };
    let block = resolve(&s).map_err(|bad| match bad {
        Bad::Length => ApiError::BadLength { endpoint },
        Bad::Id => ApiError::BadId {
            endpoint,
            include_extensions,
            echo: s.to_vec(),
        },
    })?;
    Ok(Request {
        endpoint,
        block,
        echo: s.into_owned(),
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

/// fc `to_double` (boost `lexical_cast<double>`, which reads through libc++'s `num_get` and so
/// `strtod`) of a number fc read with a `.`: the nearest double. libc++ fails the read when
/// `strtod` reports `ERANGE`: for a value too large, and for one that is not zero but rounds
/// below the smallest normal double. (An exactly written subnormal, hundreds of digits long,
/// is refused here although `strtod` takes it.)
fn to_double(text: &[u8]) -> Option<f64> {
    let v: f64 = std::str::from_utf8(text).ok()?.parse().ok()?;
    let zero = text.iter().all(|c| matches!(c, b'0' | b'-' | b'.'));
    (v.is_finite() && (zero || v.abs() >= f64::MIN_POSITIVE)).then_some(v)
}

/// fc `variant::as_string` of a double: `std::fixed` with 17 decimals.
fn fixed(v: f64) -> String {
    format!("{v:.17}")
}

/// C++ `static_cast<uint64_t>` of a double, as clang compiles it for x86-64: two truncating
/// conversions to i64 (`cvttsd2si`, which gives i64::MIN out of range), of `v` and of
/// `v - 2^63`, the second one taken when the first is negative. So a negative value wraps
/// through i64, and one at or past 2^64 or below -2^63 is 2^63.
fn cast_u64(v: f64) -> u64 {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    let cvt = |x: f64| {
        if (-TWO_63..TWO_63).contains(&x) {
            x as i64
        } else {
            i64::MIN
        }
    };
    let low = cvt(v);
    (low | (cvt(v - TWO_63) & (low >> 63))) as u64
}

/// A request field an endpoint reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    BlockNumOrId,
    BlockNum,
    IncludeExtensions,
}

/// The first value of each field, by `Field as usize`.
type Found<'a> = [Option<Value<'a>>; 3];

impl Field {
    const ALL: [Self; 3] = [Self::BlockNumOrId, Self::BlockNum, Self::IncludeExtensions];
    /// The longest name: a longer key names no field.
    const LONGEST: usize = 18;

    fn name(self) -> &'static [u8] {
        match self {
            Self::BlockNumOrId => b"block_num_or_id",
            Self::BlockNum => b"block_num",
            Self::IncludeExtensions => b"include_extensions",
        }
    }

    /// The most bytes of an escaped string the field's conversion needs: `block_num_or_id`
    /// is too long past 64 whatever it holds, and `include_extensions` reads `true` or `false`.
    /// `block_num` takes any number of leading zeros.
    fn keep(self) -> usize {
        match self {
            Self::BlockNumOrId => 65,
            Self::BlockNum => usize::MAX,
            Self::IncludeExtensions => 6,
        }
    }
}

/// A parsed value, as far as these endpoints read it: containers only have to be well-formed.
#[derive(Debug, PartialEq)]
enum Value<'a> {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    Double(f64),
    Str(Cow<'a, [u8]>),
    Array,
    Object,
}

impl Value<'_> {
    /// fc `variant::as_string`, for a `std::string` field.
    fn as_string(&self) -> Option<Cow<'_, [u8]>> {
        Some(match self {
            Self::Null => Cow::Borrowed(&b""[..]),
            Self::Bool(true) => Cow::Borrowed(&b"true"[..]),
            Self::Bool(false) => Cow::Borrowed(&b"false"[..]),
            Self::U64(v) => Cow::Owned(v.to_string().into_bytes()),
            Self::I64(v) => Cow::Owned(v.to_string().into_bytes()),
            Self::Double(v) => Cow::Owned(fixed(*v).into_bytes()),
            Self::Str(s) => Cow::Borrowed(s.as_ref()),
            Self::Array | Self::Object => return None,
        })
    }

    /// fc `variant::as_uint64` cut to 32 bits, for a `uint32_t` field.
    fn as_u32(&self) -> Option<u32> {
        let v = match self {
            Self::Null => 0,
            Self::Bool(b) => u64::from(*b),
            Self::U64(v) => *v,
            Self::I64(v) => *v as u64,
            Self::Double(v) => cast_u64(*v),
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
            Self::Double(v) => Some(*v != 0.0),
            Self::Str(s) => match s.as_ref() {
                b"true" => Some(true),
                b"false" => Some(false),
                _ => None,
            },
            Self::Array | Self::Object => None,
        }
    }
}

/// `s` cut to `keep` bytes, copied.
fn cut(s: &[u8], keep: usize) -> Vec<u8> {
    s[..s.len().min(keep)].to_vec()
}

/// Append `c` to `out` while it is shorter than `keep`.
fn push_cut(out: &mut Vec<u8>, c: u8, keep: usize) {
    if out.len() < keep {
        out.push(c);
    }
}

/// `Borrowed(raw)`, or `decoded` when escapes made one.
fn text<'a>(raw: &'a [u8], decoded: Option<Vec<u8>>) -> Cow<'a, [u8]> {
    decoded.map_or(Cow::Borrowed(raw), Cow::Owned)
}

/// The parser of the module docs. `None` is "unparsable". Every `keep` is the most bytes of an
/// escaped string decoded into the value; a value read with `keep` 0 is only checked.
struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    /// The first value of each `wanted` field among the members of the top-level object.
    fn top(&mut self, wanted: &[Field]) -> Option<Found<'a>> {
        self.ws();
        if self.peek()? != b'{' {
            return None;
        }
        let mut found = Found::default();
        // fc `variant_from_stream(200)` hands the object 199.
        self.object(MAX_DEPTH - 1, Some((wanted, &mut found)))?;
        Some(found)
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
    fn value(&mut self, depth: u32, keep: usize) -> Option<Value<'a>> {
        if depth == 0 {
            return None;
        }
        self.ws();
        match self.peek()? {
            b'"' => self.string(keep).map(Value::Str),
            b'{' => self
                .object(depth.wrapping_sub(1), None)
                .map(|()| Value::Object),
            b'[' => self.array(depth.wrapping_sub(1)).map(|()| Value::Array),
            b'-' | b'.' | b'0'..=b'9' => self.number(keep),
            b'n' | b't' | b'f' => Some(self.word(keep)),
            _ => None,
        }
    }

    /// fc `objectFromStream`. Given `wanted` fields, the first value of each goes into
    /// `found`; every other value is only checked.
    fn object(&mut self, depth: u32, mut fields: Option<(&[Field], &mut Found<'a>)>) -> Option<()> {
        if !self.eat(b'{') {
            return None;
        }
        loop {
            match self.peek()? {
                b'}' => {
                    self.pos += 1;
                    return Some(());
                }
                b',' | b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => {
                    let key = self.key()?;
                    self.ws();
                    if !self.eat(b':') {
                        return None;
                    }
                    let slot = match (&mut fields, key) {
                        (Some((wanted, found)), Some(field)) if wanted.contains(&field) => {
                            Some(&mut found[field as usize]).filter(|slot| slot.is_none())
                        }
                        _ => None,
                    };
                    let depth = depth.wrapping_sub(1);
                    match (slot, key) {
                        (Some(slot), Some(field)) => {
                            *slot = Some(self.value(depth, field.keep())?);
                        }
                        _ => {
                            self.value(depth, 0)?;
                        }
                    }
                }
            }
        }
    }

    /// fc `arrayFromStream`: values separated by any number of commas, or none.
    fn array(&mut self, depth: u32) -> Option<()> {
        if !self.eat(b'[') {
            return None;
        }
        loop {
            match self.peek()? {
                b']' => {
                    self.pos += 1;
                    return Some(());
                }
                b',' | b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => {
                    self.value(depth.wrapping_sub(1), 0)?;
                }
            }
        }
    }

    /// An object key, and the field it names. Decoding an escaped key stops past the longest
    /// field name, which is enough to tell.
    fn key(&mut self) -> Option<Option<Field>> {
        let key = self.string(Field::LONGEST + 1)?;
        Some(Field::ALL.into_iter().find(|f| *key == *f.name()))
    }

    /// fc `stringFromStream`, from the opening quote to the closing one: borrowed from the
    /// body, or decoded into at most `keep` bytes when it holds escapes.
    fn string(&mut self, keep: usize) -> Option<Cow<'a, [u8]>> {
        if !self.eat(b'"') {
            return None;
        }
        let (b, start) = (self.b, self.pos);
        let mut decoded: Option<Vec<u8>> = None;
        loop {
            let at = self.pos;
            match self.next()? {
                b'"' => return Some(text(&b[start..at], decoded)),
                0x04 => return None,
                b'\\' => {
                    let out = decoded.get_or_insert_with(|| cut(&b[start..at], keep));
                    push_cut(out, self.escape(), keep);
                }
                c => {
                    if let Some(out) = &mut decoded {
                        push_cut(out, c, keep);
                    }
                }
            }
        }
    }

    /// fc `parseEscape`, after the backslash: `\t`, `\n` and `\r` decode as usual and any other
    /// byte is itself. At the end of the input fc gives its end-of-file marker, the byte 0xff.
    fn escape(&mut self) -> u8 {
        match self.next() {
            Some(b't') => b'\t',
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(c) => c,
            None => 0xff,
        }
    }

    /// fc `number_from_stream`.
    fn number(&mut self, keep: usize) -> Option<Value<'a>> {
        let start = self.pos;
        let negative = self.eat(b'-');
        let mut dot = false;
        loop {
            match self.peek() {
                Some(b'0'..=b'9') => self.pos += 1,
                Some(b'.') if dot => return None,
                Some(b'.') => {
                    dot = true;
                    self.pos += 1;
                }
                // fc reads on as a bare token, and the whole is a string.
                Some(c) if c.is_ascii_alphanumeric() => {
                    return Some(Value::Str(self.token(start, keep)));
                }
                _ => break,
            }
        }
        let text = &self.b[start..self.pos];
        if matches!(text, b"-" | b"." | b"-.") {
            return None;
        }
        if dot {
            return to_double(text).map(Value::Double);
        }
        let magnitude = decimal(&text[usize::from(negative)..])?;
        if !negative {
            return Some(Value::U64(magnitude));
        }
        // i64::MIN's magnitude is one more than i64::MAX.
        0i64.checked_sub_unsigned(magnitude).map(Value::I64)
    }

    /// fc `token_from_stream`, at an `n`, `t` or `f`.
    fn word(&mut self, keep: usize) -> Value<'a> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'n' | b'u' | b'l' | b't' | b'r' | b'e' | b'f' | b'a' | b's')
        ) {
            self.pos += 1;
        }
        match &self.b[start..self.pos] {
            b"null" => Value::Null,
            b"true" => Value::Bool(true),
            b"false" => Value::Bool(false),
            _ => Value::Str(self.token(start, keep)),
        }
    }

    /// fc `stringFromToken`, continuing the token `self.b[start..self.pos]`: letters, digits and
    /// `_-.:/`, with the string escapes, up to a space, TAB or LF, which it takes, or any other
    /// byte. Borrowed or decoded as [`Self::string`] is.
    fn token(&mut self, start: usize, keep: usize) -> Cow<'a, [u8]> {
        let b = self.b;
        let mut decoded: Option<Vec<u8>> = None;
        loop {
            let at = self.pos;
            match self.peek() {
                Some(b'\\') => {
                    self.pos += 1;
                    let out = decoded.get_or_insert_with(|| cut(&b[start..at], keep));
                    push_cut(out, self.escape(), keep);
                }
                Some(c) if c.is_ascii_alphanumeric() || b"_-.:/".contains(&c) => {
                    self.pos += 1;
                    if let Some(out) = &mut decoded {
                        push_cut(out, c, keep);
                    }
                }
                end => {
                    if matches!(end, Some(b' ' | b'\t' | b'\n')) {
                        self.pos += 1;
                    }
                    return text(&b[start..at], decoded);
                }
            }
        }
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
        // So may arrays, whose elements commas separate as they separate members.
        for x in ["[1,,2]", "[1,]", "[,1]", "[1 2]", "[ , ]", r#"["a""b"]"#] {
            let body = format!(r#"{{"x":{x},"block_num_or_id":5}}"#);
            assert_eq!(num(&body).0, 5, "{body}");
        }
        // A later duplicate is still checked.
        assert_eq!(
            raw(r#"{"block_num_or_id":5,"block_num_or_id":-}"#),
            Err(ApiError::Unparsable)
        );
    }

    #[test]
    fn numbers_and_bare_tokens_are_read_as_fc_reads_them() {
        // A fraction or exponent in a field nobody reads, as nodeos answers it.
        for x in [
            "1.5", ".5", "5.", "-.5", "-1.5", "0.0", "-0.0", "1e5", "1.5e-3", "-1E-9", "12ab",
            "-x", ".x", "1_", "nulls", "fals", "tru", "nullnull", "t", "f\\}",
        ] {
            let body = format!(r#"{{"x":{x},"block_num_or_id":5}}"#);
            let want = if x == "1_" {
                // The `_` ends the number, and then is no key.
                Err(ApiError::Unparsable)
            } else {
                raw(r#"{"block_num_or_id":5}"#)
            };
            assert_eq!(raw(&body), want, "{body}");
        }
        // In block_num_or_id: a token is a string; a double is written with 17 decimals.
        for (x, echo) in [
            ("1e5", "1e5"),
            ("1.5e3", "1.5e3"),
            ("1x", "1x"),
            ("-a", "-a"),
            ("-.x", "-.x"),
            ("nulls", "nulls"),
            ("tru", "tru"),
            (r#"1e\"5"#, "1e\"5"),
            (r"1e\}", "1e}"),
            ("1e5 ", "1e5"),
            ("1.5", "1.50000000000000000"),
            (".5", "0.50000000000000000"),
            ("-0.0", "-0.00000000000000000"),
            ("12.25", "12.25000000000000000"),
            ("0.1", "0.10000000000000001"),
        ] {
            let body = format!(r#"{{"block_num_or_id":{x}}}"#);
            assert_eq!(bad_id(&body), echo.as_bytes(), "{body}");
        }
        // An even-length hex token is an id.
        let r = raw(r#"{"block_num_or_id":1e50}"#).unwrap();
        assert_eq!(r.block, BlockRef::Id(id_of("1e50")));
        // A double past 46 integer digits is longer than 64 characters.
        assert_eq!(
            raw(&format!(r#"{{"block_num_or_id":{}.5}}"#, "9".repeat(47))),
            Err(ApiError::BadLength {
                endpoint: Endpoint::GetRawBlock
            })
        );
        // `truex` is `true`, then a stray `x`; `1-2` is 1, then a stray `-`.
        for body in [
            r#"{"block_num_or_id":truex}"#,
            r#"{"block_num_or_id":1-2}"#,
            r#"{"block_num_or_id":true1}"#,
        ] {
            assert_eq!(raw(body), Err(ApiError::Unparsable), "{body}");
        }
        assert_eq!(bad_id(r#"{"block_num_or_id":true"x":1}"#), b"true");
    }

    #[test]
    fn doubles_convert_as_spring_s_release_build_converts_them() {
        let info = |body: &str| parse(Endpoint::GetBlockInfo, body.as_bytes());
        let n = |x: &str| match info(&format!(r#"{{"block_num":{x}}}"#)) {
            Ok(r) => r.block.num(),
            Err(e) => panic!("{x}: {e}"),
        };
        assert_eq!(n("7.9"), 7);
        assert_eq!(n("-0.5"), 0);
        assert_eq!(n("-1.5"), u32::MAX);
        assert_eq!(n("-3.5"), u32::MAX - 2);
        assert_eq!(n("4294967297.5"), 1);
        // 2^63 and past it, and below -2^63: out of range, 2^63, cut to 32 bits.
        assert_eq!(n("9223372036854775808.0"), 0);
        assert_eq!(n("18446744073709549568.0"), 0xffff_f800);
        assert_eq!(n("18446744073709551616.0"), 0);
        assert_eq!(n("-9223372036854777856.0"), 0);
        assert_eq!(n("-9223372036854775807.0"), 0);
        // A token is a string, which must be a number.
        assert_eq!(info(r#"{"block_num":1e5}"#), Err(ApiError::Unparsable));
        // Out of strtod's range: too large, or below the smallest normal double.
        let huge = format!("1{}.0", "0".repeat(309));
        let tiny = format!("0.{}1", "0".repeat(320));
        let zero = format!("0.{}", "0".repeat(400));
        for x in [&huge, &tiny] {
            let body = format!(r#"{{"block_num":{x}}}"#);
            assert_eq!(info(&body), Err(ApiError::Unparsable), "{x}");
            // Even where nobody reads it.
            let body = format!(r#"{{"x":{x},"block_num":3}}"#);
            assert_eq!(info(&body), Err(ApiError::Unparsable), "{x}");
        }
        assert_eq!(n(&zero), 0);
        assert_eq!(n(&format!("1{}.0", "0".repeat(307))), 0);

        let hdr = |x: &str| {
            let body = format!(r#"{{"block_num_or_id":1,"include_extensions":{x}}}"#);
            parse(Endpoint::GetBlockHeader, body.as_bytes()).map(|r| r.include_extensions)
        };
        assert_eq!(hdr("0.0"), Ok(false));
        assert_eq!(hdr("-0.0"), Ok(false));
        assert_eq!(hdr("0.5"), Ok(true));
        assert_eq!(hdr("1e5"), Err(ApiError::Unparsable));
        assert_eq!(hdr("trues"), Err(ApiError::Unparsable));

        assert_eq!(cast_u64(f64::NAN), 1 << 63);
        assert_eq!(fixed(1e22), "10000000000000000000000.00000000000000000");
    }

    #[test]
    fn bodies_fc_refuses_are_unparsable() {
        for body in [
            r#"{"block_num_or_id":1.2.3}"#,
            r#"{"block_num_or_id":.}"#,
            r#"{"block_num_or_id":-.}"#,
            r#"{"block_num_or_id":-}"#,
            r#"{"block_num_or_id":--1}"#,
            r#"{"block_num_or_id":18446744073709551616}"#,
            r#"{"block_num_or_id":-9223372036854775809}"#,
            r#"{"block_num_or_id":abc}"#,
            r#"{"block_num_or_id":+1}"#,
            // A token stops at `+`, which then is no key.
            r#"{"x":1e+5,"block_num_or_id":5}"#,
            r#"{"block_num_or_id":[1]}"#,
            r#"{"block_num_or_id":{}}"#,
            r#"{"x":[1,2},"block_num_or_id":5}"#,
            r#"{"x":[1 x],"block_num_or_id":5}"#,
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
            "{\"block_num_or_id\":5\0}",
            "{\"block_num_or_id\":\x04}",
            "{\"block_num_or_id\":5,\"x\":nul\0}",
        ] {
            assert_eq!(raw(body), Err(ApiError::Unparsable), "{body:?}");
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
    fn only_the_fields_read_are_kept() {
        // Members, strings and escapes the endpoint does not read cost nothing: a body of many
        // parses to the one field read.
        let mut body = String::from("{");
        for i in 0..10_000 {
            body.push_str(&format!(r#""k{i}":"v\"{i}","#));
        }
        body.push_str(r#""block_num_or_id":"7"}"#);
        assert_eq!(num(&body), (7, "7".into()));
        let found = Parser::new(body.as_bytes())
            .top(&[Field::BlockNumOrId])
            .unwrap();
        assert!(matches!(&found[0], Some(Value::Str(Cow::Borrowed(b"7")))));
        assert!(found[1].is_none() && found[2].is_none());
        // An escaped value is decoded only as far as its field reads it.
        let long = format!(r#"{{"block_num_or_id":"\{}"}}"#, "a".repeat(1000));
        assert_eq!(
            raw(&long),
            Err(ApiError::BadLength {
                endpoint: Endpoint::GetRawBlock
            })
        );
        let found = Parser::new(long.as_bytes())
            .top(&[Field::BlockNumOrId])
            .unwrap();
        let Some(Value::Str(Cow::Owned(s))) = &found[0] else {
            panic!("{:?}", found[0])
        };
        assert_eq!(s.len(), 65);
        // block_num reads any number of leading zeros, escaped or not.
        let zeros = format!(r#"{{"block_num":"\0{}9"}}"#, "0".repeat(100));
        let r = parse(Endpoint::GetBlockInfo, zeros.as_bytes()).unwrap();
        assert_eq!(r.block.num(), 9);
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
        assert_eq!(n(r#"{"block_num":7.0}"#), 7);
        // The id field means nothing here.
        assert_eq!(n(r#"{"block_num_or_id":"zz","block_num":3}"#), 3);
        for body in [
            r#"{"block_num":"x"}"#,
            r#"{"block_num":" 7"}"#,
            r#"{"block_num":""}"#,
            r#"{"block_num":[7]}"#,
            r#"{"block_num":{}}"#,
            r#"{"block_num":7x}"#,
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
        assert!(ext(r#"{"block_num_or_id":1,"include_extensions":"tru\e"}"#));
        for body in [
            r#"{"block_num_or_id":1,"include_extensions":"yes"}"#,
            r#"{"block_num_or_id":1,"include_extensions":""}"#,
            r#"{"block_num_or_id":1,"include_extensions":[]}"#,
            r#"{"block_num_or_id":1,"include_extensions":"true\ "}"#,
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
