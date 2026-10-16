//! Controlled replay: slice inputs into a NEW directory, never let nodeos open original logs.
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, IntoInnerError, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::format::{Blocks, Kind, Log, encode_container, u64le};
use crate::io::{Disk, create_dir, create_file, hash, sync_dir};
use crate::repair::{LogFile, Sources, rebuild, target_flags};
use crate::report::{Out, arg};
use crate::target::{Extent, Target};

pub struct ReplayOptions<'a> {
    pub nodeos: &'a Path,
    pub snapshot: &'a Path,
    /// Operator-supplied head; nodeos independently verifies snapshot contents.
    pub snapshot_block: u32,
    pub blocks_dir: &'a Path,
    pub out: &'a Path,
    pub debug_traces: bool,
    pub state_db_size_mb: u32,
}

pub fn run(log: &Log, t: &Target, o: ReplayOptions<'_>) -> Result<Out> {
    ensure!(
        o.state_db_size_mb >= 256,
        "replay state database must be at least 256 MiB"
    );
    ensure!(
        log.kind != Kind::Blocks,
        "replay-ship requires a SHIP target"
    );
    // One replay covers every region: from the snapshot through the last region's anchor.
    let (first, last) = (t.first_block(), t.last_block());
    ensure!(
        o.snapshot_block > 0 && o.snapshot_block <= first - 2,
        "snapshot head must be at or before {}; the first replayed block may emit a full-state \
         delta and must precede the damaged entries",
        first - 2
    );
    let nodeos = o
        .nodeos
        .canonicalize()
        .context("resolve nodeos executable")?;
    let snapshot = o
        .snapshot
        .canonicalize()
        .context("resolve uncompressed portable snapshot")?;
    let snap = Disk::open(&snapshot, false)?;
    ensure!(
        snap.read(0, 8)? == [0x50, 0x05, 0x51, 0x30, 1, 0, 0, 0],
        "expected an uncompressed portable Antelope snapshot (magic 0x30510550, file format 1)"
    );
    let blocks = Log::open(&o.blocks_dir.join("blocks.log"), None, false)?;
    ensure!(
        blocks.kind == Kind::Blocks && blocks.first <= o.snapshot_block + 1 && blocks.last > last,
        "blocklog must cover snapshot+1 through block {}",
        last + 1
    );
    // Anchor chain compatibility is checked BEFORE copying anything.
    for r in &t.regions {
        let (left, right) = r.source_range();
        for n in [left, right] {
            ensure!(
                blocks.id(n, &blocks.body(n)?)? == log.ship_entry_id(n)?,
                "replay blocklog belongs to a different chain/fork at anchor {n}"
            );
        }
    }
    fs2::FileExt::try_lock_shared(&blocks.log.file).context("lock replay blocks.log")?;
    fs2::FileExt::try_lock_shared(&blocks.index.file).context("lock replay blocks.index")?;
    let before = (blocks.log.stamp()?, blocks.index.stamp()?);
    create_dir(o.out)?;
    let out = o.out.canonicalize()?;
    let data = out.join("data");
    let config = out.join("config");
    let sliced_blocks = data.join("blocks");
    fs::create_dir(&data)?;
    fs::create_dir(&config)?;
    fs::create_dir(&sliced_blocks)?;
    create_file(
        &config.join("config.ini"),
        b"# Isolated repair replay. No peers, producer, or HTTP plugins.\n",
    )?;
    // Include the snapshot head itself when present; nodeos replays only the blocks after it.
    let (slice_first, slice_last) = (o.snapshot_block.max(blocks.first), last + 1);
    eprintln!(
        "Writing blocks {slice_first}..{slice_last} of the intact blocklog into {}. \
         Replay does not open original log paths.",
        sliced_blocks.display()
    );
    write_slice(&blocks, slice_first, slice_last, &sliced_blocks)?;
    ensure!(
        (blocks.log.stamp()?, blocks.index.stamp()?) == before,
        "replay input changed during copy; stop its node and use a new output directory"
    );
    let history = data.join("state-history");
    let mut args: Vec<String> = vec![
        "--data-dir".into(),
        path_text(&data)?,
        "--config-dir".into(),
        path_text(&config)?,
        "--snapshot".into(),
        path_text(&snapshot)?,
        "--blocks-dir".into(),
        path_text(&sliced_blocks)?,
        "--plugin".into(),
        "eosio::state_history_plugin".into(),
        "--p2p-listen-endpoint".into(),
        "".into(),
        "--http-server-address".into(),
        "".into(),
        "--trace-history".into(),
        "--chain-state-history".into(),
        "--disable-replay-opts".into(),
        "--chain-state-db-size-mb".into(),
        o.state_db_size_mb.to_string(),
        "--state-history-dir".into(),
        path_text(&history)?,
        "--state-history-endpoint".into(),
        "127.0.0.1:0".into(),
        "--terminate-at-block".into(),
        (last + 1).to_string(),
    ];
    if o.debug_traces {
        args.push("--trace-history-debug-mode".into());
    }
    let record = serde_json::json!({
        "executable": nodeos,
        "argv": args,
        "blocklog_slice": [slice_first, slice_last],
        "snapshot_head_operator_supplied": o.snapshot_block,
        "snapshot_head_verified_by_tool": false,
        "requirements": "Use the same nodeos version, consensus upgrades, and trace debug mode \
            as the original. Snapshot contents and chain are checked by nodeos; resulting source \
            ids, lengths and readable boundary bytes are checked by build.",
    });
    create_file(
        &out.join("replay.json"),
        &serde_json::to_vec_pretty(&record)?,
    )?;
    let stdout = new_file(&out.join("nodeos.stdout.log"))?;
    let stderr = new_file(&out.join("nodeos.stderr.log"))?;
    eprintln!(
        "Running isolated replay; follow nodeos.stderr.log. Stop the child with your \
         service/process manager if needed; target remains untouched."
    );
    let status = Command::new(&nodeos)
        .args(&args)
        .current_dir(&out)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .context("start isolated nodeos replay")?;
    ensure!(
        status.success(),
        "nodeos replay failed ({status}); see nodeos.stderr.log; target is untouched"
    );
    let donor = format!("data/state-history/{}.log", log.kind.stem());
    let regenerated = Log::open(&out.join(&donor), None, false)?;
    ensure!(
        regenerated.first < first && regenerated.last > last,
        "replay did not produce the required history range"
    );
    // One P2PBLK01 container per region, cut from the slice nodeos just replayed.
    let sliced = Log::open(&sliced_blocks.join("blocks.log"), None, false)?;
    let mut relative = Sources {
        donors: vec![LogFile {
            log: donor.into(),
            index: None,
        }],
        ..Sources::default()
    };
    let mut files = Vec::new();
    for r in &t.regions {
        let (left, right) = r.source_range();
        let mut source = Blocks::new();
        for n in left..=right {
            source.insert(n, sliced.body(n)?);
        }
        let container = encode_container(&source)?;
        let name = format!("blocks-{left}-{right}.p2pblk");
        create_file(&out.join(&name), &container)?;
        relative.blocks.push(name.into());
        files.push(json!({"range": [left, right], "blocks_sha256": hash(&container)}));
    }
    relative
        .under(&out)
        .load(log.kind)
        .and_then(|loaded| rebuild(log, t, &loaded))
        .context(
            "replay completed but bytes do not fit the target; use the original nodeos \
             version/configuration or a matching archive; no target writes performed",
        )?;
    let report = json!({
        "ok": true,
        "source": "replay",
        "replay": record,
        "regions": files,
        "sources": relative,
        "target_compatible": true,
    });
    create_file(
        &out.join("source.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    let extents: Vec<Extent> = t.extents().copied().collect();
    let text = format!(
        "Replayed blocks {}..{} from the snapshot in {}.\n\
         The regenerated history fits the target's lengths and readable bytes.\n\nNext:\n  \
         antelope-log-repair build {} --sources {} --out BUNDLE\n",
        o.snapshot_block + 1,
        last + 1,
        out.display(),
        target_flags(log, t.record_size, &extents),
        arg(out.to_string_lossy()),
    );
    Ok(Out { json: report, text })
}

/// Write a nodeos-readable v3 `blocks.log` + `blocks.index` holding `first..=last`, with
/// trailers and index rebased. The header carries the chain ID: copied from a sliced source,
/// or, for a source starting at genesis, the SHA-256 of its packed genesis state. nodeos
/// checks that ID against the snapshot before replaying.
pub fn write_slice(src: &Log, first: u32, last: u32, dir: &Path) -> Result<()> {
    ensure!(
        src.kind == Kind::Blocks && src.first <= first && first <= last && last <= src.last,
        "blocklog slice {first}..{last} is outside {}..{}",
        src.first,
        src.last
    );
    let first_pos = src.pos(src.first)?;
    let chain_id: [u8; 32] = if src.first == 1 {
        Sha256::digest(src.log.read(8, first_pos - 16)?).into()
    } else {
        ensure!(first_pos == 48, "unexpected blocks.log v3 header length");
        src.log.read(8, 32)?.try_into().unwrap()
    };
    let mut log = BufWriter::new(new_file(&dir.join("blocks.log"))?);
    let mut index = BufWriter::new(new_file(&dir.join("blocks.index"))?);
    log.write_all(&3u32.to_le_bytes())?;
    log.write_all(&first.to_le_bytes())?;
    log.write_all(&chain_id)?;
    log.write_all(&[255; 8])?;
    let mut pos = 48u64;
    for n in first..=last {
        let (a, b) = src.bounds(n)?;
        let entry = src.log.read(a, b - a)?;
        let body = entry.len() - 8;
        ensure!(
            u64le(&entry[body..]) == a,
            "position trailer mismatch at block {n}"
        );
        log.write_all(&entry[..body])?;
        log.write_all(&pos.to_le_bytes())?;
        index.write_all(&pos.to_le_bytes())?;
        pos += entry.len() as u64;
    }
    for w in [log, index] {
        w.into_inner()
            .map_err(IntoInnerError::into_error)?
            .sync_all()?;
    }
    sync_dir(dir)
}

fn new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create new {}", path.display()))
}

fn path_text(path: &Path) -> Result<String> {
    // Canonical Windows verbatim paths work in std APIs but not every nodeos build.
    Ok(path
        .to_str()
        .context("nodeos path must be UTF-8")?
        .to_string())
}
