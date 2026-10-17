//! abi-scanner — high-performance Antelope SHiP ABI scanner.
//!
//! Extracts every contract ABI version (`setabi`) across a chain's history into
//! a portable, Elasticsearch-ingestible NDJSON snapshot, in the Hyperion
//! abi-index shape: `{account, block, abi, abi_hex, actions[], tables[]}`.
//!
//! Two sources, one decode path:
//!   * [`disk`] reads the append-only `chain_state_history.{log,index}` directly,
//!     in parallel, read-only — bypassing nodeos/SHiP entirely.
//!   * [`ship`] streams **deltas-only** from a SHiP node or fleet-router.
//!
//! Both source the raw `table_delta[]` bytes, walk **only** the `account` table
//! (skipping the dense `contract_row` payload by length), and hand-parse each
//! `account` row (`[variant][name u64][creation_date u32][abi bytes]`) — so no
//! SHiP ABI is required. rs_abieos is used only for `name_to_string` and
//! `abi_bin_to_json`.
//!
//! Module map:
//!   * [`delta`] — varuint, the get_blocks_result_v0 envelope, the table_delta[]
//!     walk, and the manual `account` row decode.
//!   * [`abi`]   — abi_def decode (hex → JSON) and the NDJSON doc builder
//!     (tags malformed on-chain ABIs with `abi_decode_error`).
//!   * [`disk`]  — the parallel direct-from-disk reader.
//!   * [`ship`]  — the SHiP websocket scanner.
//!
//! Feature `archive` adds the strict, fail-closed pieces for serving frozen logs over the
//! SHiP protocol: `binio` (positional reads, the open-file LRU, the fc-compatible byte
//! reader), `shiplog` and `blocklog` (index-addressed log readers), `catalog` (file discovery
//! and contiguous per-kind catalogs) and `protocol` (request decoding, status and result
//! encoding, the vendored Spring SHiP ABI). Feature `testkit` adds `testkit`, synthetic
//! fixture writers for tests.

pub mod abi;
#[cfg(feature = "archive")]
pub mod binio;
#[cfg(feature = "archive")]
pub mod blocklog;
pub mod blocks;
#[cfg(feature = "archive")]
pub mod catalog;
pub mod delta;
pub mod disk;
#[cfg(feature = "archive")]
pub mod protocol;
#[cfg(feature = "ship")]
pub mod ship;
#[cfg(feature = "archive")]
pub mod shiplog;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod trace;
