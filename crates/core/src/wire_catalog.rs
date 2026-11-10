//! A verified chain of sealed Wire ABI history catalogs.
//!
//! [`crate::wire_history`] seals a catalog as a manifest plus two members: the sparse deployment
//! records and the block membership sidecar (32 bytes per block). An extension is a complete
//! catalog again, but its manifest names its base: the base manifest's SHA-256, end block and
//! last block id. Consumers pin the genesis catalog (the one without a base) and accept any
//! extension whose chain of bases reaches it.
//!
//! [`load`] applies the checks the 4.5 API applies to a pinned catalog (`disk-abi-history.ts`):
//! bounded manifest, source and profile identity, irreversible coverage, member descriptors and
//! digests, every record's shape and position, and every record's block id against the
//! membership. [`verify_extension`] adds what makes a segment an extension of its base: it names
//! the base's digest, end and last id; it starts at the base's end + 1; the base's records and
//! block ids are a byte prefix of its own; and its global sequence does not go back.
//!
//! The member files are pinned by identity when they are loaded. [`Loaded::revalidate`] checks
//! that no path now names another file, so a replaced member is never read as the verified one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::wire_history::FORMAT;

/// Bounds the 4.5 API puts on a catalog.
const MAX_MANIFEST: u64 = 65_536;
const MAX_DEPLOYMENTS: u64 = 128 * 1024 * 1024;
const MAX_RECORDS: u64 = 1_000_000;
const MAX_RECORD: usize = 16 * 1024 * 1024;

/// What a catalog must belong to.
#[derive(Clone, Debug)]
pub struct Pin {
    pub chain_id: String,
    pub node_revision: String,
    pub producer_profile_id: String,
}

/// An extension's reference to its base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseRef {
    pub manifest_sha256: String,
    pub end_block: u32,
    pub last_block_id: String,
}

/// A member file pinned at load.
#[derive(Clone, Debug)]
struct Pinned {
    path: PathBuf,
    identity: Identity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    dev_ino: (u64, u64),
}

fn identity(meta: &fs::Metadata) -> Identity {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Identity {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        dev_ino: (meta.dev(), meta.ino()),
    }
}

/// One sealed catalog: its manifest and what was checked about its members.
#[derive(Clone, Debug)]
pub struct Segment {
    pub manifest_sha256: String,
    pub manifest_path: PathBuf,
    /// The manifest exactly as sealed.
    pub manifest_text: String,
    pub manifest: Value,
    pub end_block: u32,
    pub first_block_id: String,
    pub last_block_id: String,
    pub last_global_sequence: Option<u64>,
    pub ship_abi_sha256: String,
    pub base: Option<BaseRef>,
    pub records: u64,
    pub deployments_sha256: String,
    pub block_ids_sha256: String,
}

/// A segment with its members open: the deployment records in memory, the membership on disk.
pub struct Loaded {
    pub segment: Segment,
    pub deployments: Arc<Vec<u8>>,
    ids: File,
    pinned: Vec<Pinned>,
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `/^(0|[1-9][0-9]*)$/` and at most `max`.
fn decimal(v: &Value, max: Option<u64>) -> Option<u64> {
    let s = v.as_str()?;
    let canonical = s == "0" || (!s.is_empty() && !s.starts_with('0'));
    if !canonical || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = s.parse().ok()?;
    max.is_none_or(|m| n <= m).then_some(n)
}

/// A JavaScript safe integer of at least `min`.
fn uint(v: &Value, min: u64) -> Option<u64> {
    let n = v.as_u64()?;
    (n >= min && n <= 9_007_199_254_740_991).then_some(n)
}

/// Read `path` whole, at most `bound` bytes, and pin its identity.
fn pin_read(path: &Path, bound: u64, expected_len: Option<u64>) -> Result<(Vec<u8>, Pinned, File)> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.is_file(),
        "{} is not a regular file (links are refused)",
        path.display()
    );
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let opened = identity(&file.metadata()?);
    ensure!(
        opened == identity(&meta),
        "{} changed while it was opened",
        path.display()
    );
    let len = expected_len.unwrap_or(opened.len);
    ensure!(
        len <= bound && opened.len == len,
        "{} has {} bytes, expected {len} (bound {bound})",
        path.display(),
        opened.len
    );
    let mut bytes = Vec::with_capacity(len as usize);
    (&mut file).take(len + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == len, "{} is truncated", path.display());
    Ok((
        bytes,
        Pinned {
            path: path.to_path_buf(),
            identity: opened,
        },
        file,
    ))
}

/// Hash a file of `len` bytes (and pin it) without holding it in memory.
fn pin_hash(
    path: &Path,
    len: u64,
    prefix: Option<u64>,
) -> Result<(String, Option<String>, Pinned, File)> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    ensure!(
        meta.is_file(),
        "{} is not a regular file (links are refused)",
        path.display()
    );
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let opened = identity(&file.metadata()?);
    ensure!(
        opened == identity(&meta) && opened.len == len,
        "{} has {} bytes, expected {len}",
        path.display(),
        opened.len
    );
    let mut whole = Sha256::new();
    let mut head = prefix.map(|_| Sha256::new());
    let mut head_hex = None;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let chunk = &buffer[..n];
        whole.update(chunk);
        if let (Some(h), Some(p)) = (head.as_mut(), prefix)
            && total < p
        {
            let take = ((p - total) as usize).min(n);
            h.update(&chunk[..take]);
            if total + take as u64 == p {
                head_hex = Some(hex::encode(head.take().unwrap().finalize()));
            }
        }
        total += n as u64;
    }
    ensure!(
        total == len,
        "{} changed while it was hashed",
        path.display()
    );
    if prefix == Some(0) {
        head_hex = Some(sha(&[]));
    }
    Ok((
        hex::encode(whole.finalize()),
        head_hex,
        Pinned {
            path: path.to_path_buf(),
            identity: opened,
        },
        file,
    ))
}

#[cfg(unix)]
fn pread(file: &File, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, off)
}

#[cfg(windows)]
fn pread(file: &File, mut off: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, off)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

/// The manifest's own checks, without opening its members.
fn read_manifest(path: &Path, expected_sha: Option<&str>, pin: &Pin) -> Result<(Segment, Pinned)> {
    let (raw, pinned, _) = pin_read(path, MAX_MANIFEST, None)?;
    ensure!(!raw.is_empty(), "{} is empty", path.display());
    let manifest_sha256 = sha(&raw);
    if let Some(expected) = expected_sha {
        ensure!(
            manifest_sha256 == expected,
            "{} has SHA-256 {manifest_sha256}, not the pinned {expected}",
            path.display()
        );
    }
    let manifest_text = String::from_utf8(raw).context("manifest is not UTF-8")?;
    let m: Value = serde_json::from_str(&manifest_text).context("invalid catalog manifest")?;
    ensure!(
        m["format"] == FORMAT
            && m["complete"] == true
            && m["chainId"] == pin.chain_id.as_str()
            && m["nodeRevision"] == pin.node_revision.as_str()
            && m["producerProfileId"] == pin.producer_profile_id.as_str(),
        "manifest source/profile mismatch"
    );
    let ship_abi_sha256 = m["shipAbiSha256"]
        .as_str()
        .filter(|s| is_hex64(s))
        .context("invalid shipAbiSha256")?
        .to_owned();
    let end_block = uint(&m["endBlock"], 1)
        .and_then(|e| u32::try_from(e).ok())
        .context("invalid irreversible coverage")?;
    let first_block_id = m["firstBlockId"].as_str().unwrap_or("").to_owned();
    let last_block_id = m["lastBlockId"].as_str().unwrap_or("").to_owned();
    ensure!(
        m["startBlock"] == 1
            && is_hex64(&first_block_id)
            && is_hex64(&last_block_id)
            && m["irreversibleBoundary"]["blockNum"] == end_block
            && m["irreversibleBoundary"]["blockId"] == last_block_id.as_str()
            && (m["lastGlobalSequence"].is_null()
                || decimal(&m["lastGlobalSequence"], None).is_some()),
        "invalid irreversible coverage"
    );
    let last_global_sequence = decimal(&m["lastGlobalSequence"], None);
    ensure!(
        m["blockIds"]["count"] == end_block
            && m["blockIds"]["bytes"] == u64::from(end_block) * 32
            && uint(&m["deployments"]["records"], 0).is_some_and(|r| r <= MAX_RECORDS),
        "invalid coverage/member count"
    );
    let base = match &m["base"] {
        Value::Null => None,
        b => Some(BaseRef {
            manifest_sha256: b["manifestSha256"]
                .as_str()
                .filter(|s| is_hex64(s))
                .context("invalid base manifest digest")?
                .to_owned(),
            end_block: uint(&b["endBlock"], 1)
                .and_then(|e| u32::try_from(e).ok())
                .context("invalid base end block")?,
            last_block_id: b["lastBlockId"]
                .as_str()
                .filter(|s| is_hex64(s))
                .context("invalid base last block id")?
                .to_owned(),
        }),
    };
    let records = m["deployments"]["records"].as_u64().unwrap();
    let digest = |member: &Value| -> Result<String> {
        Ok(member["sha256"]
            .as_str()
            .filter(|s| is_hex64(s))
            .context("invalid member descriptor")?
            .to_owned())
    };
    Ok((
        Segment {
            manifest_sha256,
            manifest_path: path.to_path_buf(),
            deployments_sha256: digest(&m["deployments"])?,
            block_ids_sha256: digest(&m["blockIds"])?,
            manifest_text,
            end_block,
            first_block_id,
            last_block_id,
            last_global_sequence,
            ship_abi_sha256,
            base,
            records,
            manifest: m,
        },
        pinned,
    ))
}

fn member_path(manifest: &Path, member: &Value, bound: u64) -> Result<(PathBuf, u64)> {
    let name = member["name"]
        .as_str()
        .context("invalid member descriptor")?;
    ensure!(
        !name.is_empty()
            && Path::new(name).file_name() == Some(std::ffi::OsStr::new(name))
            && name != "."
            && name != "..",
        "member {name} is not a plain file name"
    );
    let bytes = uint(&member["bytes"], 0)
        .filter(|b| *b <= bound)
        .context("invalid member descriptor")?;
    let dir = manifest.parent().context("manifest has no directory")?;
    Ok((dir.join(name), bytes))
}

/// Load and verify one sealed catalog. With `expected_sha`, its manifest must hash to it.
pub fn load(path: &Path, expected_sha: Option<&str>, pin: &Pin) -> Result<Loaded> {
    load_inner(path, expected_sha, pin, None).map(|(loaded, _)| loaded)
}

fn load_inner(
    path: &Path,
    expected_sha: Option<&str>,
    pin: &Pin,
    ids_prefix: Option<u64>,
) -> Result<(Loaded, Option<String>)> {
    let (segment, manifest_pin) = read_manifest(path, expected_sha, pin)?;
    let m = &segment.manifest;
    let (ids_path, ids_bytes) = member_path(path, &m["blockIds"], 128 * 1024 * 1024 * 1024)?;
    let (rows_path, rows_bytes) = member_path(path, &m["deployments"], MAX_DEPLOYMENTS)?;
    let (ids_sha, ids_head, ids_pin, ids) = pin_hash(&ids_path, ids_bytes, ids_prefix)?;
    ensure!(
        ids_sha == segment.block_ids_sha256,
        "block membership differs from its manifest"
    );
    let (deployments, rows_pin, _) = pin_read(&rows_path, MAX_DEPLOYMENTS, Some(rows_bytes))?;
    ensure!(
        sha(&deployments) == segment.deployments_sha256,
        "deployment records differ from their manifest"
    );
    let loaded = Loaded {
        segment,
        deployments: Arc::new(deployments),
        ids,
        pinned: vec![manifest_pin, ids_pin, rows_pin],
    };
    check_records(&loaded, 0)?;
    let end = loaded.segment.end_block;
    ensure!(
        hex::encode(loaded.block_id(1)?) == loaded.segment.first_block_id
            && hex::encode(loaded.block_id(end)?) == loaded.segment.last_block_id,
        "coverage boundary identity mismatch"
    );
    Ok((loaded, ids_head))
}

/// Check every record from byte `from` of the deployments member; return how many there were
/// and the lowest block they name.
fn check_records(loaded: &Loaded, from: usize) -> Result<(u64, Option<u32>)> {
    let end = loaded.segment.end_block;
    let mut positions = HashSet::new();
    let mut count = 0u64;
    let mut lowest: Option<u32> = None;
    for line in loaded.deployments[from..].split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        ensure!(line.len() <= MAX_RECORD, "ABI row exceeds bound");
        let row: Value = serde_json::from_slice(line).context("invalid sparse ABI row")?;
        let account = row["account"].as_str().unwrap_or("");
        let block = uint(&row["blockNum"], 1).and_then(|b| u32::try_from(b).ok());
        let phase = row["phase"].as_str().unwrap_or("");
        let abi_hex = row["abiHex"].as_str();
        let valid = !account.is_empty()
            && account.len() <= 256
            && block.is_some_and(|b| b <= end)
            && row["blockId"].as_str().is_some_and(is_hex64)
            && matches!(phase, "deployment" | "block-final")
            && row["present"].is_boolean()
            && abi_hex.is_some_and(|h| {
                h.len() % 2 == 0
                    && h.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            && decimal(&row["abiSequence"], Some(0xffff_ffff)).is_some();
        ensure!(valid, "invalid sparse ABI row");
        let position = if phase == "deployment" {
            let action = &row["action"];
            ensure!(
                row["present"] == true
                    && action["transactionId"].as_str().is_some_and(is_hex64)
                    && uint(&action["actionOrdinal"], 1).is_some()
                    && decimal(&action["globalSequence"], None).is_some_and(|g| g != 0),
                "invalid deployment action"
            );
            format!(
                "{account}:{}:deployment:{}",
                block.unwrap(),
                action["globalSequence"].as_str().unwrap()
            )
        } else {
            let ordinal = uint(&row["deltaOrdinal"], 0).context("invalid final-state ordinal")?;
            format!("{account}:{}:block-final:{ordinal}", block.unwrap())
        };
        ensure!(positions.insert(position), "duplicate sparse ABI position");
        let block = block.unwrap();
        ensure!(
            hex::encode(loaded.block_id(block)?) == row["blockId"].as_str().unwrap(),
            "ABI row differs from covered block identity at block {block}"
        );
        lowest = Some(lowest.map_or(block, |l| l.min(block)));
        count += 1;
    }
    if from == 0 {
        ensure!(count == loaded.segment.records, "ABI row count mismatch");
    }
    Ok((count, lowest))
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("segment", &self.segment.manifest_sha256)
            .field("end_block", &self.segment.end_block)
            .finish()
    }
}

impl Loaded {
    /// The membership's id for `block`.
    pub fn block_id(&self, block: u32) -> Result<[u8; 32]> {
        ensure!(
            block >= 1 && block <= self.segment.end_block,
            "block {block} is outside the catalog's coverage 1..={}",
            self.segment.end_block
        );
        let mut id = [0u8; 32];
        pread(&self.ids, u64::from(block - 1) * 32, &mut id)
            .context("truncated block identity index")?;
        Ok(id)
    }

    /// The membership's ids for `from..=to`, in one read.
    pub fn block_ids(&self, from: u32, to: u32) -> Result<Vec<[u8; 32]>> {
        ensure!(
            from >= 1 && from <= to && to <= self.segment.end_block,
            "blocks {from}..={to} are outside the catalog's coverage 1..={}",
            self.segment.end_block
        );
        let mut buf = vec![0u8; (to - from + 1) as usize * 32];
        pread(&self.ids, u64::from(from - 1) * 32, &mut buf)
            .context("truncated block identity index")?;
        let (ids, _) = buf.as_chunks::<32>();
        Ok(ids.to_vec())
    }

    /// Fail if any member path now names another file than the one verified.
    pub fn revalidate(&self) -> Result<()> {
        for p in &self.pinned {
            let meta = fs::symlink_metadata(&p.path)
                .with_context(|| format!("catalog member {} disappeared", p.path.display()))?;
            ensure!(
                meta.is_file() && identity(&meta) == p.identity,
                "catalog member {} changed or was replaced",
                p.path.display()
            );
        }
        Ok(())
    }

    /// The member files, by their names in the manifest.
    pub fn members(&self) -> Vec<(String, PathBuf)> {
        self.pinned
            .iter()
            .filter_map(|p| Some((p.path.file_name()?.to_str()?.to_owned(), p.path.clone())))
            .collect()
    }
}

/// Check that `next` extends `prev`. `next_ids_head` is the digest of `next`'s first
/// `prev.end_block * 32` membership bytes.
fn check_extension(prev: &Loaded, next: &Loaded, next_ids_head: Option<&str>) -> Result<()> {
    let (p, n) = (&prev.segment, &next.segment);
    let expected = BaseRef {
        manifest_sha256: p.manifest_sha256.clone(),
        end_block: p.end_block,
        last_block_id: p.last_block_id.clone(),
    };
    ensure!(
        n.base.as_ref() == Some(&expected),
        "catalog {} does not name {} (end {}) as its base",
        n.manifest_sha256,
        p.manifest_sha256,
        p.end_block
    );
    ensure!(
        n.end_block > p.end_block
            && n.first_block_id == p.first_block_id
            && n.ship_abi_sha256 == p.ship_abi_sha256,
        "catalog {} does not extend its base's coverage",
        n.manifest_sha256
    );
    ensure!(
        next.deployments.starts_with(&prev.deployments),
        "catalog {} does not keep its base's records byte for byte",
        n.manifest_sha256
    );
    let (added, lowest) = check_records(next, prev.deployments.len())?;
    ensure!(
        added == n.records - p.records,
        "catalog {} adds {added} records but counts {}",
        n.manifest_sha256,
        n.records - p.records
    );
    if let Some(lowest) = lowest {
        ensure!(
            lowest > p.end_block,
            "catalog {} adds a record at block {lowest}, inside its base (start must be {})",
            n.manifest_sha256,
            p.end_block + 1
        );
    }
    ensure!(
        next_ids_head == Some(p.block_ids_sha256.as_str()),
        "catalog {} does not keep its base's block ids",
        n.manifest_sha256
    );
    match (p.last_global_sequence, n.last_global_sequence) {
        (Some(a), Some(b)) => ensure!(
            b >= a,
            "catalog {} goes back in global sequence",
            n.manifest_sha256
        ),
        (Some(_), None) => bail!(
            "catalog {} lost its last global sequence",
            n.manifest_sha256
        ),
        _ => {}
    }
    Ok(())
}

/// Load `path` as an extension of `prev` and verify it.
pub fn load_extension(prev: &Loaded, path: &Path, pin: &Pin) -> Result<Loaded> {
    let (next, head) = load_inner(
        path,
        None,
        pin,
        Some(u64::from(prev.segment.end_block) * 32),
    )?;
    check_extension(prev, &next, head.as_deref())?;
    Ok(next)
}

/// Verify that `next` extends `prev` (both already loaded).
pub fn verify_extension(prev: &Loaded, next: &Loaded) -> Result<()> {
    let head = {
        let len = u64::from(prev.segment.end_block) * 32;
        let mut h = Sha256::new();
        let mut off = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        while off < len {
            let n = ((len - off) as usize).min(buf.len());
            pread(&next.ids, off, &mut buf[..n])?;
            h.update(&buf[..n]);
            off += n as u64;
        }
        hex::encode(h.finalize())
    };
    check_extension(prev, next, Some(&head))
}

/// The pinned genesis and the verified segments that extend it, oldest first.
pub struct Chain {
    pub pin: Pin,
    pub segments: Vec<Segment>,
    pub newest: Arc<Loaded>,
    /// Manifests already looked at, so following a directory only reads new ones.
    seen: HashSet<PathBuf>,
}

/// Manifest files under `dir` (two levels deep), sorted.
fn manifests(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut walk = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = walk.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("list {}", d.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() && depth < 2 {
                walk.push((path, depth + 1));
            } else if kind.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".manifest.json"))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Quick look at a manifest: its digest and base digest, or `None` if it is not one.
fn peek(path: &Path) -> Option<(String, Option<String>, u32)> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        return None;
    }
    let raw = fs::read(path).ok()?;
    let m: Value = serde_json::from_slice(&raw).ok()?;
    if m["format"] != FORMAT {
        return None;
    }
    let base = m["base"]["manifestSha256"].as_str().map(str::to_owned);
    Some((
        sha(&raw),
        base,
        u32::try_from(m["endBlock"].as_u64()?).ok()?,
    ))
}

impl Chain {
    /// Find the genesis catalog by its digest under `dir`, then the longest verified chain of
    /// extensions from it. A manifest that does not verify is reported and left out.
    pub fn discover(dir: &Path, genesis_sha256: &str, pin: Pin) -> Result<Self> {
        let paths = manifests(dir)?;
        let mut by_sha: HashMap<String, (PathBuf, Option<String>, u32)> = HashMap::new();
        for path in &paths {
            if let Some((sha, base, end)) = peek(path) {
                by_sha.entry(sha).or_insert((path.clone(), base, end));
            }
        }
        let (genesis_path, _, _) = by_sha
            .get(genesis_sha256)
            .with_context(|| {
                format!(
                    "no manifest under {} hashes to {genesis_sha256}",
                    dir.display()
                )
            })?
            .clone();
        let genesis = load(&genesis_path, Some(genesis_sha256), &pin)?;
        ensure!(
            genesis.segment.base.is_none(),
            "the pinned genesis catalog is itself an extension"
        );
        let mut chain = Chain {
            pin,
            segments: vec![genesis.segment.clone()],
            newest: Arc::new(genesis),
            seen: paths.into_iter().collect(),
        };
        let mut children: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        for (sha, (_, base, end)) in &by_sha {
            if let Some(base) = base {
                children
                    .entry(base.clone())
                    .or_default()
                    .push((*end, sha.clone()));
            }
        }
        chain.adopt_descendants(&by_sha, &children)?;
        Ok(chain)
    }

    /// Extend the chain greedily: among the newest segment's verified children, the one reaching
    /// furthest (then the lowest digest) is adopted, until none is left.
    fn adopt_descendants(
        &mut self,
        by_sha: &HashMap<String, (PathBuf, Option<String>, u32)>,
        children: &BTreeMap<String, Vec<(u32, String)>>,
    ) -> Result<bool> {
        let mut grew = false;
        loop {
            let mut candidates = children
                .get(&self.newest.segment.manifest_sha256)
                .cloned()
                .unwrap_or_default();
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            let mut adopted = false;
            for (_, sha) in candidates {
                let path = &by_sha[&sha].0;
                match load_extension(&self.newest, path, &self.pin) {
                    Ok(next) => {
                        self.segments.push(next.segment.clone());
                        self.newest = Arc::new(next);
                        adopted = true;
                        grew = true;
                        break;
                    }
                    Err(e) => eprintln!("[wire-catalog] {} is not adopted: {e:#}", path.display()),
                }
            }
            if !adopted {
                return Ok(grew);
            }
        }
    }

    /// Look for manifests added under `dir` since the last look and adopt the ones that extend
    /// the newest segment. Returns whether the chain grew.
    pub fn follow(&mut self, dir: &Path) -> Result<bool> {
        let fresh: Vec<PathBuf> = manifests(dir)?
            .into_iter()
            .filter(|p| !self.seen.contains(p))
            .collect();
        if fresh.is_empty() {
            return Ok(false);
        }
        let mut by_sha = HashMap::new();
        let mut children: BTreeMap<String, Vec<(u32, String)>> = BTreeMap::new();
        for path in fresh {
            // A manifest is published last, so one that does not parse yet is looked at again.
            match peek(&path) {
                Some((sha, base, end)) => {
                    if let Some(base) = &base {
                        children
                            .entry(base.clone())
                            .or_default()
                            .push((end, sha.clone()));
                    }
                    by_sha.entry(sha).or_insert((path.clone(), base, end));
                    self.seen.insert(path);
                }
                None => continue,
            }
        }
        self.adopt_descendants(&by_sha, &children)
    }

    pub fn genesis(&self) -> &Segment {
        &self.segments[0]
    }

    /// The oldest segment whose coverage reaches `block`: the catalog that sealed it.
    pub fn sealing(&self, block: u32) -> Option<&Segment> {
        self.segments.iter().find(|s| s.end_block >= block)
    }

    /// The segments after the one with digest `after` (all of them for `None`).
    pub fn after(&self, after: Option<&str>) -> Result<&[Segment]> {
        match after {
            None => Ok(&self.segments),
            Some(sha) => {
                let i = self
                    .segments
                    .iter()
                    .position(|s| s.manifest_sha256 == sha)
                    .with_context(|| format!("catalog {sha} is not in the verified chain"))?;
                Ok(&self.segments[i + 1..])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CHAIN: &str = "9999999999999999999999999999999999999999999999999999999999999999";

    fn pin() -> Pin {
        Pin {
            chain_id: CHAIN.into(),
            node_revision: "rev".into(),
            producer_profile_id: "profile".into(),
        }
    }

    fn id(n: u32) -> [u8; 32] {
        let mut id = [0xab; 32];
        id[..4].copy_from_slice(&n.to_be_bytes());
        id
    }

    fn record(account: &str, block: u32, phase: &str, sequence: u64) -> String {
        let mut r = json!({"account":account,"blockNum":block,"blockId":hex::encode(id(block)),
            "phase":phase,"present":true,"abiHex":"0e","abiSequence":"1"});
        if phase == "deployment" {
            r["action"] = json!({"transactionId":"11".repeat(32),"actionOrdinal":1,"globalSequence":sequence.to_string()});
        } else {
            r["deltaOrdinal"] = json!(0);
        }
        r.to_string()
    }

    /// Write a sealed catalog ending at `end` with `records`, extending `base` when given.
    fn seal(
        dir: &Path,
        name: &str,
        end: u32,
        records: &[String],
        base: Option<&(String, u32)>,
    ) -> (PathBuf, String) {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        let d = dir.join(name);
        let rows: String = records.iter().map(|r| format!("{r}\n")).collect();
        let ids: Vec<u8> = (1..=end).flat_map(id).collect();
        std::fs::write(d.join("h.ndjson"), &rows).unwrap();
        std::fs::write(d.join("h.ndjson.blocks.bin"), &ids).unwrap();
        let mut m = json!({"format":FORMAT,"chainId":CHAIN,"nodeRevision":"rev","producerProfileId":"profile",
            "shipAbiSha256":"22".repeat(32),"startBlock":1,"endBlock":end,"firstBlockId":hex::encode(id(1)),
            "lastBlockId":hex::encode(id(end)),"irreversibleBoundary":{"blockNum":end,"blockId":hex::encode(id(end))},
            "complete":true,"lastGlobalSequence":(end * 10).to_string(),
            "deployments":{"name":"h.ndjson","bytes":rows.len(),"sha256":sha(rows.as_bytes()),"records":records.len()},
            "blockIds":{"name":"h.ndjson.blocks.bin","bytes":ids.len(),"sha256":sha(&ids),"count":end}});
        if let Some((sha, base_end)) = base {
            m["base"] = json!({"manifestSha256":sha,"endBlock":base_end,"lastBlockId":hex::encode(id(*base_end))});
        }
        let text = format!("{m}\n");
        let path = d.join("h.ndjson.manifest.json");
        std::fs::write(&path, &text).unwrap();
        (path, sha(text.as_bytes()))
    }

    fn tmp(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wire-catalog-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_chain_of_extensions_is_followed_from_the_pinned_genesis() {
        let d = tmp("chain");
        let r1 = vec![
            record("sysio", 3, "deployment", 7),
            record("sysio", 3, "block-final", 0),
        ];
        let (_, g) = seal(&d, "a", 10, &r1, None);
        let mut r2 = r1.clone();
        r2.push(record("token", 12, "block-final", 0));
        let (_, e1) = seal(&d, "b", 20, &r2, Some(&(g.clone(), 10)));
        let mut chain = Chain::discover(&d, &g, pin()).unwrap();
        assert_eq!(chain.segments.len(), 2);
        assert_eq!(chain.newest.segment.manifest_sha256, e1);
        assert_eq!(chain.sealing(5).unwrap().manifest_sha256, g);
        assert_eq!(chain.sealing(11).unwrap().manifest_sha256, e1);
        assert!(chain.sealing(21).is_none());
        assert_eq!(chain.after(Some(&g)).unwrap().len(), 1);
        assert_eq!(
            chain.newest.block_ids(9, 11).unwrap(),
            vec![id(9), id(10), id(11)]
        );

        // Following the directory adopts a new extension and ignores what does not extend.
        assert!(!chain.follow(&d).unwrap());
        let mut r3 = r2.clone();
        r3.push(record("token", 25, "block-final", 0));
        seal(&d, "c", 30, &r3, Some(&(e1.clone(), 20)));
        let mut bad = r2.clone();
        bad.push(record("token", 15, "block-final", 0)); // inside its base
        seal(&d, "d", 40, &bad, Some(&(e1.clone(), 20)));
        assert!(chain.follow(&d).unwrap());
        assert_eq!(chain.newest.segment.end_block, 30);
        chain.newest.revalidate().unwrap();
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn extensions_must_keep_their_base() {
        let d = tmp("base");
        let r1 = vec![record("sysio", 3, "block-final", 0)];
        let (gp, g) = seal(&d, "a", 10, &r1, None);
        let genesis = load(&gp, Some(&g), &pin()).unwrap();
        // Records rewritten: not a byte prefix.
        let (p, _) = seal(
            &d,
            "b",
            20,
            &[record("sysio", 4, "block-final", 0)],
            Some(&(g.clone(), 10)),
        );
        assert!(
            format!("{:#}", load_extension(&genesis, &p, &pin()).unwrap_err())
                .contains("records byte for byte")
        );
        // Wrong base end.
        let (p, _) = seal(&d, "c", 20, &r1, Some(&(g.clone(), 9)));
        assert!(load_extension(&genesis, &p, &pin()).is_err());
        // A good one, then its block ids changed below the base end.
        let (p, _) = seal(&d, "e", 20, &r1, Some(&(g.clone(), 10)));
        load_extension(&genesis, &p, &pin()).unwrap();
        let ids_path = p.parent().unwrap().join("h.ndjson.blocks.bin");
        let mut ids = std::fs::read(&ids_path).unwrap();
        ids[5 * 32 + 7] ^= 1;
        std::fs::write(&ids_path, &ids).unwrap();
        assert!(
            load_extension(&genesis, &p, &pin()).is_err(),
            "member digest no longer matches"
        );
        // The genesis pin is a digest.
        assert!(load(&gp, Some(&"00".repeat(32)), &pin()).is_err());
        let other = Pin {
            chain_id: "88".repeat(32),
            ..pin()
        };
        assert!(load(&gp, Some(&g), &other).is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn records_are_checked_like_the_api_checks_them() {
        let d = tmp("records");
        let mut wrong_id: Value =
            serde_json::from_str(&record("sysio", 3, "block-final", 0)).unwrap();
        wrong_id["blockId"] = json!(hex::encode(id(4)));
        let (p, s) = seal(&d, "a", 10, &[wrong_id.to_string()], None);
        assert!(
            format!("{:#}", load(&p, Some(&s), &pin()).unwrap_err())
                .contains("covered block identity")
        );
        let dup = record("sysio", 3, "block-final", 0);
        let (p, s) = seal(&d, "b", 10, &[dup.clone(), dup], None);
        assert!(format!("{:#}", load(&p, Some(&s), &pin()).unwrap_err()).contains("duplicate"));
        let mut zero: Value = serde_json::from_str(&record("sysio", 3, "deployment", 7)).unwrap();
        zero["action"]["globalSequence"] = json!("0");
        let (p, s) = seal(&d, "c", 10, &[zero.to_string()], None);
        assert!(load(&p, Some(&s), &pin()).is_err());
        let (p, s) = seal(&d, "f", 10, &[record("sysio", 3, "block-final", 0)], None);
        let loaded = load(&p, Some(&s), &pin()).unwrap();
        std::fs::write(p.parent().unwrap().join("h.ndjson"), b"{}\n").unwrap();
        assert!(
            loaded.revalidate().is_err(),
            "a replaced member is detected"
        );
        std::fs::remove_dir_all(d).ok();
    }
}
