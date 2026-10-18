//! Strict block-log reader (versions 1 to 3): preamble and chain id, per-block bounds and
//! bodies, the `previous` link, and `block_header` decoding. Any inconsistency fails closed.
//!
//! Layout (Spring `libraries/chain/block_log.cpp`), all integers little-endian:
//!
//! ```text
//! blocks.log:   | u32 version | preamble | block first | u64 pos(first) | ... | block last | u64 pos(last) |
//! blocks.index: | u64 pos(first) | u64 pos(first + 1) | ... | u64 pos(last) |
//! ```
//!
//! The preamble depends on the version: v1 is the packed `genesis_state` alone (the log starts
//! at block 1); v2 is `u32 first`, the genesis state and the totem; v3 is `u32 first`, then the
//! genesis state when `first == 1` or the 32-byte chain id otherwise, then the totem. A block
//! is a packed `signed_block`, and the block log only ever receives irreversible blocks, so
//! the file is strictly sequential: no gaps, no fork leftovers.
//!
//! Nothing here repairs anything. Where nodeos would rebuild an index or vacuum a pruned log
//! (both writes), the reader refuses the file and names the problem.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::binio::{FcReader, FileCache, PosFile, num_from_id};

/// The separator after a v2 or v3 preamble (`block_log::npos`).
pub const TOTEM: u64 = u64::MAX;

/// Bytes of a v3 preamble that carries a chain id: version, first block, chain id, totem.
pub const CHAIN_ID_PREAMBLE_BYTES: u64 = 48;

/// Set in the version word of a pruned (`block-log-retain-blocks`) log.
const PRUNED_FLAG: u32 = 1 << 31;

/// `previous` sits after `timestamp` (u32), `producer` (u64) and `confirmed` (u16).
const PREVIOUS_AT: u64 = 14;

/// The smallest packed `block_header`: the fixed fields (timestamp to `schedule_version`,
/// 114 bytes), an absent `new_producers` and an empty `header_extensions`.
const MIN_HEADER_BYTES: u64 = 116;

/// Packed `chain_config_v0`: `max_block_net_usage` (u64), 14 × u32, 2 × u16.
const CHAIN_CONFIG_V0_BYTES: usize = 68;

/// Bytes read to size a genesis state. A K1 or R1 genesis is 110 bytes; only a WebAuthn
/// initial key's relying-party id can make it longer, and then exactly what is needed is read.
const GENESIS_PROBE: u64 = 512;

/// Bytes read to decode a block header without reading the whole block. A header with
/// `new_producers` or large extensions may exceed it; the whole block is read then.
const HEADER_PROBE: u64 = 8192;

/// One block-log file pair, validated at open. Holds paths and metadata only; the files are
/// opened through the shared [`FileCache`] on each read.
pub struct BlockLog {
    log: PathBuf,
    index: PathBuf,
    cache: Arc<FileCache>,
    version: u32,
    first: u32,
    /// Inclusive.
    last: u32,
    chain_id: [u8; 32],
    /// Offset of the first block: the preamble length.
    first_pos: u64,
    size: u64,
}

impl BlockLog {
    /// Open and validate `log` with its `index`. `Ok(None)` for a log holding only its
    /// preamble (the 48-byte v3 file nodeos leaves right after a split); its index is not
    /// read.
    pub fn open(log: &Path, index: &Path, cache: &Arc<FileCache>) -> Result<Option<Self>> {
        let f = cache.get(log)?;
        let size = f.len();
        let name = log.display();
        ensure!(
            size >= 4,
            "{name} is too short for a block log ({size} bytes)"
        );
        let version = f.u32_at(0)?;
        ensure!(
            version & PRUNED_FLAG == 0,
            "pruned block log {name} (block-log-retain-blocks) is not supported: use a vacuumed \
             copy (`spring-util block-log vacuum`)"
        );
        ensure!(
            version != 0,
            "block log {name} was not set up properly: its version is 0, so nodeos never \
             completed its preamble"
        );
        ensure!(
            (1..=3).contains(&version),
            "unsupported block log version {version} in {name}: versions 1 to 3 are supported"
        );

        let (first, mut at) = if version == 1 {
            (1, 4)
        } else {
            (f.u32_at(4).context("block log preamble")?, 8)
        };
        ensure!(first > 0, "block log {name} claims to start at block 0");
        // Spring `block_log::contains_genesis_state`: v1, v2, and v3 logs that start at block
        // 1 carry the genesis state; v3 logs that start later carry the chain id.
        let chain_id = if version < 3 || first == 1 {
            let genesis =
                read_genesis(&f, at).with_context(|| format!("genesis state of {name}"))?;
            at += genesis.len() as u64;
            chain_id_from_genesis(&genesis)
        } else {
            let mut id = [0; 32];
            f.read_exact_at(at, &mut id)
                .with_context(|| format!("chain id of {name}"))?;
            at += 32;
            id
        };
        if version != 1 {
            let totem = f.u64_at(at).context("block log preamble")?;
            ensure!(
                totem == TOTEM,
                "{name}: the separator after the preamble is {totem:#018x}, not \
                 0xffffffffffffffff"
            );
            at += 8;
        }
        let first_pos = at;
        if size == first_pos {
            return Ok(None);
        }

        ensure!(
            size >= first_pos + MIN_HEADER_BYTES + 8,
            "{name}: corrupted tail: {} bytes after the preamble cannot hold a block",
            size - first_pos
        );
        let last_pos = f.u64_at(size - 8)?;
        ensure!(
            last_pos >= first_pos
                && last_pos
                    .checked_add(MIN_HEADER_BYTES + 8)
                    .is_some_and(|end| end <= size),
            "{name}: corrupted tail: the last block's position {last_pos} is outside the log \
             ({first_pos}..{size})"
        );
        let mut prev = [0; 32];
        f.read_exact_at(last_pos + PREVIOUS_AT, &mut prev)?;
        let last = num_from_id(&prev).checked_add(1).filter(|&n| n >= first);
        let Some(last) = last else {
            bail!(
                "{name}: corrupted tail: the last block follows block {}, but the log starts \
                 at block {first}",
                num_from_id(&prev)
            );
        };

        let count = u64::from(last - first) + 1;
        let idx = cache.get(index)?;
        let iname = index.display();
        ensure!(
            idx.len() == count * 8,
            "index mismatch for {name}: {iname} has {} bytes, but blocks {first}..={last} need \
             {}; rebuild it from the log (`spring-util block-log make-index`)",
            idx.len(),
            count * 8
        );
        let (index_first, index_last) = (idx.u64_at(0)?, idx.u64_at(idx.len() - 8)?);
        ensure!(
            index_first == first_pos && index_last == last_pos,
            "index mismatch for {name}: {iname} places blocks {first} and {last} at {index_first} \
             and {index_last}, the log at {first_pos} and {last_pos}; rebuild it from the log \
             (`spring-util block-log make-index`)"
        );

        Ok(Some(Self {
            log: log.to_path_buf(),
            index: index.to_path_buf(),
            cache: Arc::clone(cache),
            version,
            first,
            last,
            chain_id,
            first_pos,
            size,
        }))
    }

    /// `[first, last + 1)`.
    pub fn range(&self) -> (u32, u32) {
        (self.first, self.last + 1)
    }

    /// The preamble version, 1 to 3.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// From the preamble, or the sha256 of the genesis state it holds.
    pub fn chain_id(&self) -> [u8; 32] {
        self.chain_id
    }

    pub fn log_path(&self) -> &Path {
        &self.log
    }

    pub fn index_path(&self) -> &Path {
        &self.index
    }

    /// Offset and length (without the position trailer) of block `n`. The trailer must name
    /// the block's own position.
    pub fn bounds(&self, n: u32) -> Result<(u64, u64)> {
        let (_, pos, len) = self.entry(n)?;
        Ok((pos, len))
    }

    /// The stored `signed_block` bytes of block `n`, as SHiP's `block` field carries them.
    pub fn body(&self, n: u32, out: &mut Vec<u8>) -> Result<()> {
        let (f, pos, len) = self.entry(n)?;
        out.clear();
        out.resize(usize::try_from(len)?, 0);
        f.read_exact_at(pos, out)?;
        let previous = out[PREVIOUS_AT as usize..][..32].try_into()?;
        self.check_link(n, &previous)
    }

    /// Block `n`'s `previous` field, which is the id of block `n - 1`.
    pub fn previous(&self, n: u32) -> Result<[u8; 32]> {
        let (f, pos, _) = self.entry(n)?;
        let mut previous = [0; 32];
        f.read_exact_at(pos + PREVIOUS_AT, &mut previous)?;
        self.check_link(n, &previous)?;
        Ok(previous)
    }

    /// Block `n`'s decoded header. Reads only the start of the block unless the header is
    /// longer than that.
    pub fn header(&self, n: u32) -> Result<BlockHeader> {
        let (f, pos, len) = self.entry(n)?;
        let context = || format!("block {n} in {}", self.log.display());
        let probe = f.read_vec(pos, usize::try_from(len.min(HEADER_PROBE))?)?;
        let header = match block_header(&probe) {
            Ok(h) => h,
            Err(_) if len > HEADER_PROBE => {
                block_header(&f.read_vec(pos, usize::try_from(len)?)?).with_context(context)?
            }
            Err(e) => return Err(e.context(context())),
        };
        self.check_link(n, &header.previous)?;
        Ok(header)
    }

    /// Block `n`'s id, recomputed from its header.
    pub fn id(&self, n: u32) -> Result<[u8; 32]> {
        Ok(self.header(n)?.id)
    }

    /// The file, position and length of block `n`, after the index and trailer checks.
    fn entry(&self, n: u32) -> Result<(PosFile, u64, u64)> {
        ensure!(
            (self.first..=self.last).contains(&n),
            "block {n} is not in {} (blocks {}..={})",
            self.log.display(),
            self.first,
            self.last
        );
        let count = u64::from(self.last - self.first) + 1;
        let idx = self.file(&self.index, count * 8)?;
        let slot = u64::from(n - self.first) * 8;
        let (pos, end) = if n < self.last {
            let mut b = [0; 16];
            idx.read_exact_at(slot, &mut b)?;
            let (pos, next) = b.split_at(8);
            (
                u64::from_le_bytes(pos.try_into()?),
                u64::from_le_bytes(next.try_into()?),
            )
        } else {
            (idx.u64_at(slot)?, self.size)
        };
        ensure!(
            pos >= self.first_pos
                && pos
                    .checked_add(MIN_HEADER_BYTES + 8)
                    .is_some_and(|min_end| min_end <= end)
                && end <= self.size,
            "{} places block {n} at {pos}..{end}, which is not a block of {} ({}..{})",
            self.index.display(),
            self.log.display(),
            self.first_pos,
            self.size
        );
        let f = self.file(&self.log, self.size)?;
        let trailer = f.u64_at(end - 8)?;
        ensure!(
            trailer == pos,
            "position trailer mismatch at block {n} in {}: the block at {pos} ends with {trailer}",
            self.log.display()
        );
        Ok((f, pos, end - 8 - pos))
    }

    /// Block `n` must follow block `n - 1`: the number in its `previous` id is `n - 1`.
    fn check_link(&self, n: u32, previous: &[u8; 32]) -> Result<()> {
        let prev = num_from_id(previous);
        ensure!(
            prev == n - 1,
            "block {n} in {} follows block {prev} instead of {}",
            self.log.display(),
            n - 1
        );
        Ok(())
    }

    /// The file from the cache, which must still have the length validated at open. The
    /// inputs are frozen; a file that changed is refused rather than half-read.
    fn file(&self, path: &Path, len: u64) -> Result<PosFile> {
        let f = self.cache.get(path)?;
        ensure!(
            f.len() == len,
            "{} changed since it was opened: {len} bytes then, {} now",
            path.display(),
            f.len()
        );
        Ok(f)
    }
}

impl fmt::Debug for BlockLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockLog")
            .field("log", &self.log)
            .field("index", &self.index)
            .field("version", &self.version)
            .field("range", &self.range())
            .field("chain_id", &hex::encode(self.chain_id))
            .finish_non_exhaustive()
    }
}

/// What a block's header says about its place in the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u32,
    pub id: [u8; 32],
    pub previous: [u8; 32],
}

/// Decode the `block_header` at the start of a packed `signed_block` and compute the block
/// id as Spring's `block_header::calculate_id` does: the sha256 of the packed header (which
/// excludes `producer_signature`) with its first four bytes replaced by the block number,
/// big-endian. The number is `previous`'s plus one.
///
/// Producer keys in a legacy `new_producers` schedule may be K1, R1 or WebAuthn. Decoding
/// follows fc: non-canonical varuints are accepted, the optional flag must be 0 or 1, and
/// bytes after the header (the signature, transactions and extensions) are not read. The id
/// hashes the stored header bytes, which are what Spring re-packs for any canonically encoded
/// block (all that nodeos writes).
pub fn block_header(body: &[u8]) -> Result<BlockHeader> {
    let mut r = FcReader::new(body);
    r.bytes(PREVIOUS_AT as usize)?; // timestamp, producer, confirmed
    let previous = r.id()?;
    r.bytes(68)?; // transaction_mroot, action_mroot, schedule_version
    if r.bool().context("block header new_producers flag")? {
        r.u32()?; // schedule version
        for _ in 0..r.varuint32()? {
            r.u64()?; // producer_name
            let rest = public_key_rest(&mut r).context("block header producer key")?;
            r.bytes(rest)?;
        }
    }
    // header_extensions: (u16 type, bytes data) pairs.
    for _ in 0..r.varuint32()? {
        r.bytes(2)?;
        let len = r.varuint32()?;
        r.bytes(usize::try_from(len)?)?;
    }
    let number = num_from_id(&previous)
        .checked_add(1)
        .context("block number overflow")?;
    let mut id: [u8; 32] = Sha256::digest(&body[..r.pos]).into();
    id[..4].copy_from_slice(&number.to_be_bytes());
    Ok(BlockHeader {
        number,
        id,
        previous,
    })
}

/// A chain id from a packed `genesis_state`: its sha256 (Spring
/// `genesis_state::compute_chain_id`).
pub fn chain_id_from_genesis(genesis_packed: &[u8]) -> [u8; 32] {
    Sha256::digest(genesis_packed).into()
}

/// Reads a packed `public_key` up to its trailing variable part and returns that part's
/// length: the 33-byte point for K1 (variant 0) and R1 (1); for WebAuthn (2) the point and
/// the user-presence byte are consumed and the relying-party id string's length is returned.
fn public_key_rest(r: &mut FcReader) -> Result<usize> {
    match r.varuint32()? {
        0 | 1 => Ok(33),
        2 => {
            r.bytes(33)?;
            r.u8()?;
            Ok(usize::try_from(r.varuint32()?)?)
        }
        k => bail!("unknown public key type {k}"),
    }
}

/// Length of the packed `genesis_state` at the start of `b`: `initial_timestamp` (u64),
/// `initial_key`, `initial_configuration` (`chain_config_v0`). Only the fixed prefix up to
/// the key's variable part has to be in `b`.
fn genesis_len(b: &[u8]) -> Result<usize> {
    let mut r = FcReader::new(b);
    r.u64()?; // initial_timestamp
    let rest = public_key_rest(&mut r).context("initial_key")?;
    r.pos
        .checked_add(rest)
        .and_then(|n| n.checked_add(CHAIN_CONFIG_V0_BYTES))
        .context("genesis state length overflow")
}

/// The packed genesis state starting at offset `at` of `f`.
fn read_genesis(f: &PosFile, at: u64) -> Result<Vec<u8>> {
    let avail = f.len().saturating_sub(at);
    let mut b = f.read_vec(at, usize::try_from(avail.min(GENESIS_PROBE))?)?;
    let len = genesis_len(&b)?;
    ensure!(
        len as u64 <= avail,
        "truncated genesis state: {len} bytes at offset {at}, {avail} left in the file"
    );
    if len <= b.len() {
        b.truncate(len);
    } else {
        b = f.read_vec(at, len)?;
    }
    Ok(b)
}

/// The fixtures come from the testkit writers, so these run with `--features testkit` (as CI
/// does).
#[cfg(all(test, feature = "testkit"))]
mod tests {
    use super::*;
    use crate::binio::TestDir;
    use crate::testkit::blocks::{
        self, Block, BlockSpec, Genesis, Key, LogFiles, Preamble, chain, synthetic_id,
    };

    const EOS_CHAIN_ID: &str = "aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906";

    fn open(files: &LogFiles) -> Result<Option<BlockLog>> {
        BlockLog::open(&files.log, &files.index, &FileCache::new(8))
    }

    fn open_some(files: &LogFiles) -> BlockLog {
        open(files).unwrap().expect("a log with blocks")
    }

    fn err<T: fmt::Debug>(r: Result<T>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    /// `body` into a buffer that starts out dirty, to show it is replaced.
    fn body(log: &BlockLog, n: u32) -> Result<Vec<u8>> {
        let mut out = vec![0xaa; 3];
        log.body(n, &mut out).map(|()| out)
    }

    /// Every block reads back exactly as written, through every accessor.
    fn assert_reads_back(log: &BlockLog, blocks: &[Block]) {
        for b in blocks {
            assert_eq!(body(log, b.num).unwrap(), b.body, "body of {}", b.num);
            assert_eq!(
                log.previous(b.num).unwrap(),
                b.previous,
                "previous of {}",
                b.num
            );
            let h = log.header(b.num).unwrap();
            assert_eq!((h.number, h.id, h.previous), (b.num, b.id, b.previous));
            assert_eq!(log.id(b.num).unwrap(), b.id);
            assert_eq!(log.bounds(b.num).unwrap().1, b.body.len() as u64);
        }
    }

    fn write(dir: &TestDir, stem: &str, preamble: &Preamble, blocks: &[Block]) -> LogFiles {
        blocks::write(dir.path(), stem, preamble, blocks).unwrap()
    }

    fn patch(bytes: &mut [u8], at: usize, with: &[u8]) {
        bytes[at..at + with.len()].copy_from_slice(with);
    }

    #[test]
    fn real_eos_block_header_reproduces_its_nodeos_id() {
        let hex = include_str!("../testdata/blocks/eos-mainnet-480385480.hex");
        let bytes = hex::decode(hex.trim()).unwrap();
        let h = block_header(&bytes).unwrap();
        assert_eq!(h.number, 480_385_480);
        assert_eq!(num_from_id(&h.previous), 480_385_479);
        assert_eq!(
            hex::encode(h.id),
            "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4"
        );
        for n in [0, 14, 46, 114, 115] {
            assert!(block_header(&bytes[..n]).is_err(), "prefix of {n} bytes");
        }
    }

    #[test]
    fn eos_mainnet_genesis_hashes_to_its_chain_id() {
        let packed = Genesis::eos_mainnet().pack();
        assert_eq!(packed.len(), 110);
        assert_eq!(hex::encode(chain_id_from_genesis(&packed)), EOS_CHAIN_ID);
        // Sizing needs only the prefix up to the key's variable part.
        assert_eq!(genesis_len(&packed).unwrap(), 110);
        assert_eq!(genesis_len(&packed[..9]).unwrap(), 110);
        assert!(genesis_len(&packed[..8]).is_err());
    }

    #[test]
    fn genesis_preambles_carry_the_chain_id_of_their_genesis_state() {
        let dir = TestDir::new("blocklog-genesis");
        let genesis = Genesis::eos_mainnet();
        let preambles = [
            Preamble::V1 {
                genesis: genesis.clone(),
            },
            Preamble::V2 {
                first: 1,
                genesis: genesis.clone(),
            },
            // A v2 log that starts later still carries the genesis state.
            Preamble::V2 {
                first: 500,
                genesis: genesis.clone(),
            },
            Preamble::V3Genesis { genesis },
        ];
        for preamble in preambles {
            let (version, first) = (preamble.version(), preamble.first());
            let blocks = chain(first, 12).unwrap();
            let files = write(&dir, &format!("v{version}-{first}"), &preamble, &blocks);
            let log = open_some(&files);
            assert_eq!(log.version(), version);
            assert_eq!(log.range(), (first, first + 12));
            assert_eq!(hex::encode(log.chain_id()), EOS_CHAIN_ID);
            // v1 has no first-block field and no totem.
            let preamble_len = if version == 1 { 4 + 110 } else { 8 + 110 + 8 };
            assert_eq!(log.bounds(first).unwrap().0, preamble_len);
            assert_eq!(log.log_path(), files.log);
            assert_eq!(log.index_path(), files.index);
            assert_reads_back(&log, &blocks);
        }
    }

    #[test]
    fn v3_chain_id_preamble_is_48_bytes() {
        let dir = TestDir::new("blocklog-v3");
        let blocks = chain(1000, 10).unwrap();
        let preamble = Preamble::V3ChainId {
            first: 1000,
            chain_id: [0x34; 32],
        };
        let log = open_some(&write(&dir, "blocks", &preamble, &blocks));
        assert_eq!(
            (log.version(), log.range(), log.chain_id()),
            (3, (1000, 1010), [0x34; 32])
        );
        assert_eq!(log.bounds(1000).unwrap().0, CHAIN_ID_PREAMBLE_BYTES);
        assert_reads_back(&log, &blocks);
        let dbg = format!("{log:?}");
        assert!(
            dbg.contains("(1000, 1010)") && dbg.contains(&"34".repeat(32)),
            "{dbg}"
        );
    }

    #[test]
    fn preamble_only_logs_are_none() {
        let dir = TestDir::new("blocklog-empty");
        let split = Preamble::V3ChainId {
            first: 2001,
            chain_id: [7; 32],
        };
        let split = write(&dir, "blocks", &split, &[]);
        assert_eq!(std::fs::metadata(&split.log).unwrap().len(), 48);
        assert_eq!(std::fs::metadata(&split.index).unwrap().len(), 0);
        assert!(open(&split).unwrap().is_none());
        // The index of a block-less log is never read.
        std::fs::remove_file(&split.index).unwrap();
        assert!(open(&split).unwrap().is_none());

        let genesis_only = Preamble::V3Genesis {
            genesis: Genesis::eos_mainnet(),
        };
        assert!(
            open(&write(&dir, "genesis", &genesis_only, &[]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pruned_unset_and_unknown_versions_are_refused() {
        let dir = TestDir::new("blocklog-versions");
        let preamble = Preamble::V3ChainId {
            first: 1000,
            chain_id: [1; 32],
        };
        let (log, index) = blocks::encode(&preamble, &chain(1000, 3).unwrap()).unwrap();
        let index_path = dir.write("blocks.index", &index);
        let attempt = |log: &[u8]| {
            let path = dir.write("blocks.log", log);
            err(BlockLog::open(&path, &index_path, &FileCache::new(2)))
        };

        let mut pruned = log.clone();
        patch(&mut pruned, 0, &(3 | PRUNED_FLAG).to_le_bytes());
        assert!(attempt(&pruned).contains("pruned block log"));
        for (version, needle) in [
            (0u32, "not set up properly"),
            (4, "unsupported block log version 4"),
        ] {
            let mut bytes = log.clone();
            patch(&mut bytes, 0, &version.to_le_bytes());
            let e = attempt(&bytes);
            assert!(e.contains(needle), "{e}");
        }
        let mut zero_first = log.clone();
        patch(&mut zero_first, 4, &0u32.to_le_bytes());
        assert!(attempt(&zero_first).contains("block 0"));
        assert!(attempt(&log[..2]).contains("too short"));
        // A preamble cut short is an error, not an empty log.
        assert!(attempt(&log[..30]).contains("chain id"));
        assert!(attempt(&log[..44]).contains("preamble"));
    }

    #[test]
    fn a_bad_totem_is_refused() {
        let dir = TestDir::new("blocklog-totem");
        let cases = [
            (
                Preamble::V3ChainId {
                    first: 1000,
                    chain_id: [1; 32],
                },
                40,
            ),
            (
                Preamble::V2 {
                    first: 1,
                    genesis: Genesis::eos_mainnet(),
                },
                8 + 110,
            ),
        ];
        for (preamble, totem_at) in cases {
            let blocks = chain(preamble.first(), 2).unwrap();
            let (mut log, index) = blocks::encode(&preamble, &blocks).unwrap();
            patch(&mut log, totem_at, &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
            let e = err(BlockLog::open(
                &dir.write("blocks.log", &log),
                &dir.write("blocks.index", &index),
                &FileCache::new(2),
            ));
            assert!(
                e.contains("separator") && e.contains("0x00000000ffffffff"),
                "{e}"
            );
        }
    }

    /// A v3 log of blocks 1000..1010 as bytes, and each block's position.
    fn fixture() -> (Vec<Block>, Vec<u8>, Vec<u8>, Vec<usize>) {
        let blocks = chain(1000, 10).unwrap();
        let preamble = Preamble::V3ChainId {
            first: 1000,
            chain_id: [9; 32],
        };
        let (log, index) = blocks::encode(&preamble, &blocks).unwrap();
        let pos = index
            .chunks(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()) as usize)
            .collect();
        (blocks, log, index, pos)
    }

    fn write_fixture(dir: &TestDir, log: &[u8], index: &[u8]) -> LogFiles {
        LogFiles {
            log: dir.write("blocks.log", log),
            index: dir.write("blocks.index", index),
        }
    }

    #[test]
    fn a_trailer_mismatch_is_detected_for_its_block_only() {
        let dir = TestDir::new("blocklog-trailer");
        let (blocks, mut log, index, pos) = fixture();
        // Block 1004's trailer is the 8 bytes before block 1005.
        patch(&mut log, pos[5] - 8, &12345u64.to_le_bytes());
        let log = open_some(&write_fixture(&dir, &log, &index));
        for e in [
            err(log.bounds(1004)),
            err(body(&log, 1004)),
            err(log.previous(1004)),
            err(log.header(1004)),
        ] {
            assert!(e.contains("trailer mismatch at block 1004"), "{e}");
        }
        assert_reads_back(&log, &blocks[..4]);
        assert_reads_back(&log, &blocks[5..]);
    }

    #[test]
    fn a_block_out_of_place_is_detected() {
        let dir = TestDir::new("blocklog-link");
        let (blocks, mut log, index, pos) = fixture();
        patch(&mut log, pos[4] + 14, &1002u32.to_be_bytes());
        let log = open_some(&write_fixture(&dir, &log, &index));
        for e in [
            err(body(&log, 1004)),
            err(log.previous(1004)),
            err(log.header(1004)),
        ] {
            assert!(
                e.contains("block 1004") && e.contains("follows block 1002"),
                "{e}"
            );
        }
        assert_reads_back(&log, &blocks[..4]);
        assert_reads_back(&log, &blocks[5..]);
        for n in [999, 1010] {
            assert!(err(log.previous(n)).contains("is not in"), "block {n}");
        }
    }

    #[test]
    fn index_mismatches_are_refused() {
        let dir = TestDir::new("blocklog-index");
        let (_, log, index, pos) = fixture();
        let short = &index[..index.len() - 8];
        let mut first = index.clone();
        patch(&mut first, 0, &49u64.to_le_bytes());
        let mut last = index.clone();
        patch(&mut last, index.len() - 8, &(pos[8] as u64).to_le_bytes());
        for bad in [short, &first[..], &last[..], &[][..]] {
            let e = err(open(&write_fixture(&dir, &log, bad)));
            assert!(
                e.contains("index mismatch") && e.contains("make-index"),
                "{e}"
            );
        }
        let missing = LogFiles {
            log: dir.write("blocks.log", &log),
            index: dir.path().join("missing.index"),
        };
        assert!(err(open(&missing)).contains("missing.index"));

        // Two slots naming one position: neither block reads.
        let mut doubled = index.clone();
        patch(&mut doubled, 3 * 8, &(pos[4] as u64).to_le_bytes());
        let log = open_some(&write_fixture(&dir, &log, &doubled));
        assert!(err(log.bounds(1002)).contains("trailer mismatch at block 1002"));
        assert!(err(log.bounds(1003)).contains("not a block"));
        assert!(log.bounds(1004).is_ok());
    }

    #[test]
    fn corrupted_tails_are_refused() {
        let dir = TestDir::new("blocklog-tail");
        let (_, log, index, pos) = fixture();
        // Without its trailer, the log ends in block 1009's extension bytes.
        let e = err(open(&write_fixture(&dir, &log[..log.len() - 8], &index)));
        assert!(e.contains("corrupted tail"), "{e}");
        // Nothing after the preamble but a stub.
        let e = err(open(&write_fixture(&dir, &log[..48 + 100], &index)));
        assert!(
            e.contains("corrupted tail") && e.contains("cannot hold a block"),
            "{e}"
        );

        // The last block claims a number before the log's first...
        let mut low = log.clone();
        patch(&mut low, pos[9] + 14, &5u32.to_be_bytes());
        let e = err(open(&write_fixture(&dir, &low, &index)));
        assert!(
            e.contains("corrupted tail") && e.contains("follows block 5"),
            "{e}"
        );
        // ... or a number past the end of the index.
        let mut high = log.clone();
        patch(&mut high, pos[9] + 14, &2000u32.to_be_bytes());
        let e = err(open(&write_fixture(&dir, &high, &index)));
        assert!(
            e.contains("index mismatch") && e.contains("1000..=2001"),
            "{e}"
        );
    }

    /// `signed_block` as Spring's SHiP ABI declares it, cut to what these fixtures hold (no
    /// transactions).
    const SIGNED_BLOCK_ABI: &str = r#"{
        "version": "eosio::abi/1.1",
        "types": [], "actions": [], "tables": [], "ricardian_clauses": [], "variants": [],
        "structs": [
            {"name": "extension", "base": "", "fields": [
                {"name": "type", "type": "uint16"}, {"name": "data", "type": "bytes"}]},
            {"name": "producer_key", "base": "", "fields": [
                {"name": "producer_name", "type": "name"},
                {"name": "block_signing_key", "type": "public_key"}]},
            {"name": "producer_schedule", "base": "", "fields": [
                {"name": "version", "type": "uint32"},
                {"name": "producers", "type": "producer_key[]"}]},
            {"name": "signed_block", "base": "", "fields": [
                {"name": "timestamp", "type": "block_timestamp_type"},
                {"name": "producer", "type": "name"},
                {"name": "confirmed", "type": "uint16"},
                {"name": "previous", "type": "checksum256"},
                {"name": "transaction_mroot", "type": "checksum256"},
                {"name": "action_mroot", "type": "checksum256"},
                {"name": "schedule_version", "type": "uint32"},
                {"name": "new_producers", "type": "producer_schedule?"},
                {"name": "header_extensions", "type": "extension[]"},
                {"name": "producer_signature", "type": "signature"},
                {"name": "transactions", "type": "uint8[]"},
                {"name": "block_extensions", "type": "extension[]"}]}
        ]
    }"#;

    #[test]
    fn webauthn_producer_keys_are_decoded() {
        let previous = synthetic_id(41);
        let webauthn = Key::WebAuthn {
            point: [0x02; 33],
            user_presence: 1,
            rpid: "example.com".into(),
        };
        let spec = BlockSpec {
            timestamp: 42,
            producer: blocks::EOSIO,
            new_producers: Some((
                7,
                vec![
                    (0x1111, Key::K1([0x02; 33])),
                    (0x2222, Key::R1([0x03; 33])),
                    (0x3333, webauthn),
                ],
            )),
            header_extensions: vec![(1, vec![0xab; 40])],
            block_extensions: vec![(3, vec![0xcd; 20])],
            ..BlockSpec::default()
        };
        let body = spec.signed_block(&previous);
        let h = block_header(&body).unwrap();
        let mut expected: [u8; 32] = Sha256::digest(spec.header(&previous)).into();
        expected[..4].copy_from_slice(&42u32.to_be_bytes());
        assert_eq!((h.number, h.id, h.previous), (42, expected, previous));

        // An independent decoder agrees on the layout, through the signature to the end.
        let abieos = rs_abieos::Abieos::new();
        abieos.set_abi_json("blocktest", SIGNED_BLOCK_ABI).unwrap();
        let json = abieos
            .bin_to_json("blocktest", "signed_block", &body)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let keys = &v["new_producers"]["producers"];
        let key = |i: usize| keys[i]["block_signing_key"].as_str().unwrap().to_owned();
        assert!(key(1).starts_with("PUB_R1_"), "{json}");
        assert!(key(2).starts_with("PUB_WA_"), "{json}");
        assert_eq!(v["header_extensions"][0]["data"], "AB".repeat(40));
        assert_eq!(v["block_extensions"][0]["data"], "CD".repeat(20));

        // Producer 1's key type follows the fixed header (114 bytes), the flag, the schedule
        // version, the count and the name.
        let mut unknown = body.clone();
        assert_eq!(unknown[128], 0);
        unknown[128] = 3;
        let e = err(block_header(&unknown));
        assert!(e.contains("unknown public key type 3"), "{e}");
        let mut flag = body.clone();
        flag[114] = 2;
        assert!(err(block_header(&flag)).contains("new_producers"));
    }

    #[test]
    fn a_webauthn_genesis_key_with_a_long_rpid_is_read_whole() {
        let dir = TestDir::new("blocklog-webauthn-genesis");
        let genesis = Genesis {
            initial_key: Key::WebAuthn {
                point: [0x03; 33],
                user_presence: 2,
                rpid: "r".repeat(1000),
            },
            ..Genesis::eos_mainnet()
        };
        let packed = genesis.pack();
        assert!(packed.len() as u64 > GENESIS_PROBE);
        let blocks = chain(1, 3).unwrap();
        let preamble = Preamble::V3Genesis { genesis };
        let log = open_some(&write(&dir, "blocks", &preamble, &blocks));
        assert_eq!(log.chain_id(), chain_id_from_genesis(&packed));
        assert_eq!(log.bounds(1).unwrap().0, 8 + packed.len() as u64 + 8);
        assert_reads_back(&log, &blocks);
    }

    #[test]
    fn headers_longer_than_the_probe_are_read_whole() {
        let dir = TestDir::new("blocklog-long-header");
        // Block 101's header outgrows the probe; block 102 is large only after its header.
        let blocks = blocks::extend(&synthetic_id(99), 3, |n| BlockSpec {
            timestamp: n,
            header_extensions: if n == 101 {
                vec![(2, vec![1; 10_000])]
            } else {
                vec![]
            },
            block_extensions: if n == 102 {
                vec![(0, vec![2; 20_000])]
            } else {
                vec![]
            },
            ..BlockSpec::default()
        })
        .unwrap();
        let preamble = Preamble::V3ChainId {
            first: 100,
            chain_id: [5; 32],
        };
        let log = open_some(&write(&dir, "blocks", &preamble, &blocks));
        assert_reads_back(&log, &blocks);
    }

    #[test]
    fn files_changed_after_open_are_refused() {
        let dir = TestDir::new("blocklog-changed");
        let (blocks, log_bytes, index, _) = fixture();
        let files = write_fixture(&dir, &log_bytes, &index);
        // One open file at a time: every read reopens the log after the index.
        let cache = FileCache::new(1);
        let log = BlockLog::open(&files.log, &files.index, &cache)
            .unwrap()
            .unwrap();
        assert_reads_back(&log, &blocks);
        assert_eq!(cache.open_files(), 1);

        let mut grown = log_bytes.clone();
        grown.push(0);
        std::fs::write(&files.log, &grown).unwrap();
        let e = err(body(&log, 1003));
        assert!(e.contains("changed since it was opened"), "{e}");
    }

    #[test]
    fn split_files_link_across_their_seams() {
        let dir = TestDir::new("blocklog-split");
        let blocks = chain(5, 20).unwrap();
        let preamble = Preamble::V2 {
            first: 5,
            genesis: Genesis::eos_mainnet(),
        };
        let files = blocks::write_split(dir.path(), &preamble, &blocks, 10).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|f| f.log.file_name().unwrap().to_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            ["blocks-5-10.log", "blocks-11-20.log", "blocks-21-24.log"]
        );
        let logs: Vec<_> = files.iter().map(open_some).collect();
        let shape: Vec<_> = logs.iter().map(|l| (l.version(), l.range())).collect();
        assert_eq!(shape, [(2, (5, 11)), (3, (11, 21)), (3, (21, 25))]);
        assert!(
            logs.iter()
                .all(|l| hex::encode(l.chain_id()) == EOS_CHAIN_ID)
        );
        assert_eq!(logs[1].bounds(11).unwrap().0, CHAIN_ID_PREAMBLE_BYTES);
        // The id at a file's end is the next file's first `previous`.
        assert_eq!(logs[0].id(10).unwrap(), logs[1].previous(11).unwrap());
        assert_eq!(logs[1].id(20).unwrap(), logs[2].previous(21).unwrap());
        assert_reads_back(&logs[0], &blocks[..6]);
        assert_reads_back(&logs[1], &blocks[6..16]);
        assert_reads_back(&logs[2], &blocks[16..]);
    }
}
