//! In-place live-node qualification, not a live archive server. A caller supplies the LIB
//! reported by the node and a fixed range strictly below it. Only indexed, complete records
//! in that range are exposed to the ordinary strict readers. The descriptors stay pinned
//! through append and rename; no files, indexes, links or snapshots are written.
//!
//! This relies on nodeos preserving irreversible bytes. Pruning, vacuuming, overwriting or
//! repairing these files during a run is unsupported; integrity checks still fail closed.

use anyhow::{Context, Result, ensure};
use hyperion_ship::binio::{FileCache, PosFile, num_from_id};
use hyperion_ship::catalog::{self, Discovered, Finding, PathPair};
use hyperion_ship::protocol::BlockPos;
use hyperion_ship::shiplog::{LogKind, unsupported_magic};

use crate::store::{BlocksMode, Store, StoreConfig};

/// Capture a fixed irreversible view of the original files, then use the archive's normal
/// readers, catalogs and validation. Leave the newest irreversible block out: its block-log
/// index may still be catching up, and the next index offset bounds the preceding block.
pub fn open(cfg: &StoreConfig, lib: BlockPos) -> Result<(Store, Vec<Finding>)> {
    let (begin, end) = cfg
        .range
        .context("live test requires a fixed block range")?;
    ensure!(
        begin > 0 && begin < end && end <= lib.num,
        "live test range {begin}..{end} must be nonempty and strictly below LIB {}",
        lib.num
    );
    ensure!(
        cfg.blocks == BlocksMode::Required,
        "live test requires matching block logs"
    );
    let found = catalog::discover(&cfg.dirs, &cfg.index_dirs, cfg.recursive)?;
    let mut selected = Discovered::default();
    let mut files = Vec::new();
    for (kind, output) in [
        (LogKind::Trace, &mut selected.trace),
        (LogKind::ChainState, &mut selected.chain_state),
        (LogKind::Finality, &mut selected.finality),
    ] {
        for pair in found.ship(kind) {
            if let Some((log, index)) = ship_prefix(pair, begin.saturating_sub(1), end)? {
                pin(&mut files, log, index, cfg.max_open_files)?;
                output.push(pair.clone());
            }
        }
    }
    for pair in &found.blocks {
        if let Some((log, index)) = block_prefix(pair, begin, end)? {
            pin(&mut files, log, index, cfg.max_open_files)?;
            selected.blocks.push(pair.clone());
        }
    }
    let (store, findings) = Store::open_files(cfg, selected, FileCache::pinned(files)?)?;
    ensure!(
        store.served.blocks == (begin, end),
        "block logs do not cover the live test range {begin}..{end}; retry if rotation is in progress"
    );
    for kind in LogKind::ALL {
        if !found.ship(kind).is_empty() {
            ensure!(
                store.served.ship(kind) == (begin, end),
                "{} logs do not cover the live test range {begin}..{end}; select a range they hold or retry after rotation",
                kind.label()
            );
        }
    }
    Ok((store, findings))
}

fn pin(files: &mut Vec<PosFile>, log: PosFile, index: PosFile, limit: usize) -> Result<()> {
    ensure!(
        files.len() + 2 <= limit,
        "live test needs more than {limit} pinned descriptors; narrow the range or increase max_open_files"
    );
    files.extend([log, index]);
    Ok(())
}

fn ship_prefix(pair: &PathPair, begin: u32, end: u32) -> Result<Option<(PosFile, PosFile)>> {
    let log = PosFile::open(&pair.log)?;
    if log.is_empty() {
        return Ok(None);
    }
    if let Some(problem) = unsupported_magic(log.u64_at(0)?) {
        anyhow::bail!("{}: {problem}", pair.log.display());
    }
    let mut id = [0; 32];
    log.read_exact_at(8, &mut id)?;
    let first = num_from_id(&id);
    if first >= end {
        return Ok(None);
    }
    let index = PosFile::open(&pair.index)?;
    if index.len() < 8 {
        return Ok(None);
    }
    let available = u64::from(first) + index.len() / 8;
    if u64::from(begin) >= available || first >= end {
        return Ok(None);
    }
    let last = u64::from(end).min(available) - 1;
    let index_len = (last - u64::from(first) + 1) * 8;
    let pos = index.u64_at(index_len - 8)?;
    let size = log.u64_at(pos.checked_add(40).context("entry offset overflow")?)?;
    let log_len = pos
        .checked_add(56)
        .and_then(|n| n.checked_add(size))
        .context("entry size overflow")?;
    ensure!(
        log.u64_at(log_len.checked_sub(8).context("short entry")?)? == pos,
        "{}: incomplete indexed entry below LIB",
        pair.log.display()
    );
    Ok(Some((log.prefix(log_len)?, index.prefix(index_len)?)))
}

fn block_prefix(pair: &PathPair, begin: u32, end: u32) -> Result<Option<(PosFile, PosFile)>> {
    let log = PosFile::open(&pair.log)?;
    let version = log.u32_at(0)?;
    ensure!(
        (1..=3).contains(&version),
        "{}: live tests require a non-pruned v1-v3 block log",
        pair.log.display()
    );
    let first = if version == 1 { 1 } else { log.u32_at(4)? };
    if first >= end {
        return Ok(None);
    }
    let index = PosFile::open(&pair.index)?;
    if index.len() < 8 {
        return Ok(None);
    }
    let count = index.len() / 8;
    let available = u64::from(first) + count;
    if u64::from(begin) >= available || first >= end {
        return Ok(None);
    }
    let mut after = u64::from(end).min(available);
    // An active block's packed size is not stored in its header. A following index slot
    // gives an exact end even if the physical EOF is halfway through a newer block.
    let active = pair.log.file_name().is_some_and(|n| n == "blocks.log");
    if active && after == available {
        after -= 1;
    }
    if after <= u64::from(begin) || after <= u64::from(first) {
        return Ok(None);
    }
    let index_len = (after - u64::from(first)) * 8;
    let log_len = if after < available {
        index.u64_at(index_len)?
    } else {
        log.len()
    };
    Ok(Some((log.prefix(log_len)?, index.prefix(index_len)?)))
}
