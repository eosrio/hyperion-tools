use super::*;
use std::fs;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use format::{Blocks, Kind, Log, block_header, encode_container, encode_ship, ship_payload};
use repair::{LogFile, Sources, State};
use serde_json::{Value, json};
use target::{Extent, Selection, Target};
use tempfile::TempDir;

const RS: u64 = 1024;

/// Test-only media fault: reads overlapping a damaged range fail; a write covering a whole
/// range heals it, as a full-record write does on ZFS.
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Fault {
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    writes: Arc<AtomicUsize>,
    pub errno: i32,
    pub short_write: bool,
    fail_readback: bool,
}
#[cfg_attr(not(unix), allow(dead_code))]
impl Fault {
    fn new(extents: &[Extent]) -> Self {
        Self {
            ranges: Arc::new(Mutex::new(
                extents.iter().map(|e| (e.offset, e.end())).collect(),
            )),
            writes: Arc::new(AtomicUsize::new(0)),
            errno: 5,
            short_write: false,
            fail_readback: false,
        }
    }
    pub fn unreadable(&self, o: u64, n: u64) -> bool {
        n > 0
            && self
                .ranges
                .lock()
                .unwrap()
                .iter()
                .any(|&(s, e)| o < e && o + n > s)
    }
    #[cfg(unix)]
    pub fn after_write(&self, offset: u64, n: usize) {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if !self.fail_readback {
            let end = offset + n as u64;
            self.ranges
                .lock()
                .unwrap()
                .retain(|&(s, e)| !(offset <= s && e <= end));
        }
    }
    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

fn packed(previous: [u8; 32], seed: u8) -> Vec<u8> {
    let mut b = vec![0; 14];
    b.extend_from_slice(&previous);
    b.extend_from_slice(&[seed; 64]);
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend((0..80 + usize::from(seed) * 13 % 250).map(|i| (i % 251) as u8));
    b
}

fn ext(first_record: u64, last_record: u64) -> Extent {
    Extent {
        offset: first_record * RS,
        len: (last_record - first_record + 1) * RS,
    }
}

struct Fixture {
    dir: TempDir,
    kind: Kind,
    blocks: Blocks,
    log_bytes: Vec<u8>,
    path: PathBuf,
    source: Sources,
}
impl Fixture {
    fn new(kind: Kind) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.log", kind.stem()));
        let mut blocks = Blocks::new();
        let mut previous = [17; 32];
        previous[..4].copy_from_slice(&999u32.to_be_bytes());
        for n in 1000..1040 {
            let body = packed(previous, (n - 1000) as u8);
            previous = block_header(&body).unwrap().id;
            blocks.insert(n, body);
        }
        let mut log_bytes = if kind == Kind::Blocks {
            let mut h = 3u32.to_le_bytes().to_vec();
            h.extend_from_slice(&1000u32.to_le_bytes());
            h.extend_from_slice(&[34; 32]);
            h.extend_from_slice(&[255; 8]);
            h
        } else {
            Vec::new()
        };
        let mut index = Vec::new();
        for (n, body) in &blocks {
            let body = if kind == Kind::Blocks {
                body.clone()
            } else {
                let mut raw = vec![(*n % 250) as u8; 130 + (*n as usize * 37) % 300];
                raw[0] = 0;
                encode_ship(&block_header(body).unwrap().id, &raw, 1).unwrap()
            };
            let p = log_bytes.len() as u64;
            index.extend_from_slice(&p.to_le_bytes());
            log_bytes.extend_from_slice(&body);
            log_bytes.extend_from_slice(&p.to_le_bytes());
        }
        fs::write(&path, &log_bytes).unwrap();
        fs::write(path.with_extension("index"), &index).unwrap();
        let source_path = dir.path().join("blocks.p2pblk");
        fs::write(&source_path, encode_container(&blocks).unwrap()).unwrap();
        let donors = if kind != Kind::Blocks {
            let donor = dir.path().join("donor");
            fs::create_dir(&donor).unwrap();
            let p = donor.join(format!("{}.log", kind.stem()));
            fs::copy(&path, &p).unwrap();
            fs::copy(path.with_extension("index"), p.with_extension("index")).unwrap();
            vec![LogFile {
                log: p,
                index: None,
            }]
        } else {
            Vec::new()
        };
        Self {
            dir,
            kind,
            blocks,
            log_bytes,
            path,
            source: Sources {
                blocks: vec![source_path],
                donors,
                ..Sources::default()
            },
        }
    }
    fn open(&self, writable: bool) -> Log {
        Log::open(&self.path, None, writable).unwrap()
    }
    /// One record well inside the log.
    fn target(&self, log: &Log) -> Target {
        Target::new(log, RS, &[ext(4, 4)]).unwrap()
    }
    /// Two separate regions, one of them a two-record extent.
    fn multi(&self, log: &Log) -> Target {
        let t = Target::new(log, RS, &[ext(3, 3), ext(8, 9)]).unwrap();
        assert_eq!(t.regions.len(), 2, "fixture extents must form two regions");
        t
    }
    fn expected(&self, t: &Target) -> Vec<u8> {
        t.extents()
            .flat_map(|e| self.log_bytes[e.offset as usize..e.end() as usize].to_vec())
            .collect()
    }
    fn bundle(&self) -> PathBuf {
        self.dir.path().join("bundle")
    }
    fn rebuild(&self, log: &Log, t: &Target) -> anyhow::Result<repair::Rebuilt> {
        repair::rebuild(log, t, &self.source.load(self.kind)?)
    }
    #[cfg(unix)]
    fn damage(&self, log: &mut Log, t: &Target) -> Fault {
        let extents: Vec<Extent> = t.extents().copied().collect();
        let f = Fault::new(&extents);
        log.log.fault = Some(f.clone());
        f
    }
    fn rewrite_container(&self, blocks: &Blocks) {
        fs::write(&self.source.blocks[0], encode_container(blocks).unwrap()).unwrap();
    }
}

trait ErrorString {
    fn unwrap_err_string(self) -> String;
}
impl<T> ErrorString for anyhow::Result<T> {
    fn unwrap_err_string(self) -> String {
        match self {
            Ok(_) => panic!("expected failure"),
            Err(e) => format!("{e:#}"),
        }
    }
}

fn approval(out: &report::Out) -> String {
    out.json["plan_sha256"].as_str().unwrap().to_string()
}

#[test]
fn real_eos_block_header_matches_independent_nodeos_id() {
    let bytes = hex::decode(include_str!("../testdata/eos-mainnet-480385480.hex").trim()).unwrap();
    let h = block_header(&bytes).unwrap();
    assert_eq!(h.number, 480385480);
    assert_eq!(
        hex::encode(h.id),
        "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4"
    );
    for n in [0, 46, 114] {
        assert!(block_header(&bytes[..n]).is_err());
    }
}

#[test]
fn rebuilds_every_region_of_every_kind_byte_exactly_without_target_writes() {
    for kind in [Kind::Blocks, Kind::Trace, Kind::ChainState] {
        let f = Fixture::new(kind);
        let log = f.open(false);
        let t = f.multi(&log);
        let r = f.rebuild(&log, &t).unwrap();
        assert_eq!(r.replacement, f.expected(&t));
        let report = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
        assert_eq!(
            report.json["extent_states"],
            json!(["identical", "identical"])
        );
        assert!(report.text.contains("plan_sha256"));
        assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
        assert!(repair::build(&log, &t, &f.source, &f.bundle()).is_err());
    }
}

#[test]
fn every_pair_of_damaged_records_groups_into_valid_regions_and_rebuilds() {
    let mut multi_extent_region = false;
    for (kind, rs) in [(Kind::Blocks, 512), (Kind::Blocks, RS), (Kind::Trace, 512)] {
        let f = Fixture::new(kind);
        let log = f.open(false);
        let records = log.size / rs;
        let record = |i: u64| Extent {
            offset: i * rs,
            len: rs,
        };
        for a in 0..records {
            for b in a + 2..records {
                let extents = [record(a), record(b)];
                let findings = target::analyze(&log, rs, &extents).unwrap();
                let Ok(t) = Target::new(&log, rs, &extents) else {
                    assert!(findings.iter().any(|f| f.problem.is_some()));
                    continue;
                };
                for w in t.regions.windows(2) {
                    // Separate regions share at most one readable anchor entry.
                    assert!(w[1].first_block >= w[0].last_block + 2);
                }
                for r in &t.regions {
                    // Both anchor entries lie wholly outside every damaged extent.
                    let (left, right) = r.source_range();
                    let (_, left_end) = log.bounds(left).unwrap();
                    let (right_start, _) = log.bounds(right).unwrap();
                    assert!(r.extents.iter().all(|e| e.offset >= left_end));
                    assert!(r.extents.iter().all(|e| e.end() <= right_start));
                    multi_extent_region |= r.extents.len() > 1;
                }
                let rebuilt = f.rebuild(&log, &t).unwrap();
                assert_eq!(rebuilt.replacement, f.expected(&t));
            }
        }
    }
    assert!(multi_extent_region, "some pair must share a region");
}

#[test]
fn record_ranges_merge_into_sorted_extents() {
    let e = target::extents_from_ranges(RS, &[(9, 10), (5, 5), (4, 4), (10, 12)]).unwrap();
    assert_eq!(e, vec![ext(4, 5), ext(9, 12)]);
    assert!(target::extents_from_ranges(RS, &[(u64::MAX, u64::MAX)]).is_err());
}

#[test]
fn rejects_wrong_lengths_chain_tamper_gaps_and_duplicate_container_records() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let t = f.target(&log);
    let first = t.regions[0].first_block;
    let mut blocks = f.blocks.clone();
    blocks.get_mut(&first).unwrap().push(0);
    f.rewrite_container(&blocks);
    assert!(f.rebuild(&log, &t).unwrap_err_string().contains("length"));
    let mut blocks = f.blocks.clone();
    blocks.get_mut(&first).unwrap()[50] ^= 1;
    f.rewrite_container(&blocks);
    assert!(f.rebuild(&log, &t).is_err());
    let mut blocks = f.blocks.clone();
    blocks.remove(&first);
    f.rewrite_container(&blocks);
    assert!(format::container(&f.source.blocks[0]).is_err());
    let mut data = encode_container(&f.blocks).unwrap();
    data.extend_from_slice(&[0; 8]);
    fs::write(&f.source.blocks[0], data).unwrap();
    assert!(format::container(&f.source.blocks[0]).is_err());
}

#[test]
fn block_sources_that_disagree_are_all_refused() {
    let f = Fixture::new(Kind::Blocks);
    let mut blocks = f.blocks.clone();
    blocks.get_mut(&1010).unwrap()[120] ^= 1;
    let other = f.dir.path().join("other.p2pblk");
    fs::write(&other, encode_container(&blocks).unwrap()).unwrap();
    let mut sources = f.source.clone();
    sources.blocks.push(other);
    assert!(
        sources
            .load(Kind::Blocks)
            .unwrap_err_string()
            .contains("disagree on block 1010")
    );
}

#[test]
fn an_intact_blocks_log_copy_is_a_block_source_but_the_target_itself_is_not() {
    for kind in [Kind::Blocks, Kind::Trace] {
        let f = Fixture::new(kind);
        let log = f.open(false);
        let t = f.multi(&log);
        // Another node's blocks.log: same blocks, its own header and offsets.
        let other = f.dir.path().join("other-node");
        fs::create_dir(&other).unwrap();
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        bytes.extend_from_slice(&[34; 32]);
        bytes.extend_from_slice(&[255; 8]);
        let mut index = Vec::new();
        for body in f.blocks.values() {
            let p = bytes.len() as u64;
            index.extend_from_slice(&p.to_le_bytes());
            bytes.extend_from_slice(body);
            bytes.extend_from_slice(&p.to_le_bytes());
        }
        fs::write(other.join("blocks.log"), bytes).unwrap();
        fs::write(other.join("blocks.index"), index).unwrap();
        let mut sources = f.source.clone();
        sources.blocks.clear();
        sources.block_logs.push(LogFile {
            log: other.join("blocks.log"),
            index: None,
        });
        let rebuilt = repair::rebuild(&log, &t, &sources.load(kind).unwrap()).unwrap();
        assert_eq!(rebuilt.replacement, f.expected(&t));
        if kind == Kind::Blocks {
            sources.block_logs[0].log = f.path.clone();
            assert!(
                repair::rebuild(&log, &t, &sources.load(kind).unwrap())
                    .unwrap_err_string()
                    .contains("damaged target itself")
            );
        }
    }
}

#[test]
fn rejects_changed_readable_bytes_even_when_block_headers_still_match() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let t = (1..10)
        .filter_map(|n| Target::new(&log, RS, &[ext(n, n)]).ok())
        .find(|t| t.regions[0].extents[0].offset - t.regions[0].span_start > 116)
        .unwrap();
    let mut blocks = f.blocks.clone();
    blocks.get_mut(&t.regions[0].first_block).unwrap()[116] ^= 1;
    f.rewrite_container(&blocks);
    assert!(
        f.rebuild(&log, &t)
            .unwrap_err_string()
            .contains("differ from the readable target bytes")
    );
}

#[test]
fn geometry_header_pruned_and_bad_index_are_refused() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let tail = (log.size - 1) / RS;
    for extents in [
        vec![ext(0, 0)],               // log header
        vec![ext(tail, tail)],         // head entry / end of file
        vec![ext(tail + 1, tail + 1)], // past the end
        vec![Extent {
            offset: 4096,
            len: 100,
        }], // misaligned
        vec![ext(6, 6), ext(4, 4)],    // unsorted
        vec![ext(4, 4), ext(5, 5)],    // adjacent, not merged
    ] {
        assert!(Target::new(&log, RS, &extents).is_err(), "{extents:?}");
    }
    assert!(Target::new(&log, 1000, &[ext(4, 4)]).is_err());
    assert!(Target::new(&log, 1 << 30, &[ext(0, 0)]).is_err());
    drop(log);
    let mut data = f.log_bytes.clone();
    data[3] |= 128;
    fs::write(&f.path, data).unwrap();
    assert!(Log::open(&f.path, None, false).is_err());
    fs::write(&f.path, &f.log_bytes).unwrap();
    fs::write(f.path.with_extension("index"), [0; 7]).unwrap();
    assert!(Log::open(&f.path, None, false).is_err());
    let f = Fixture::new(Kind::Trace);
    let mut bytes = f.log_bytes.clone();
    bytes[2] |= 1;
    fs::write(&f.path, bytes).unwrap();
    assert!(Log::open(&f.path, None, false).is_err());
}

#[test]
fn ship_rebases_donor_trailers_and_requires_both_external_block_anchors() {
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.target(&log);
    let (left, right) = t.regions[0].source_range();
    let donor = f.source.donors[0].log.clone();
    let original = Log::open(&donor, None, false).unwrap();
    let mut bytes = Vec::new();
    let mut index = Vec::new();
    for n in left..=right {
        let p = bytes.len() as u64;
        index.extend_from_slice(&p.to_le_bytes());
        bytes.extend_from_slice(&original.body(n).unwrap());
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    drop(original);
    fs::write(&donor, bytes).unwrap();
    fs::write(donor.with_extension("index"), index).unwrap();
    assert_eq!(f.rebuild(&log, &t).unwrap().replacement, f.expected(&t));
    let blocks: Blocks = f
        .blocks
        .range(left + 1..right)
        .map(|(n, b)| (*n, b.clone()))
        .collect();
    f.rewrite_container(&blocks);
    assert!(f.rebuild(&log, &t).unwrap_err_string().contains("anchor"));
}

#[test]
fn donor_anchor_is_checked_from_framing_even_when_it_holds_oversized_full_state() {
    use std::io::{Seek, SeekFrom, Write};
    let f = Fixture::new(Kind::ChainState);
    let log = f.open(false);
    let t = f.target(&log);
    let (left, right) = t.regions[0].source_range();
    let donor = f.source.donors[0].log.clone();
    let original = Log::open(&donor, None, false).unwrap();
    let bodies: Vec<_> = (left..=right).map(|n| original.body(n).unwrap()).collect();
    drop(original);
    // A replayed donor's first entry is the full chain state: larger than any entry read.
    // Its payload is never read, so it is left as zeros (also invalid zlib). Unix files are
    // sparse; elsewhere a small payload keeps the test from allocating 256 MiB.
    let huge = if cfg!(unix) { io::MAX_BYTES + 64 } else { 64 };
    let mut file = fs::File::create(&donor).unwrap();
    let mut index = Vec::new();
    let mut header = bodies[0][..48].to_vec();
    header[40..48].copy_from_slice(&huge.to_le_bytes());
    file.write_all(&header).unwrap();
    file.seek(SeekFrom::Start(48 + huge)).unwrap();
    file.write_all(&0u64.to_le_bytes()).unwrap();
    index.extend_from_slice(&0u64.to_le_bytes());
    let mut p = 48 + huge + 8;
    for body in &bodies[1..] {
        index.extend_from_slice(&p.to_le_bytes());
        file.write_all(body).unwrap();
        file.write_all(&p.to_le_bytes()).unwrap();
        p += body.len() as u64 + 8;
    }
    drop(file);
    fs::write(donor.with_extension("index"), index).unwrap();
    let d = Log::open(&donor, None, false).unwrap();
    assert!(
        d.body(left).is_err(),
        "a full read must fail: capped or invalid zlib"
    );
    drop(d);
    assert_eq!(f.rebuild(&log, &t).unwrap().replacement, f.expected(&t));
}

#[test]
fn ship_corrupt_zlib_size_and_magic_are_refused() {
    let mut id = [0; 32];
    id[..4].copy_from_slice(&123u32.to_be_bytes());
    for marker in [0, 1, 2] {
        let body = encode_ship(&id, b"independent payload", marker).unwrap();
        assert_eq!(ship_payload(&body).unwrap().1, b"independent payload");
        let mut bad = body.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(ship_payload(&bad).is_err());
        let mut bad = body.clone();
        bad[40] ^= 1;
        assert!(ship_payload(&bad).is_err());
        let mut bad = body.clone();
        bad[0] = 1;
        assert!(ship_payload(&bad).is_err());
        for cut in 1..=5 {
            let mut bad = body[..body.len() - cut].to_vec();
            let payload_size = (bad.len() - 48) as u64;
            bad[40..48].copy_from_slice(&payload_size.to_le_bytes());
            assert!(ship_payload(&bad).is_err(), "truncated zlib must not pass");
        }
    }
    assert_eq!(format::SHIP_MAGIC, 0xc35d500000000000); // EOSIO name encoding of "ship".
}

#[test]
fn ship_payloads_use_nodeos_stored_block_split() {
    // nodeos (Spring 1.2.x) writes stored zlib in 32 KiB blocks; payloads above one block
    // must reproduce that split byte for byte, or real traces and deltas never match.
    let mut id = [0; 32];
    id[..4].copy_from_slice(&7u32.to_be_bytes());
    for (len, expected) in [
        (0usize, vec![(0u16, true)]),
        (2440, vec![(2440, true)]),
        (49349, vec![(32768, false), (16581, true)]),
        (65536, vec![(32768, false), (32768, true)]),
        (70000, vec![(32768, false), (32768, false), (4464, true)]),
    ] {
        let raw: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let body = encode_ship(&id, &raw, 1).unwrap();
        assert_eq!(ship_payload(&body).unwrap().1, raw);
        let z = &body[48 + 12..];
        assert_eq!(&z[..2], &[0x78, 0x01]);
        let (mut at, mut blocks) = (2, Vec::new());
        loop {
            let size = u16::from_le_bytes([z[at + 1], z[at + 2]]);
            assert_eq!(!size, u16::from_le_bytes([z[at + 3], z[at + 4]]));
            let last = z[at] == 1;
            blocks.push((size, last));
            at += 5 + usize::from(size);
            if last {
                break;
            }
        }
        assert_eq!(blocks, expected, "payload of {len} bytes");
        assert_eq!(at + 4, z.len(), "adler32 trailer follows the final block");
    }
}

#[test]
fn reviewed_bundle_detects_hash_and_artifact_tampering_and_missing_ack() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let t = f.multi(&log);
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let approval = approval(&b);
    assert!(repair::read_plan(&f.bundle(), &"00".repeat(32)).is_err());
    assert!(
        repair::apply(&f.bundle(), &approval, false)
            .unwrap_err_string()
            .contains("node-stopped")
    );
    fs::write(f.bundle().join("replacement.bin"), vec![0; 3 * RS as usize]).unwrap();
    assert!(repair::read_plan(&f.bundle(), &approval).is_err());
}

#[test]
fn apply_is_idempotent_and_refuses_readable_different_data() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let t = f.multi(&log);
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    drop(log);
    let approval = approval(&b);
    let r = repair::apply(&f.bundle(), &approval, true).unwrap();
    assert_eq!(r.json["applied"], false);
    let mut data = f.log_bytes.clone();
    let second = t.regions[1].extents[0];
    data[second.offset as usize + 100] ^= 1;
    fs::write(&f.path, data).unwrap();
    assert!(
        repair::apply(&f.bundle(), &approval, true)
            .unwrap_err_string()
            .contains("readable and differ")
    );
    assert!(!f.bundle().join("apply-intent.json").exists());
}

#[test]
fn sources_or_index_cannot_change_after_review() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let t = f.target(&log);
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    drop(log);
    let approval = approval(&b);
    let mut blocks = f.blocks.clone();
    // A transaction-body byte is not committed to by this header-only parser;
    // plan evidence MUST pin it even if header ids and edge bytes are unchanged.
    let n = t.regions[0].first_block + 1;
    assert!(n < t.regions[0].last_block);
    blocks.get_mut(&n).unwrap()[120] ^= 1;
    f.rewrite_container(&blocks);
    assert!(
        repair::apply(&f.bundle(), &approval, true)
            .unwrap_err_string()
            .contains("evidence changed")
    );
    f.rewrite_container(&f.blocks);
    let idx = f.path.with_extension("index");
    let bytes = fs::read(&idx).unwrap();
    fs::write(&idx, bytes).unwrap();
    assert!(
        repair::apply(&f.bundle(), &approval, true)
            .unwrap_err_string()
            .contains("index changed")
    );
}

#[test]
fn clean_scan_finds_nothing_and_reports_it() {
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let located = target::locate(&log, Some(RS), &Selection::Whole, 8).unwrap();
    assert!(located.extents.is_empty());
    assert_eq!(located.scan.as_ref().unwrap().failed_reads, 0);
    let out = repair::scan_report(&log, &located).unwrap();
    assert_eq!(out.json["damaged"], false);
    assert!(out.text.contains("No unreadable data found"));
}

#[cfg(unix)]
#[test]
fn scan_finds_every_damaged_extent_and_reports_repairability() {
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(false);
    let t = f.multi(&log);
    let tail = (log.size - 1) / RS;
    // Two repairable regions plus damage in the record holding the head entry's end.
    let mut damaged: Vec<Extent> = t.extents().copied().collect();
    damaged.push(ext(tail, tail));
    let fault = Fault::new(&damaged);
    log.log.fault = Some(fault);
    let located = target::locate(&log, Some(RS), &Selection::Whole, 64).unwrap();
    assert_eq!(located.extents, damaged);
    let out = repair::scan_report(&log, &located).unwrap();
    let regions = out.json["regions"].as_array().unwrap();
    assert_eq!(regions.len(), 3);
    assert_eq!(regions[0]["repairable"], true);
    assert_eq!(regions[1]["repairable"], true);
    assert_eq!(regions[2]["repairable"], false);
    let next = out.json["next_command"].as_str().unwrap();
    assert!(
        next.contains("--record-index 3 --record-index 8-9"),
        "{next}"
    );
    assert!(out.text.contains("NOT repairable"));

    // A one-block scan follows the damage outward and ignores other regions.
    let second = &t.regions[1];
    let located =
        target::locate(&log, Some(RS), &Selection::Block(second.first_block), 64).unwrap();
    assert_eq!(located.extents, second.extents);

    // Failed-read budget.
    assert!(
        target::locate(&log, Some(RS), &Selection::Whole, 2)
            .unwrap_err_string()
            .contains("failed reads")
    );
}

#[cfg(unix)]
#[test]
fn unreadable_bytes_outside_the_known_extents_make_a_region_unrepairable() {
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(false);
    let t = f.target(&log);
    let (anchor_start, _) = log.bounds(t.regions[0].first_block - 1).unwrap();
    // The extent itself, plus one bad byte in the left anchor entry that a narrow scan or
    // an explicit record list would not know about.
    let mut damaged: Vec<Extent> = t.regions[0].extents.clone();
    damaged.push(Extent {
        offset: anchor_start,
        len: 1,
    });
    log.log.fault = Some(Fault::new(&damaged));
    let error = Target::new(&log, RS, &t.regions[0].extents).unwrap_err_string();
    assert!(error.contains("unreadable too"), "{error}");
}

#[cfg(unix)]
#[test]
fn media_repair_writes_each_unreadable_extent_once_and_verifies_every_kind() {
    for kind in [Kind::Blocks, Kind::Trace, Kind::ChainState] {
        let f = Fixture::new(kind);
        let mut log = f.open(true);
        let t = f.multi(&log);
        let fault = f.damage(&mut log, &t);
        let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
        assert_eq!(b.json["extent_states"], json!(["unreadable", "unreadable"]));
        let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&b)).unwrap();
        let report = repair::apply_open(&log, &plan, &replacement, &f.bundle()).unwrap();
        assert_eq!(report.json["applied"], true);
        assert_eq!(fault.writes(), 2);
        assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
        assert!(f.bundle().join("apply-intent.json").exists());
        assert!(f.bundle().join("receipt.json").exists());
        assert_eq!(
            repair::apply_open(&log, &plan, &replacement, &f.bundle())
                .unwrap()
                .json["applied"],
            false
        );
    }
}

#[cfg(unix)]
#[test]
fn a_plan_with_readable_identical_extents_writes_only_the_unreadable_ones() {
    let f = Fixture::new(Kind::Trace);
    let mut log = f.open(true);
    let t = f.multi(&log);
    let fault = Fault::new(&t.regions[1].extents);
    log.log.fault = Some(fault.clone());
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    assert_eq!(b.json["extent_states"], json!(["identical", "unreadable"]));
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&b)).unwrap();
    let report = repair::apply_open(&log, &plan, &replacement, &f.bundle()).unwrap();
    assert_eq!(report.json["written_extents"], json!(t.regions[1].extents));
    assert_eq!(fault.writes(), 1);
    assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
}

#[cfg(unix)]
#[test]
fn short_write_is_journaled_and_never_automatically_retried() {
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(true);
    let t = f.multi(&log);
    let mut fault = f.damage(&mut log, &t);
    fault.short_write = true;
    log.log.fault = Some(fault.clone());
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&b)).unwrap();
    let error = repair::apply_open(&log, &plan, &replacement, &f.bundle()).unwrap_err_string();
    assert!(error.contains("SHORT WRITE"), "{error}");
    assert!(error.contains("0 of 2 extents already written"), "{error}");
    assert_eq!(fault.writes(), 1, "the second extent must not be attempted");
    assert!(f.bundle().join("apply-intent.json").exists());
    assert!(!f.bundle().join("receipt.json").exists());
    assert!(repair::apply_open(&log, &plan, &replacement, &f.bundle()).is_err());
    assert_eq!(fault.writes(), 1);
}

#[cfg(unix)]
#[test]
fn non_media_errors_and_existing_journals_do_not_authorize_writes() {
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(true);
    let t = f.target(&log);
    let mut fault = f.damage(&mut log, &t);
    fault.errno = libc::EACCES;
    log.log.fault = Some(fault.clone());
    assert!(repair::build(&log, &t, &f.source, &f.bundle()).is_err());
    fault.errno = libc::ENODATA;
    log.log.fault = Some(fault.clone());
    let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&b)).unwrap();
    fs::write(f.bundle().join("apply-intent.json"), b"prior attempt").unwrap();
    assert!(repair::apply_open(&log, &plan, &replacement, &f.bundle()).is_err());
    assert_eq!(fault.writes(), 0);
}

#[cfg(unix)]
#[test]
fn failed_readback_retains_intent_without_claiming_success() {
    let f = Fixture::new(Kind::ChainState);
    let mut log = f.open(true);
    let t = f.target(&log);
    let mut fault = f.damage(&mut log, &t);
    fault.fail_readback = true;
    log.log.fault = Some(fault.clone());
    let built = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&built)).unwrap();
    assert!(
        repair::apply_open(&log, &plan, &replacement, &f.bundle())
            .unwrap_err_string()
            .contains("read-back mismatch")
    );
    assert_eq!(fault.writes(), 1);
    assert!(f.bundle().join("apply-intent.json").exists());
    assert!(!f.bundle().join("receipt.json").exists());
}

#[cfg(unix)]
#[test]
fn competing_lock_and_replaced_target_path_are_refused_before_journaling() {
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(true);
    let t = f.target(&log);
    let fault = f.damage(&mut log, &t);
    let built = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&built)).unwrap();
    let competing = f.open(true);
    fs2::FileExt::try_lock_exclusive(&competing.log.file).unwrap();
    assert!(
        repair::apply_open(&log, &plan, &replacement, &f.bundle())
            .unwrap_err_string()
            .contains("log lock")
    );
    drop(competing);
    fs::rename(&f.path, f.path.with_extension("old")).unwrap();
    fs::write(&f.path, &f.log_bytes).unwrap();
    assert!(
        repair::apply_open(&log, &plan, &replacement, &f.bundle())
            .unwrap_err_string()
            .contains("path was replaced")
    );
    assert_eq!(fault.writes(), 0);
    assert!(!f.bundle().join("apply-intent.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn another_process_holding_the_log_open_blocks_apply() {
    use std::time::{Duration, Instant};
    let f = Fixture::new(Kind::Blocks);
    let mut log = f.open(true);
    let t = f.target(&log);
    let fault = f.damage(&mut log, &t);
    let built = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
    let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&built)).unwrap();
    let mut holder = std::process::Command::new("sh")
        .args(["-c", "exec 3<\"$0\"; exec sleep 30"])
        .arg(&f.path)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.log.other_openers().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "holder never opened the log");
        std::thread::sleep(Duration::from_millis(20));
    }
    let error = repair::apply_open(&log, &plan, &replacement, &f.bundle()).unwrap_err_string();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(error.contains("open; stop nodeos"), "{error}");
    assert_eq!(fault.writes(), 0);
    assert!(!f.bundle().join("apply-intent.json").exists());
}

#[test]
fn extent_state_classifies_readable_records_individually() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let e = ext(8, 9);
    let mut expected = f.log_bytes[e.offset as usize..e.end() as usize].to_vec();
    assert_eq!(
        repair::extent_state(&log.log, RS, &e, &expected).unwrap(),
        State::Identical
    );
    expected[RS as usize + 3] ^= 1;
    assert_eq!(
        repair::extent_state(&log.log, RS, &e, &expected).unwrap(),
        State::Differs
    );
}

#[test]
fn malformed_lengths_and_noncanonical_varints_return_errors() {
    for data in [vec![0x80, 0], vec![255, 255, 255, 255, 16], vec![255; 6]] {
        assert!(format::Cursor::new(&data).var().is_err());
    }
    let f = Fixture::new(Kind::Blocks);
    let mut data = encode_container(&f.blocks).unwrap();
    data[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&f.source.blocks[0], data).unwrap();
    assert!(format::container(&f.source.blocks[0]).is_err());
    let index = f.path.with_extension("index");
    let mut bad = fs::read(&index).unwrap();
    let offset = u64::MAX;
    bad[8..16].copy_from_slice(&offset.to_le_bytes());
    fs::write(index, bad).unwrap();
    let log = f.open(false);
    assert!(log.bounds(1000).is_err());
}

fn abi() -> Value {
    let spec: [(&str, Vec<(&str, &str)>); 4] = [
        (
            "block_position",
            vec![("block_num", "uint32"), ("block_id", "checksum256")],
        ),
        (
            "get_blocks_request_v0",
            vec![
                ("start_block_num", "uint32"),
                ("end_block_num", "uint32"),
                ("max_messages_in_flight", "uint32"),
                ("have_positions", "block_position[]"),
                ("irreversible_only", "bool"),
                ("fetch_block", "bool"),
                ("fetch_traces", "bool"),
                ("fetch_deltas", "bool"),
            ],
        ),
        (
            "get_blocks_ack_request_v0",
            vec![("num_messages", "uint32")],
        ),
        (
            "get_blocks_result_v0",
            vec![
                ("head", "block_position"),
                ("last_irreversible", "block_position"),
                ("this_block", "block_position?"),
                ("prev_block", "block_position?"),
                ("block", "bytes?"),
                ("traces", "bytes?"),
                ("deltas", "bytes?"),
            ],
        ),
    ];
    let structs: Vec<Value> = spec
        .into_iter()
        .map(|(name, fields)| {
            let fields: Vec<Value> = fields
                .into_iter()
                .map(|(n, t)| json!({"name": n, "type": t}))
                .collect();
            json!({"name": name, "fields": fields})
        })
        .collect();
    json!({
        "structs": structs,
        "variants": [
            {
                "name": "request",
                "types": ["get_status_request_v0", "get_blocks_request_v0", "get_blocks_ack_request_v0"]
            },
            {"name": "result", "types": ["get_status_result_v0", "get_blocks_result_v0"]}
        ]
    })
}
fn var(mut n: u32, b: &mut Vec<u8>) {
    loop {
        let mut v = (n & 127) as u8;
        n >>= 7;
        if n > 0 {
            v |= 128;
        }
        b.push(v);
        if n == 0 {
            break;
        }
    }
}
fn optional_bytes(bytes: &[u8], b: &mut Vec<u8>) {
    b.push(1);
    var(bytes.len() as u32, b);
    b.extend_from_slice(bytes);
}
fn result_message(f: &Fixture, n: u32, kind: Kind) -> Vec<u8> {
    let h = block_header(&f.blocks[&n]).unwrap();
    let head = block_header(f.blocks.last_key_value().unwrap().1).unwrap();
    let mut b = vec![1];
    for _ in 0..2 {
        b.extend_from_slice(&head.number.to_le_bytes());
        b.extend_from_slice(&head.id);
    }
    b.push(1);
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&h.id);
    b.push(1);
    b.extend_from_slice(&(n - 1).to_le_bytes());
    b.extend_from_slice(&h.previous);
    optional_bytes(&f.blocks[&n], &mut b);
    if kind == Kind::Blocks {
        b.extend_from_slice(&[0, 0]);
        return b;
    }
    let log = f.open(false);
    let raw = ship_payload(&log.body(n).unwrap()).unwrap().1;
    if kind == Kind::Trace {
        optional_bytes(&raw, &mut b);
        b.push(0);
    } else {
        b.push(0);
        optional_bytes(&raw, &mut b);
    }
    b
}

/// One SHIP session per region: the expected request and the results to send.
type Session = (Vec<u8>, Vec<Vec<u8>>);

fn sessions(f: &Fixture, t: &Target) -> Vec<Session> {
    t.regions
        .iter()
        .map(|r| {
            let (first, last) = r.source_range();
            let request = ship::request(first, last, f.kind).unwrap();
            let messages = (first..=last)
                .map(|n| result_message(f, n, f.kind))
                .collect();
            (request, messages)
        })
        .collect()
}

/// Loopback SHIP server: per connection, sends the ABI, checks the request, then sends one
/// result per acknowledgement.
async fn ship_server(sessions: Vec<Session>) -> (String, tokio::task::JoinHandle<()>) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for (expected_request, messages) in sessions {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text(abi().to_string().into()))
                .await
                .unwrap();
            let req = ws.next().await.unwrap().unwrap();
            assert_eq!(req.into_data().as_ref(), &expected_request);
            let last = messages.len() - 1;
            for (i, m) in messages.into_iter().enumerate() {
                ws.send(Message::Binary(m.into())).await.unwrap();
                if i < last {
                    assert_eq!(
                        ws.next().await.unwrap().unwrap().into_data().as_ref(),
                        &[2, 1, 0, 0, 0]
                    );
                }
            }
        }
    });
    (url, server)
}

#[test]
fn ship_protocol_refuses_unsupported_abi_bad_variants_and_reversible_blocks() {
    ship::validate_abi(&abi().to_string()).unwrap();
    let mut bad = abi();
    bad["variants"][0]["types"][1] = json!("future_version");
    assert!(ship::validate_abi(&bad.to_string()).is_err());
    let f = Fixture::new(Kind::Trace);
    let message = result_message(&f, 1010, Kind::Trace);
    assert_eq!(ship::decode(&message, Kind::Trace).unwrap().number, 1010);
    let mut bad = message.clone();
    bad[0] = 2;
    assert!(ship::decode(&bad, Kind::Trace).is_err());
    let mut bad = message.clone();
    bad[73] = 2;
    assert!(ship::decode(&bad, Kind::Trace).is_err());
    let mut bad = message.clone();
    bad[37..41].copy_from_slice(&1000u32.to_le_bytes());
    bad[41..45].copy_from_slice(&1000u32.to_be_bytes());
    assert!(ship::decode(&bad, Kind::Trace).is_err());
    let mut bad = message;
    bad.push(0);
    assert!(ship::decode(&bad, Kind::Trace).is_err());
    assert_eq!(
        &ship::request(100, 105, Kind::Trace).unwrap()[13..],
        &[0, 1, 1, 1, 0]
    );
    assert_eq!(
        u32::from_le_bytes(
            ship::request(100, 105, Kind::Trace).unwrap()[5..9]
                .try_into()
                .unwrap()
        ),
        106
    );
}

#[tokio::test]
async fn websocket_fetch_stages_verified_sources_for_every_region_and_kind() {
    for kind in [Kind::Blocks, Kind::Trace, Kind::ChainState] {
        let f = Fixture::new(kind);
        let log = f.open(false);
        let t = f.multi(&log);
        let out = f.dir.path().join("fetched");
        let (url, server) = ship_server(sessions(&f, &t)).await;
        let r = ship::fetch(&log, &t, &[url], &out, 5).await.unwrap();
        server.await.unwrap();
        assert_eq!(r.json["target_compatible"], true);
        assert_eq!(r.json["regions"].as_array().unwrap().len(), 2);
        let sources = Sources::from_dir(&out).unwrap();
        assert_eq!(sources.blocks.len(), 2);
        assert_eq!(
            sources.donors.len(),
            if kind == Kind::Blocks { 0 } else { 2 }
        );
        let loaded = sources.load(kind).unwrap();
        assert_eq!(
            repair::rebuild(&log, &t, &loaded).unwrap().replacement,
            f.expected(&t)
        );
        // The directory is self-describing: build straight from it.
        repair::build(&log, &t, &sources, &f.bundle()).unwrap();
        assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
    }
}

#[tokio::test]
async fn websocket_endpoints_must_agree_byte_for_byte() {
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.target(&log);
    let good = sessions(&f, &t);
    let (a, server_a) = ship_server(good.clone()).await;
    let (b, server_b) = ship_server(good.clone()).await;
    let out = f.dir.path().join("agreed");
    let r = ship::fetch(&log, &t, &[a, b], &out, 5).await.unwrap();
    server_a.await.unwrap();
    server_b.await.unwrap();
    assert_eq!(r.json["target_compatible"], true);

    // One endpoint serves a different trace payload byte: every source is refused.
    let mut tampered = good.clone();
    let m = &mut tampered[0].1[1];
    let last = m.len() - 2; // final payload byte, before the absent-deltas flag
    m[last] ^= 1;
    let (a, server_a) = ship_server(good).await;
    let (b, server_b) = ship_server(tampered).await;
    let out = f.dir.path().join("disagreed");
    let error = ship::fetch(&log, &t, &[a.clone(), b], &out, 5)
        .await
        .unwrap_err_string();
    server_a.await.unwrap();
    server_b.await.unwrap();
    assert!(error.contains("disagree"), "{error}");
    assert!(!out.exists());
    assert!(
        ship::fetch(&log, &t, &[a.clone(), a], &out, 5)
            .await
            .unwrap_err_string()
            .contains("duplicate")
    );
}

#[tokio::test]
async fn websocket_bad_abi_leaves_target_and_output_untouched() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.target(&log);
    let out = f.dir.path().join("fetched");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(Message::Text("{}".into())).await.unwrap();
    });
    assert!(
        ship::fetch(&log, &t, &[format!("ws://{addr}")], &out, 2)
            .await
            .is_err()
    );
    server.await.unwrap();
    assert!(!out.exists());
    assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
}

#[tokio::test]
async fn websocket_deadline_bounds_a_stalled_source_without_target_writes() {
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.target(&log);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let _ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        std::future::pending::<()>().await;
    });
    let out = f.dir.path().join("stalled-source");
    let error = ship::fetch(&log, &t, &[format!("ws://{addr}")], &out, 1)
        .await
        .unwrap_err_string();
    server.abort();
    let _ = server.await;
    assert!(error.contains("timed out"));
    assert!(!out.exists());
    assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
}

#[tokio::test]
async fn wss_initializes_crypto_and_rejects_a_non_tls_peer_without_panicking() {
    use tokio::io::AsyncWriteExt;
    // Rustls must have a provider in this standalone binary; feature unification
    // from an unrelated workspace must never be required to make wss work.
    let _config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.target(&log);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let _ = tcp.write_all(b"this is not a TLS stream\r\n").await;
    });
    let out = f.dir.path().join("bad-tls");
    let error = ship::fetch(&log, &t, &[format!("wss://{addr}")], &out, 3)
        .await
        .unwrap_err_string();
    server.await.unwrap();
    assert!(error.contains("connect SHIP WebSocket"));
    assert!(!out.exists());
    assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
}

#[test]
fn replay_refuses_a_snapshot_at_or_after_first_affected_minus_one() {
    let f = Fixture::new(Kind::Trace);
    let log = f.open(false);
    let t = f.multi(&log);
    let result = replay::run(
        &log,
        &t,
        replay::ReplayOptions {
            nodeos: Path::new("missing-nodeos"),
            snapshot: Path::new("missing-snapshot"),
            snapshot_block: t.first_block() - 1,
            blocks_dir: f.dir.path(),
            out: &f.dir.path().join("replay"),
            debug_traces: false,
            state_db_size_mb: 1024,
        },
    );
    assert!(result.unwrap_err_string().contains("snapshot head"));
    assert!(!f.dir.path().join("replay").exists());
}

#[test]
fn replay_slice_is_a_rebased_v3_blocklog_with_the_source_chain_id() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    let dir = f.dir.path().join("slice");
    fs::create_dir(&dir).unwrap();
    replay::write_slice(&log, 1010, 1020, &dir).unwrap();
    let s = Log::open(&dir.join("blocks.log"), None, false).unwrap();
    assert_eq!((s.first, s.last), (1010, 1020));
    assert_eq!(s.log.read(8, 32).unwrap(), [34; 32]);
    for n in 1010..=1020 {
        assert_eq!(s.body(n).unwrap(), f.blocks[&n]);
    }
    let outside = f.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    assert!(replay::write_slice(&log, 999, 1001, &outside).is_err());
    assert!(replay::write_slice(&log, 1030, 1040, &outside).is_err());
}

#[test]
fn cli_value_parsers() {
    assert_eq!(parse_integer("0x20000").unwrap(), 131072);
    assert_eq!(parse_integer("4").unwrap(), 4);
    assert!(parse_integer("-1").is_err());
    assert!(parse_integer("18446744073709551616").is_err());
    assert_eq!(parse_records("0x10").unwrap(), (16, 16));
    assert_eq!(parse_records("3-5").unwrap(), (3, 5));
    assert!(parse_records("5-3").is_err());
    assert_eq!(parse_block_range("100-200").unwrap(), (100, 200));
    assert_eq!(parse_block_range("100..200").unwrap(), (100, 200));
    assert!(parse_block_range("200-100").is_err());
    assert_eq!(report::bytes(131072), "128 KiB");
    assert_eq!(report::bytes(1536), "1.5 KiB");
    assert_eq!(report::arg("/data/x.log"), "/data/x.log");
    assert_eq!(report::arg("a b"), "'a b'");
}

#[test]
fn cli_definition_is_consistent() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

#[cfg(target_os = "linux")]
#[test]
fn scan_drops_only_the_pages_it_brought_into_the_cache() {
    let f = Fixture::new(Kind::Blocks);
    let log = f.open(false);
    log.log.file.sync_all().unwrap();
    let all = log.log.cached_pages(0, log.size).unwrap();
    let pages = all.cached.len();
    assert!(pages >= 3, "fixture must span several pages");
    log.log.uncache(&io::CachedPages {
        start: 0,
        cached: vec![false; pages],
    });
    if log
        .log
        .cached_pages(0, log.size)
        .unwrap()
        .cached
        .iter()
        .any(|c| *c)
    {
        eprintln!("this filesystem keeps pages cached regardless (tmpfs?); skipping");
        return;
    }
    // The node's working set: the newest page. Readahead only runs forward, so reading the
    // last page caches nothing else.
    log.log.read(log.size - 1, 1).unwrap();
    target::scan(&log.log, log.size, RS, 0, log.size, 8).unwrap();
    let after = log.log.cached_pages(0, log.size).unwrap().cached;
    assert!(
        after[pages - 1],
        "a page cached before the scan stays cached"
    );
    assert!(
        after[..pages - 1].iter().all(|c| !c),
        "pages only the scan read are dropped: {after:?}"
    );
}

/// The fixture rewritten as nodeos leaves a SHIP log after a fork: two abandoned entries for
/// blocks `after + 1` and `after + 2`, from another fork, sit between block `after`'s entry
/// and the next index position. The donor keeps the original layout, without orphans.
fn orphaned(kind: Kind, after: u32) -> Fixture {
    let mut f = Fixture::new(kind);
    let log = f.open(false);
    let (mut bytes, mut index) = (Vec::new(), Vec::new());
    for n in log.first..=log.last {
        let p = bytes.len() as u64;
        index.extend_from_slice(&p.to_le_bytes());
        bytes.extend_from_slice(&log.body(n).unwrap());
        bytes.extend_from_slice(&p.to_le_bytes());
        if n == after {
            for orphan in [after + 1, after + 2] {
                let mut id = [0xEE; 32];
                id[..4].copy_from_slice(&orphan.to_be_bytes());
                let p = bytes.len() as u64;
                bytes.extend_from_slice(&encode_ship(&id, b"abandoned fork entry", 1).unwrap());
                bytes.extend_from_slice(&p.to_le_bytes());
            }
        }
    }
    drop(log);
    fs::write(&f.path, &bytes).unwrap();
    fs::write(f.path.with_extension("index"), index).unwrap();
    f.log_bytes = bytes;
    f
}

/// Block `n`'s own entry end and the next index position in `log`.
fn own_and_next(log: &Log, n: u32) -> (u64, u64, u64) {
    let (a, b) = log.bounds(n).unwrap();
    let own = format::ship_entry_size(&fs::read(&log.log.path).unwrap()[a as usize..]);
    (a, a + own, b)
}

/// A block followed by orphans whose entry crosses a record boundary before its orphans
/// begin, so one record holds part of the entry and none of the orphans.
fn straddling(kind: Kind) -> (Fixture, u32, u64) {
    for n in 1005..1030 {
        let f = orphaned(kind, n);
        let log = f.open(false);
        let (a, end, b) = own_and_next(&log, n);
        assert!(end < b, "orphans follow block {n}");
        let boundary = (a / RS + 1) * RS;
        if a % RS != 0 && boundary <= end {
            return (f, n, a / RS);
        }
    }
    panic!("no fixture block crosses a record boundary before its orphans");
}

#[test]
fn ship_logs_frame_entries_by_payload_size_and_accept_orphaned_fork_entries() {
    for kind in [Kind::Trace, Kind::ChainState] {
        let clean = Fixture::new(kind);
        let f = orphaned(kind, 1012);
        let log = f.open(false);
        let reference = clean.open(false);
        for n in log.first..=log.last {
            assert_eq!(
                log.body(n).unwrap(),
                reference.body(n).unwrap(),
                "{kind:?} {n}"
            );
            assert_eq!(
                log.ship_entry_id(n).unwrap(),
                reference.ship_entry_id(n).unwrap()
            );
        }
        // An orphan whose trailer does not name its own position is not an orphan.
        let (_, end, _) = own_and_next(&log, 1012);
        let mut bytes = f.log_bytes.clone();
        let orphan_size = format::ship_entry_size(&bytes[end as usize..]) as usize;
        bytes[end as usize + orphan_size - 8] ^= 1;
        drop(log);
        fs::write(&f.path, &bytes).unwrap();
        let log = f.open(false);
        let error = log.body(1012).unwrap_err_string();
        assert!(error.contains("wrong position trailer"), "{error}");
        assert!(log.ship_entry_id(1012).is_err());
    }
}

#[test]
fn rebuild_keeps_readable_orphans_and_refuses_damage_that_reaches_them() {
    for kind in [Kind::Trace, Kind::ChainState] {
        // Damage in the entry before its orphans: the donor has no orphans, the target keeps
        // its own, and the rebuild is byte-exact.
        let (f, n, record) = straddling(kind);
        let log = f.open(false);
        let t = Target::new(&log, RS, &[ext(record, record)]).unwrap();
        assert!(t.regions[0].last_block >= n, "{kind:?}");
        assert_eq!(f.rebuild(&log, &t).unwrap().replacement, f.expected(&t));
        let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
        assert_eq!(b.json["extent_states"], json!(["identical"]));

        // A donor that has the same orphans works as well.
        let donor = &f.source.donors[0].log;
        fs::copy(&f.path, donor).unwrap();
        fs::copy(
            f.path.with_extension("index"),
            donor.with_extension("index"),
        )
        .unwrap();
        assert_eq!(f.rebuild(&log, &t).unwrap().replacement, f.expected(&t));

        // Damage that reaches the orphans: the header is readable, so the plan refuses it.
        let (a, end, _) = own_and_next(&log, n);
        let gap_record = end / RS;
        assert!(a < gap_record * RS && gap_record > record);
        let error = Target::new(&log, RS, &[ext(gap_record, gap_record)]).unwrap_err_string();
        assert!(error.contains("orphaned fork entries"), "{kind:?}: {error}");
    }
    // Damage covering an entry's header and its orphans: refused once the rebuilt length
    // shows where the orphans start.
    for kind in [Kind::Trace, Kind::ChainState] {
        let mut refused = false;
        for n in 1005..1030 {
            let f = orphaned(kind, n);
            let log = f.open(false);
            let (a, end, _) = own_and_next(&log, n);
            if a / RS != end / RS {
                continue;
            }
            let record = a / RS;
            let Ok(t) = Target::new(&log, RS, &[ext(record, record)]) else {
                continue;
            };
            let error = f.rebuild(&log, &t).unwrap_err_string();
            assert!(error.contains("orphaned fork entries"), "{kind:?}: {error}");
            assert!(error.contains(target::ORPHANS_UNREPAIRABLE), "{error}");
            refused = true;
            break;
        }
        assert!(
            refused,
            "{kind:?}: no fixture block has its header and orphans in one record"
        );
    }
}

#[cfg(unix)]
#[test]
fn media_repair_next_to_orphaned_fork_entries_heals_byte_exactly() {
    for kind in [Kind::Trace, Kind::ChainState] {
        let (f, _, record) = straddling(kind);
        let mut log = f.open(true);
        let t = Target::new(&log, RS, &[ext(record, record)]).unwrap();
        let fault = f.damage(&mut log, &t);
        let b = repair::build(&log, &t, &f.source, &f.bundle()).unwrap();
        assert_eq!(b.json["extent_states"], json!(["unreadable"]));
        let (plan, replacement) = repair::read_plan(&f.bundle(), &approval(&b)).unwrap();
        let report = repair::apply_open(&log, &plan, &replacement, &f.bundle()).unwrap();
        assert_eq!(report.json["applied"], true, "{kind:?}");
        assert_eq!(fault.writes(), 1);
        assert_eq!(fs::read(&f.path).unwrap(), f.log_bytes);
    }
}
