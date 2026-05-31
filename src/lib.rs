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

pub mod abi;
pub mod blocks;
pub mod delta;
pub mod disk;
pub mod ship;
pub mod trace;
