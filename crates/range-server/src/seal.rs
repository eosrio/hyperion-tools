//! The continuous catalog sealer: the service extends its own catalog chain as the node's
//! irreversible history grows.
//!
//! The block log holds irreversible blocks only, so its last complete block is a block the chain
//! can never fork away from. When at least `--seal-blocks` such blocks follow the newest link (or
//! fewer have waited `--seal-max-wait-ms`), the sealer walks them once
//! ([`hyperion_ship::wire_history::Sealer`]), checks the new block ids against the block log
//! (every header hashes to its id and names the previous block's), writes one segment, loads it
//! back through the same verifier consumers use, and serves it. A segment holds only what it adds,
//! so a seal costs the new blocks, never the history.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use hyperion_ship::wire_catalog::load_segment;
use hyperion_ship::wire_history::{Sealer, Tip};

use crate::Service;

/// How the sealer runs.
pub struct Config {
    pub schema: String,
    pub dir: PathBuf,
    pub blocks: u32,
    pub max_wait: Duration,
    pub poll: Duration,
    pub threads: usize,
}

/// What `/v1/info` reports about the sealer.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub segments: u64,
    pub last_seal_ms: Option<u64>,
    pub last_error: Option<String>,
}

fn open(svc: &Service, cfg: &Config) -> Result<Sealer> {
    let newest = svc.chain.lock().unwrap().newest.clone();
    Sealer::open(
        &svc.protocol,
        &cfg.schema,
        &svc.chain_id,
        &svc.state_history,
        newest.tip()?,
        &newest.deployments,
        cfg.threads,
    )
}

/// The block log's last complete block and its header id.
fn irreversible(svc: &Service) -> Result<Option<(u32, [u8; 32])>> {
    let Some(last) = svc.blocks.last_block()? else {
        return Ok(None);
    };
    let slab = svc.blocks.slab(last, last)?;
    let block = action_proto::walk_block(slab.block(last), last, None)?;
    Ok(Some((last, block.id)))
}

/// Every header of `start..start + ids.len()` must hash to its id and name the previous one's.
fn check_headers(svc: &Service, tip: &Tip, start: u32, ids: &[[u8; 32]]) -> Result<()> {
    let end = start + ids.len() as u32 - 1;
    let mut previous = tip.last_block_id;
    let mut at = start;
    while at <= end {
        let to = end.min(at + 2047);
        let slab = svc.blocks.slab(at, to)?;
        for n in at..=to {
            let id = ids[(n - start) as usize];
            let block = action_proto::walk_block(slab.block(n), n, Some(&id))?;
            ensure!(
                block.previous == previous,
                "block {n} names {} as its parent, but block {} is {}",
                hex::encode(block.previous),
                n - 1,
                hex::encode(previous)
            );
            previous = id;
        }
        at = to + 1;
    }
    Ok(())
}

/// One seal, if one is due: returns the segment's end block.
fn step(
    svc: &Service,
    cfg: &Config,
    sealer: &mut Sealer,
    waiting: &mut Option<Instant>,
) -> Result<Option<u32>> {
    let Some((target, target_id)) = irreversible(svc)? else {
        return Ok(None);
    };
    let target = match sealer.logs_end()? {
        Some(logs) if logs < target => return Ok(None),
        _ => target,
    };
    let tip = sealer.tip().clone();
    if target <= tip.end_block {
        *waiting = None;
        return Ok(None);
    }
    let since = *waiting.get_or_insert_with(Instant::now);
    if target - tip.end_block < cfg.blocks && since.elapsed() < cfg.max_wait {
        return Ok(None);
    }
    let started = Instant::now();
    let sealed = sealer.seal(target, target_id, &cfg.dir, &mut |start, ids| {
        check_headers(svc, &tip, start, ids)
    })?;
    // Serve the segment only as consumers will see it: loaded back through the verifier.
    let mut chain = svc.chain.lock().unwrap();
    let next = load_segment(&chain.newest, &sealed.manifest, &chain.pin).with_context(|| {
        format!(
            "the sealed segment {} does not verify",
            sealed.manifest.display()
        )
    })?;
    ensure!(
        next.tip()? == *sealer.tip(),
        "the sealed segment differs from what the sealer recorded"
    );
    chain.adopt(next);
    let view = svc.view(&chain)?;
    *svc.current.write().unwrap() = Arc::new(view);
    drop(chain);
    let ms = started.elapsed().as_millis() as u64;
    {
        let mut status = svc.seal_status.lock().unwrap();
        status.segments += 1;
        status.last_seal_ms = Some(ms);
        status.last_error = None;
    }
    eprintln!(
        "[range-server] sealed {}..={} ({} records) in {ms} ms: {}",
        sealed.start_block,
        sealed.end_block,
        sealed.records,
        sealed.manifest.display()
    );
    *waiting = None;
    Ok(Some(target))
}

/// Run the sealer until the process ends. Errors are reported and retried at the next poll; a
/// chain extended by someone else (`--follow`) is picked up by reopening the sealer on it.
pub fn run(svc: Arc<Service>, cfg: Config) {
    let mut sealer: Option<Sealer> = None;
    let mut waiting = None;
    loop {
        let outcome = (|| -> Result<()> {
            let newest = svc.chain.lock().unwrap().newest.tip()?;
            if sealer.as_ref().is_none_or(|s| *s.tip() != newest) {
                sealer = Some(open(&svc, &cfg)?);
            }
            step(&svc, &cfg, sealer.as_mut().unwrap(), &mut waiting)?;
            Ok(())
        })();
        if let Err(e) = outcome {
            let message = format!("{e:#}");
            eprintln!("[range-server] sealing: {message}");
            svc.seal_status.lock().unwrap().last_error = Some(message);
            // A failed seal changes nothing; the next attempt starts from the served chain.
            sealer = None;
        }
        std::thread::sleep(cfg.poll);
    }
}

/// The directory segments go to: `--seal-dir`, or `segments` under the catalog directory.
pub fn dir(catalog_dir: &Path, seal_dir: Option<&Path>) -> PathBuf {
    seal_dir.map_or_else(|| catalog_dir.join("segments"), Path::to_path_buf)
}
