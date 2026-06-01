//! Direct-from-disk reader.
//!
//! Parses the append-only `chain_state_history.{log,index}` in parallel,
//! read-only, and streams abi-index docs out. Bypasses nodeos/SHiP entirely, so
//! it scales with CPU cores (the work is zlib inflate) and never loads nodeos.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use anyhow::{anyhow, bail, Context, Result};
use rs_abieos::Abieos;

use crate::abi::build_abi_doc;
use crate::delta::{account_setabi, for_each_account_row};

/// The high 32 bits of a state-history entry magic are the "ship" name; the low
/// 16 are the version. Validates we're really reading a state-history log.
pub fn is_ship_magic(magic: u64) -> bool {
    (magic & 0xffff_ffff_0000_0000) == 0xc35d_5000_0000_0000
}

/// Read a checkpoint file: the block up to which the scan is contiguously complete.
fn read_checkpoint(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Atomically persist the resume watermark (write-temp-then-rename, so a crash mid-write
/// never leaves a torn checkpoint).
fn write_checkpoint(path: &str, resume_block: u64) {
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, resume_block.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Does the log's second indexed entry carry the ship magic? Used to recognise a
/// snapshot-restored log, whose first entry (the init delta) has a distinct magic.
fn second_entry_is_ship(log_path: &str, idx_path: &str) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut idx = File::open(idx_path)?;
    idx.seek(SeekFrom::Start(8))?; // offset of the 2nd entry
    let mut ob = [0u8; 8];
    idx.read_exact(&mut ob)?;
    let mut log = File::open(log_path)?;
    log.seek(SeekFrom::Start(u64::from_le_bytes(ob)))?;
    let mut m = [0u8; 8];
    Ok(log.read_exact(&mut m).is_ok() && is_ship_magic(u64::from_le_bytes(m)))
}

/// zlib-inflate a SHiP log payload (the bytes after the 4-byte size prefix).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut dec = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).context("zlib inflate")?;
    Ok(out)
}

/// Decode the zlib payload of a state-history entry into the raw table_delta[] bytes.
/// Handles both Leap encodings: `[u32=1][u64 decompressed_size][zlib]` and the
/// default `[u32 (ignored)][zlib]` (see read_unpacked_entry in Leap's log.hpp).
pub fn decode_payload(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() < 4 {
        return Ok(Vec::new());
    }
    let s = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let zstart = if s == 1 && payload.len() >= 12 { 12 } else { 4 };
    if payload.len() <= zstart {
        return Ok(Vec::new());
    }
    inflate(&payload[zstart..])
}

/// Read a LEB128 varuint from a streaming reader (for the early-exit path).
fn read_varuint_r<R: std::io::Read>(r: &mut R) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)?;
        value |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            bail!("varuint too long");
        }
    }
}

/// Stream-inflate a state-history entry only as far as the `account` table, emitting any
/// setabi rows, then stop. Used for *huge* entries (a snapshot init-delta spans thousands of
/// 128K records): we read just the compressed prefix up to the account table and let the
/// caller seek past the rest, instead of reading + inflating (+ allocating) the whole payload.
/// `log` must be positioned at the payload start (right after the 48-byte header).
fn scan_account_streaming_entry<R: std::io::Read>(
    log: &mut R,
    abieos: &Abieos,
    block_num: u32,
    sink: &std::sync::mpsc::Sender<String>,
) -> Result<()> {
    use std::io::{self, Read};
    // payload prefix: [u32 s][optional u64 decompressed_size when s==1]
    let mut s4 = [0u8; 4];
    log.read_exact(&mut s4)?;
    if u32::from_le_bytes(s4) == 1 {
        let mut sz = [0u8; 8];
        log.read_exact(&mut sz)?;
    }
    let mut z = flate2::read::ZlibDecoder::new(log);
    let n_tables = read_varuint_r(&mut z)?;
    for ti in 0..n_tables {
        let _variant = read_varuint_r(&mut z)?;
        let name_len = read_varuint_r(&mut z)? as usize;
        let mut name = vec![0u8; name_len];
        z.read_exact(&mut name)?;
        let n_rows = read_varuint_r(&mut z)?;
        let is_account = name == b"account";
        // `account` is the first table in Leap/Spring's fixed delta order, so if the first present
        // table isn't `account`, this block carries no setabi — stop before inflating any further
        // (the dense contract_row table that typically follows is never decompressed or walked).
        if !is_account && ti == 0 {
            return Ok(());
        }
        for _ in 0..n_rows {
            let mut present = [0u8; 1];
            z.read_exact(&mut present)?;
            let data_len = read_varuint_r(&mut z)?;
            if is_account {
                let mut data = vec![0u8; data_len as usize];
                z.read_exact(&mut data)?;
                match account_setabi(abieos, &data) {
                    Ok(Some((acct, abi_hex))) => {
                        let _ = sink.send(build_abi_doc(abieos, &acct, block_num, &abi_hex));
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("[disk] block {block_num}: account row: {e}"),
                }
            } else {
                // skip this row's data without materialising it
                io::copy(&mut z.by_ref().take(data_len), &mut io::sink())?;
            }
        }
        if is_account {
            return Ok(()); // account table done — caller seeks past the rest of the delta
        }
    }
    Ok(())
}

/// Scan one contiguous block chunk: seek to the chunk's start offset (via the index),
/// then read entries sequentially. Found ABI docs are streamed out over `sink`.
/// Per-block/row errors are logged and skipped — a single bad entry never aborts the scan.
#[allow(clippy::too_many_arguments)]
fn worker_scan(
    log_path: &str,
    idx_path: &str,
    first_block: u32,
    cs: u32,
    ce: u32,
    log_len: u64,
    stream_threshold: u64,
    sink: &std::sync::mpsc::Sender<String>,
    scanned: &AtomicU64,
) -> Result<()> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let abieos = Abieos::new();
    let mut idx = File::open(idx_path)?;
    idx.seek(SeekFrom::Start((cs - first_block) as u64 * 8))?;
    let mut ob = [0u8; 8];
    idx.read_exact(&mut ob)?;
    let mut pos = u64::from_le_bytes(ob); // byte offset of the current entry
                                          // 8 MiB buffer to cut read syscalls and feed the filesystem prefetcher on a cold, I/O-bound log.
    let mut log = BufReader::with_capacity(8 << 20, File::open(log_path)?);
    log.seek(SeekFrom::Start(pos))?;

    let mut hdr = [0u8; 48];
    loop {
        if log.read_exact(&mut hdr).is_err() {
            break; // clean EOF (or truncated tail) — stop this worker
        }
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num > ce {
            break;
        }
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        if payload_size > log_len {
            // a payload can't exceed the whole log — a wrong offset, not a real entry.
            return Err(anyhow!(
                "payload_size {payload_size} at block {block_num} exceeds log length {log_len}: format/offset error"
            ));
        }
        let entry_end = pos + 48 + payload_size;
        if payload_size >= stream_threshold {
            // Default path (stream_threshold = 0 -> every entry): stream-inflate only up to the
            // account table (the first table emitted), then seek past the rest. A block with no
            // setabi decompresses only its first table header instead of its whole delta — and a
            // snapshot init-delta still avoids a multi-GB read/allocation. Raise --stream-threshold
            // to fall back to whole-payload inflate (the branch below) for entries beneath it.
            if let Err(e) = scan_account_streaming_entry(&mut log, &abieos, block_num, sink) {
                eprintln!("[disk] block {block_num}: streaming scan: {e}");
            }
            log.seek(SeekFrom::Start(entry_end))?;
        } else {
            let mut payload = vec![0u8; payload_size as usize];
            log.read_exact(&mut payload)?;
            match decode_payload(&payload) {
                Ok(d) if !d.is_empty() => {
                    let walk = for_each_account_row(&d, |row| {
                        match account_setabi(&abieos, row) {
                            Ok(Some((name, abi_hex))) => {
                                let _ =
                                    sink.send(build_abi_doc(&abieos, &name, block_num, &abi_hex));
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!("[disk] block {block_num}: account row: {e}"),
                        }
                        Ok(())
                    });
                    if let Err(e) = walk {
                        eprintln!("[disk] block {block_num}: delta walk: {e}");
                    }
                }
                Ok(_) => {} // empty delta — nothing to do
                Err(e) => eprintln!("[disk] block {block_num}: inflate failed: {e}"),
            }
        }
        // Every entry is followed by an 8-byte position suffix == the entry's own start
        // offset — EXCEPT the snapshot init-delta entry, which omits it. (log is now at
        // entry_end in both paths.) Peek the next 8 bytes: if they equal `pos` it's the
        // suffix; otherwise they're the next entry's header, so rewind — keeping alignment.
        let mut suf = [0u8; 8];
        pos = if log.read_exact(&mut suf).is_ok() {
            if u64::from_le_bytes(suf) == pos {
                entry_end + 8
            } else {
                log.seek_relative(-8)?; // not a suffix — unread the peeked header bytes
                entry_end
            }
        } else {
            entry_end // EOF right after the payload; next header read will end the loop
        };
        scanned.fetch_add(1, Relaxed);
    }
    Ok(())
}

/// Read ABIs directly from the append-only state-history log (no nodeos), in parallel,
/// streaming results to `out`. Bounds the range to the indexed (committed) blocks so it
/// never races the entry nodeos is currently appending.
#[allow(clippy::too_many_arguments)]
pub fn scan_disk<W: Write + Send>(
    dir: &str,
    start: u32,
    end: u32,
    threads: u32,
    chunk_size: u64,
    stream_threshold: u64,
    checkpoint: Option<&str>,
    out: &mut W,
) -> Result<()> {
    use std::collections::BTreeSet;
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    let log_path = format!("{dir}/chain_state_history.log");
    let idx_path = format!("{dir}/chain_state_history.index");
    let log_len = std::fs::metadata(&log_path)
        .with_context(|| format!("open {log_path}"))?
        .len();
    let mut f = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut hdr = [0u8; 48];
    f.read_exact(&mut hdr).context("read first header")?;
    let first_block = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    // The index has one committed entry per block; clamp to it so we never read the live tail.
    let n_idx = (std::fs::metadata(&idx_path)?.len() / 8) as u32;
    // A normal log's first entry carries the ship magic. A log restored from a *snapshot*
    // opens with the full-state init-delta entry, which uses a distinct magic — but its
    // SECOND entry is a normal block with the ship magic. Accept either so snapshot-restored
    // logs aren't rejected outright.
    let recognised = is_ship_magic(u64::from_le_bytes(hdr[0..8].try_into().unwrap()))
        || (n_idx >= 2 && second_entry_is_ship(&log_path, &idx_path).unwrap_or(false));
    if !recognised {
        bail!("{log_path} is not a state-history log (bad ship magic)");
    }
    let last_block = first_block + n_idx.saturating_sub(1);
    let start = start.max(first_block);
    let end = end.min(last_block);
    if start > end {
        bail!("empty range after clamp to log [{first_block}..{last_block}]");
    }
    // Resume: a checkpoint records the block up to which the scan is contiguously done.
    // Re-running the same command continues from there (the output is opened in append mode
    // by the caller). Blocks above the checkpoint that were scanned before an interruption
    // get re-scanned — harmless, since abi-index docs are keyed by (block, account).
    let start = match checkpoint.and_then(read_checkpoint) {
        Some(resume) if resume > end as u64 => {
            eprintln!("[disk] checkpoint says [{start}..{end}] already complete — nothing to do");
            return Ok(());
        }
        Some(resume) => {
            let r = (resume as u32).max(start);
            eprintln!("[disk] resuming from checkpoint at block {r}");
            r
        }
        None => start,
    };
    let threads = threads.max(1);
    let total = (end - start + 1) as u64;
    // Dynamic work-stealing: hand out contiguous chunks from a shared cursor rather than one
    // fixed 1/threads slice each. Real chains get much denser (and their reads colder, off the
    // ARC cache) toward the head, so static slices finish wildly out of step — the light early
    // slices drain and leave the dense tail to a shrinking set of threads, idling cores.
    // Chunk size is a tuning knob (`--chunk-size`). Counter-intuitively, *smaller* chunks are
    // faster on a cold, I/O-bound full-chain scan: they keep the N threads' read cursors
    // clustered close together so they share filesystem prefetch/cache locality. Large,
    // widely-spread chunks scatter the streams and seek more; too many threads thrash a shared
    // array. Measured on WAX over a ZFS NVMe array, ~8 threads with ~20k-block chunks peaked
    // (4 or 16 threads, and 250k chunks, were all markedly slower).
    let chunk = chunk_size.max(1);
    let cursor = Arc::new(AtomicU64::new(start as u64));
    // Completed chunk start-offsets, for computing the contiguous resume watermark.
    let completed = Arc::new(Mutex::new(BTreeSet::<u64>::new()));
    eprintln!("[disk] log [{first_block}..{last_block}]; scanning [{start}..{end}] ({total} blocks) with {threads} threads");
    let t0 = Instant::now();

    let scanned = Arc::new(AtomicU64::new(0));
    let found = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();

    std::thread::scope(|s| {
        // single writer — streams docs to `out` as they're found
        let found_w = found.clone();
        s.spawn(move || {
            for line in rx {
                let _ = writeln!(out, "{line}");
                found_w.fetch_add(1, Relaxed);
            }
            let _ = out.flush();
        });
        // progress monitor + checkpoint writer
        let (scanned_m, found_m, done_m, completed_m) = (
            scanned.clone(),
            found.clone(),
            done.clone(),
            completed.clone(),
        );
        let ckpt = checkpoint.map(String::from);
        s.spawn(move || {
            let (mut last, mut last_t) = (0u64, Instant::now());
            let mut wm = start as u64; // contiguous watermark: blocks [start..wm-1] are done
            while !done_m.load(Relaxed) {
                std::thread::sleep(Duration::from_secs(3));
                let sc = scanned_m.load(Relaxed);
                let rate = (sc - last) as f64 / last_t.elapsed().as_secs_f64().max(1e-9);
                // advance the contiguous watermark (the absolute block we're confirmed through)
                {
                    let c = completed_m.lock().unwrap();
                    while c.contains(&wm) {
                        wm += chunk;
                    }
                }
                let at_block = wm.min(end as u64 + 1); // next absolute block to confirm
                eprintln!(
                    "[disk] block {at_block}/{end}  |  {sc}/{total} this run ({:.1}%)  {} ABIs  {rate:.0} blk/s",
                    sc as f64 / total as f64 * 100.0,
                    found_m.load(Relaxed)
                );
                (last, last_t) = (sc, Instant::now());
                if let Some(cp) = &ckpt {
                    // cap at end+1: the final chunk is clamped to `end`, so the watermark
                    // must not advance past it (else a later resume with a grown chain skips
                    // the [end..chunk_boundary] tail).
                    write_checkpoint(cp, at_block);
                }
            }
        });
        // workers — each pulls the next chunk from the shared cursor until the range is done
        let mut handles = Vec::new();
        for i in 0..threads {
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (txc, scc, cur, cmp) = (
                tx.clone(),
                scanned.clone(),
                cursor.clone(),
                completed.clone(),
            );
            handles.push(s.spawn(move || loop {
                let cs64 = cur.fetch_add(chunk, Relaxed);
                if cs64 > end as u64 {
                    break;
                }
                let cs = cs64 as u32;
                let ce = (cs64 + chunk - 1).min(end as u64) as u32;
                match worker_scan(
                    &lp,
                    &ip,
                    first_block,
                    cs,
                    ce,
                    log_len,
                    stream_threshold,
                    &txc,
                    &scc,
                ) {
                    // mark the chunk done only on success, so a failed chunk is re-scanned on resume
                    Ok(()) => {
                        cmp.lock().unwrap().insert(cs64);
                    }
                    Err(e) => eprintln!("[disk] worker {i} [{cs}..{ce}] FAILED: {e:#}"),
                }
            }));
        }
        drop(tx); // so rx closes once all worker tx clones drop
        for h in handles {
            let _ = h.join();
        }
        done.store(true, Relaxed); // stop the monitor; writer ends when rx drains
    });

    // Final watermark: a clean run pushes it past `end`, so re-running is a no-op; if any
    // chunk failed, it stops at that chunk so the re-run picks up exactly the gap.
    if let Some(cp) = checkpoint {
        let mut wm = start as u64;
        let c = completed.lock().unwrap();
        while c.contains(&wm) {
            wm += chunk;
        }
        drop(c);
        write_checkpoint(cp, wm.min(end as u64 + 1));
    }

    let secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "[disk] done: {} blocks in {secs:.1}s ({:.0} blk/s), {} ABI versions",
        scanned.load(Relaxed),
        scanned.load(Relaxed) as f64 / secs.max(1e-9),
        found.load(Relaxed)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ship_magic() {
        assert!(is_ship_magic(0xc35d_5000_0000_0000)); // version 0 (WAX)
        assert!(is_ship_magic(0xc35d_5000_0000_0001)); // same name, version 1
        assert!(!is_ship_magic(0));
    }

    #[test]
    fn payload_both_encodings() {
        use std::io::Write as _;
        let original = b"raw table_delta[] bytes here";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(original).unwrap();
        let z = enc.finish().unwrap();

        // default: [u32 (ignored)][zlib]
        let mut def = (z.len() as u32).to_le_bytes().to_vec();
        def.extend_from_slice(&z);
        assert_eq!(decode_payload(&def).unwrap(), original);

        // s==1: [u32=1][u64 decompressed_size][zlib]
        let mut s1 = 1u32.to_le_bytes().to_vec();
        s1.extend_from_slice(&(original.len() as u64).to_le_bytes());
        s1.extend_from_slice(&z);
        assert_eq!(decode_payload(&s1).unwrap(), original);

        assert!(decode_payload(&[1, 2]).unwrap().is_empty()); // too short
    }

    /// Build a synthetic 2-entry state-history log in `dir`: a snapshot-style init entry
    /// (non-ship magic, NO trailing suffix) at block 10, then a normal ship block at 11.
    fn write_synth_log(dir: &std::path::Path) {
        use std::io::Write as _;
        fn entry(magic: u64, block_num: u32, payload: &[u8]) -> Vec<u8> {
            let mut e = magic.to_le_bytes().to_vec();
            e.extend_from_slice(&block_num.to_be_bytes()); // first 4 bytes of block_id
            e.extend_from_slice(&[0u8; 28]); // rest of the 32-byte block_id
            e.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            e.extend_from_slice(payload);
            e
        }
        let payload = [0u8; 4]; // decode_payload -> empty (block skipped, no ABI)
        let mut log = Vec::new();
        log.extend_from_slice(&entry(0x0000_0000_dead_beef, 10, &payload)); // init: NO suffix
        let e2 = log.len() as u64;
        log.extend_from_slice(&entry(0xc35d_5000_0000_0000, 11, &payload)); // normal block
        log.extend_from_slice(&e2.to_le_bytes()); // its suffix == its own start offset
        let mut index = 0u64.to_le_bytes().to_vec();
        index.extend_from_slice(&e2.to_le_bytes());

        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        std::fs::File::create(dir.join("chain_state_history.log"))
            .unwrap()
            .write_all(&log)
            .unwrap();
        std::fs::File::create(dir.join("chain_state_history.index"))
            .unwrap()
            .write_all(&index)
            .unwrap();
    }

    /// A snapshot-restored log opens with a non-ship-magic init-delta entry that omits the
    /// trailing position suffix; the second entry is a normal ship block. scan_disk must
    /// accept it (via the 2nd-entry magic) and stay byte-aligned across the suffix-less entry.
    #[test]
    fn snapshot_log_is_accepted_and_aligned() {
        let dir = std::env::temp_dir().join(format!("abi-scanner-snap-{}", std::process::id()));
        write_synth_log(&dir);
        let mut out = Vec::new();
        let r = scan_disk(
            dir.to_str().unwrap(),
            10,
            11,
            1,
            20_000,
            16 << 20,
            None,
            &mut out,
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok(), "snapshot-restored log should be accepted: {r:?}");
        assert!(out.is_empty(), "empty payloads -> no ABIs emitted");
    }

    /// A checkpoint records the contiguous-done watermark; once the scan completes it sits
    /// past `end`, so re-running the same command with that checkpoint is a clean no-op.
    #[test]
    fn checkpoint_makes_completed_scan_a_noop() {
        let dir = std::env::temp_dir().join(format!("abi-scanner-ckpt-{}", std::process::id()));
        write_synth_log(&dir);
        let ckpt = dir.join("scan.ckpt");
        let cp = ckpt.to_str().unwrap();

        // first run: scans [10..11] and writes a watermark past the end
        let mut out1 = Vec::new();
        scan_disk(
            dir.to_str().unwrap(),
            10,
            11,
            1,
            20_000,
            16 << 20,
            Some(cp),
            &mut out1,
        )
        .unwrap();
        let wm: u64 = std::fs::read_to_string(&ckpt)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wm > 11,
            "watermark should be past end after a complete scan, got {wm}"
        );

        // second run: checkpoint says complete -> no work, returns Ok with no output
        let mut out2 = Vec::new();
        let r = scan_disk(
            dir.to_str().unwrap(),
            10,
            11,
            1,
            20_000,
            16 << 20,
            Some(cp),
            &mut out2,
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok());
        assert!(out2.is_empty(), "resume of a complete scan emits nothing");
    }

    /// The streaming early-exit path must extract the same setabi rows as the whole-payload
    /// path: build a zlib delta whose first table is `account` with one setabi row, and verify
    /// scan_account_streaming_entry emits it (and stops without needing the rest of the stream).
    #[test]
    fn streaming_entry_extracts_account_setabi() {
        use std::io::Write as _;
        // one account row: [variant 0][name "eosio"][creation_date][abi: len 2, bytes 0e 65]
        let mut row = vec![0x00];
        row.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0xea, 0x30, 0x55]); // "eosio"
        row.extend_from_slice(&[0x80, 0xd6, 0x14, 0x49]); // creation_date
        row.extend_from_slice(&[0x02, 0x0e, 0x65]); // abi: varuint len 2, bytes 0e 65
                                                    // table_delta[]: n_tables=1, table0 = account with 1 row
        let mut deltas = vec![0x01]; // n_tables = 1
        deltas.push(0x00); // variant
        deltas.push(0x07); // name_len = 7
        deltas.extend_from_slice(b"account");
        deltas.push(0x01); // n_rows = 1
        deltas.push(0x01); // present = 1
        deltas.push(row.len() as u8); // data_len (varuint, < 128)
        deltas.extend_from_slice(&row);
        // a trailing non-account table the stream should never need to read
        deltas.extend_from_slice(&[0x00, 0x09]); // (garbage: proves we stop after `account`)

        // compress and frame as a payload: [u32 s=0][zlib]
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&deltas).unwrap();
        let z = enc.finish().unwrap();
        let mut payload = 0u32.to_le_bytes().to_vec(); // s = 0
        payload.extend_from_slice(&z);

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let mut cur = std::io::Cursor::new(payload);
        scan_account_streaming_entry(&mut cur, &Abieos::new(), 49, &tx).unwrap();
        drop(tx);
        let docs: Vec<String> = rx.iter().collect();
        assert_eq!(docs.len(), 1, "exactly one setabi extracted");
        let v: serde_json::Value = serde_json::from_str(&docs[0]).unwrap();
        assert_eq!(v["account"], "eosio");
        assert_eq!(v["block"], 49);
        assert_eq!(v["abi_hex"], "0e65");
    }
}
