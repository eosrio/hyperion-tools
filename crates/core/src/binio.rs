//! Binary I/O shared by the strict archive readers ([`crate::shiplog`], [`crate::blocklog`])
//! and the SHiP request decoder ([`crate::protocol`]).
//!
//!   * [`PosFile`] — a cheaply clonable, read-only file handle with exact positional reads
//!     (`pread` on Unix, `seek_read` on Windows), so any number of threads read one file
//!     without a shared cursor. The length is taken once at open: the inputs are frozen logs,
//!     and a read past it is an error naming the file, never a short read.
//!   * [`FileCache`] — an LRU of open [`PosFile`]s, so a catalog of thousands of retained log
//!     files keeps at most `cap` descriptors open. Evicting an entry only drops the cache's
//!     handle: a clone already handed out keeps reading until it is dropped.
//!   * [`FcReader`] — a cursor with fc `raw::unpack` semantics: little-endian integers, a
//!     varuint32 that accepts non-canonical encodings and stops after five bytes, and a bool
//!     that must be 0 or 1. Trailing bytes are left to the caller (fc ignores them).
//!   * [`num_from_id`] — the block number embedded in a block id.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail, ensure};

/// A read-only file opened for positional reads. Clones share the descriptor.
#[derive(Clone, Debug)]
pub struct PosFile {
    file: Arc<File>,
    len: u64,
    path: Arc<Path>,
}

impl PosFile {
    /// Open `path` read-only and record its length.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let meta = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?;
        ensure!(meta.is_file(), "{} is not a regular file", path.display());
        Ok(Self {
            file: Arc::new(file),
            len: meta.len(),
            path: Arc::from(path),
        })
    }

    /// The length recorded at open.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// A read-only view ending at a complete record boundary chosen by the caller. The
    /// original descriptor stays open, so a rename never redirects reads to a new file.
    /// This is a bound, not a filesystem snapshot: bytes inside it must be irreversible.
    pub fn prefix(mut self, len: u64) -> Result<Self> {
        ensure!(
            len <= self.len,
            "prefix of {} exceeds its length",
            self.path.display()
        );
        self.len = len;
        Ok(self)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fill `buf` from offset `off`. The whole range must lie within the recorded length.
    pub fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        let n = buf.len();
        self.check_range(off, n)?;
        pread_exact(&self.file, off, buf)
            .with_context(|| format!("read {n} bytes at offset {off} of {}", self.path.display()))
    }

    /// `len` bytes from offset `off`. The range is checked before allocating, so a corrupt
    /// length can never allocate more than the file holds.
    pub fn read_vec(&self, off: u64, len: usize) -> Result<Vec<u8>> {
        self.check_range(off, len)?;
        let mut buf = vec![0; len];
        self.read_exact_at(off, &mut buf)?;
        Ok(buf)
    }

    /// Little-endian `u32` at `off`.
    pub fn u32_at(&self, off: u64) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array_at(off)?))
    }

    /// Little-endian `u64` at `off`.
    pub fn u64_at(&self, off: u64) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array_at(off)?))
    }

    fn array_at<const N: usize>(&self, off: u64) -> Result<[u8; N]> {
        let mut b = [0; N];
        self.read_exact_at(off, &mut b)?;
        Ok(b)
    }

    fn check_range(&self, off: u64, n: usize) -> Result<()> {
        let end = u64::try_from(n).ok().and_then(|n| off.checked_add(n));
        ensure!(
            end.is_some_and(|end| end <= self.len),
            "read of {n} bytes at offset {off} is past the end of {} ({} bytes)",
            self.path.display(),
            self.len
        );
        Ok(())
    }
}

#[cfg(unix)]
fn pread_exact(file: &File, off: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, off)
}

/// `seek_read` moves the handle's cursor, but nothing here uses the cursor, so concurrent
/// positional reads through clones stay independent.
#[cfg(windows)]
fn pread_exact(file: &File, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, off) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short positional read",
                ));
            }
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// At most `cap` open files, least recently used closed first. Lookups are by the exact path
/// given (catalogs always pass the same path for the same file).
pub struct FileCache {
    cap: usize,
    lru: Mutex<Lru>,
    pinned: Option<HashMap<PathBuf, PosFile>>,
}

#[derive(Default)]
struct Lru {
    clock: u64,
    open: HashMap<PathBuf, (PosFile, u64)>,
}

impl Lru {
    fn touch(&mut self, path: &Path) -> Option<PosFile> {
        self.clock += 1;
        let (file, used) = self.open.get_mut(path)?;
        *used = self.clock;
        Some(file.clone())
    }

    /// Linear in `cap`, paid only on a miss, next to an `open` syscall.
    fn evict_oldest(&mut self) {
        let oldest = self
            .open
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(path, _)| path.clone());
        if let Some(path) = oldest {
            self.open.remove(&path);
        }
    }
}

impl FileCache {
    /// A cache holding at most `cap` open files (at least one).
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap: cap.max(1),
            lru: Mutex::default(),
            pinned: None,
        })
    }

    /// Fixed file views for an in-place live-node test. No eviction, reopen or fallback:
    /// callers must bound the number of descriptors and select irreversible records.
    pub fn pinned(files: Vec<PosFile>) -> Result<Arc<Self>> {
        let mut pinned = HashMap::new();
        for file in files {
            let path = file.path().to_path_buf();
            ensure!(
                pinned.insert(path.clone(), file).is_none(),
                "duplicate pinned file {}",
                path.display()
            );
        }
        Ok(Arc::new(Self {
            cap: 0,
            lru: Mutex::default(),
            pinned: Some(pinned),
        }))
    }

    /// The open file for `path`, opening it on a miss. A failed open is not cached.
    pub fn get(&self, path: &Path) -> Result<PosFile> {
        if let Some(files) = &self.pinned {
            let file = files
                .get(path)
                .with_context(|| format!("{} is outside the pinned view", path.display()))?;
            ensure!(
                file.file.metadata()?.len() >= file.len,
                "{} was truncated inside the pinned view",
                path.display()
            );
            return Ok(file.clone());
        }
        if let Some(file) = self.lock().touch(path) {
            return Ok(file);
        }
        // Opened outside the lock, so a slow open never stalls readers of cached files.
        let file = PosFile::open(path)?;
        let mut lru = self.lock();
        if let Some(raced) = lru.touch(path) {
            return Ok(raced);
        }
        while lru.open.len() >= self.cap {
            lru.evict_oldest();
        }
        let used = lru.clock;
        lru.open.insert(path.to_path_buf(), (file.clone(), used));
        Ok(file)
    }

    /// Files currently held open by the cache (clones handed out are not counted).
    pub fn open_files(&self) -> usize {
        self.pinned
            .as_ref()
            .map_or_else(|| self.lock().open.len(), HashMap::len)
    }

    /// Every critical section leaves the map consistent, so a panic elsewhere while holding
    /// the lock does not invalidate it.
    fn lock(&self) -> MutexGuard<'_, Lru> {
        self.lru.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The block number embedded in a block id: its first four bytes, big-endian.
pub fn num_from_id(id: &[u8; 32]) -> u32 {
    u32::from_be_bytes([id[0], id[1], id[2], id[3]])
}

/// A cursor over `b` with fc `raw::unpack` semantics (Spring `libfc/include/fc/io/raw.hpp`).
/// `pos` is public so callers can inspect or slice what remains.
#[derive(Clone, Debug)]
pub struct FcReader<'a> {
    pub b: &'a [u8],
    pub pos: usize,
}

impl<'a> FcReader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    /// Bytes left after `pos`.
    pub fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }

    /// The next `n` bytes. On error `pos` is unchanged.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let Some(s) = self
            .pos
            .checked_add(n)
            .and_then(|end| self.b.get(self.pos..end))
        else {
            bail!(
                "unpack past end: {n} bytes at offset {} of {}",
                self.pos,
                self.b.len()
            );
        };
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into()?))
    }

    /// fc `unsigned_int`: 7-bit groups, least significant first. Non-canonical (zero-padded)
    /// encodings are accepted. Like fc, it reads at most five bytes, stopping after the fifth
    /// even when that byte has the continuation bit set, and keeps only the low 32 bits.
    pub fn varuint32(&mut self) -> Result<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            v |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok(v)
    }

    /// fc `bool`: one byte that must be 0 or 1.
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => bail!("invalid bool {b:#04x} at offset {}", self.pos - 1),
        }
    }

    /// A 32-byte checksum (block id, chain id).
    pub fn id(&mut self) -> Result<[u8; 32]> {
        Ok(self.bytes(32)?.try_into()?)
    }
}

/// A fresh, empty directory under the system temp dir, removed on drop. Shared by the
/// archive modules' unit tests.
#[cfg(test)]
pub(crate) struct TestDir(PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "hyperion-ship-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Write `bytes` to `name` inside the directory and return its path.
    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).expect("write test file");
        path
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical fc `unsigned_int` encoding, as `fc::raw::pack` writes it.
    fn pack_varuint32(mut v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn ramp(n: usize) -> Vec<u8> {
        (0..n).map(|i| i as u8).collect()
    }

    #[test]
    fn num_from_id_reads_the_big_endian_prefix() {
        let mut id = [0xee; 32];
        id[..4].copy_from_slice(&480_385_480u32.to_be_bytes());
        assert_eq!(num_from_id(&id), 480_385_480);
        assert_eq!(num_from_id(&[0; 32]), 0);
    }

    #[test]
    fn fc_integers_are_little_endian() {
        let mut b = vec![0x2a];
        b.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        b.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        b.extend_from_slice(&[7; 32]);
        b.push(0x99);
        let mut r = FcReader::new(&b);
        assert_eq!(r.u8().unwrap(), 0x2a);
        assert_eq!(r.u32().unwrap(), 0xdead_beef);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.id().unwrap(), [7; 32]);
        // fc leaves trailing bytes alone; the caller sees them.
        assert_eq!((r.pos, r.remaining()), (45, 1));
        assert_eq!(r.bytes(1).unwrap(), &[0x99]);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn varuint32_round_trips_canonical_encodings() {
        for v in [
            0,
            1,
            127,
            128,
            2000,
            16_383,
            16_384,
            1 << 21,
            1 << 28,
            u32::MAX,
        ] {
            let enc = pack_varuint32(v);
            let mut r = FcReader::new(&enc);
            assert_eq!(r.varuint32().unwrap(), v, "value {v}");
            assert_eq!(r.pos, enc.len(), "length of {v}");
        }
        assert_eq!(pack_varuint32(2000), [0xd0, 0x0f]);
        assert_eq!(pack_varuint32(u32::MAX).len(), 5);
    }

    #[test]
    fn varuint32_accepts_non_canonical_encodings() {
        for (enc, v) in [
            (&[0x80, 0x00][..], 0),
            (&[0xff, 0x00][..], 127),
            (&[0x81, 0x80, 0x80, 0x80, 0x00][..], 1),
            (&[0xd0, 0x8f, 0x80, 0x00][..], 2000),
        ] {
            let mut r = FcReader::new(enc);
            assert_eq!(r.varuint32().unwrap(), v, "{enc:02x?}");
            assert_eq!(r.pos, enc.len());
        }
    }

    #[test]
    fn varuint32_stops_after_five_bytes_like_fc() {
        // The fifth byte ends the value even with its continuation bit set; the sixth byte
        // belongs to the next field.
        let b = [0xff, 0xff, 0xff, 0xff, 0xff, 0x2a];
        let mut r = FcReader::new(&b);
        assert_eq!(r.varuint32().unwrap(), u32::MAX);
        assert_eq!(r.pos, 5);
        assert_eq!(r.u8().unwrap(), 0x2a);
        // Bits above 32 in the fifth group are dropped, as fc's 32-bit shift drops them.
        let mut r = FcReader::new(&[0x80, 0x80, 0x80, 0x80, 0x7f]);
        assert_eq!(r.varuint32().unwrap(), 0xf000_0000);
    }

    #[test]
    fn truncated_values_are_errors_and_do_not_advance() {
        for enc in [&[][..], &[0x80], &[0x80, 0x80, 0x80, 0x80]] {
            assert!(FcReader::new(enc).varuint32().is_err(), "{enc:02x?}");
        }
        let b = [1, 2, 3];
        let mut r = FcReader::new(&b);
        assert!(r.u32().is_err());
        assert!(r.bytes(4).is_err());
        assert_eq!(r.pos, 0);
        r.pos = 1;
        let err = r.bytes(usize::MAX).unwrap_err().to_string();
        assert!(err.contains("past end"), "{err}");
        assert_eq!(r.pos, 1);
        assert!(r.u64().is_err());
        assert!(r.id().is_err());
        assert_eq!(r.bytes(2).unwrap(), &[2, 3]);
        assert!(r.u8().is_err());
    }

    #[test]
    fn bool_must_be_zero_or_one() {
        let mut r = FcReader::new(&[0, 1, 2, 0xff]);
        assert!(!r.bool().unwrap());
        assert!(r.bool().unwrap());
        let err = r.bool().unwrap_err().to_string();
        assert!(err.contains("0x02") && err.contains("offset 2"), "{err}");
        assert!(r.bool().is_err());
    }

    #[test]
    fn pos_file_reads_exact_ranges() {
        let dir = TestDir::new("posfile");
        let path = dir.write("data.bin", &ramp(256));
        let f = PosFile::open(&path).unwrap();
        assert_eq!(
            (f.len(), f.is_empty(), f.path()),
            (256, false, path.as_path())
        );

        let mut buf = [0; 4];
        f.read_exact_at(10, &mut buf).unwrap();
        assert_eq!(buf, [10, 11, 12, 13]);
        assert_eq!(f.read_vec(250, 6).unwrap(), [250, 251, 252, 253, 254, 255]);
        assert_eq!(f.u32_at(0).unwrap(), 0x0302_0100);
        assert_eq!(f.u64_at(248).unwrap(), 0xfffe_fdfc_fbfa_f9f8);
        assert!(f.read_vec(256, 0).unwrap().is_empty());

        let err = f.read_vec(250, 7).unwrap_err().to_string();
        assert!(
            err.contains("past the end") && err.contains("data.bin"),
            "{err}"
        );
        assert!(f.u64_at(249).is_err());
        assert!(f.u32_at(u64::MAX).is_err());
        assert!(f.read_vec(u64::MAX, 1).is_err());
        // A corrupt length is refused before anything is allocated.
        assert!(f.read_vec(0, usize::MAX).is_err());
    }

    #[test]
    fn pos_file_handles_empty_missing_and_directories() {
        let dir = TestDir::new("posfile-edge");
        let empty = PosFile::open(&dir.write("empty.log", &[])).unwrap();
        assert!(empty.is_empty());
        assert!(empty.read_vec(0, 0).unwrap().is_empty());
        assert!(empty.u32_at(0).is_err());

        let err = PosFile::open(&dir.path().join("missing.log")).unwrap_err();
        assert!(format!("{err:#}").contains("missing.log"), "{err:#}");
        assert!(PosFile::open(dir.path()).is_err());
    }

    #[test]
    fn pos_file_clones_read_concurrently() {
        let dir = TestDir::new("posfile-threads");
        let data = ramp(64 * 1024);
        let f = PosFile::open(&dir.write("big.bin", &data)).unwrap();
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let f = f.clone();
                let data = &data;
                s.spawn(move || {
                    for i in 0..200u64 {
                        let off = (t * 7919 + i * 131) % (data.len() as u64 - 512);
                        let got = f.read_vec(off, 512).unwrap();
                        assert_eq!(got, data[off as usize..off as usize + 512]);
                    }
                });
            }
        });
    }

    #[test]
    fn file_cache_reuses_handles_and_evicts_the_least_recent() {
        let dir = TestDir::new("filecache");
        let [a, b, c] = ["a", "b", "c"].map(|n| dir.write(n, n.as_bytes()));
        let cache = FileCache::new(2);
        let cached = |p: &Path| cache.lock().open.contains_key(p);

        let a1 = cache.get(&a).unwrap();
        let b1 = cache.get(&b).unwrap();
        let a2 = cache.get(&a).unwrap();
        assert!(
            Arc::ptr_eq(&a1.file, &a2.file),
            "a hit reuses the open file"
        );
        assert_eq!(cache.open_files(), 2);

        // `b` is now the least recently used, so opening `c` closes it.
        cache.get(&c).unwrap();
        assert!(cached(&a) && !cached(&b) && cached(&c));
        assert_eq!(cache.open_files(), 2);
        // A handle handed out before eviction keeps working.
        assert_eq!(b1.read_vec(0, 1).unwrap(), b"b");

        // Reopening `b` evicts `a` (older than `c`) and yields a new descriptor.
        let b2 = cache.get(&b).unwrap();
        assert!(!Arc::ptr_eq(&b1.file, &b2.file));
        assert!(!cached(&a) && cached(&b) && cached(&c));
        assert_eq!(a1.read_vec(0, 1).unwrap(), b"a");
    }

    #[test]
    fn file_cache_keeps_at_least_one_and_never_caches_failures() {
        let dir = TestDir::new("filecache-edge");
        let cache = FileCache::new(0);
        assert!(cache.get(&dir.path().join("missing")).is_err());
        assert_eq!(cache.open_files(), 0);
        let x = dir.write("x", b"x");
        let y = dir.write("y", b"y");
        assert_eq!(cache.get(&x).unwrap().read_vec(0, 1).unwrap(), b"x");
        assert_eq!(cache.get(&y).unwrap().read_vec(0, 1).unwrap(), b"y");
        assert_eq!(cache.open_files(), 1);
    }
}
