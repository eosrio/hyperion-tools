//! Block-log writers for synthetic fixtures.
//!
//! Bodies are real packed `signed_block`s (a header, a K1 producer signature, no transactions,
//! optional block extensions), so [`block_header`] derives their ids and each block links to
//! its predecessor through `previous`. Files are laid out as Spring writes them: the preamble,
//! then each block followed by its own position, and an index of those positions. Tests that
//! need a damaged log start from [`encode`] and change the bytes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::binio::num_from_id;
use crate::blocklog::{TOTEM, block_header, chain_id_from_genesis};

/// The account name `eosio`, the producer of the blocks [`BlockSpec::synthetic`] describes.
pub const EOSIO: u64 = 0x5530_ea00_0000_0000;

/// A `public_key` variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    K1([u8; 33]),
    R1([u8; 33]),
    /// A compressed point, the user-presence byte and the relying-party id.
    WebAuthn {
        point: [u8; 33],
        user_presence: u8,
        rpid: String,
    },
}

impl Key {
    /// Append the packed key: the variant index, then the key.
    pub fn pack_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::K1(point) => {
                out.push(0);
                out.extend_from_slice(point);
            }
            Self::R1(point) => {
                out.push(1);
                out.extend_from_slice(point);
            }
            Self::WebAuthn {
                point,
                user_presence,
                rpid,
            } => {
                out.push(2);
                out.extend_from_slice(point);
                out.push(*user_presence);
                bytes_into(out, rpid.as_bytes());
            }
        }
    }
}

/// `chain_config_v0`, the configuration a genesis state carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainConfigV0 {
    pub max_block_net_usage: u64,
    /// `target_block_net_usage_pct` through `max_inline_action_size`, in declaration order.
    pub limits: [u32; 14],
    /// `max_inline_action_depth` and `max_authority_depth`.
    pub depths: [u16; 2],
}

/// A `genesis_state`. The default is EOS mainnet's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Genesis {
    /// Microseconds since the unix epoch.
    pub initial_timestamp: u64,
    pub initial_key: Key,
    pub initial_configuration: ChainConfigV0,
}

impl Genesis {
    /// The EOS mainnet genesis state (Spring `genesis/vaulta_genesis.json`), whose chain id is
    /// `aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906`.
    pub fn eos_mainnet() -> Self {
        Self {
            // 2018-06-08T08:08:08.888
            initial_timestamp: 1_528_445_288_888_000,
            // EOS7EarnUhcyYqmdnPon8rm7mBCTnBoot6o7fE2WzjvEX2TdggbL3
            initial_key: Key::K1([
                0x03, 0x35, 0x05, 0x29, 0xef, 0xca, 0x8c, 0x60, 0x74, 0x21, 0xe9, 0x58, 0x46, 0xcc,
                0x2a, 0x3d, 0x2e, 0xfa, 0xa8, 0x45, 0x40, 0x18, 0xde, 0xb7, 0x5a, 0x20, 0x4e, 0x27,
                0xac, 0xf2, 0x9e, 0xe5, 0xcc,
            ]),
            initial_configuration: ChainConfigV0 {
                max_block_net_usage: 1_048_576,
                limits: [
                    1000, 524_288, 12, 500, 20, 100, 200_000, 1000, 150_000, 100, 3600, 600,
                    3_888_000, 4096,
                ],
                depths: [4, 6],
            },
        }
    }

    /// Packed in Spring's field order: `initial_timestamp`, `initial_key`,
    /// `initial_configuration`.
    pub fn pack(&self) -> Vec<u8> {
        let mut out = self.initial_timestamp.to_le_bytes().to_vec();
        self.initial_key.pack_into(&mut out);
        let c = &self.initial_configuration;
        out.extend_from_slice(&c.max_block_net_usage.to_le_bytes());
        for v in c.limits {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in c.depths {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    pub fn chain_id(&self) -> [u8; 32] {
        chain_id_from_genesis(&self.pack())
    }
}

impl Default for Genesis {
    fn default() -> Self {
        Self::eos_mainnet()
    }
}

/// A block-log preamble, per version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Preamble {
    /// v1: the genesis state alone; the log starts at block 1.
    V1 { genesis: Genesis },
    /// v2: the first block number and the genesis state, then the totem.
    V2 { first: u32, genesis: Genesis },
    /// v3 starting at block 1: the genesis state, then the totem.
    V3Genesis { genesis: Genesis },
    /// v3 starting after block 1: the chain id, then the totem (48 bytes in all).
    V3ChainId { first: u32, chain_id: [u8; 32] },
}

impl Preamble {
    /// The first block the log holds.
    pub fn first(&self) -> u32 {
        match self {
            Self::V1 { .. } | Self::V3Genesis { .. } => 1,
            Self::V2 { first, .. } | Self::V3ChainId { first, .. } => *first,
        }
    }

    pub fn version(&self) -> u32 {
        match self {
            Self::V1 { .. } => 1,
            Self::V2 { .. } => 2,
            Self::V3Genesis { .. } | Self::V3ChainId { .. } => 3,
        }
    }

    pub fn chain_id(&self) -> [u8; 32] {
        match self {
            Self::V1 { genesis } | Self::V2 { genesis, .. } | Self::V3Genesis { genesis } => {
                genesis.chain_id()
            }
            Self::V3ChainId { chain_id, .. } => *chain_id,
        }
    }

    pub fn pack(&self) -> Vec<u8> {
        let v1 = self.version() == 1;
        let mut out = self.version().to_le_bytes().to_vec();
        if !v1 {
            out.extend_from_slice(&self.first().to_le_bytes());
        }
        match self {
            Self::V1 { genesis } | Self::V2 { genesis, .. } | Self::V3Genesis { genesis } => {
                out.extend_from_slice(&genesis.pack());
            }
            Self::V3ChainId { chain_id, .. } => out.extend_from_slice(chain_id),
        }
        if !v1 {
            out.extend_from_slice(&TOTEM.to_le_bytes());
        }
        out
    }
}

/// The fields of a synthetic `signed_block`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockSpec {
    /// `block_timestamp_type` slot.
    pub timestamp: u32,
    pub producer: u64,
    pub confirmed: u16,
    pub transaction_mroot: [u8; 32],
    pub action_mroot: [u8; 32],
    pub schedule_version: u32,
    /// A legacy `new_producers` schedule: its version and `(producer_name, key)` pairs.
    pub new_producers: Option<(u32, Vec<(u64, Key)>)>,
    /// `(type, data)` pairs, part of the header and so of the id.
    pub header_extensions: Vec<(u16, Vec<u8>)>,
    /// `(type, data)` pairs after the transactions, outside the id.
    pub block_extensions: Vec<(u16, Vec<u8>)>,
}

impl BlockSpec {
    /// What [`chain`] builds for block `n`: produced by `eosio` at slot `n`, with a block
    /// extension of `n % 5 * 16` bytes so that block sizes vary.
    pub fn synthetic(n: u32) -> Self {
        Self {
            timestamp: n,
            producer: EOSIO,
            block_extensions: vec![(0, vec![n as u8; n as usize % 5 * 16])],
            ..Self::default()
        }
    }

    /// The packed `block_header` of a block following `previous`: the bytes its id hashes.
    pub fn header(&self, previous: &[u8; 32]) -> Vec<u8> {
        let mut out = self.timestamp.to_le_bytes().to_vec();
        out.extend_from_slice(&self.producer.to_le_bytes());
        out.extend_from_slice(&self.confirmed.to_le_bytes());
        out.extend_from_slice(previous);
        out.extend_from_slice(&self.transaction_mroot);
        out.extend_from_slice(&self.action_mroot);
        out.extend_from_slice(&self.schedule_version.to_le_bytes());
        match &self.new_producers {
            None => out.push(0),
            Some((version, producers)) => {
                out.push(1);
                out.extend_from_slice(&version.to_le_bytes());
                varuint32_into(&mut out, producers.len());
                for (name, key) in producers {
                    out.extend_from_slice(&name.to_le_bytes());
                    key.pack_into(&mut out);
                }
            }
        }
        extensions_into(&mut out, &self.header_extensions);
        out
    }

    /// The packed `signed_block`: the header, a K1 `producer_signature` of zeros, no
    /// transactions, then the block extensions.
    pub fn signed_block(&self, previous: &[u8; 32]) -> Vec<u8> {
        let mut out = self.header(previous);
        out.push(0);
        out.extend_from_slice(&[0; 65]);
        out.push(0);
        extensions_into(&mut out, &self.block_extensions);
        out
    }
}

/// A synthetic block: its number and id as [`block_header`] computes them, the id it follows,
/// and its packed `signed_block`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub num: u32,
    pub id: [u8; 32],
    pub previous: [u8; 32],
    pub body: Vec<u8>,
}

impl Block {
    /// The block following `previous`, built from `spec`.
    pub fn new(previous: &[u8; 32], spec: &BlockSpec) -> Result<Self> {
        let body = spec.signed_block(previous);
        let h = block_header(&body)?;
        Ok(Self {
            num: h.number,
            id: h.id,
            previous: *previous,
            body,
        })
    }
}

/// A made-up id for block `n` (its number in the first four bytes): the predecessor of a
/// synthetic chain that does not start at block 1.
pub fn synthetic_id(n: u32) -> [u8; 32] {
    let mut id: [u8; 32] = Sha256::new()
        .chain_update(b"testkit block id")
        .chain_update(n.to_le_bytes())
        .finalize()
        .into();
    id[..4].copy_from_slice(&n.to_be_bytes());
    id
}

/// `count` linked [`BlockSpec::synthetic`] blocks from block `first`. Block 1 follows the
/// all-zero id, as on a real chain; any other first block follows [`synthetic_id`].
pub fn chain(first: u32, count: u32) -> Result<Vec<Block>> {
    ensure!(first > 0, "block numbers start at 1");
    let previous = if first == 1 {
        [0; 32]
    } else {
        synthetic_id(first - 1)
    };
    extend(&previous, count, BlockSpec::synthetic)
}

/// `count` linked blocks after `previous`, block `n` built from `spec(n)`.
pub fn extend(
    previous: &[u8; 32],
    count: u32,
    mut spec: impl FnMut(u32) -> BlockSpec,
) -> Result<Vec<Block>> {
    let mut previous = *previous;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let n = num_from_id(&previous)
            .checked_add(1)
            .context("block number overflow")?;
        let block = Block::new(&previous, &spec(n))?;
        previous = block.id;
        out.push(block);
    }
    Ok(out)
}

/// The files of one block log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogFiles {
    pub log: PathBuf,
    pub index: PathBuf,
}

/// The `.log` and `.index` bytes of a block log holding `blocks` after `preamble`. The blocks
/// must be numbered consecutively from `preamble.first()`; their links are not checked, so a
/// test can write a chain that breaks.
pub fn encode(preamble: &Preamble, blocks: &[Block]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut log = preamble.pack();
    let mut index = Vec::with_capacity(blocks.len() * 8);
    let mut expected = preamble.first();
    for block in blocks {
        ensure!(
            block.num == expected,
            "block {} where the log expects block {expected}",
            block.num
        );
        let pos = log.len() as u64;
        log.extend_from_slice(&block.body);
        log.extend_from_slice(&pos.to_le_bytes());
        index.extend_from_slice(&pos.to_le_bytes());
        expected = expected.wrapping_add(1);
    }
    Ok((log, index))
}

/// Write `<dir>/<stem>.log` and `<dir>/<stem>.index`. With no blocks this is a preamble-only
/// log and an empty index, as nodeos leaves `blocks.log` right after a split.
pub fn write(dir: &Path, stem: &str, preamble: &Preamble, blocks: &[Block]) -> Result<LogFiles> {
    let (log_bytes, index_bytes) = encode(preamble, blocks)?;
    let files = LogFiles {
        log: dir.join(format!("{stem}.log")),
        index: dir.join(format!("{stem}.index")),
    };
    std::fs::write(&files.log, log_bytes)
        .with_context(|| format!("write {}", files.log.display()))?;
    std::fs::write(&files.index, index_bytes)
        .with_context(|| format!("write {}", files.index.display()))?;
    Ok(files)
}

/// The layout of `spring-util block-log split-blocks` (and of a node's retained files): one
/// `blocks-<first>-<last>` pair per `stride` window `[k * stride + 1, (k + 1) * stride]`. The
/// file holding the chain's first block keeps `preamble`; every later file gets a 48-byte v3
/// chain-id preamble.
pub fn write_split(
    dir: &Path,
    preamble: &Preamble,
    blocks: &[Block],
    stride: u32,
) -> Result<Vec<LogFiles>> {
    ensure!(stride > 0, "stride must be positive");
    let chain_id = preamble.chain_id();
    let mut out = Vec::new();
    let mut rest = blocks;
    while let Some(head) = rest.first() {
        let stride = u64::from(stride);
        let window_end = u64::from(head.num).saturating_sub(1) / stride * stride + stride;
        let len = rest
            .iter()
            .position(|b| u64::from(b.num) > window_end)
            .unwrap_or(rest.len());
        let (file, tail) = rest.split_at(len);
        let p = if out.is_empty() {
            preamble.clone()
        } else {
            Preamble::V3ChainId {
                first: head.num,
                chain_id,
            }
        };
        let last = file.last().map_or(head.num, |b| b.num);
        out.push(write(
            dir,
            &format!("blocks-{}-{last}", head.num),
            &p,
            file,
        )?);
        rest = tail;
    }
    Ok(out)
}

/// A varuint32 (fc `unsigned_int`), canonical.
fn varuint32_into(out: &mut Vec<u8>, v: usize) {
    let mut v = u32::try_from(v).expect("varuint32 value fits in u32");
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// fc `bytes` and `string`: a varuint32 length, then the data.
fn bytes_into(out: &mut Vec<u8>, data: &[u8]) {
    varuint32_into(out, data.len());
    out.extend_from_slice(data);
}

/// `extensions_type`: a count, then `(u16 type, bytes data)` pairs.
fn extensions_into(out: &mut Vec<u8>, extensions: &[(u16, Vec<u8>)]) {
    varuint32_into(out, extensions.len());
    for (kind, data) in extensions {
        out.extend_from_slice(&kind.to_le_bytes());
        bytes_into(out, data);
    }
}
