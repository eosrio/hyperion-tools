//! nodeos-compatible `/v1/chain` block endpoints over stored blocks: the request parsing, the
//! response bodies and the error bodies a server of archived ranges answers with, byte for
//! byte as a Spring 1.1.0 to 1.2.2 node holding the same block answers
//! (`plugins/chain_plugin/chain_plugin.cpp`, `plugins/http_plugin`).
//!
//! Everything here is a pure function over bytes, so an HTTP layer only frames, routes, reads
//! the block and sends what these return:
//!
//!   * [`params`] — nodeos's empty-body rule, a parser for the subset of fc's legacy JSON that
//!     fc and it read identically, fc's field conversions and the `block_num_or_id` rules.
//!   * [`block`] — a borrowed, fail-closed walk of a packed `signed_block`.
//!   * [`render`] — the `get_block_info`, `get_block_header` (both modes) and `get_raw_block`
//!     bodies, and the `get_supported_apis` list.
//!   * [`errors`] — nodeos's error bodies for these endpoints with Spring 1.1.0 to 1.2.2's
//!     source lines, and `503 Busy` bodies for the server's own failures.
//!   * [`fcjson`] and [`text`] — fc's JSON writing (string escaping, large-integer quoting,
//!     hex) and the text forms of names, block timestamps, public keys and signatures.
//!
//! These endpoints need no contract ABI: their bodies carry no action data. `get_block`, whose
//! actions are decoded through the ABI each account had, is not rendered here.

pub mod block;
pub mod errors;
pub mod fcjson;
pub mod params;
pub mod render;
pub mod text;

pub use block::{SignedBlock, parse_signed_block};
pub use errors::ApiError;
pub use params::{is_empty_content, parse, to_uint64};
pub use render::{render, supported_apis};

use crate::binio::num_from_id;

/// A block endpoint this module renders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
    GetBlockInfo,
    GetBlockHeader,
    GetRawBlock,
}

impl Endpoint {
    /// Every endpoint, sorted by path (the order nodeos lists its handlers in).
    pub const ALL: [Self; 3] = [Self::GetBlockHeader, Self::GetBlockInfo, Self::GetRawBlock];

    /// The endpoint whose path is exactly `target`. nodeos looks handlers up by the whole raw
    /// request-target, so a query string or a trailing slash matches nothing.
    pub fn from_target(target: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ep| ep.path() == target)
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::GetBlockInfo => "/v1/chain/get_block_info",
            Self::GetBlockHeader => "/v1/chain/get_block_header",
            Self::GetRawBlock => "/v1/chain/get_raw_block",
        }
    }

    /// The handler's name: the `method` of its error details, and a fit metric label.
    pub fn name(self) -> &'static str {
        match self {
            Self::GetBlockInfo => "get_block_info",
            Self::GetBlockHeader => "get_block_header",
            Self::GetRawBlock => "get_raw_block",
        }
    }
}

/// The block a request names: a number, or an id whose first four bytes give the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockRef {
    Num(u32),
    /// Zero-padded when the request gave fewer than 32 bytes, as fc's `sha256` conversion does.
    Id([u8; 32]),
}

impl BlockRef {
    /// The block number to look up.
    pub fn num(self) -> u32 {
        match self {
            Self::Num(n) => n,
            Self::Id(id) => num_from_id(&id),
        }
    }

    /// Whether block `num()`, whose id is `id`, is the block referenced: always for a number;
    /// for an id only when it is that block's id (Spring `controller::fetch_block_by_id`).
    pub fn matches(self, id: &[u8; 32]) -> bool {
        match self {
            Self::Num(_) => true,
            Self::Id(want) => want == *id,
        }
    }
}

/// A parsed block endpoint request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub endpoint: Endpoint,
    pub block: BlockRef,
    /// What an Unknown Block answer echoes: the `block_num_or_id` string as fc converted it, or
    /// `get_block_info`'s 32-bit block number in decimal. Raw bytes, since a JSON string may hold
    /// any; the error writer cleans them as fc does.
    pub echo: Vec<u8>,
    /// `get_block_header` only; false elsewhere.
    pub include_extensions: bool,
}

impl Request {
    /// The answer when the block is not served, or the block at that number has another id.
    pub fn unknown_block(&self) -> ApiError {
        ApiError::UnknownBlock {
            endpoint: self.endpoint,
            include_extensions: self.include_extensions,
            echo: self.echo.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_match_whole_targets_only() {
        for ep in Endpoint::ALL {
            assert_eq!(Endpoint::from_target(ep.path()), Some(ep));
            assert!(ep.path().ends_with(ep.name()));
            for other in ["?", "?x=1", "/", "#"] {
                assert_eq!(
                    Endpoint::from_target(&format!("{}{other}", ep.path())),
                    None
                );
            }
        }
        for target in [
            "/v1/chain/get_block",
            "/v1/chain/get_info",
            "/v1/chain/get_block_header_state",
            "/v1/node/get_supported_apis",
            "/V1/chain/get_raw_block",
            "v1/chain/get_raw_block",
            "",
        ] {
            assert_eq!(Endpoint::from_target(target), None, "{target}");
        }
        let mut paths: Vec<_> = Endpoint::ALL.iter().map(|ep| ep.path()).collect();
        paths.sort_unstable();
        assert_eq!(paths, Endpoint::ALL.map(Endpoint::path));
    }

    #[test]
    fn block_refs_resolve_to_numbers_and_match_ids() {
        let mut id = [0xab; 32];
        id[..4].copy_from_slice(&7u32.to_be_bytes());
        assert_eq!(BlockRef::Num(7).num(), 7);
        assert_eq!(BlockRef::Id(id).num(), 7);
        assert!(BlockRef::Num(7).matches(&id));
        assert!(BlockRef::Id(id).matches(&id));
        let mut other = id;
        other[31] ^= 1;
        assert!(!BlockRef::Id(id).matches(&other));
    }
}
