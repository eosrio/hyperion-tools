//! Synthetic fixture writers for tests of the archive readers and of the servers built on them.
//!
//! [`ship`] writes state-history logs and [`blocks`] block logs, each on its own terms.
//! [`SyntheticChain`] ties the two together: one chain of real, linked blocks whose ids every
//! log it writes shares, so the block log's `previous` fields, the SHiP entry headers and any
//! seam check over them agree, whatever the layout (head files, retained files, split or
//! partitioned runs, any number of directories). Every payload is deterministic and starts with
//! its kind and block number, so a test can tell what it was served.

pub mod blocks;
pub mod ship;
pub mod traces;

use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};

use crate::catalog::PathPair;
use crate::shiplog::LogKind;
use blocks::{Block, BlockSpec, Genesis, Preamble};
use ship::{Encoding, LogBuilder};

/// What a [`SyntheticChain`] writer names the file it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileName<'a> {
    /// `<stem>.log`, a node's current log.
    Head,
    /// `<stem>-<first>-<last>.log` for the blocks written, as Spring names a retained file.
    Retained,
    /// `<name>.log`, for names that say something else.
    Custom(&'a str),
}

impl FileName<'_> {
    fn stem(self, stem: &str, blocks: &Range<u32>) -> String {
        match self {
            Self::Head => stem.to_owned(),
            Self::Retained => ship::retained_stem(stem, blocks.start, blocks.end.saturating_sub(1)),
            Self::Custom(name) => name.to_owned(),
        }
    }
}

/// A chain of [`BlockSpec::synthetic`] blocks on the EOS mainnet genesis (what
/// [`blocks::chain`] builds) with deterministic state-history payloads for each block, and
/// writers for logs of it that agree on every id.
#[derive(Clone, Debug)]
pub struct SyntheticChain {
    genesis: Genesis,
    first: u32,
    /// The id block `first` follows.
    root: [u8; 32],
    blocks: Vec<Block>,
    payload_lens: HashMap<(LogKind, u32), usize>,
}

impl SyntheticChain {
    /// Blocks `first..first + count`. Block 1 follows the all-zero id, as on a real chain; a
    /// later first block follows [`blocks::synthetic_id`]`(first - 1)`.
    pub fn new(first: u32, count: u32) -> Self {
        assert!(first > 0, "block numbers start at 1");
        let root = if first == 1 {
            [0; 32]
        } else {
            blocks::synthetic_id(first - 1)
        };
        let blocks = blocks::extend(&root, count, BlockSpec::synthetic)
            .unwrap_or_else(|e| panic!("a chain of {count} blocks from {first}: {e:#}"));
        Self {
            genesis: Genesis::eos_mainnet(),
            first,
            root,
            blocks,
            payload_lens: HashMap::new(),
        }
    }

    /// The blocks of the chain, `[first, end)`.
    pub fn range(&self) -> (u32, u32) {
        (self.first, self.first + self.blocks.len() as u32)
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Block `n`, which must be in [`Self::range`].
    pub fn block(&self, n: u32) -> &Block {
        &self.blocks[self.offset(n)]
    }

    /// The id of block `n`: any block of the chain, or `first - 1`, the block it follows.
    pub fn id(&self, n: u32) -> [u8; 32] {
        if n.checked_add(1) == Some(self.first) {
            self.root
        } else {
            self.block(n).id
        }
    }

    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn chain_id(&self) -> [u8; 32] {
        self.genesis.chain_id()
    }

    /// The preamble of a block log starting at block `first`, as nodeos writes it today: v3
    /// with the genesis state from block 1, v3 with the chain id after it.
    pub fn preamble(&self, first: u32) -> Preamble {
        if first == 1 {
            Preamble::V3Genesis {
                genesis: self.genesis.clone(),
            }
        } else {
            Preamble::V3ChainId {
                first,
                chain_id: self.chain_id(),
            }
        }
    }

    /// The length of block `n`'s `kind` payload: set by [`Self::set_payload_len`], otherwise
    /// a few dozen to a few hundred bytes, varying with the block.
    pub fn payload_len(&self, kind: LogKind, n: u32) -> usize {
        self.payload_lens
            .get(&(kind, n))
            .copied()
            .unwrap_or(16 + (n as usize * 37 + usize::from(tag(kind))) % 300)
    }

    /// Make block `n`'s `kind` payload `len` bytes long (a large entry, an empty one).
    pub fn set_payload_len(&mut self, kind: LogKind, n: u32, len: usize) -> &mut Self {
        self.payload_lens.insert((kind, n), len);
        self
    }

    /// Block `n`'s `kind` payload, the bytes a server inflates and sends: a [`ship`] synthetic
    /// payload whose first bytes (as many as fit) are the kind's tag (`T`, `C` or `F`) and the
    /// block number, little-endian.
    pub fn payload(&self, kind: LogKind, n: u32) -> Vec<u8> {
        let len = self.payload_len(kind, n);
        let seed = n.wrapping_mul(3).wrapping_add(u32::from(tag(kind)));
        let mut p = ship::synthetic_payload(seed, len);
        let mut prefix = vec![tag(kind)];
        prefix.extend_from_slice(&n.to_le_bytes());
        let k = prefix.len().min(len);
        p[..k].copy_from_slice(&prefix[..k]);
        p
    }

    /// The `kind` log of `blocks`, in `encoding`, still in memory: for tests that fork, damage
    /// or extend it before writing it.
    pub fn ship_log(&self, kind: LogKind, blocks: Range<u32>, encoding: Encoding) -> LogBuilder {
        self.check(&blocks);
        let mut b = LogBuilder::new(encoding);
        for n in blocks {
            b.push(self.id(n), &self.payload(kind, n));
        }
        b
    }

    /// Write the `kind` log of `blocks` (none: a 0-byte log) and its index into `dir`,
    /// creating it.
    pub fn write_ship(
        &self,
        dir: &Path,
        kind: LogKind,
        blocks: Range<u32>,
        encoding: Encoding,
        name: FileName,
    ) -> Result<PathPair> {
        let stem = name.stem(kind.stem(), &blocks);
        let (log, index) = self
            .ship_log(kind, blocks, encoding)
            .write(dir, &stem)
            .with_context(|| format!("write {stem} into {}", dir.display()))?;
        Ok(PathPair { log, index })
    }

    /// Write the `kind` log of `blocks` the way a node with `state-history-stride = stride`
    /// leaves it ([`ship::write_partitioned`]): retained files in `retained_dir`, the rest in
    /// the head log in `head_dir`. Returns the retained files first, the head log last.
    pub fn write_ship_partitioned(
        &self,
        head_dir: &Path,
        retained_dir: &Path,
        kind: LogKind,
        blocks: Range<u32>,
        stride: u32,
        encoding: Encoding,
    ) -> Result<Vec<PathPair>> {
        self.check(&blocks);
        let entries = blocks.map(|n| (self.id(n), self.payload(kind, n)));
        let files = ship::write_partitioned(
            head_dir,
            retained_dir,
            kind.stem(),
            stride,
            encoding,
            entries,
        )
        .with_context(|| format!("write the partitioned {} log", kind.label()))?;
        Ok(files
            .into_iter()
            .map(|(log, index)| PathPair { log, index })
            .collect())
    }

    /// Write the block log of `blocks` (none: a preamble-only log) and its index into `dir`,
    /// creating it, with [`Self::preamble`]`(blocks.start)`.
    pub fn write_blocks(&self, dir: &Path, blocks: Range<u32>, name: FileName) -> Result<PathPair> {
        let stem = name.stem("blocks", &blocks);
        let preamble = self.preamble(blocks.start);
        let slice = self.slice(blocks);
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let files = blocks::write(dir, &stem, &preamble, slice)?;
        Ok(PathPair {
            log: files.log,
            index: files.index,
        })
    }

    /// Write the block log of `blocks` split on `stride` windows ([`blocks::write_split`]),
    /// as `spring-util block-log split-blocks` or a node's retained files lay it out.
    pub fn write_blocks_split(
        &self,
        dir: &Path,
        blocks: Range<u32>,
        stride: u32,
    ) -> Result<Vec<PathPair>> {
        let preamble = self.preamble(blocks.start);
        let slice = self.slice(blocks);
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(blocks::write_split(dir, &preamble, slice, stride)?
            .into_iter()
            .map(|f| PathPair {
                log: f.log,
                index: f.index,
            })
            .collect())
    }

    /// Write a node's files for the whole chain into `dir`: the head log of each of `kinds`
    /// in `encoding`, and `blocks.log`.
    pub fn write_node(&self, dir: &Path, kinds: &[LogKind], encoding: Encoding) -> Result<()> {
        let (first, end) = self.range();
        for &kind in kinds {
            self.write_ship(dir, kind, first..end, encoding, FileName::Head)?;
        }
        self.write_blocks(dir, first..end, FileName::Head)?;
        Ok(())
    }

    fn offset(&self, n: u32) -> usize {
        let (first, end) = self.range();
        assert!(
            (first..end).contains(&n),
            "block {n} is not in the chain's {first}..{end}"
        );
        (n - first) as usize
    }

    fn check(&self, blocks: &Range<u32>) {
        let (first, end) = self.range();
        assert!(
            blocks.is_empty() || (first <= blocks.start && blocks.end <= end),
            "blocks {blocks:?} are not all in the chain's {first}..{end}"
        );
    }

    fn slice(&self, blocks: Range<u32>) -> &[Block] {
        self.check(&blocks);
        if blocks.is_empty() {
            return &[];
        }
        &self.blocks[self.offset(blocks.start)..=self.offset(blocks.end - 1)]
    }
}

/// The first payload byte of a kind's entries.
fn tag(kind: LogKind) -> u8 {
    match kind {
        LogKind::Trace => b'T',
        LogKind::ChainState => b'C',
        LogKind::Finality => b'F',
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::binio::{FileCache, TestDir};
    use crate::blocklog::BlockLog;
    use crate::shiplog::ShipLog;

    fn cache() -> Arc<FileCache> {
        FileCache::new(16)
    }

    #[test]
    fn every_log_of_a_chain_shares_its_ids() {
        let dir = TestDir::new("testkit-chain");
        let chain = SyntheticChain::new(1, 40);
        assert_eq!(chain.range(), (1, 41));
        assert_eq!(chain.id(0), [0; 32], "block 1 follows the all-zero id");
        for (kind, encoding) in LogKind::ALL.into_iter().zip(Encoding::ALL) {
            let pair = chain
                .write_ship(dir.path(), kind, 1..41, encoding, FileName::Head)
                .unwrap();
            let log = ShipLog::open(&pair.log, &pair.index, kind, &cache())
                .unwrap()
                .unwrap();
            assert_eq!(log.range(), (1, 41));
            for n in 1..41 {
                let h = log.header(n).unwrap();
                assert_eq!(h.id, chain.id(n));
                let mut raw = Vec::new();
                log.read_inline(&h, u64::MAX, &mut raw).unwrap();
                assert_eq!(raw, chain.payload(kind, n), "{kind:?} block {n}");
            }
        }
        let pair = chain
            .write_blocks(dir.path(), 1..41, FileName::Head)
            .unwrap();
        assert!(pair.log.ends_with("blocks.log"));
        let blocks = BlockLog::open(&pair.log, &pair.index, &cache())
            .unwrap()
            .unwrap();
        assert_eq!((blocks.range(), blocks.version()), ((1, 41), 3));
        assert_eq!(blocks.chain_id(), chain.chain_id());
        for n in 1..41 {
            assert_eq!(blocks.id(n).unwrap(), chain.id(n));
            assert_eq!(blocks.previous(n).unwrap(), chain.id(n - 1));
        }
    }

    #[test]
    fn a_chain_after_block_one_links_to_its_predecessor() {
        let dir = TestDir::new("testkit-later");
        let chain = SyntheticChain::new(500, 30);
        assert_eq!(chain.id(499), blocks::synthetic_id(499));
        assert_eq!(chain.block(500).previous, chain.id(499));
        assert_eq!(
            chain.preamble(510),
            Preamble::V3ChainId {
                first: 510,
                chain_id: chain.chain_id()
            }
        );

        let retained = chain
            .write_ship(
                dir.path(),
                LogKind::Trace,
                500..520,
                Encoding::SizedStored,
                FileName::Retained,
            )
            .unwrap();
        assert!(retained.log.ends_with("trace_history-500-519.log"));
        assert!(retained.index.ends_with("trace_history-500-519.index"));
        let custom = chain
            .write_blocks(dir.path(), 510..530, FileName::Custom("odd"))
            .unwrap();
        assert!(custom.log.ends_with("odd.log"));
        let blocks = BlockLog::open(&custom.log, &custom.index, &cache())
            .unwrap()
            .unwrap();
        assert_eq!(blocks.range(), (510, 530));
        assert_eq!(blocks.previous(510).unwrap(), chain.id(509));

        // No blocks: a 0-byte SHiP log, a preamble-only block log.
        let empty = chain
            .write_ship(
                dir.path(),
                LogKind::ChainState,
                520..520,
                Encoding::SizedStored,
                FileName::Head,
            )
            .unwrap();
        assert_eq!(fs::metadata(&empty.log).unwrap().len(), 0);
        let head = chain
            .write_blocks(&dir.path().join("blocks"), 530..530, FileName::Head)
            .unwrap();
        assert_eq!(fs::metadata(&head.log).unwrap().len(), 48);
    }

    #[test]
    fn payloads_name_their_kind_and_block_and_take_set_lengths() {
        let mut chain = SyntheticChain::new(1, 10);
        let p = chain.payload(LogKind::ChainState, 7);
        assert_eq!(p.len(), chain.payload_len(LogKind::ChainState, 7));
        assert_eq!(&p[..5], &[b'C', 7, 0, 0, 0]);
        assert_ne!(
            chain.payload(LogKind::Trace, 7)[5..],
            p[5..],
            "kinds differ beyond the prefix"
        );
        chain
            .set_payload_len(LogKind::Trace, 3, 100_000)
            .set_payload_len(LogKind::Finality, 4, 2);
        assert_eq!(chain.payload(LogKind::Trace, 3).len(), 100_000);
        assert_eq!(chain.payload(LogKind::Finality, 4), [b'F', 4]);
        assert!(chain.payload_len(LogKind::Trace, 5) < 400);
    }

    #[test]
    fn partitioned_and_split_layouts_line_up() {
        let dir = TestDir::new("testkit-layouts");
        let chain = SyntheticChain::new(2, 69);
        let (head, retained) = (dir.path().join("head"), dir.path().join("retained"));
        let ship = chain
            .write_ship_partitioned(
                &head,
                &retained,
                LogKind::Trace,
                2..71,
                20,
                Encoding::SizedDeflate,
            )
            .unwrap();
        let names: Vec<_> = ship
            .iter()
            .map(|p| p.log.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "trace_history-2-20.log",
                "trace_history-21-40.log",
                "trace_history-41-60.log",
                "trace_history.log"
            ]
        );
        let blocks = chain
            .write_blocks_split(&dir.path().join("blocks"), 2..71, 20)
            .unwrap();
        let ranges: Vec<_> = blocks
            .iter()
            .map(|p| {
                BlockLog::open(&p.log, &p.index, &cache())
                    .unwrap()
                    .unwrap()
                    .range()
            })
            .collect();
        assert_eq!(ranges, [(2, 21), (21, 41), (41, 61), (61, 71)]);
    }

    #[test]
    fn a_node_directory_holds_every_log() {
        let dir = TestDir::new("testkit-node");
        let chain = SyntheticChain::new(1, 5);
        chain
            .write_node(dir.path(), &LogKind::ALL, Encoding::SizedStored)
            .unwrap();
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "blocks.index",
                "blocks.log",
                "chain_state_history.index",
                "chain_state_history.log",
                "finality_data_history.index",
                "finality_data_history.log",
                "trace_history.index",
                "trace_history.log"
            ]
        );
    }
}
