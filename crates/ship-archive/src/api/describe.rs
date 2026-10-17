//! The two answers the chain API gives from memory, with no file read:
//!
//!   * `/v1/node/get_supported_apis`, nodeos's list of its endpoints: `{"apis":[...]}`, the
//!     block endpoints served, sorted. As in nodeos, the list never names itself.
//!   * `/v1/archive/describe`, what an HTTP router or proxy in front of nodeos-compatible APIs
//!     needs to route block requests here: the chain, the last block (the SHiP head, `L*`), the
//!     blocks the endpoints serve (`[begin, end)`, the served block-log range), the endpoints,
//!     and the request slots. It is not nodeos's, so it is never listed.
//!
//! ```json
//! {"server":"antelope-ship-archive/0.1.0","chain_id":"<64 hex>",
//!  "last":{"num":50000000,"id":"<64 hex>"},
//!  "blocks":{"begin":1,"end":50000001},
//!  "apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","/v1/chain/get_raw_block"],
//!  "get_block":"off","max_in_flight":4,"in_flight":0}
//! ```
//!
//! `get_block` is `"off"`: decoded `get_block` needs the contracts' ABIs, so it is left to
//! live nodes, and `/v1/chain/get_block` is answered 404 here.

use hyperion_ship::chainapi::fcjson::{write_hex, write_str, write_u64};
use hyperion_ship::chainapi::{Endpoint, supported_apis};

use crate::SERVER_HEADER;
use crate::store::Store;

/// The path of the self-description.
pub const DESCRIBE: &str = "/v1/archive/describe";
/// nodeos's endpoint list.
pub const SUPPORTED_APIS: &str = "/v1/node/get_supported_apis";
/// How `get_block` is served: not at all.
pub const GET_BLOCK: &str = "off";

/// The endpoints served, in nodeos's (sorted) order.
pub fn served() -> &'static [Endpoint] {
    &Endpoint::ALL
}

/// `get_supported_apis`'s body.
pub fn supported() -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    supported_apis(served(), &mut out);
    out
}

/// `/v1/archive/describe`'s body, with `in_flight` of `max_in_flight` request slots taken.
pub fn describe(store: &Store, max_in_flight: usize, in_flight: u64) -> Vec<u8> {
    let (begin, end) = store.served.blocks;
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(b"{\"server\":");
    write_str(&mut out, SERVER_HEADER.as_bytes());
    out.extend_from_slice(b",\"chain_id\":");
    write_hex(&mut out, &store.chain_id);
    out.extend_from_slice(b",\"last\":{\"num\":");
    write_u64(&mut out, u64::from(store.last.num));
    out.extend_from_slice(b",\"id\":");
    write_hex(&mut out, &store.last.id);
    out.extend_from_slice(b"},\"blocks\":{\"begin\":");
    write_u64(&mut out, u64::from(begin));
    out.extend_from_slice(b",\"end\":");
    write_u64(&mut out, u64::from(end));
    out.extend_from_slice(b"},\"apis\":[");
    for (i, ep) in served().iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        write_str(&mut out, ep.path().as_bytes());
    }
    out.extend_from_slice(b"],\"get_block\":");
    write_str(&mut out, GET_BLOCK.as_bytes());
    out.extend_from_slice(b",\"max_in_flight\":");
    write_u64(&mut out, max_in_flight as u64);
    out.extend_from_slice(b",\"in_flight\":");
    write_u64(&mut out, in_flight);
    out.push(b'}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_served_endpoints_are_sorted_and_never_list_the_listings() {
        let paths: Vec<_> = served().iter().map(|ep| ep.path()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
        assert_eq!(
            String::from_utf8(supported()).unwrap(),
            r#"{"apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","/v1/chain/get_raw_block"]}"#
        );
        for listing in [DESCRIBE, SUPPORTED_APIS, "/v1/chain/get_block"] {
            assert!(!paths.contains(&listing));
            assert_eq!(Endpoint::from_target(listing), None);
        }
    }
}
