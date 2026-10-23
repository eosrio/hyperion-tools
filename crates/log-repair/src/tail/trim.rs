//! Journaled, suffix-only block-log trimming. No valid-prefix bytes are rewritten.
#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail, ensure};
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::super::{Pin, Summary, start, walk};
    use crate::format::Kind;
    use crate::io::{self, Disk, MAX_BYTES, Stamp};
    use crate::report::Out;

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Target {
        path: PathBuf,
        before: Stamp,
        keep: u64,
        before_sha256: String,
        prefix_sha256: String,
        tail_sha256: String,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Intent {
        schema: u32,
        operation: String,
        tool_version: String,
        first_block: u32,
        last_block: u32,
        indexed_head_hint: Option<u32>,
        failure_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inspection_sha256: Option<String>,
        log: Target,
        index: Target,
    }

    /// Linux ctime also detects writes followed by restoring mtime.
    #[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct LiveStamp {
        persisted: Stamp,
        changed_seconds: i64,
        changed_nanos: i64,
    }
    fn live_stamp(disk: &Disk) -> Result<LiveStamp> {
        let m = disk.file.metadata()?;
        Ok(LiveStamp {
            persisted: Stamp {
                size: m.len(),
                modified_ns: m
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                device: m.dev(),
                inode: m.ino(),
            },
            changed_seconds: m.ctime(),
            changed_nanos: m.ctime_nsec(),
        })
    }

    struct Pair {
        log: Disk,
        index: Disk,
    }
    impl Pair {
        fn open(log: &Path, index: &Path, stopped: bool) -> Result<Self> {
            ensure!(
                stopped,
                "--node-stopped is required; stop every reader and writer first"
            );
            ensure!(
                Kind::from_path(log)? == Kind::Blocks,
                "in-place tail trim supports blocks.log only; use copy-out recovery for SHIP"
            );
            let out = Self {
                log: Disk::open(log, true)?,
                index: Disk::open(index, true)?,
            };
            out.lock(stopped)?;
            Ok(out)
        }
        fn lock(&self, stopped: bool) -> Result<()> {
            ensure!(
                stopped,
                "--node-stopped is required; stop every reader and writer first"
            );
            ensure!(
                Kind::from_path(&self.log.path)? == Kind::Blocks,
                "inspection checkpoints support blocks.log only"
            );
            ensure!(
                !same(&self.log, &self.index)?,
                "log and index are the same file"
            );
            for disk in [&self.log, &self.index] {
                fs2::FileExt::try_lock_exclusive(&disk.file)
                    .context("another repair holds a file lock")?;
                let pids = disk.other_openers()?;
                ensure!(
                    pids.is_empty(),
                    "process(es) {pids:?} have {} open; stop every reader/writer",
                    disk.path.display()
                );
                disk.check_path()?;
            }
            Ok(())
        }
        fn check_paths(&self) -> Result<()> {
            self.log.check_path()?;
            self.index.check_path()
        }
    }

    fn same(a: &Disk, b: &Disk) -> Result<bool> {
        Ok(same_file::Handle::from_file(a.file.try_clone()?)?
            == same_file::Handle::from_file(b.file.try_clone()?)?)
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Checkpoint {
        schema: u32,
        operation: String,
        boot_id: String,
        executable_sha256: String,
        log: PathBuf,
        index: PathBuf,
        log_stamp: LiveStamp,
        index_stamp: LiveStamp,
        preamble_sha256: String,
        anchor_sha256: String,
        summary: Summary,
    }

    fn boot_id() -> Result<String> {
        let id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        let id = id.trim();
        ensure!(id.len() == 36, "invalid Linux boot identity");
        Ok(id.into())
    }

    fn executable_hash() -> Result<String> {
        let mut file = fs::File::open("/proc/self/exe")?;
        let mut hash = Sha256::new();
        let mut bytes = [0u8; 65536];
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
        }
        Ok(hex::encode(hash.finalize()))
    }

    /// A checkpoint is an optimization for unchanged local files, not independent
    /// evidence against silent media corruption. Full post-trim verification remains mandatory.
    pub(in crate::tail) struct Inspection {
        pair: Pair,
        dir: PathBuf,
        stamps: (LiveStamp, LiveStamp),
        boot: String,
        executable: String,
    }
    impl Inspection {
        pub(in crate::tail) fn begin(
            log: &Disk,
            index: &Disk,
            dir: &Path,
            stopped: bool,
        ) -> Result<Self> {
            ensure!(
                !dir.try_exists()?,
                "inspection directory already exists; choose a new directory"
            );
            let pair = Pair {
                log: log.try_clone()?,
                index: index.try_clone()?,
            };
            pair.lock(stopped)?;
            // Reuse relies on local ctime semantics; refuse remote or unknown filesystems.
            for disk in [&pair.log, &pair.index] {
                let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
                use std::os::fd::AsRawFd;
                // SAFETY: the descriptor is live and stat points to writable storage.
                ensure!(
                    unsafe { libc::fstatfs(disk.file.as_raw_fd(), stat.as_mut_ptr()) } == 0,
                    "cannot identify checkpoint filesystem"
                );
                // SAFETY: fstatfs initialized stat on success.
                let kind = i128::from(unsafe { stat.assume_init() }.f_type);
                ensure!(
                    matches!(
                        kind,
                        0xef53 | 0x58465342 | 0x9123683e | 0x2fc12fc1 | 0x01021994
                    ),
                    "inspection reuse requires ext4, XFS, Btrfs, ZFS or tmpfs; use ordinary trim-tail on this filesystem"
                );
            }
            let stamps = (live_stamp(&pair.log)?, live_stamp(&pair.index)?);
            Ok(Self {
                pair,
                dir: dir.into(),
                stamps,
                boot: boot_id()?,
                executable: executable_hash()?,
            })
        }
        pub(in crate::tail) fn save(&self, summary: &Summary) -> Result<()> {
            trim_geometry(summary)?;
            let (preamble, anchor) = boundaries(&self.pair, summary)?;
            unchanged(&self.pair, &self.stamps)?;
            let checkpoint = Checkpoint {
                schema: 1,
                operation: "tail_inspection".into(),
                boot_id: self.boot.clone(),
                executable_sha256: self.executable.clone(),
                log: self.pair.log.path.clone(),
                index: self.pair.index.path.clone(),
                log_stamp: self.stamps.0.clone(),
                index_stamp: self.stamps.1.clone(),
                preamble_sha256: preamble,
                anchor_sha256: anchor,
                summary: summary.clone(),
            };
            io::create_dir(&self.dir)?;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
            let bytes = serde_json::to_vec_pretty(&checkpoint)?;
            atomic_file(&self.dir, "inspection.json", &bytes)?;
            unchanged(&self.pair, &self.stamps)?;
            atomic_file(&self.dir, "inspection.sha256", io::hash(&bytes).as_bytes())?;
            unchanged(&self.pair, &self.stamps)?;
            Ok(())
        }
    }

    fn boundaries(pair: &Pair, summary: &Summary) -> Result<(String, String)> {
        let begin = start(&pair.log, Kind::Blocks)?;
        let keep_index = summary
            .entries
            .checked_mul(8)
            .context("inspection index length overflow")?;
        ensure!(keep_index >= 8, "checkpoint requires an anchor block");
        let at = crate::format::u64le(&pair.index.read(keep_index - 8, 8)?);
        ensure!(
            at >= begin.bytes.len() as u64 && at < summary.physical_prefix_end,
            "invalid checkpoint anchor position"
        );
        let bytes = pair.log.read(at, summary.physical_prefix_end - at)?;
        let (header, end) = crate::framing::signed_block(&bytes)?;
        ensure!(
            end + 8 == bytes.len()
                && crate::format::u64le(&bytes[end..]) == at
                && Some(header.number) == summary.last_block,
            "checkpoint anchor does not match the cut"
        );
        Ok((io::hash(&begin.bytes), io::hash(&bytes)))
    }

    struct SavedInspection {
        pins: Vec<Pin>,
        hash: String,
        checkpoint: Checkpoint,
    }
    impl SavedInspection {
        fn load(dir: &Path, pair: &Pair) -> Result<Self> {
            ensure!(
                fs::metadata(dir)?.mode() & 0o022 == 0,
                "inspection directory must not be writable by group or others"
            );
            let mut pins = Vec::new();
            let mut read = |name: &str, limit: u64| -> Result<Vec<u8>> {
                let path = dir.join(name);
                ensure!(
                    !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                    "inspection artifacts must not be symlinks"
                );
                let pin = Pin::new(&path, false)?;
                let disk = pin.disk.as_ref().unwrap();
                let size = disk.stamp()?.size;
                ensure!(size <= limit, "inspection artifact exceeds its size limit");
                let bytes = disk.read(0, size)?;
                pin.check()?;
                pins.push(pin);
                Ok(bytes)
            };
            let bytes = read("inspection.json", 1 << 20)?;
            let committed = read("inspection.sha256", 128)?;
            let hash = io::hash(&bytes);
            ensure!(
                committed == hash.as_bytes(),
                "inspection checksum mismatch; inspect again"
            );
            let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
            ensure!(
                checkpoint.schema == 1 && checkpoint.operation == "tail_inspection",
                "unsupported inspection checkpoint"
            );
            ensure!(
                checkpoint.boot_id == boot_id()?,
                "inspection belongs to another boot; inspect again"
            );
            ensure!(
                checkpoint.executable_sha256 == executable_hash()?,
                "inspection used a different executable; inspect again"
            );
            ensure!(
                pair.log.path == checkpoint.log && pair.index.path == checkpoint.index,
                "inspection names different target paths; inspect again"
            );
            unchanged(
                pair,
                &(checkpoint.log_stamp.clone(), checkpoint.index_stamp.clone()),
            )
            .context("inspection is stale; inspect again")?;
            trim_geometry(&checkpoint.summary)?;
            let (preamble, anchor) = boundaries(pair, &checkpoint.summary)?;
            ensure!(
                preamble == checkpoint.preamble_sha256 && anchor == checkpoint.anchor_sha256,
                "inspection boundary bytes changed; inspect again"
            );
            let saved = Self {
                pins,
                hash,
                checkpoint,
            };
            saved.check()?;
            Ok(saved)
        }
        fn check(&self) -> Result<()> {
            for pin in &self.pins {
                pin.check()?;
            }
            let disk = self.pins[0].disk.as_ref().unwrap();
            ensure!(
                io::hash(&disk.read(0, disk.stamp()?.size)?) == self.hash,
                "inspection artifact changed; refusing mutation"
            );
            let checksum = self.pins[1].disk.as_ref().unwrap();
            ensure!(
                checksum.read(0, checksum.stamp()?.size)? == self.hash.as_bytes(),
                "inspection checksum changed; refusing mutation"
            );
            for pin in &self.pins {
                pin.check()?;
            }
            Ok(())
        }
    }

    /// Publish only a completely written, synced file, without replacing an existing name.
    fn atomic_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
        for n in 0..32 {
            let temporary = dir.join(format!(".{name}.{}.{n}.pending", std::process::id()));
            let mut file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::hard_link(&temporary, dir.join(name))
                .with_context(|| format!("publish new journal file {name}"))?;
            io::sync_dir(dir)?;
            fs::remove_file(temporary)?;
            io::sync_dir(dir)?;
            return Ok(());
        }
        bail!("too many interrupted journal files; inspect the journal directory")
    }

    struct Journal {
        dir: PathBuf,
        lock: Disk,
        pins: Vec<Pin>,
        fingerprints: Vec<String>,
        prepared_stamps: Option<(LiveStamp, LiveStamp)>,
        intent: Intent,
        hash: String,
        log_tail: Vec<u8>,
        index_tail: Vec<u8>,
    }
    impl Journal {
        fn load(dir: &Path) -> Result<Self> {
            let dir = dir.canonicalize()?;
            let mode = fs::metadata(&dir)?.mode();
            ensure!(
                mode & 0o022 == 0,
                "journal directory must not be writable by group or others"
            );
            let lock = Disk::open(&dir.join("lock"), true)?;
            fs2::FileExt::try_lock_exclusive(&lock.file).context("journal is in use")?;
            let mut pins = Vec::new();
            let mut fingerprints = Vec::new();
            let mut read = |name: &str, limit: u64| -> Result<Vec<u8>> {
                let path = dir.join(name);
                ensure!(
                    !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                    "journal artifacts must not be symlinks"
                );
                let pin = Pin::new(&path, false)?;
                let disk = pin.disk.as_ref().unwrap();
                let size = disk.stamp()?.size;
                ensure!(
                    size <= limit,
                    "journal artifact {name} exceeds its size limit"
                );
                let bytes = disk.read(0, size)?;
                pin.check()?;
                fingerprints.push(io::hash(&bytes));
                pins.push(pin);
                Ok(bytes)
            };
            let bytes = read("intent.json", 1 << 20)?;
            let committed = read("intent.sha256", 128)?;
            let hash = io::hash(&bytes);
            ensure!(
                committed == hash.as_bytes(),
                "journal intent checksum mismatch; no files changed"
            );
            let intent: Intent = serde_json::from_slice(&bytes)?;
            validate_intent(&intent)?;
            let log_tail = read("log.tail", MAX_BYTES)?;
            let index_tail = read("index.tail", MAX_BYTES)?;
            for (target, tail) in [(&intent.log, &log_tail), (&intent.index, &index_tail)] {
                ensure!(
                    tail.len() as u64 == target.before.size - target.keep
                        && io::hash(tail) == target.tail_sha256,
                    "saved tail length or checksum mismatch; no files changed"
                );
            }
            let journal = Self {
                dir,
                lock,
                pins,
                fingerprints,
                prepared_stamps: None,
                intent,
                hash,
                log_tail,
                index_tail,
            };
            journal.check()?;
            Ok(journal)
        }
        fn check(&self) -> Result<()> {
            self.lock.check_path()?;
            for (pin, fingerprint) in self.pins.iter().zip(&self.fingerprints) {
                pin.check()?;
                let disk = pin.disk.as_ref().unwrap();
                ensure!(
                    io::hash(&disk.read(0, disk.stamp()?.size)?) == *fingerprint,
                    "saved journal artifact changed; refusing mutation"
                );
                pin.check()?;
            }
            Ok(())
        }
        fn check_pair(&self, pair: &Pair) -> Result<()> {
            self.check()?;
            pair.check_paths()?;
            for (disk, target) in [
                (&pair.log, &self.intent.log),
                (&pair.index, &self.intent.index),
            ] {
                let now = disk.stamp()?;
                ensure!(
                    disk.path == target.path
                        && now.device == target.before.device
                        && now.inode == target.before.inode,
                    "target file identity changed; no files changed"
                );
                for pin in &self.pins {
                    ensure!(
                        !same(disk, pin.disk.as_ref().unwrap())?,
                        "journal artifact aliases a target file"
                    );
                }
                ensure!(
                    !same(disk, &self.lock)?,
                    "journal lock aliases a target file"
                );
            }
            Ok(())
        }
        fn marker(&self, name: &str, action: &str) -> Result<bool> {
            let path = self.dir.join(name);
            if !path.try_exists()? {
                return Ok(false);
            }
            let pin = Pin::new(&path, false)?;
            let disk = pin.disk.as_ref().unwrap();
            ensure!(
                disk.stamp()?.size <= 1 << 20,
                "journal marker exceeds 1 MiB"
            );
            let value: serde_json::Value =
                serde_json::from_slice(&disk.read(0, disk.stamp()?.size)?)?;
            pin.check()?;
            ensure!(
                value["intent_sha256"] == self.hash && value["action"] == action,
                "journal marker does not match the intent"
            );
            Ok(true)
        }
        fn publish(&self, name: &str, action: &str) -> Result<()> {
            atomic_file(
                &self.dir,
                name,
                &serde_json::to_vec_pretty(&json!({
                    "schema": 1, "intent_sha256": self.hash, "action": action,
                    "log": self.intent.log.path, "index": self.intent.index.path,
                    "last_block": self.intent.last_block,
                }))?,
            )
        }
    }

    fn validate_intent(intent: &Intent) -> Result<()> {
        ensure!(
            ((intent.schema == 1 && intent.inspection_sha256.is_none())
                || (intent.schema == 2
                    && intent.inspection_sha256.as_ref().is_some_and(
                        |h| h.len() == 64 && hex::decode(h).is_ok_and(|v| v.len() == 32)
                    )))
                && intent.operation == "trim_tail",
            "unsupported trim journal"
        );
        ensure!(
            intent.first_block > 0
                && intent.last_block >= intent.first_block
                && intent.last_block < u32::MAX,
            "invalid journal block range"
        );
        ensure!(
            intent.log.keep >= 48
                && intent.index.keep
                    == (u64::from(intent.last_block) - u64::from(intent.first_block) + 1) * 8,
            "invalid journal prefix geometry"
        );
        ensure!(
            intent.log.path.is_absolute()
                && intent.index.path.is_absolute()
                && Kind::from_path(&intent.log.path)? == Kind::Blocks,
            "invalid journal target paths"
        );
        let mut removed = 0u64;
        for target in [&intent.log, &intent.index] {
            ensure!(
                target.keep <= target.before.size,
                "journal would extend a file during trim"
            );
            removed = removed
                .checked_add(target.before.size - target.keep)
                .context("journal length overflow")?;
            for hash in [
                &target.before_sha256,
                &target.prefix_sha256,
                &target.tail_sha256,
            ] {
                ensure!(
                    hash.len() == 64 && hex::decode(hash)?.len() == 32,
                    "invalid journal hash"
                );
            }
        }
        ensure!(
            removed > 0 && removed <= MAX_BYTES,
            "saved suffixes must total 1 byte through 256 MiB"
        );
        Ok(())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Step {
        Prepared,
        LogTrimmed,
        IndexTrimmed,
        Verified,
        IndexRestored,
        LogRestored,
    }

    fn inspect(pair: &Pair, phase: &str) -> Result<Summary> {
        walk(
            &pair.log,
            Some(&pair.index),
            Kind::Blocks,
            &start(&pair.log, Kind::Blocks)?,
            None,
            phase,
            |_| Ok(()),
        )
    }

    fn prepare(
        pair: &Pair,
        dir: &Path,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Journal> {
        let before_log = live_stamp(&pair.log)?;
        let before_index = live_stamp(&pair.index)?;
        let summary = inspect(pair, "inspect trim input (1/2)")?;
        prepare_inspected(pair, dir, &summary, (before_log, before_index), None, hook)
    }

    fn trim_geometry(summary: &Summary) -> Result<(u64, u64)> {
        ensure!(
            summary.entries > 0,
            "trim requires at least one complete anchor block"
        );
        let keep_index = summary
            .entries
            .checked_mul(8)
            .context("inspection index length overflow")?;
        ensure!(
            summary
                .last_block
                .is_some_and(|last| last >= summary.first_block
                    && u64::from(last) - u64::from(summary.first_block) + 1 == summary.entries),
            "inspection block range mismatch"
        );
        ensure!(
            summary.index_bytes >= keep_index,
            "index lacks retained slots; use copy-out recovery to rebuild it"
        );
        ensure!(
            summary.index_discrepancies == summary.index_bytes.div_ceil(8) - summary.entries,
            "retained index prefix is inconsistent; use copy-out recovery to rebuild it"
        );
        let removed_index = summary.index_bytes - keep_index;
        ensure!(
            summary
                .excluded_suffix_bytes
                .checked_add(removed_index)
                .is_some_and(|n| n > 0 && n <= MAX_BYTES),
            "in-place trim requires 1 byte through 256 MiB of total suffix data; use copy-out recovery otherwise"
        );
        Ok((keep_index, removed_index))
    }

    fn prepare_inspected(
        pair: &Pair,
        dir: &Path,
        summary: &Summary,
        stamps: (LiveStamp, LiveStamp),
        inspection_sha256: Option<String>,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Journal> {
        let (before_log, before_index) = stamps;
        let (keep_index, removed_index) = trim_geometry(summary)?;
        ensure!(
            summary
                .physical_prefix_end
                .checked_add(summary.excluded_suffix_bytes)
                == Some(before_log.persisted.size)
                && summary.index_bytes == before_index.persisted.size,
            "inspection file geometry mismatch"
        );
        let log_tail = pair
            .log
            .read(summary.physical_prefix_end, summary.excluded_suffix_bytes)?;
        let index_tail = pair.index.read(keep_index, removed_index)?;
        ensure!(
            io::hash(&log_tail) == summary.excluded_suffix_sha256
                && Some(io::hash(&index_tail)) == summary.index_suffix_sha256,
            "input suffix changed during inspection"
        );
        pair.check_paths()?;
        ensure!(
            live_stamp(&pair.log)? == before_log && live_stamp(&pair.index)? == before_index,
            "input changed during inspection"
        );
        let intent = Intent {
            schema: if inspection_sha256.is_some() { 2 } else { 1 },
            operation: "trim_tail".into(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            first_block: summary.first_block,
            last_block: summary.last_block.unwrap(),
            indexed_head_hint: summary.indexed_head_hint,
            failure_reason: summary.failure.as_ref().map(|f| f.reason.clone()),
            inspection_sha256,
            log: Target {
                path: pair.log.path.clone(),
                before: before_log.persisted.clone(),
                keep: summary.physical_prefix_end,
                before_sha256: summary.input_sha256.clone(),
                prefix_sha256: summary.physical_prefix_sha256.clone(),
                tail_sha256: summary.excluded_suffix_sha256.clone(),
            },
            index: Target {
                path: pair.index.path.clone(),
                before: before_index.persisted.clone(),
                keep: keep_index,
                before_sha256: summary
                    .index_sha256
                    .clone()
                    .context("missing inspected index hash")?,
                prefix_sha256: summary
                    .index_prefix_sha256
                    .clone()
                    .context("missing inspected index prefix hash")?,
                tail_sha256: summary
                    .index_suffix_sha256
                    .clone()
                    .context("missing inspected index suffix hash")?,
            },
        };
        validate_intent(&intent)?;
        ensure!(
            fs2::available_space(io::parent_dir(dir))?
                >= log_tail.len() as u64 + index_tail.len() as u64 + (1 << 20),
            "insufficient journal space"
        );
        io::create_dir(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        io::sync_dir(dir)?;
        atomic_file(dir, "lock", &[])?;
        atomic_file(dir, "log.tail", &log_tail)?;
        atomic_file(dir, "index.tail", &index_tail)?;
        let bytes = serde_json::to_vec_pretty(&intent)?;
        atomic_file(dir, "intent.json", &bytes)?;
        atomic_file(dir, "intent.sha256", io::hash(&bytes).as_bytes())?;
        let mut journal = Journal::load(dir)?;
        journal.prepared_stamps = Some((before_log, before_index));
        journal.check_pair(pair)?;
        unchanged(pair, journal.prepared_stamps.as_ref().unwrap())?;
        ensure!(
            pair.log.stamp()? == journal.intent.log.before
                && pair.index.stamp()? == journal.intent.index.before,
            "input changed while saving the journal; no files trimmed"
        );
        eprintln!(
            "trim-tail: retaining blocks {}..{}; saved {} log bytes and {} index bytes in {}",
            journal.intent.first_block,
            journal.intent.last_block,
            log_tail.len(),
            index_tail.len(),
            journal.dir.display()
        );
        hook(Step::Prepared)?;
        Ok(journal)
    }

    fn checked_state(
        disk: &Disk,
        target: &Target,
        tail: &[u8],
        partial: bool,
    ) -> Result<LiveStamp> {
        let before = live_stamp(disk)?;
        ensure!(
            before.persisted.size >= target.keep
                && before.persisted.size <= target.before.size
                && (partial
                    || before.persisted.size == target.keep
                    || before.persisted.size == target.before.size),
            "target has an unexpected file length"
        );
        eprintln!("trim-tail: verify saved prefix of {}", disk.path.display());
        let mut hash = Sha256::new();
        let mut at = 0;
        let mut progress = 1u64 << 30;
        while at < target.keep {
            let count = (target.keep - at).min(16 << 20);
            hash.update(disk.read(at, count)?);
            at += count;
            if at >= progress {
                eprintln!(
                    "trim-tail [verify saved prefix]: checked {} of {}",
                    crate::report::bytes(at),
                    crate::report::bytes(target.keep)
                );
                progress = at.saturating_add(1 << 30);
            }
        }
        ensure!(
            hex::encode(hash.clone().finalize()) == target.prefix_sha256,
            "retained prefix changed; refusing mutation"
        );
        let present = (before.persisted.size - target.keep) as usize;
        let actual = disk.read(target.keep, present as u64)?;
        ensure!(
            actual == tail[..present],
            "target suffix differs from saved bytes; refusing mutation"
        );
        hash.update(actual);
        if before.persisted.size == target.before.size {
            ensure!(
                hex::encode(hash.finalize()) == target.before_sha256,
                "original file fingerprint mismatch"
            );
        }
        disk.check_path()?;
        ensure!(
            live_stamp(disk)? == before,
            "target changed during verification"
        );
        Ok(before)
    }

    fn unchanged(pair: &Pair, stamps: &(LiveStamp, LiveStamp)) -> Result<()> {
        ensure!(
            live_stamp(&pair.log)? == stamps.0 && live_stamp(&pair.index)? == stamps.1,
            "target changed after verification; refusing mutation"
        );
        for disk in [&pair.log, &pair.index] {
            ensure!(
                disk.other_openers()?.is_empty(),
                "another process opened a target; stop every reader/writer"
            );
        }
        pair.check_paths()
    }

    fn execute(
        pair: &Pair,
        journal: &Journal,
        rollback: bool,
        prepared: bool,
        hook: &mut impl FnMut(Step) -> Result<()>,
    ) -> Result<Out> {
        journal.check_pair(pair)?;
        let rollback_started = journal.marker("rollback-intent.json", "rollback")?;
        ensure!(
            rollback || !rollback_started,
            "rollback has started; resume with --rollback, never restart the trim"
        );
        let receipt = if rollback {
            "rollback-receipt.json"
        } else {
            "receipt.json"
        };
        let action = if rollback { "rollback" } else { "trim" };
        let complete = journal.marker(receipt, action)?;
        let mut stamps = if !prepared || complete || rollback {
            (
                checked_state(
                    &pair.log,
                    &journal.intent.log,
                    &journal.log_tail,
                    rollback_started,
                )?,
                checked_state(
                    &pair.index,
                    &journal.intent.index,
                    &journal.index_tail,
                    rollback_started,
                )?,
            )
        } else {
            journal
                .prepared_stamps
                .clone()
                .context("missing prepared verification stamps")?
        };
        unchanged(pair, &stamps)?;
        if complete {
            for (disk, target) in [
                (&pair.log, &journal.intent.log),
                (&pair.index, &journal.intent.index),
            ] {
                let expected = if rollback {
                    target.before.size
                } else {
                    target.keep
                };
                ensure!(
                    disk.stamp()?.size == expected,
                    "completed journal no longer matches the target"
                );
            }
        } else if rollback {
            if !rollback_started {
                journal.publish("rollback-intent.json", "rollback")?;
            }
            for (disk, target, tail, step) in [
                (
                    &pair.index,
                    &journal.intent.index,
                    &journal.index_tail,
                    Step::IndexRestored,
                ),
                (
                    &pair.log,
                    &journal.intent.log,
                    &journal.log_tail,
                    Step::LogRestored,
                ),
            ] {
                journal.check_pair(pair)?;
                unchanged(pair, &stamps)?;
                let at = disk.stamp()?.size;
                if at < target.before.size {
                    disk.write_extent(at, &tail[(at - target.keep) as usize..])?;
                }
                disk.file.sync_all()?;
                ensure!(
                    disk.stamp()?.size == target.before.size,
                    "rollback write length mismatch"
                );
                match step {
                    Step::IndexRestored => {
                        ensure!(
                            live_stamp(&pair.log)? == stamps.0,
                            "log changed during index restore"
                        );
                        stamps.1 = live_stamp(&pair.index)?;
                    }
                    Step::LogRestored => {
                        ensure!(
                            live_stamp(&pair.index)? == stamps.1,
                            "index changed during log restore"
                        );
                        stamps.0 = live_stamp(&pair.log)?;
                    }
                    _ => unreachable!(),
                }
                hook(step)?;
            }
            unchanged(pair, &stamps)?;
            checked_state(&pair.log, &journal.intent.log, &journal.log_tail, false)?;
            checked_state(
                &pair.index,
                &journal.intent.index,
                &journal.index_tail,
                false,
            )?;
            ensure!(
                pair.log.stamp()?.size == journal.intent.log.before.size
                    && pair.index.stamp()?.size == journal.intent.index.before.size,
                "rollback read-back length mismatch"
            );
            hook(Step::Verified)?;
            journal.check_pair(pair)?;
            unchanged(pair, &stamps)?;
            journal.publish(receipt, action)?;
        } else {
            for (disk, target, step) in [
                (&pair.log, &journal.intent.log, Step::LogTrimmed),
                (&pair.index, &journal.intent.index, Step::IndexTrimmed),
            ] {
                journal.check_pair(pair)?;
                unchanged(pair, &stamps)?;
                if disk.stamp()?.size != target.keep {
                    disk.file.set_len(target.keep)?;
                }
                disk.file.sync_all()?;
                ensure!(disk.stamp()?.size == target.keep, "trim length mismatch");
                match step {
                    Step::LogTrimmed => {
                        ensure!(
                            live_stamp(&pair.index)? == stamps.1,
                            "index changed during log trim"
                        );
                        stamps.0 = live_stamp(&pair.log)?;
                    }
                    Step::IndexTrimmed => {
                        ensure!(
                            live_stamp(&pair.log)? == stamps.0,
                            "log changed during index trim"
                        );
                        stamps.1 = live_stamp(&pair.index)?;
                    }
                    _ => unreachable!(),
                }
                hook(step)?;
            }
            unchanged(pair, &stamps)?;
            let summary = inspect(
                pair,
                if journal.intent.inspection_sha256.is_some() {
                    "verify trimmed pair (1/1; inspection reused)"
                } else {
                    "verify trimmed pair (2/2)"
                },
            )?;
            ensure!(
                summary.failure.is_none()
                    && summary.index_discrepancies == 0
                    && summary.last_block == Some(journal.intent.last_block)
                    && summary.input_sha256 == journal.intent.log.prefix_sha256
                    && summary.index_sha256.as_ref() == Some(&journal.intent.index.prefix_sha256),
                "trimmed pair failed read-back; keep journal and resume or roll back"
            );
            ensure!(
                live_stamp(&pair.log)? == stamps.0 && live_stamp(&pair.index)? == stamps.1,
                "target changed during read-back"
            );
            hook(Step::Verified)?;
            journal.check_pair(pair)?;
            unchanged(pair, &stamps)?;
            journal.publish(receipt, action)?;
        }
        journal.check_pair(pair)?;
        unchanged(pair, &stamps)?;
        io::sync_dir(&journal.dir)?;
        let output = json!({
            "ok": true, "operation": "in_place_tail_trim", "action": action, "journal": journal.dir,
            "intent_sha256": journal.hash, "receipt": journal.dir.join(receipt),
            "kept_blocks": [journal.intent.first_block, journal.intent.last_block],
            "indexed_head_hint": journal.intent.indexed_head_hint,
            "log_bytes_removed": journal.intent.log.before.size - journal.intent.log.keep,
            "index_bytes_removed": journal.intent.index.before.size - journal.intent.index.keep,
            "log_size": pair.log.stamp()?.size, "index_size": pair.index.stamp()?.size,
            "already_complete": complete,
            "inspection_reused": journal.intent.inspection_sha256.is_some(),
            "inspection_sha256": journal.intent.inspection_sha256,
            "missing_history": "Trimming keeps only the complete prefix. Rollback restores the saved damaged originals, not missing history.",
        });
        Ok(Out {
            json: output,
            text: format!(
                "Tail {action} verified. Checked prefix: blocks {}..{}.\n\
                 Saved suffixes: {} log bytes, {} index bytes. Journal: {}.\n\
                 Keep the journal and both saved suffixes. Rollback restores the damaged originals, not missing history.\n",
                journal.intent.first_block,
                journal.intent.last_block,
                journal.log_tail.len(),
                journal.index_tail.len(),
                journal.dir.display()
            ),
        })
    }

    pub fn run(log: &Path, index: Option<&Path>, journal: &Path, stopped: bool) -> Result<Out> {
        run_with(log, index, journal, stopped, |_| Ok(()))
    }
    pub fn run_inspected(
        log: &Path,
        index: Option<&Path>,
        journal: &Path,
        inspection: &Path,
        stopped: bool,
    ) -> Result<Out> {
        run_inspected_with(log, index, journal, inspection, stopped, |_| Ok(()))
    }
    fn run_inspected_with(
        log: &Path,
        index: Option<&Path>,
        journal: &Path,
        inspection: &Path,
        stopped: bool,
        mut hook: impl FnMut(Step) -> Result<()>,
    ) -> Result<Out> {
        ensure!(
            !journal.try_exists()?,
            "journal already exists; use resume-tail-trim or choose a new directory"
        );
        let pair = Pair::open(
            log,
            &index
                .map(Path::to_path_buf)
                .unwrap_or_else(|| log.with_extension("index")),
            stopped,
        )?;
        let saved = SavedInspection::load(inspection, &pair)?;
        let checkpoint = &saved.checkpoint;
        eprintln!(
            "trim-tail: reusing saved inspection; skipping input scan, full post-trim verification remains"
        );
        let journal = prepare_inspected(
            &pair,
            journal,
            &checkpoint.summary,
            (checkpoint.log_stamp.clone(), checkpoint.index_stamp.clone()),
            Some(saved.hash.clone()),
            &mut hook,
        )?;
        saved.check()?;
        execute(&pair, &journal, false, true, &mut hook)
    }
    fn run_with(
        log: &Path,
        index: Option<&Path>,
        journal: &Path,
        stopped: bool,
        mut hook: impl FnMut(Step) -> Result<()>,
    ) -> Result<Out> {
        ensure!(
            !journal.try_exists()?,
            "journal already exists; use resume-tail-trim or choose a new directory"
        );
        let pair = Pair::open(
            log,
            &index
                .map(Path::to_path_buf)
                .unwrap_or_else(|| log.with_extension("index")),
            stopped,
        )?;
        let saved = prepare(&pair, journal, &mut hook)?;
        execute(&pair, &saved, false, true, &mut hook)
    }
    pub fn resume(dir: &Path, rollback: bool, stopped: bool) -> Result<Out> {
        resume_with(dir, rollback, stopped, |_| Ok(()))
    }
    fn resume_with(
        dir: &Path,
        rollback: bool,
        stopped: bool,
        mut hook: impl FnMut(Step) -> Result<()>,
    ) -> Result<Out> {
        ensure!(stopped, "--node-stopped is required");
        let journal = Journal::load(dir)?;
        let pair = Pair::open(
            &journal.intent.log.path,
            &journal.intent.index.path,
            stopped,
        )?;
        execute(&pair, &journal, rollback, false, &mut hook)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tail::tests::Fixture;

        fn damaged() -> (Fixture, Vec<u8>, Vec<u8>) {
            let f = Fixture::new();
            let original = f.bytes[..f.offsets[3] as usize + 30].to_vec();
            fs::write(&f.log, &original).unwrap();
            let index = fs::read(f.log.with_extension("index")).unwrap();
            (f, original, index)
        }
        fn fail_at(wanted: Step) -> impl FnMut(Step) -> Result<()> {
            move |at| {
                if at == wanted {
                    bail!("injected interruption at {at:?}");
                }
                Ok(())
            }
        }
        fn assert_trimmed(f: &Fixture) {
            assert_eq!(fs::read(&f.log).unwrap(), f.bytes[..f.offsets[3] as usize]);
            assert_eq!(
                fs::metadata(f.log.with_extension("index")).unwrap().len(),
                24
            );
        }
        fn checkpoint(f: &Fixture) -> PathBuf {
            let dir = f.dir.path().join("inspection");
            let guard = Inspection::begin(
                &Disk::open(&f.log, false).unwrap(),
                &Disk::open(&f.log.with_extension("index"), false).unwrap(),
                &dir,
                true,
            )
            .unwrap();
            let summary = inspect(&guard.pair, "test inspection").unwrap();
            guard.save(&summary).unwrap();
            dir
        }
        #[test]
        fn checkpoint_reuse_trims_and_rolls_back_without_checkpoint_dependency() {
            let (f, original, index) = damaged();
            let saved = checkpoint(&f);
            assert!(resume(&saved, false, true).is_err());
            assert_eq!(fs::read(&f.log).unwrap(), original);
            assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
            let journal = f.dir.path().join("trim");
            let out = run_inspected(&f.log, None, &journal, &saved, true).unwrap();
            assert!(out.json["inspection_reused"].as_bool().unwrap());
            assert_trimmed(&f);
            fs::remove_dir_all(&saved).unwrap();
            resume(&journal, true, true).unwrap();
            assert_eq!(fs::read(&f.log).unwrap(), original);
            assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
        }
        #[test]
        fn checkpoint_trim_resumes_and_rolls_back_at_every_durable_boundary() {
            for rollback in [false, true] {
                for step in [
                    Step::Prepared,
                    Step::LogTrimmed,
                    Step::IndexTrimmed,
                    Step::Verified,
                ] {
                    let (f, original, index) = damaged();
                    let saved = checkpoint(&f);
                    let journal = f.dir.path().join("trim");
                    assert!(
                        run_inspected_with(&f.log, None, &journal, &saved, true, fail_at(step))
                            .is_err()
                    );
                    assert!(!journal.join("receipt.json").exists());
                    fs::remove_dir_all(saved).unwrap();
                    resume(&journal, rollback, true).unwrap();
                    if rollback {
                        assert_eq!(fs::read(&f.log).unwrap(), original);
                        assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
                    } else {
                        assert_trimmed(&f);
                    }
                }
            }
        }
        #[test]
        fn checkpoint_changes_and_target_races_after_journaling_refuse_mutation() {
            for what in ["checkpoint", "log", "index"] {
                let (f, original, index) = damaged();
                let saved = checkpoint(&f);
                let journal = f.dir.path().join("trim");
                let result = run_inspected_with(&f.log, None, &journal, &saved, true, |step| {
                    if step == Step::Prepared {
                        let path = match what {
                            "checkpoint" => saved.join("inspection.json"),
                            "log" => f.log.clone(),
                            _ => f.log.with_extension("index"),
                        };
                        OpenOptions::new()
                            .append(true)
                            .open(path)?
                            .write_all(b" ")?;
                    }
                    Ok(())
                });
                assert!(result.is_err(), "{what}");
                assert!(!journal.join("receipt.json").exists());
                assert!(fs::read(&f.log).unwrap().starts_with(&original));
                assert!(
                    fs::read(f.log.with_extension("index"))
                        .unwrap()
                        .starts_with(&index)
                );
            }
        }
        #[test]
        fn reused_inspection_still_checks_entire_prefix_hash_before_a_receipt() {
            let (f, _, _) = damaged();
            let saved = checkpoint(&f);
            // Simulate a wrong historical digest while leaving the bounded anchors intact.
            let path = saved.join("inspection.json");
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["summary"]["physical_prefix_sha256"] = json!("0".repeat(64));
            let bytes = serde_json::to_vec(&value).unwrap();
            fs::write(path, &bytes).unwrap();
            fs::write(saved.join("inspection.sha256"), io::hash(&bytes)).unwrap();
            let journal = f.dir.path().join("trim");
            let error = run_inspected(&f.log, None, &journal, &saved, true)
                .err()
                .unwrap();
            assert!(format!("{error:#}").contains("failed read-back"));
            assert!(!journal.join("receipt.json").exists());
            assert_trimmed(&f);
        }
        #[test]
        fn stale_or_incomplete_checkpoints_refuse_before_creating_a_journal() {
            for what in [
                "append", "log", "index", "replace", "checksum", "partial", "boot", "binary",
                "schema", "anchor", "preamble",
            ] {
                let (f, _, _) = damaged();
                let saved = checkpoint(&f);
                match what {
                    "append" => OpenOptions::new()
                        .append(true)
                        .open(&f.log)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap(),
                    "log" | "index" => {
                        let path = if what == "log" {
                            f.log.clone()
                        } else {
                            f.log.with_extension("index")
                        };
                        let modified = fs::metadata(&path).unwrap().modified().unwrap();
                        let mut bytes = fs::read(&path).unwrap();
                        bytes[8] ^= 1;
                        fs::write(&path, bytes).unwrap();
                        OpenOptions::new()
                            .write(true)
                            .open(path)
                            .unwrap()
                            .set_modified(modified)
                            .unwrap();
                    }
                    "replace" => {
                        fs::rename(&f.log, f.log.with_extension("old")).unwrap();
                        fs::copy(f.log.with_extension("old"), &f.log).unwrap();
                    }
                    "checksum" => fs::write(saved.join("inspection.sha256"), b"wrong").unwrap(),
                    "partial" => fs::remove_file(saved.join("inspection.sha256")).unwrap(),
                    _ => {
                        let path = saved.join("inspection.json");
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                        match what {
                            "boot" => value["boot_id"] = json!("old-boot"),
                            "binary" => value["executable_sha256"] = json!("0".repeat(64)),
                            "schema" => value["schema"] = json!(999),
                            "anchor" => value["anchor_sha256"] = json!("0".repeat(64)),
                            "preamble" => value["preamble_sha256"] = json!("0".repeat(64)),
                            _ => unreachable!(),
                        }
                        let bytes = serde_json::to_vec(&value).unwrap();
                        fs::write(&path, &bytes).unwrap();
                        fs::write(saved.join("inspection.sha256"), io::hash(&bytes)).unwrap();
                    }
                }
                let before = (
                    fs::read(&f.log).unwrap(),
                    fs::read(f.log.with_extension("index")).unwrap(),
                );
                let journal = f.dir.path().join("trim");
                assert!(
                    run_inspected(&f.log, None, &journal, &saved, true).is_err(),
                    "{what}"
                );
                assert!(!journal.exists(), "{what}");
                assert_eq!(fs::read(&f.log).unwrap(), before.0, "{what}");
                assert_eq!(
                    fs::read(f.log.with_extension("index")).unwrap(),
                    before.1,
                    "{what}"
                );
            }
        }
        #[test]
        fn trims_only_suffixes_and_saves_exact_bytes() {
            let (f, original, index) = damaged();
            let dir = f.dir.path().join("trim");
            let out = run(&f.log, None, &dir, true).unwrap();
            assert_eq!(out.json["log_bytes_removed"], 30);
            assert_eq!(out.json["index_bytes_removed"], 8);
            assert_trimmed(&f);
            assert_eq!(
                fs::read(dir.join("log.tail")).unwrap(),
                original[f.offsets[3] as usize..]
            );
            assert_eq!(fs::read(dir.join("index.tail")).unwrap(), index[24..]);
            assert!(dir.join("receipt.json").exists());
            let before = fs::metadata(&f.log).unwrap().modified().unwrap();
            assert!(
                resume(&dir, false, true).unwrap().json["already_complete"]
                    .as_bool()
                    .unwrap()
            );
            assert_eq!(fs::metadata(&f.log).unwrap().modified().unwrap(), before);
        }
        #[test]
        fn interrupted_trim_resumes_at_every_durable_boundary() {
            for step in [
                Step::Prepared,
                Step::LogTrimmed,
                Step::IndexTrimmed,
                Step::Verified,
            ] {
                let (f, _, _) = damaged();
                let dir = f.dir.path().join("trim");
                assert!(run_with(&f.log, None, &dir, true, fail_at(step)).is_err());
                assert!(dir.join("intent.sha256").exists());
                assert!(!dir.join("receipt.json").exists());
                resume(&dir, false, true).unwrap();
                assert_trimmed(&f);
            }
        }
        #[test]
        fn interrupted_trim_can_be_rolled_back_at_every_boundary() {
            for step in [
                Step::Prepared,
                Step::LogTrimmed,
                Step::IndexTrimmed,
                Step::Verified,
            ] {
                let (f, original, index) = damaged();
                let dir = f.dir.path().join("trim");
                assert!(run_with(&f.log, None, &dir, true, fail_at(step)).is_err());
                resume(&dir, true, true).unwrap();
                assert_eq!(fs::read(&f.log).unwrap(), original);
                assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
                assert!(resume(&dir, false, true).is_err());
                assert!(
                    resume(&dir, true, true).unwrap().json["already_complete"]
                        .as_bool()
                        .unwrap()
                );
            }
        }
        #[test]
        fn interrupted_rollback_resumes_and_never_restarts_trimming() {
            for step in [Step::IndexRestored, Step::LogRestored, Step::Verified] {
                let (f, original, index) = damaged();
                let dir = f.dir.path().join("trim");
                run(&f.log, None, &dir, true).unwrap();
                assert!(resume_with(&dir, true, true, fail_at(step)).is_err());
                assert!(resume(&dir, false, true).is_err());
                resume(&dir, true, true).unwrap();
                assert_eq!(fs::read(&f.log).unwrap(), original);
                assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
            }
        }
        #[test]
        fn partial_tail_restore_is_resumable() {
            let (f, original, index) = damaged();
            let dir = f.dir.path().join("trim");
            run(&f.log, None, &dir, true).unwrap();
            let journal = Journal::load(&dir).unwrap();
            journal.publish("rollback-intent.json", "rollback").unwrap();
            drop(journal);
            let mut file = OpenOptions::new().append(true).open(&f.log).unwrap();
            file.write_all(&original[f.offsets[3] as usize..f.offsets[3] as usize + 5])
                .unwrap();
            file.sync_all().unwrap();
            drop(file);
            resume(&dir, true, true).unwrap();
            assert_eq!(fs::read(&f.log).unwrap(), original);
            assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
        }
        #[test]
        fn changed_prefix_paths_intent_and_saved_bytes_refuse_mutation() {
            for what in ["prefix", "path", "intent", "tail"] {
                let (f, _, _) = damaged();
                let dir = f.dir.path().join("trim");
                assert!(run_with(&f.log, None, &dir, true, fail_at(Step::LogTrimmed)).is_err());
                match what {
                    "prefix" => {
                        let mut bytes = fs::read(&f.log).unwrap();
                        bytes[8] ^= 1;
                        fs::write(&f.log, bytes).unwrap();
                    }
                    "path" => {
                        let bytes = fs::read(&f.log).unwrap();
                        fs::rename(&f.log, f.log.with_extension("old")).unwrap();
                        fs::write(&f.log, bytes).unwrap();
                    }
                    "intent" => {
                        let mut bytes = fs::read(dir.join("intent.json")).unwrap();
                        bytes.push(b' ');
                        fs::write(dir.join("intent.json"), bytes).unwrap();
                    }
                    "tail" => {
                        let mut bytes = fs::read(dir.join("log.tail")).unwrap();
                        bytes[0] ^= 1;
                        fs::write(dir.join("log.tail"), bytes).unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = (
                    fs::read(&f.log).unwrap(),
                    fs::read(f.log.with_extension("index")).unwrap(),
                );
                assert!(resume(&dir, false, true).is_err(), "{what}");
                assert!(resume(&dir, true, true).is_err(), "{what}");
                assert_eq!(fs::read(&f.log).unwrap(), before.0);
                assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), before.1);
            }
        }
        #[test]
        fn mismatched_missing_or_aliased_indexes_and_missing_stop_ack_are_refused() {
            for what in ["wrong", "missing", "alias", "ack", "lock"] {
                let (f, original, _) = damaged();
                let path = f.log.with_extension("index");
                let mut lock = None;
                match what {
                    "wrong" => {
                        let mut bytes = fs::read(&path).unwrap();
                        bytes[8] ^= 1;
                        fs::write(&path, bytes).unwrap();
                    }
                    "missing" => fs::remove_file(&path).unwrap(),
                    "alias" => {
                        fs::remove_file(&path).unwrap();
                        fs::hard_link(&f.log, &path).unwrap();
                    }
                    "lock" => {
                        let file = OpenOptions::new().read(true).open(&f.log).unwrap();
                        fs2::FileExt::try_lock_exclusive(&file).unwrap();
                        lock = Some(file);
                    }
                    _ => (),
                }
                let dir = f.dir.path().join("trim");
                assert!(run(&f.log, None, &dir, what != "ack").is_err(), "{what}");
                assert!(!dir.exists());
                assert_eq!(fs::read(&f.log).unwrap(), original);
                drop(lock);
            }
        }
        #[test]
        fn input_changes_after_journal_commit_are_not_truncated() {
            let (f, _, _) = damaged();
            let dir = f.dir.path().join("trim");
            let result = run_with(&f.log, None, &dir, true, |step| {
                if step == Step::Prepared {
                    let mut file = OpenOptions::new().append(true).open(&f.log)?;
                    file.write_all(b"unexpected append")?;
                }
                Ok(())
            });
            assert!(result.is_err());
            assert!(fs::read(&f.log).unwrap().ends_with(b"unexpected append"));
            assert!(!dir.join("receipt.json").exists());
        }
        #[test]
        fn same_size_saved_artifact_changes_with_restored_mtime_refuse_trim() {
            for name in ["log.tail", "index.tail", "intent.json", "intent.sha256"] {
                let (f, original, index) = damaged();
                let dir = f.dir.path().join("trim");
                let result = run_with(&f.log, None, &dir, true, |step| {
                    if step == Step::Prepared {
                        let path = dir.join(name);
                        let modified = fs::metadata(&path)?.modified()?;
                        let mut bytes = fs::read(&path)?;
                        bytes[0] ^= 1;
                        fs::write(&path, bytes)?;
                        OpenOptions::new()
                            .write(true)
                            .open(&path)?
                            .set_times(fs::FileTimes::new().set_modified(modified))?;
                    }
                    Ok(())
                });
                assert!(result.is_err(), "{name}");
                assert!(!dir.join("receipt.json").exists());
                assert_eq!(fs::read(&f.log).unwrap(), original);
                assert_eq!(fs::read(f.log.with_extension("index")).unwrap(), index);
            }
        }
        #[test]
        fn restored_mtime_target_changes_refuse_mutation_and_receipts() {
            for step in [Step::Prepared, Step::Verified] {
                for rollback in [false, true] {
                    if rollback && step == Step::Prepared {
                        continue;
                    }
                    for index in [false, true] {
                        let (f, original, _) = damaged();
                        let dir = f.dir.path().join("trim");
                        if rollback {
                            run(&f.log, None, &dir, true).unwrap();
                        }
                        let hook = |at| {
                            if at == step {
                                let path = if index {
                                    f.log.with_extension("index")
                                } else {
                                    f.log.clone()
                                };
                                let modified = fs::metadata(&path)?.modified()?;
                                let mut bytes = fs::read(&path)?;
                                bytes[8] ^= 1;
                                fs::write(&path, bytes)?;
                                OpenOptions::new()
                                    .write(true)
                                    .open(&path)?
                                    .set_times(fs::FileTimes::new().set_modified(modified))?;
                            }
                            Ok(())
                        };
                        let result = if rollback {
                            resume_with(&dir, true, true, hook)
                        } else {
                            run_with(&f.log, None, &dir, true, hook)
                        };
                        assert!(result.is_err());
                        assert!(
                            !dir.join(if rollback {
                                "rollback-receipt.json"
                            } else {
                                "receipt.json"
                            })
                            .exists()
                        );
                        if step == Step::Prepared {
                            assert_eq!(fs::metadata(&f.log).unwrap().len(), original.len() as u64);
                        }
                    }
                }
            }
        }
        #[test]
        fn changes_after_readback_never_publish_a_receipt() {
            for rollback in [false, true] {
                for index in [false, true] {
                    let (f, _, _) = damaged();
                    let dir = f.dir.path().join("trim");
                    if rollback {
                        run(&f.log, None, &dir, true).unwrap();
                    }
                    let hook = |step| {
                        if step == Step::Verified {
                            let path = if index {
                                f.log.with_extension("index")
                            } else {
                                f.log.clone()
                            };
                            let mut file = OpenOptions::new().append(true).open(path)?;
                            file.write_all(b"unexpected append")?;
                        }
                        Ok(())
                    };
                    let result = if rollback {
                        resume_with(&dir, true, true, hook)
                    } else {
                        run_with(&f.log, None, &dir, true, hook)
                    };
                    assert!(result.is_err());
                    assert!(
                        !dir.join(if rollback {
                            "rollback-receipt.json"
                        } else {
                            "receipt.json"
                        })
                        .exists()
                    );
                }
            }
        }
        #[test]
        fn an_intact_log_with_only_excess_index_bytes_can_be_trimmed_and_undone() {
            let f = Fixture::new();
            let path = f.log.with_extension("index");
            let mut index = fs::read(&path).unwrap();
            index.extend_from_slice(&[0; 11]);
            fs::write(&path, &index).unwrap();
            let dir = f.dir.path().join("trim");
            let out = run(&f.log, None, &dir, true).unwrap();
            assert_eq!(out.json["log_bytes_removed"], 0);
            assert_eq!(out.json["index_bytes_removed"], 11);
            assert_eq!(fs::read(&f.log).unwrap(), f.bytes);
            resume(&dir, true, true).unwrap();
            assert_eq!(fs::read(&path).unwrap(), index);
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::Inspection;
#[cfg(target_os = "linux")]
pub use linux::{resume, run, run_inspected};

#[cfg(not(target_os = "linux"))]
pub fn run_inspected(
    _: &std::path::Path,
    _: Option<&std::path::Path>,
    _: &std::path::Path,
    _: &std::path::Path,
    _: bool,
) -> anyhow::Result<crate::report::Out> {
    anyhow::bail!("saved inspection reuse is supported on Linux only")
}

#[cfg(not(target_os = "linux"))]
pub fn run(
    _: &std::path::Path,
    _: Option<&std::path::Path>,
    _: &std::path::Path,
    _: bool,
) -> anyhow::Result<crate::report::Out> {
    anyhow::bail!(
        "in-place tail trim is supported on Linux only; use recover-tail copy-out on this platform"
    )
}
#[cfg(not(target_os = "linux"))]
pub fn resume(_: &std::path::Path, _: bool, _: bool) -> anyhow::Result<crate::report::Out> {
    anyhow::bail!("in-place tail trim is supported on Linux only")
}
