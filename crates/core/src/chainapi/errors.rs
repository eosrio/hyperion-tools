//! The error bodies of the block endpoints, as a Spring 1.1.0 to 1.2.2 node writes them with
//! `verbose-http-errors` off (one detail), and the bodies for what a server of archived ranges
//! adds: an illegal request-target, an oversized body and `503 Busy`.
//!
//! nodeos's shape (`http_plugin.hpp` `error_results`) is
//!
//! ```text
//! {"code":H,"message":M,"error":{"code":C,"name":N,"what":W,
//!  "details":[{"message":D,"file":F,"line_number":L,"method":X}]}}
//! ```
//!
//! where `D` is the first log message of the exception: its format with each argument cleaned
//! by fc's `clean_append` (`get_limited_message`, which caps a long argument with `...`). The
//! source lines are the same in every Spring release from 1.1.0 to 1.2.2; other releases differ
//! only in `line_number`.

use std::fmt;

use super::Endpoint;
use super::fcjson::{clean, write_i64, write_str, write_u64};

/// `line_number` of `get_raw_block`'s `block_num_or_id` length assert, which spans several
/// lines. The compiler records one of them; until a capture from a node pins it, this is the
/// first.
pub const L_RAW: u32 = 1949;

/// `line_number` of `get_block_header`'s length assert (one assert serves both modes), as
/// [`L_RAW`].
pub const L_HDR: u32 = 1987;

/// fc `format_string`'s cap on a minimized message, and on its arguments together.
const MINIMIZE_MAX_SIZE: usize = 1024;

/// An answer other than the requested body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// An empty body (see [`super::is_empty_content`]) where parameters are required.
    BodyRequired,
    /// A body that does not parse, or whose fields do not convert.
    Unparsable,
    /// `block_num_or_id` empty or longer than 64 bytes.
    BadLength { endpoint: Endpoint },
    /// `block_num_or_id` neither a number nor even-length hex.
    BadId {
        endpoint: Endpoint,
        include_extensions: bool,
        echo: Vec<u8>,
    },
    /// The block is not served, or the block at that number has another id.
    UnknownBlock {
        endpoint: Endpoint,
        include_extensions: bool,
        echo: Vec<u8>,
    },
    /// No such endpoint.
    NotFound,
    /// A request-target that is empty, not absolute or holds `..`.
    IllegalTarget,
    /// A body larger than the cap, in bytes.
    TooLarge(usize),
    /// The server cannot answer now; the text says why. An HTTP router or proxy in front of it
    /// may retry elsewhere.
    Busy(String),
}

impl ApiError {
    /// All request slots stayed taken for the whole queue wait.
    pub fn too_many_requests(in_flight: usize) -> Self {
        Self::Busy(format!("Too many requests in flight: {in_flight}"))
    }

    /// The memory budget refused the request's reservation.
    pub fn too_many_bytes(in_flight: usize) -> Self {
        Self::Busy(format!("Too many bytes in flight: {in_flight}"))
    }

    /// The block could not be read from its log.
    pub fn read_failed() -> Self {
        Self::Busy("Block read failed".into())
    }

    /// The block's bytes did not decode, or the body outgrew its reservation.
    pub fn integrity_failed() -> Self {
        Self::Busy("Block data failed validation".into())
    }

    /// Anything else, such as a panic while rendering.
    pub fn internal() -> Self {
        Self::Busy("Internal error".into())
    }

    /// The HTTP status.
    pub fn status(&self) -> u16 {
        match self {
            Self::BodyRequired
            | Self::Unparsable
            | Self::UnknownBlock { .. }
            | Self::IllegalTarget => 400,
            Self::NotFound => 404,
            Self::TooLarge(_) => 413,
            Self::BadLength { .. } | Self::BadId { .. } => 500,
            Self::Busy(_) => 503,
        }
    }

    /// The JSON body.
    pub fn body(&self) -> Vec<u8> {
        match self {
            Self::BodyRequired => invalid_request(257, "A Request body is required"),
            Self::Unparsable => invalid_request(278, "Unable to parse valid input from POST body"),
            Self::BadLength { endpoint } => {
                let line = match endpoint {
                    Endpoint::GetBlockHeader => L_HDR,
                    other => {
                        debug_assert_eq!(*other, Endpoint::GetRawBlock, "no length check");
                        L_RAW
                    }
                };
                let text = "Invalid Block number or ID, must be greater than 0 and less than 65 \
                            characters";
                bad_block_id(clean(text.as_bytes()), line, *endpoint)
            }
            Self::BadId {
                endpoint,
                include_extensions,
                echo,
            } => {
                let line = match (endpoint, include_extensions) {
                    (Endpoint::GetBlockHeader, false) => 2004,
                    (Endpoint::GetBlockHeader, true) => 2015,
                    (other, _) => {
                        debug_assert_eq!(*other, Endpoint::GetRawBlock, "no id check");
                        1963
                    }
                };
                let format = Format {
                    prefix: "Invalid block ID: ",
                    arg: "block_num_or_id",
                };
                bad_block_id(format.message(echo), line, *endpoint)
            }
            Self::UnknownBlock {
                endpoint,
                include_extensions,
                echo,
            } => {
                let (prefix, line) = match (endpoint, include_extensions) {
                    (Endpoint::GetRawBlock, _) => ("Could not find block: ", 1966),
                    (Endpoint::GetBlockHeader, false) => ("Could not find block header: ", 2006),
                    (Endpoint::GetBlockHeader, true) => ("Could not find block header: ", 2017),
                    (Endpoint::GetBlockInfo, _) => ("Could not find block: ", 2049),
                };
                let format = Format {
                    prefix,
                    arg: "block",
                };
                let detail = Detail {
                    message: format.message(echo),
                    file: "chain_plugin.cpp",
                    line,
                    method: endpoint.name(),
                };
                nodeos(
                    400,
                    "Unknown Block",
                    3_100_002,
                    "unknown_block_exception",
                    "Unknown block",
                    Some(detail),
                )
            }
            Self::NotFound => {
                let detail = Detail {
                    message: clean(b"Unknown Endpoint"),
                    file: "beast_http_session.hpp",
                    line: 186,
                    method: "handle_request",
                };
                nodeos(
                    404,
                    "Not Found",
                    0,
                    "exception",
                    "unspecified",
                    Some(detail),
                )
            }
            Self::IllegalTarget => nodeos(400, "Illegal request-target", 0, "", "", None),
            Self::TooLarge(cap) => nodeos(
                413,
                "Payload Too Large",
                413,
                "Payload Too Large",
                &format!("Request body exceeds {cap} bytes"),
                None,
            ),
            Self::Busy(why) => nodeos(503, "Busy", 503, "Busy", why, None),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyRequired => f.write_str("request body required"),
            Self::Unparsable => f.write_str("request body unparsable"),
            Self::BadLength { .. } => f.write_str("block_num_or_id length out of range"),
            Self::BadId { .. } => f.write_str("invalid block id"),
            Self::UnknownBlock { .. } => f.write_str("unknown block"),
            Self::NotFound => f.write_str("unknown endpoint"),
            Self::IllegalTarget => f.write_str("illegal request-target"),
            Self::TooLarge(cap) => write!(f, "request body exceeds {cap} bytes"),
            Self::Busy(why) => write!(f, "busy: {why}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// A log message format with one argument, at its end: `"<prefix>${<arg>}"`.
struct Format {
    prefix: &'static str,
    arg: &'static str,
}

impl Format {
    /// fc `log_message::get_limited_message`: the cleaned prefix, then the cleaned `value`, cut
    /// to the argument's share of the 1024-byte budget and marked with `...` when longer.
    fn message(&self, value: &[u8]) -> Vec<u8> {
        let format_len = self.prefix.len() + "${}".len() + self.arg.len();
        let share = MINIMIZE_MAX_SIZE.max(format_len) - format_len;
        let mut out = clean(self.prefix.as_bytes());
        if value.len() > share {
            let cut = share.min(MINIMIZE_MAX_SIZE.saturating_sub(out.len()));
            out.extend_from_slice(&clean(&value[..cut]));
            out.extend_from_slice(b"...");
        } else {
            out.extend_from_slice(&clean(value));
        }
        out
    }
}

/// One `error_detail`.
struct Detail {
    message: Vec<u8>,
    file: &'static str,
    line: u32,
    method: &'static str,
}

/// `invalid_http_request` (`3200006`) from `http_plugin.hpp`'s `parse_params`.
fn invalid_request(line: u32, text: &str) -> Vec<u8> {
    let detail = Detail {
        message: clean(text.as_bytes()),
        file: "http_plugin.hpp",
        line,
        method: "parse_params",
    };
    nodeos(
        400,
        "Invalid Request",
        3_200_006,
        "invalid_http_request",
        "invalid http request",
        Some(detail),
    )
}

/// `block_id_type_exception` (`3010008`), which nodeos answers as a generic 500.
fn bad_block_id(text: Vec<u8>, line: u32, endpoint: Endpoint) -> Vec<u8> {
    let detail = Detail {
        message: text,
        file: "chain_plugin.cpp",
        line,
        method: endpoint.name(),
    };
    nodeos(
        500,
        "Internal Service Error",
        3_010_008,
        "block_id_type_exception",
        "Invalid block ID",
        Some(detail),
    )
}

/// An `error_results` body.
fn nodeos(
    http: u16,
    message: &str,
    code: i64,
    name: &str,
    what: &str,
    detail: Option<Detail>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"{\"code\":");
    write_u64(&mut out, u64::from(http));
    out.extend_from_slice(b",\"message\":");
    write_str(&mut out, message.as_bytes());
    out.extend_from_slice(b",\"error\":{\"code\":");
    write_i64(&mut out, code);
    out.extend_from_slice(b",\"name\":");
    write_str(&mut out, name.as_bytes());
    out.extend_from_slice(b",\"what\":");
    write_str(&mut out, what.as_bytes());
    out.extend_from_slice(b",\"details\":[");
    if let Some(d) = detail {
        out.extend_from_slice(b"{\"message\":");
        write_str(&mut out, &d.message);
        out.extend_from_slice(b",\"file\":");
        write_str(&mut out, d.file.as_bytes());
        out.extend_from_slice(b",\"line_number\":");
        write_u64(&mut out, u64::from(d.line));
        out.extend_from_slice(b",\"method\":");
        write_str(&mut out, d.method.as_bytes());
        out.push(b'}');
    }
    out.extend_from_slice(b"]}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(e: &ApiError) -> String {
        String::from_utf8(e.body()).unwrap()
    }

    /// The nodeos body for one error with one detail.
    fn expected(head: &str, detail: &str, file: &str, line: u32, method: &str) -> String {
        format!(
            "{head},\"details\":[{{\"message\":\"{detail}\",\"file\":\"{file}\",\
             \"line_number\":{line},\"method\":\"{method}\"}}]}}}}"
        )
    }

    const UB: &str = r#"{"code":400,"message":"Unknown Block","error":{"code":3100002,"name":"unknown_block_exception","what":"Unknown block""#;
    const ID: &str = r#"{"code":500,"message":"Internal Service Error","error":{"code":3010008,"name":"block_id_type_exception","what":"Invalid block ID""#;
    const REQ: &str = r#"{"code":400,"message":"Invalid Request","error":{"code":3200006,"name":"invalid_http_request","what":"invalid http request""#;

    fn ub(endpoint: Endpoint, include_extensions: bool, echo: &[u8]) -> ApiError {
        ApiError::UnknownBlock {
            endpoint,
            include_extensions,
            echo: echo.to_vec(),
        }
    }

    fn bad_id(endpoint: Endpoint, include_extensions: bool, echo: &[u8]) -> ApiError {
        ApiError::BadId {
            endpoint,
            include_extensions,
            echo: echo.to_vec(),
        }
    }

    #[test]
    fn unknown_block_bodies_name_their_endpoint_and_mode() {
        use Endpoint::*;
        let raw = r#"{"code":400,"message":"Unknown Block","error":{"code":3100002,"name":"unknown_block_exception","what":"Unknown block","details":[{"message":"Could not find block: 123","file":"chain_plugin.cpp","line_number":1966,"method":"get_raw_block"}]}}"#;
        assert_eq!(body(&ub(GetRawBlock, false, b"123")), raw);
        assert_eq!(body(&ub(GetRawBlock, true, b"123")), raw);
        let cases = [
            (
                GetBlockHeader,
                false,
                "Could not find block header: 0001",
                2006,
                "get_block_header",
            ),
            (
                GetBlockHeader,
                true,
                "Could not find block header: 0001",
                2017,
                "get_block_header",
            ),
            (
                GetBlockInfo,
                false,
                "Could not find block: 0001",
                2049,
                "get_block_info",
            ),
        ];
        for (ep, ext, detail, line, method) in cases {
            let want = expected(UB, detail, "chain_plugin.cpp", line, method);
            assert_eq!(body(&ub(ep, ext, b"0001")), want);
        }
        for ep in Endpoint::ALL {
            assert_eq!(ub(ep, false, b"1").status(), 400);
        }
    }

    #[test]
    fn bad_id_and_length_bodies() {
        use Endpoint::*;
        let cases = [
            (GetRawBlock, false, 1963, "get_raw_block"),
            (GetBlockHeader, false, 2004, "get_block_header"),
            (GetBlockHeader, true, 2015, "get_block_header"),
        ];
        for (ep, ext, line, method) in cases {
            let e = bad_id(ep, ext, b"zz");
            assert_eq!(e.status(), 500);
            let want = expected(ID, "Invalid block ID: zz", "chain_plugin.cpp", line, method);
            assert_eq!(body(&e), want);
        }
        let len = "Invalid Block number or ID, must be greater than 0 and less than 65 characters";
        for (ep, line, method) in [
            (GetRawBlock, L_RAW, "get_raw_block"),
            (GetBlockHeader, L_HDR, "get_block_header"),
        ] {
            let e = ApiError::BadLength { endpoint: ep };
            assert_eq!(e.status(), 500);
            assert_eq!(
                body(&e),
                expected(ID, len, "chain_plugin.cpp", line, method)
            );
        }
    }

    #[test]
    fn echoed_input_is_cleaned_as_fc_does() {
        let e = bad_id(Endpoint::GetRawBlock, false, b"a\"\\\x01\t\xffb");
        let want = expected(
            ID,
            r#"Invalid block ID: a\"\\\\u0001\tb"#,
            "chain_plugin.cpp",
            1963,
            "get_raw_block",
        );
        assert_eq!(body(&e), want);
        // A long argument is cut to its share of fc's 1024-byte message budget.
        let long = vec![b'x'; 2000];
        let e = bad_id(Endpoint::GetRawBlock, false, &long);
        let text = format!("Invalid block ID: {}...", "x".repeat(1024 - 36));
        assert_eq!(
            body(&e),
            expected(ID, &text, "chain_plugin.cpp", 1963, "get_raw_block")
        );
    }

    #[test]
    fn request_and_endpoint_bodies() {
        let want = expected(
            REQ,
            "A Request body is required",
            "http_plugin.hpp",
            257,
            "parse_params",
        );
        assert_eq!(body(&ApiError::BodyRequired), want);
        let want = expected(
            REQ,
            "Unable to parse valid input from POST body",
            "http_plugin.hpp",
            278,
            "parse_params",
        );
        assert_eq!(body(&ApiError::Unparsable), want);
        assert_eq!(
            body(&ApiError::NotFound),
            r#"{"code":404,"message":"Not Found","error":{"code":0,"name":"exception","what":"unspecified","details":[{"message":"Unknown Endpoint","file":"beast_http_session.hpp","line_number":186,"method":"handle_request"}]}}"#
        );
        assert_eq!(
            body(&ApiError::IllegalTarget),
            r#"{"code":400,"message":"Illegal request-target","error":{"code":0,"name":"","what":"","details":[]}}"#
        );
        let statuses = [
            (ApiError::BodyRequired, 400),
            (ApiError::Unparsable, 400),
            (ApiError::NotFound, 404),
            (ApiError::IllegalTarget, 400),
            (ApiError::TooLarge(1), 413),
            (ApiError::internal(), 503),
        ];
        for (e, status) in statuses {
            assert_eq!(e.status(), status, "{e}");
        }
    }

    #[test]
    fn server_failure_bodies() {
        assert_eq!(
            body(&ApiError::TooLarge(2_097_152)),
            r#"{"code":413,"message":"Payload Too Large","error":{"code":413,"name":"Payload Too Large","what":"Request body exceeds 2097152 bytes","details":[]}}"#
        );
        assert_eq!(
            body(&ApiError::too_many_requests(5)),
            r#"{"code":503,"message":"Busy","error":{"code":503,"name":"Busy","what":"Too many requests in flight: 5","details":[]}}"#
        );
        let busy = |what: &str| {
            format!(
                r#"{{"code":503,"message":"Busy","error":{{"code":503,"name":"Busy","what":"{what}","details":[]}}}}"#
            )
        };
        assert_eq!(
            body(&ApiError::too_many_bytes(70)),
            busy("Too many bytes in flight: 70")
        );
        assert_eq!(body(&ApiError::read_failed()), busy("Block read failed"));
        assert_eq!(
            body(&ApiError::integrity_failed()),
            busy("Block data failed validation")
        );
        assert_eq!(body(&ApiError::internal()), busy("Internal error"));
    }
}
