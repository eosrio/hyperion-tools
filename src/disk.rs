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
    let mut log = BufReader::with_capacity(1 << 20, File::open(log_path)?);
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
        let mut payload = vec![0u8; payload_size as usize];
        log.read_exact(&mut payload)?;
        // Every entry is followed by an 8-byte position suffix == the entry's own start
        // offset — EXCEPT the snapshot init-delta entry, which omits it. Peek the next 8
        // bytes: if they equal `pos`, it's the suffix (consume it); otherwise they're the
        // next entry's header, so rewind. This keeps us aligned across the init entry.
        let entry_end = pos + 48 + payload_size;
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

        let deltas = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("[disk] block {block_num}: inflate failed: {e}");
                continue;
            }
        };
        let walk = for_each_account_row(&deltas, |row| {
            match account_setabi(&abieos, row) {
                Ok(Some((name, abi_hex))) => {
                    let _ = sink.send(build_abi_doc(&abieos, &name, block_num, &abi_hex));
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
    Ok(())
}

/// Read ABIs directly from the append-only state-history log (no nodeos), in parallel,
/// streaming results to `out`. Bounds the range to the indexed (committed) blocks so it
/// never races the entry nodeos is currently appending.
pub fn scan_disk<W: Write + Send>(
    dir: &str,
    start: u32,
    end: u32,
    threads: u32,
    out: &mut W,
) -> Result<()> {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};
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
    let threads = threads.max(1);
    let total = (end - start + 1) as u64;
    // Dynamic work-stealing: hand out many small contiguous chunks from a shared cursor
    // rather than one fixed 1/threads slice each. Real chains get much denser (and their
    // reads colder, off the ARC cache) toward the head, so static slices finish wildly out
    // of step — the light early slices drain and leave the dense tail to a shrinking set of
    // threads, idling cores. Small chunks keep every thread busy right to the end.
    const CHUNK: u64 = 20_000;
    let cursor = Arc::new(AtomicU64::new(start as u64));
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
        // progress monitor
        let (scanned_m, found_m, done_m) = (scanned.clone(), found.clone(), done.clone());
        s.spawn(move || {
            let (mut last, mut last_t) = (0u64, Instant::now());
            while !done_m.load(Relaxed) {
                std::thread::sleep(Duration::from_secs(3));
                let sc = scanned_m.load(Relaxed);
                let rate = (sc - last) as f64 / last_t.elapsed().as_secs_f64().max(1e-9);
                eprintln!(
                    "[disk] {sc}/{total} ({:.1}%)  {} ABIs  {rate:.0} blk/s",
                    sc as f64 / total as f64 * 100.0,
                    found_m.load(Relaxed)
                );
                (last, last_t) = (sc, Instant::now());
            }
        });
        // workers — each pulls the next chunk from the shared cursor until the range is done
        let mut handles = Vec::new();
        for i in 0..threads {
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (txc, scc, cur) = (tx.clone(), scanned.clone(), cursor.clone());
            handles.push(s.spawn(move || loop {
                let cs64 = cur.fetch_add(CHUNK, Relaxed);
                if cs64 > end as u64 {
                    break;
                }
                let cs = cs64 as u32;
                let ce = (cs64 + CHUNK - 1).min(end as u64) as u32;
                if let Err(e) = worker_scan(&lp, &ip, first_block, cs, ce, log_len, &txc, &scc) {
                    eprintln!("[disk] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx); // so rx closes once all worker tx clones drop
        for h in handles {
            let _ = h.join();
        }
        done.store(true, Relaxed); // stop the monitor; writer ends when rx drains
    });

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

    /// A snapshot-restored log opens with a non-ship-magic init-delta entry that
    /// omits the trailing position suffix; the second entry is a normal ship block.
    /// scan_disk must accept it (via the 2nd-entry magic) and stay byte-aligned
    /// across the suffix-less first entry.
    #[test]
    fn snapshot_log_is_accepted_and_aligned() {
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

        let dir = std::env::temp_dir().join(format!("abi-scanner-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("chain_state_history.log"))
            .unwrap()
            .write_all(&log)
            .unwrap();
        std::fs::File::create(dir.join("chain_state_history.index"))
            .unwrap()
            .write_all(&index)
            .unwrap();

        let mut out = Vec::new();
        let r = scan_disk(dir.to_str().unwrap(), 10, 11, 1, &mut out);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_ok(), "snapshot-restored log should be accepted: {r:?}");
        assert!(out.is_empty(), "empty payloads -> no ABIs emitted");
    }
}
