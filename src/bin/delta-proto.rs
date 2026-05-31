//! delta-proto — PROTOTYPE: a direct-from-disk Hyperion *delta* indexer.
//!
//! Reads `chain_state_history` off disk in parallel (no nodeos, no SHiP), walks the
//! `contract_row` table deltas, decodes each row's `value` against the contract ABI that
//! was active *at that block* (looked up in an abi-scanner ABI index), and emits Hyperion
//! `<chain>-delta-v1`-shaped NDJSON. It exists to prove the engine generalises far beyond
//! ABI extraction — the `account` table was just table 0 of ~19. Throughput + memory are
//! the point; this is a prototype, not the shipped tool.
//!
//! Decode path (per rs_abieos): set_abi_hex_native(code, hex) -> get_type_for_table_native
//! (code, table) -> bin_to_json(code, type, value). One ABI per account per context
//! (insert-no-overwrite), so a version change is delete_contract_native + set_abi_hex_native.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rs_abieos::Abieos;

use abi_scanner::delta::read_varuint;
use abi_scanner::disk::{decode_payload, is_ship_magic};

#[derive(Parser, Debug)]
#[command(about = "PROTOTYPE: decode contract_row deltas directly from the state-history log.")]
struct Args {
    /// nodeos state-history dir (chain_state_history.{log,index})
    #[arg(long)]
    from_disk: String,
    /// abi-index NDJSON produced by abi-scanner ({account, block, abi_hex, ...})
    #[arg(long)]
    abi_index: String,
    #[arg(long)]
    start: u32,
    #[arg(long)]
    end: u32,
    #[arg(long, default_value_t = 8)]
    threads: u32,
    /// output NDJSON (omit to measure pure decode throughput)
    #[arg(long)]
    out: Option<String>,
}

/// account name -> versions sorted by the block the ABI took effect (valid_from).
type AbiIndex = HashMap<String, Vec<(u32, String)>>;

fn load_abi_index(path: &str) -> Result<AbiIndex> {
    let f = BufReader::new(File::open(path).with_context(|| format!("open {path}"))?);
    let mut idx: AbiIndex = HashMap::new();
    let mut skipped = 0u64;
    for line in f.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        // tolerate the odd malformed line (e.g. a resume append-boundary) instead of aborting
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            skipped += 1;
            continue;
        };
        let (Some(acct), Some(block), Some(hex)) = (
            v.get("account").and_then(|x| x.as_str()),
            v.get("block").and_then(|x| x.as_u64()),
            v.get("abi_hex").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        if hex.is_empty() {
            continue;
        }
        idx.entry(acct.to_string())
            .or_default()
            .push((block as u32, hex.to_string()));
    }
    for versions in idx.values_mut() {
        versions.sort_by_key(|(b, _)| *b);
    }
    if skipped > 0 {
        eprintln!("[delta-proto] skipped {skipped} malformed ABI-index line(s)");
    }
    Ok(idx)
}

/// Walk every table_delta row, calling `f(table_name, present, row_data)`.
fn for_each_row<F: FnMut(&[u8], u8, &[u8]) -> Result<()>>(deltas: &[u8], mut f: F) -> Result<()> {
    let mut off = 0usize;
    let (n_tables, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad table count"))?;
    off += k;
    for _ in 0..n_tables {
        let (_var, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad variant"))?;
        off += k;
        let (name_len, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad name len"))?;
        off += k;
        let name = deltas
            .get(off..off + name_len)
            .ok_or_else(|| anyhow!("name oob"))?;
        off += name_len;
        let (rows, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad rows count"))?;
        off += k;
        for _ in 0..rows {
            let present = *deltas.get(off).ok_or_else(|| anyhow!("present oob"))?;
            off += 1;
            let (dlen, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad data len"))?;
            off += k;
            let data = deltas.get(off..off + dlen).ok_or_else(|| anyhow!("data oob"))?;
            f(name, present, data)?;
            off += dlen;
        }
    }
    Ok(())
}

struct ContractRow<'a> {
    code: u64,
    scope: u64,
    table: u64,
    primary_key: u64,
    payer: u64,
    value: &'a [u8],
}

/// Parse a `contract_row_v0`: [varuint 0][code u64][scope u64][table u64][pk u64][payer u64][bytes value].
fn parse_contract_row(row: &[u8]) -> Option<ContractRow<'_>> {
    let (_v, k) = read_varuint(row)?;
    let mut off = k;
    let rd = |off: &mut usize| -> Option<u64> {
        let n = u64::from_le_bytes(row.get(*off..*off + 8)?.try_into().ok()?);
        *off += 8;
        Some(n)
    };
    let code = rd(&mut off)?;
    let scope = rd(&mut off)?;
    let table = rd(&mut off)?;
    let primary_key = rd(&mut off)?;
    let payer = rd(&mut off)?;
    let (vlen, k) = read_varuint(&row[off..])?;
    off += k;
    let value = row.get(off..off + vlen)?;
    Some(ContractRow {
        code,
        scope,
        table,
        primary_key,
        payer,
        value,
    })
}

#[derive(Default)]
struct Stats {
    blocks: AtomicU64,
    rows: AtomicU64,   // contract_row present==1 seen
    decoded: AtomicU64, // value -> JSON ok
    no_abi: AtomicU64, // no ABI version for (code, block)
    failed: AtomicU64, // ABI present but decode failed
}

/// Ensure the abieos context holds the ABI version of `code` active at `block`.
/// Returns false if no version is known for this contract at/before `block`.
fn ensure_abi(
    abieos: &Abieos,
    loaded: &mut HashMap<u64, u32>,
    idx: &AbiIndex,
    code: u64,
    code_str: &str,
    block: u32,
) -> bool {
    let Some(versions) = idx.get(code_str) else {
        return false;
    };
    // greatest valid_from <= block
    let pos = versions.partition_point(|(vf, _)| *vf <= block);
    if pos == 0 {
        return false;
    }
    let (valid_from, abi_hex) = &versions[pos - 1];
    if loaded.get(&code) != Some(valid_from) {
        let _ = abieos.delete_contract_native(code); // no-op if absent
        if abieos.set_abi_hex_native(code, abi_hex).is_err() {
            return false;
        }
        loaded.insert(code, *valid_from);
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn worker(
    log_path: &str,
    idx_path: &str,
    first_block: u32,
    cs: u32,
    ce: u32,
    abi_index: &AbiIndex,
    stats: &Stats,
    sink: Option<&mpsc::Sender<String>>,
) -> Result<()> {
    let abieos = Abieos::new();
    let mut loaded: HashMap<u64, u32> = HashMap::new();
    // resolved struct type cache, keyed by (loaded_valid_from, table) so it invalidates on ABI change
    let mut type_cache: HashMap<(u32, u64), String> = HashMap::new();

    let mut idx = File::open(idx_path)?;
    idx.seek(SeekFrom::Start((cs - first_block) as u64 * 8))?;
    let mut ob = [0u8; 8];
    idx.read_exact(&mut ob)?;
    let mut pos = u64::from_le_bytes(ob);
    let mut log = BufReader::with_capacity(8 << 20, File::open(log_path)?);
    log.seek(SeekFrom::Start(pos))?;

    let mut hdr = [0u8; 48];
    loop {
        if log.read_exact(&mut hdr).is_err() {
            break;
        }
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num > ce {
            break;
        }
        let block_id = hex::encode(&hdr[8..40]);
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        let mut payload = vec![0u8; payload_size as usize];
        log.read_exact(&mut payload)?;
        // trailing 8-byte position suffix (present on normal genesis-synced entries)
        let entry_end = pos + 48 + payload_size;
        let mut suf = [0u8; 8];
        pos = if log.read_exact(&mut suf).is_ok() && u64::from_le_bytes(suf) == pos {
            entry_end + 8
        } else {
            log.seek_relative(-(suf.len() as i64)).ok();
            entry_end
        };
        stats.blocks.fetch_add(1, Relaxed);

        let deltas = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            _ => continue,
        };
        let _ = for_each_row(&deltas, |name, present, data| {
            if name != b"contract_row" || present != 1 {
                return Ok(());
            }
            let Some(r) = parse_contract_row(data) else {
                return Ok(());
            };
            stats.rows.fetch_add(1, Relaxed);
            let code_str = match abieos.name_to_string(r.code) {
                Ok(s) => s,
                Err(_) => return Ok(()),
            };
            if !ensure_abi(&abieos, &mut loaded, abi_index, r.code, &code_str, block_num) {
                stats.no_abi.fetch_add(1, Relaxed);
                return Ok(());
            }
            let vf = loaded[&r.code];
            let ttype = match type_cache.entry((vf, r.table)) {
                std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    match abieos.get_type_for_table_native(r.code, r.table) {
                        Ok(t) => e.insert(t).clone(),
                        Err(_) => {
                            stats.failed.fetch_add(1, Relaxed);
                            return Ok(());
                        }
                    }
                }
            };
            match abieos.bin_to_json(&code_str, &ttype, r.value) {
                Ok(json) => {
                    stats.decoded.fetch_add(1, Relaxed);
                    if let Some(tx) = sink {
                        let scope = abieos.name_to_string(r.scope).unwrap_or_default();
                        let table = abieos.name_to_string(r.table).unwrap_or_default();
                        let payer = abieos.name_to_string(r.payer).unwrap_or_default();
                        let doc = format!(
                            "{{\"present\":1,\"block_num\":{block_num},\"block_id\":\"{block_id}\",\"code\":\"{code_str}\",\"scope\":\"{scope}\",\"table\":\"{table}\",\"primary_key\":\"{}\",\"payer\":\"{payer}\",\"data\":{json}}}",
                            r.primary_key
                        );
                        let _ = tx.send(doc);
                    }
                }
                Err(_) => {
                    stats.failed.fetch_add(1, Relaxed);
                }
            }
            Ok(())
        });
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    eprintln!("[delta-proto] loading ABI index {} ...", args.abi_index);
    let abi_index = Arc::new(load_abi_index(&args.abi_index)?);
    eprintln!("[delta-proto] {} contracts in ABI index", abi_index.len());

    let log_path = format!("{}/chain_state_history.log", args.from_disk);
    let idx_path = format!("{}/chain_state_history.index", args.from_disk);
    let mut f = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut hdr = [0u8; 48];
    f.read_exact(&mut hdr).context("read first header")?;
    if !is_ship_magic(u64::from_le_bytes(hdr[0..8].try_into().unwrap())) {
        bail!("{log_path} is not a state-history log");
    }
    let first_block = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    let n_idx = (std::fs::metadata(&idx_path)?.len() / 8) as u32;
    let last_block = first_block + n_idx.saturating_sub(1);
    let start = args.start.max(first_block);
    let end = args.end.min(last_block);
    if start > end {
        bail!("empty range after clamp to [{first_block}..{last_block}]");
    }
    let threads = args.threads.max(1);
    let total = (end - start + 1) as u64;
    let chunk = total.div_ceil(threads as u64) as u32;
    eprintln!("[delta-proto] decoding contract_row deltas [{start}..{end}] ({total} blocks) with {threads} threads");

    let stats = Arc::new(Stats::default());
    let (tx, rx) = mpsc::channel::<String>();
    let mut out: Option<Box<dyn Write + Send>> = args
        .out
        .as_ref()
        .map(|p| -> Result<Box<dyn Write + Send>> {
            Ok(Box::new(BufWriter::new(File::create(p)?)))
        })
        .transpose()?;
    let emit = out.is_some();
    let t0 = Instant::now();

    std::thread::scope(|s| {
        let written = s.spawn(move || {
            let mut n = 0u64;
            if let Some(w) = out.as_mut() {
                for line in rx {
                    let _ = writeln!(w, "{line}");
                    n += 1;
                }
                let _ = w.flush();
            } else {
                for _ in rx {}
            }
            n
        });
        let mut handles = Vec::new();
        for i in 0..threads {
            let cs = start.saturating_add(i.saturating_mul(chunk));
            if cs > end {
                break;
            }
            let ce = ((cs as u64 + chunk as u64 - 1).min(end as u64)) as u32;
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (ai, st) = (abi_index.clone(), stats.clone());
            let txc = if emit { Some(tx.clone()) } else { None };
            handles.push(s.spawn(move || {
                if let Err(e) = worker(&lp, &ip, first_block, cs, ce, &ai, &st, txc.as_ref()) {
                    eprintln!("[delta-proto] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx);
        for h in handles {
            let _ = h.join();
        }
        let _ = written.join();
    });

    let secs = t0.elapsed().as_secs_f64();
    let b = stats.blocks.load(Relaxed);
    eprintln!(
        "[delta-proto] done: {b} blocks in {secs:.1}s ({:.0} blk/s) | contract_row present={} decoded={} no_abi={} failed={}",
        b as f64 / secs.max(1e-9),
        stats.rows.load(Relaxed),
        stats.decoded.load(Relaxed),
        stats.no_abi.load(Relaxed),
        stats.failed.load(Relaxed),
    );
    Ok(())
}
