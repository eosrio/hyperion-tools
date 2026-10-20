//! Minimal nodeos block-log reader — just enough to recover the per-block header fields
//! (`@timestamp` + `producer`) that live ONLY in `signed_block`, not in any state-history or
//! trace payload. Used by `action-proto` to complete the Hyperion `<chain>-action-v1` doc.
//!
//! On-disk layout (verified against a WAX archive node, block-log version 3):
//!   * `blocks.index` — one `u64` LE byte-offset per block, indexed by `block_num - first_block`.
//!   * `blocks.log`   — `[u32 version][u32 first_block_num][genesis/chain_id ...]` header, then
//!     per block the serialized `signed_block` (+ an 8-byte position trailer). The `signed_block`
//!     begins with `block_header`, whose first two fields are exactly what we need:
//!     `timestamp: block_timestamp_type` (u32 slot) and `producer: name` (u64).
//!
//! We never deserialize the whole block — `header()` seeks the index, then reads 12 bytes at the
//! block's offset. `block_timestamp_type` is a 500 ms slot counter from the 2000-01-01 epoch.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail};

/// Random-access reader over a nodeos `blocks.{log,index}` pair.
pub struct BlockLog {
    log: File,
    index: File,
    first_block: u32,
    n_blocks: u32,
}

impl BlockLog {
    /// Open the block log in `dir` (the nodeos `blocks` directory).
    pub fn open(dir: &str) -> Result<Self> {
        let log_path = format!("{dir}/blocks.log");
        let idx_path = format!("{dir}/blocks.index");
        let mut log = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
        let index = File::open(&idx_path).with_context(|| format!("open {idx_path}"))?;
        let mut hdr = [0u8; 8];
        log.read_exact(&mut hdr).context("read block-log header")?;
        let version = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let first_block = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if version == 0 || first_block == 0 {
            bail!(
                "{log_path} is not a recognizable block log (version={version}, first_block={first_block})"
            );
        }
        let n_blocks = (index.metadata()?.len() / 8) as u32;
        Ok(Self {
            log,
            index,
            first_block,
            n_blocks,
        })
    }

    pub fn first_block(&self) -> u32 {
        self.first_block
    }

    pub fn last_block(&self) -> u32 {
        self.first_block + self.n_blocks.saturating_sub(1)
    }

    /// `(producer_name_u64, slot)` from the `block_header` prefix of `block_num`, or `None` if
    /// the block is outside the log's range or the read fails.
    pub fn header(&mut self, block_num: u32) -> Option<(u64, u32)> {
        if block_num < self.first_block || block_num > self.last_block() {
            return None;
        }
        let idx_off = (block_num - self.first_block) as u64 * 8;
        self.index.seek(SeekFrom::Start(idx_off)).ok()?;
        let mut ob = [0u8; 8];
        self.index.read_exact(&mut ob).ok()?;
        let log_off = u64::from_le_bytes(ob);
        self.log.seek(SeekFrom::Start(log_off)).ok()?;
        let mut prefix = [0u8; 12];
        self.log.read_exact(&mut prefix).ok()?;
        let slot = u32::from_le_bytes(prefix[0..4].try_into().unwrap());
        let producer = u64::from_le_bytes(prefix[4..12].try_into().unwrap());
        Some((producer, slot))
    }
}

/// Format a `block_timestamp_type` slot as the Hyperion `@timestamp` string
/// (`YYYY-MM-DDTHH:MM:SS.mmm`, no timezone suffix) — identical to abieos's `block_timestamp_type`
/// rendering of the SHiP block timestamp.
pub fn slot_to_iso(slot: u32) -> String {
    crate::provider::TimestampProfile::ANTELOPE.slot_to_iso(slot)
}

/// Render a slot using the same validated protocol profile as nested value codecs.
pub fn slot_to_iso_with_provider(
    slot: u32,
    provider: &crate::provider::ProtocolProvider,
) -> Result<String> {
    Ok(provider.timestamp_profile()?.slot_to_iso(slot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_formats_match_known_blocks() {
        // Verified on the WAX node: block 2 slot, and block 190,373,745 slot.
        assert_eq!(slot_to_iso(1_229_428_973), "2019-06-24T18:01:26.500");
        assert_eq!(slot_to_iso(1_419_922_240), "2022-07-01T03:25:20.000");
    }

    #[test]
    fn epoch_slot_is_2000() {
        assert_eq!(slot_to_iso(0), "2000-01-01T00:00:00.000");
    }
}
