//! Controlled replay: slice inputs into a NEW directory, never let nodeos open original logs.
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, IntoInnerError, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::format::{Blocks, Kind, Log, encode_container, u32le, u64le};
use crate::io::{Disk, Stamp, create_dir, create_file, hash, sync_dir};
use crate::repair::{LogFile, Sources, rebuild, target_flags};
use crate::report::{Out, arg};
use crate::target::{Extent, Target};

pub struct ReplayOptions<'a> {
    pub nodeos: &'a Path,
    pub snapshot: &'a Path,
    /// Operator-supplied head; nodeos independently verifies snapshot contents.
    pub snapshot_block: u32,
    /// Directories holding the intact block history: a `blocks.log` and/or retained
    /// `blocks-FIRST-LAST.log` files (`blocks-dir`, `blocks-retained-dir`, an archive).
    pub blocks_dirs: &'a [PathBuf],
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
    let blocks = BlockSet::open(o.blocks_dirs)?;
    // Include the snapshot head itself when present; nodeos replays only the blocks after it.
    let (slice_first, slice_last) = (o.snapshot_block.max(blocks.first()), last + 1);
    ensure!(
        slice_first <= o.snapshot_block + 1,
        "the block logs start at block {}, after the snapshot head {}; nodeos needs every block \
         after the snapshot",
        blocks.first(),
        o.snapshot_block
    );
    blocks.check_covers(slice_first, slice_last)?;
    // Anchor chain compatibility is checked BEFORE copying anything.
    for r in &t.regions {
        let (left, right) = r.source_range();
        for n in [left, right] {
            ensure!(
                blocks.id(n)? == log.ship_entry_id(n)?,
                "replay blocklog belongs to a different chain/fork at anchor {n}"
            );
        }
    }
    let before = blocks.lock_and_stamp(slice_first, slice_last)?;
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
    eprintln!(
        "Writing blocks {slice_first}..{slice_last} of the intact blocklog into {}. \
         Replay does not open original log paths.",
        sliced_blocks.display()
    );
    write_slice(&blocks, slice_first, slice_last, &sliced_blocks)?;
    ensure!(
        blocks.lock_and_stamp(slice_first, slice_last)? == before,
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

/// Intact block logs covering a range: `blocks.log` files and retained `blocks-A-B.log` range
/// files, from one or more directories. Every file must carry the same chain ID.
pub struct BlockSet {
    logs: Vec<Log>,
    chain_id: [u8; 32],
}

impl BlockSet {
    pub fn open(dirs: &[PathBuf]) -> Result<Self> {
        ensure!(!dirs.is_empty(), "replay needs --blocks-dir");
        let mut logs = Vec::new();
        for dir in dirs {
            let mut paths: Vec<PathBuf> = fs::read_dir(dir)
                .with_context(|| format!("read --blocks-dir {}", dir.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| is_block_log(p))
                .collect();
            paths.sort();
            for path in paths {
                let log = Log::open(&path, None, false)
                    .with_context(|| format!("open {}", path.display()))?;
                ensure!(
                    log.kind == Kind::Blocks,
                    "{} is not a block log",
                    path.display()
                );
                logs.push(log);
            }
        }
        ensure!(
            !logs.is_empty(),
            "no blocks.log or blocks-FIRST-LAST.log in the --blocks-dir directories"
        );
        logs.sort_by_key(|l| (l.first, l.last));
        Self::from_logs(logs)
    }

    pub fn from_logs(logs: Vec<Log>) -> Result<Self> {
        let mut chain_id = None;
        for log in &logs {
            let id = chain_id_of(log)?;
            ensure!(
                chain_id.is_none_or(|c| c == id),
                "{} belongs to another chain than the other block logs",
                log.log.path.display()
            );
            chain_id = Some(id);
        }
        Ok(Self {
            logs,
            chain_id: chain_id.context("no block logs")?,
        })
    }

    pub fn first(&self) -> u32 {
        self.logs.iter().map(|l| l.first).min().unwrap_or(u32::MAX)
    }

    /// The log holding block `n`; where files overlap, the first one in block order.
    fn log_for(&self, n: u32) -> Option<&Log> {
        self.logs.iter().find(|l| l.first <= n && n <= l.last)
    }

    pub fn check_covers(&self, first: u32, last: u32) -> Result<()> {
        let mut n = first;
        while n <= last {
            let log = self.log_for(n).with_context(|| {
                format!("no block log holds block {n}; replay needs blocks {first}..{last}")
            })?;
            match log.last.checked_add(1) {
                Some(next) => n = next,
                None => break,
            }
        }
        Ok(())
    }

    pub fn id(&self, n: u32) -> Result<[u8; 32]> {
        let log = self
            .log_for(n)
            .with_context(|| format!("no block log holds block {n}"))?;
        log.id(n, &log.body(n)?)
    }

    /// Lock the files a slice reads against concurrent repair runs, and stamp them so that a
    /// change during the copy is detected.
    fn lock_and_stamp(&self, first: u32, last: u32) -> Result<Vec<(Stamp, Stamp)>> {
        let mut stamps = Vec::new();
        for log in self
            .logs
            .iter()
            .filter(|l| l.first <= last && first <= l.last)
        {
            fs2::FileExt::try_lock_shared(&log.log.file)
                .with_context(|| format!("lock {}", log.log.path.display()))?;
            fs2::FileExt::try_lock_shared(&log.index.file)
                .with_context(|| format!("lock {}", log.index.path.display()))?;
            stamps.push((log.log.stamp()?, log.index.stamp()?));
        }
        Ok(stamps)
    }
}

/// `blocks.log`, or a retained range file `blocks-FIRST-LAST.log`.
fn is_block_log(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == "blocks.log"
        || name
            .strip_prefix("blocks-")
            .and_then(|r| r.strip_suffix(".log"))
            .and_then(|r| r.split_once('-'))
            .is_some_and(|(a, b)| a.parse::<u32>().is_ok() && b.parse::<u32>().is_ok())
}

/// A block log's chain ID: from its header, or for a log starting at genesis, the SHA-256 of
/// the packed genesis state it holds.
fn chain_id_of(src: &Log) -> Result<[u8; 32]> {
    let first_pos = src.pos(src.first)?;
    ensure!(
        first_pos <= 1 << 20,
        "blocks.log preamble exceeds safety limit"
    );
    let bytes = src.log.read(0, first_pos)?;
    let header = crate::framing::preamble(&bytes)?;
    ensure!(
        header.end as u64 == first_pos,
        "first index position disagrees with blocks.log preamble"
    );
    ensure!(header.first == src.first, "blocks.log first block changed");
    Ok(header.chain_id)
}

/// Write a nodeos-readable `blocks.log` + `blocks.index` holding `first..=last`, with
/// trailers and index rebased, copied across as many source files as the range spans. The
/// header preserves the source chain identity. Legacy sources use v2 with their genesis,
/// so writers such as FIO which do not support v3 can read the slice.
pub fn write_slice(src: &BlockSet, first: u32, last: u32, dir: &Path) -> Result<()> {
    ensure!(first <= last, "empty blocklog slice {first}..{last}");
    src.check_covers(first, last)
        .with_context(|| format!("blocklog slice {first}..{last}"))?;
    let mut log = BufWriter::new(new_file(&dir.join("blocks.log"))?);
    let mut index = BufWriter::new(new_file(&dir.join("blocks.index"))?);
    let from = src.log_for(first).context("block coverage was checked")?;
    let version = u32le(&from.log.read(0, 4)?);
    let header = if version <= 2 {
        let begin = if version == 1 { 4 } else { 8 };
        let end = from.pos(from.first)? - if version == 1 { 0 } else { 8 };
        let mut header = 2u32.to_le_bytes().to_vec();
        header.extend(first.to_le_bytes());
        header.extend(from.log.read(begin, end - begin)?);
        header.extend([255; 8]);
        header
    } else if first == 1 {
        from.log.read(0, from.pos(from.first)?)?
    } else {
        let mut header = 3u32.to_le_bytes().to_vec();
        header.extend(first.to_le_bytes());
        header.extend(src.chain_id);
        header.extend([255; 8]);
        header
    };
    log.write_all(&header)?;
    let mut pos = header.len() as u64;
    for n in first..=last {
        let from = src.log_for(n).context("block coverage was checked")?;
        let (a, b) = from.bounds(n)?;
        let entry = from.log.read(a, b - a)?;
        let body = entry.len() - 8;
        ensure!(
            u64le(&entry[body..]) == a,
            "position trailer mismatch at block {n} in {}",
            from.log.path.display()
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
