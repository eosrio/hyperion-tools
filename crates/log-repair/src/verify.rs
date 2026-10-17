//! `verify`: find log entries that read back without an error but are wrong. This is the
//! silent corruption that a filesystem without data checksums (ext4, xfs) passes through.
//! Read-only.
//!
//! What the files prove on their own: every entry's position trailer matches the index;
//! a blocks.log header carries its block number and hashes to the ID its successor names
//! as `previous`; a SHIP payload's zlib stream is intact (sizes, Adler-32) and its entry
//! names the right block. Nothing in the files checks blocks.log transaction bodies or the
//! rest of a SHIP entry's block ID: `--against` compares those with an intact blocks.log
//! from another node.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Result, ensure};
use serde_json::json;

use crate::format::{
    Kind, Log, block_header, num, orphans, ship_entry_size, ship_payload, u32le, u64le,
};
use crate::io::{MAX_BYTES, media_error};
use crate::report::{self, Out};

/// Bytes read per batch; one entry larger than this is read on its own.
const CHUNK: u64 = 64 * 1024 * 1024;
/// Index entries read per batch.
const BATCH: u32 = 65_536;

struct Finding {
    block: u32,
    offset: u64,
    len: u64,
    problem: String,
}

/// Index positions for `n..=m`, plus where entry `m` ends.
fn positions(log: &Log, n: u32, m: u32) -> Result<Vec<u64>> {
    let raw = log
        .index
        .read(u64::from(n - log.first) * 8, u64::from(m - n + 1) * 8)?;
    let mut out: Vec<u64> = raw
        .as_chunks::<8>()
        .0
        .iter()
        .map(|p| u64::from_le_bytes(*p))
        .collect();
    out.push(log.pos(m + 1)?);
    Ok(out)
}

/// A reference blocks.log's packed blocks for `n..=m`; `None` where it has no such block.
fn reference_blocks(r: &Log, n: u32, m: u32) -> Result<Vec<Option<Vec<u8>>>> {
    let mut out = vec![None; (m - n + 1) as usize];
    let (lo, hi) = (n.max(r.first), m.min(r.last));
    if lo > hi {
        return Ok(out);
    }
    let pos = positions(r, lo, hi)?;
    let buf = r.log.read(pos[0], pos[pos.len() - 1] - pos[0])?;
    for (j, w) in pos.windows(2).enumerate() {
        ensure!(
            w[1] > w[0] + 8,
            "reference {} has a broken index",
            r.log.path.display()
        );
        let e = &buf[(w[0] - pos[0]) as usize..(w[1] - pos[0]) as usize];
        ensure!(
            u64le(&e[e.len() - 8..]) == w[0],
            "reference {} is itself corrupt at block {}",
            r.log.path.display(),
            lo + j as u32
        );
        out[(lo - n) as usize + j] = Some(e[..e.len() - 8].to_vec());
    }
    Ok(out)
}

pub fn verify(
    log: &Log,
    range: Option<(u32, u32)>,
    against: &[PathBuf],
    max_findings: usize,
) -> Result<Out> {
    let (a, b) = match range {
        Some((a, b)) => (a.max(log.first), b.min(log.last)),
        None => (log.first, log.last),
    };
    ensure!(
        a <= b,
        "the range holds no blocks of this log ({}..{})",
        log.first,
        log.last
    );
    let refs = against
        .iter()
        .map(|p| {
            let r = Log::open(p, None, false)?;
            ensure!(
                r.kind == Kind::Blocks,
                "--against takes a blocks.log: {}",
                p.display()
            );
            ensure!(
                !same_file::is_same_file(p, &log.log.path).unwrap_or(false),
                "the target cannot be its own reference"
            );
            Ok(r)
        })
        .collect::<Result<Vec<_>>>()?;
    let min_entry = if log.kind == Kind::Blocks { 124 } else { 60 };

    // The ID of the block before the range, so the chain is checked from its first entry.
    let mut prev: Option<[u8; 32]> = None;
    if log.kind == Kind::Blocks && a > log.first {
        prev = log
            .body(a - 1)
            .ok()
            .and_then(|body| block_header(&body).ok().map(|h| h.id));
    }

    let started = Instant::now();
    let total = log.pos(b + 1)? - log.pos(a)?;
    let progress_every = 4u64 << 30;
    let (mut done, mut next_progress) = (0u64, progress_every);
    let mut findings: Vec<Finding> = Vec::new();
    let (mut entries, mut skipped, mut unreadable, mut orphaned) = (0u64, 0u64, 0u64, 0u64);
    let mut truncated = false;
    let mut n = a;
    'batches: while n <= b {
        let pos = positions(log, n, b.min(n.saturating_add(BATCH - 1)))?;
        // The longest run of well-framed entries that fits in one read (at least one).
        let mut k = 0usize;
        while k + 1 < pos.len() {
            let (s, e) = (pos[k], pos[k + 1]);
            if e <= s || e - s < min_entry || (k > 0 && e - pos[0] > CHUNK) {
                break;
            }
            k += 1;
        }
        if k == 0 {
            if pos[1] > pos[0] && pos[1] - pos[0] > MAX_BYTES {
                skipped += 1; // e.g. the full chain state at the start of a SHIP log
            } else {
                findings.push(Finding {
                    block: n,
                    offset: pos[0],
                    len: 0,
                    problem: format!(
                        "index positions {} and {} do not frame an entry",
                        pos[0], pos[1]
                    ),
                });
            }
            prev = None;
            n += 1;
            continue;
        }
        let m = n + k as u32 - 1;
        let span = pos[k] - pos[0];
        // Drop the pages only this read brought in, as scan does.
        #[cfg(target_os = "linux")]
        let cached = log.log.cached_pages(pos[0], span);
        let buf = match log.log.read(pos[0], span) {
            Ok(buf) => Some(buf),
            Err(e) if media_error(&e) => None,
            Err(e) => return Err(e.into()),
        };
        #[cfg(target_os = "linux")]
        if let Some(cached) = &cached {
            log.log.uncache(cached);
        }
        let references = refs
            .iter()
            .map(|r| reference_blocks(r, n, m))
            .collect::<Result<Vec<_>>>()?;

        for i in 0..k {
            let block = n + i as u32;
            let (s, e) = (pos[i], pos[i + 1]);
            entries += 1;
            let owned;
            let entry: &[u8] = match &buf {
                Some(buf) => &buf[(s - pos[0]) as usize..(e - pos[0]) as usize],
                None => match log.log.read(s, e - s) {
                    Ok(bytes) => {
                        owned = bytes;
                        &owned
                    }
                    Err(err) if media_error(&err) => {
                        unreadable += 1;
                        prev = None;
                        findings.push(Finding {
                            block,
                            offset: s,
                            len: e - s,
                            problem: "unreadable (I/O error): run scan".into(),
                        });
                        continue;
                    }
                    Err(err) => return Err(err.into()),
                },
            };
            let mut problems = Vec::new();
            // Where this entry's own bytes end. A SHIP entry's header gives its size: after a
            // fork, nodeos appends the replacement entries and leaves the abandoned ones in
            // place, so the gap up to the next index position holds orphaned entries.
            let own = if log.kind == Kind::Blocks {
                entry.len()
            } else {
                let size = ship_entry_size(entry);
                if size > entry.len() as u64 {
                    problems.push(format!(
                        "payload_size runs {} bytes past the next index position",
                        size - entry.len() as u64
                    ));
                    entry.len()
                } else {
                    size as usize
                }
            };
            if u64le(&entry[own - 8..own]) != s {
                problems.push("position trailer does not match the index".to_string());
            }
            if own < entry.len() {
                match orphans(&entry[own..], s + own as u64) {
                    Ok(count) => orphaned += count,
                    Err(err) => problems.push(format!(
                        "bytes after the entry are not orphaned fork entries: {err:#}"
                    )),
                }
            }
            let body = &entry[..own - 8];
            let mut ship_id = None;
            if log.kind == Kind::Blocks {
                match block_header(body) {
                    Ok(h) => {
                        if h.number != block {
                            problems.push(format!("header names block {}", h.number));
                        }
                        if let Some(p) = prev
                            && h.previous != p
                        {
                            problems.push(format!(
                                "`previous` does not match block {}'s ID: this header or the \
                                 one before it is corrupt",
                                block - 1
                            ));
                        }
                        prev = Some(h.id);
                    }
                    Err(err) => {
                        problems.push(format!("header does not parse: {err:#}"));
                        prev = None;
                    }
                }
            } else if body.len() >= 60 && u32le(&body[48..]) == 1 && u64le(&body[52..]) > MAX_BYTES
            {
                skipped += 1;
                continue;
            } else {
                match ship_payload(body) {
                    Ok((id, _)) => {
                        if num(&id) != block {
                            problems.push(format!("entry names block {}", num(&id)));
                        }
                        ship_id = Some(id);
                    }
                    Err(err) => problems.push(format!("payload: {err:#}")),
                }
            }
            for (r, blocks) in refs.iter().zip(&references) {
                let Some(reference) = &blocks[i] else {
                    continue;
                };
                if log.kind == Kind::Blocks {
                    if reference.as_slice() != body {
                        let at = body
                            .iter()
                            .zip(reference)
                            .position(|(x, y)| x != y)
                            .unwrap_or(body.len().min(reference.len()));
                        problems.push(format!(
                            "differs from {} from byte {} of the entry (file offset {})",
                            r.log.path.display(),
                            at,
                            s + at as u64
                        ));
                    }
                } else if let (Some(id), Ok(h)) = (ship_id, block_header(reference))
                    && id != h.id
                {
                    problems.push(format!(
                        "entry block ID differs from {}",
                        r.log.path.display()
                    ));
                }
            }
            if !problems.is_empty() {
                findings.push(Finding {
                    block,
                    offset: s,
                    len: e - s,
                    problem: problems.join("; "),
                });
                if findings.len() >= max_findings {
                    truncated = true;
                    break 'batches;
                }
            }
        }
        n = m + 1;
        done += span;
        if total > progress_every && done >= next_progress {
            next_progress += progress_every;
            let rate = done as f64 / started.elapsed().as_secs_f64().max(0.001);
            eprintln!(
                "verified {} of {} ({:.0}%) at {}/s, about {} left; {} problem(s) so far",
                report::bytes(done),
                report::bytes(total),
                done as f64 * 100.0 / total as f64,
                report::bytes(rate as u64),
                report::duration((total - done) as f64 / rate),
                findings.len()
            );
        }
    }
    // The last header in the range is checked by its successor's `previous`, if there is one.
    if !truncated
        && log.kind == Kind::Blocks
        && b < log.last
        && let Some(p) = prev
        && let Ok(next) = log.body(b + 1)
        && let Ok(h) = block_header(&next)
        && h.previous != p
    {
        let (s, e) = log.bounds(b)?;
        findings.push(Finding {
            block: b,
            offset: s,
            len: e - s,
            problem: format!(
                "block {}'s `previous` does not match this block's ID",
                b + 1
            ),
        });
    }

    let mut checked = vec!["index positions and every entry's position trailer"];
    let mut not_covered = Vec::new();
    if log.kind == Kind::Blocks {
        checked.push("header block numbers and the ID chain");
        if refs.is_empty() {
            not_covered.push(
                "transaction bodies and signatures: nothing in blocks.log checks them; pass \
                 --against with another node's blocks.log",
            );
        } else {
            checked.push("every packed block, byte for byte, against the reference(s)");
        }
    } else {
        checked.push("entry block numbers, and every payload's zlib framing, sizes, and Adler-32");
        if refs.is_empty() {
            not_covered.push(
                "entry block IDs beyond their block number, and the meaning of the payloads; \
                 pass --against with a blocks.log to check the IDs",
            );
        } else {
            checked.push("entry block IDs against the reference(s)");
            not_covered.push("the meaning of the payloads beyond their checksums");
        }
    }

    let seconds = started.elapsed().as_secs_f64();
    let mut text = format!(
        "Verified {} ({}): blocks {a}..{b}, {entries} entries, {} in {}.\n",
        log.log.path.display(),
        log.kind.stem(),
        report::bytes(total),
        report::duration(seconds)
    );
    text += &format!("Checked: {}.\n", checked.join("; "));
    for gap in &not_covered {
        text += &format!("Not covered: {gap}.\n");
    }
    if orphaned > 0 {
        text += &format!(
            "Skipped {orphaned} orphaned fork entr(ies): nodeos leaves abandoned entries in \
             place after a fork; their framing was checked.\n"
        );
    }
    if skipped > 0 {
        text += &format!("Skipped {skipped} entr(ies) over the 256 MiB read limit.\n");
    }
    if findings.is_empty() {
        text += "No problems found.\n";
    } else {
        text += &format!(
            "Found {}{} problem(s){}:\n",
            findings.len(),
            if truncated { "+" } else { "" },
            if unreadable > 0 {
                format!(", {unreadable} unreadable")
            } else {
                String::new()
            }
        );
        for f in &findings {
            text += &format!(
                "  block {} (bytes {}..{}): {}\n",
                f.block,
                f.offset,
                f.offset + f.len,
                f.problem
            );
        }
        if truncated {
            text +=
                "Stopped at --max-findings; damage this widespread needs a resync or restore.\n";
        }
    }
    let json = json!({
        "ok": true,
        "log": log.log.path,
        "kind": log.kind,
        "blocks": [a, b],
        "entries": entries,
        "bytes": total,
        "seconds": seconds,
        "checked": checked,
        "not_covered": not_covered,
        "skipped_entries": skipped,
        "orphaned_fork_entries": orphaned,
        "corrupt": !findings.is_empty(),
        "truncated": truncated,
        "findings": findings.iter().map(|f| json!({
            "block": f.block, "offset": f.offset, "len": f.len, "problem": f.problem,
        })).collect::<Vec<_>>(),
    });
    Ok(Out { json, text })
}
