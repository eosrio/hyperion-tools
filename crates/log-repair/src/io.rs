use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Caps allocations from untrusted lengths and decompression (not total log size).
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Stamp {
    pub size: u64,
    pub modified_ns: u128,
    #[cfg(unix)]
    pub device: u64,
    #[cfg(unix)]
    pub inode: u64,
}

impl Stamp {
    /// Names the fields that differ from `then`, for refusal messages.
    pub fn changes(&self, then: &Stamp) -> String {
        let mut out = Vec::new();
        if self.size != then.size {
            out.push(format!("size {} -> {}", then.size, self.size));
        }
        if self.modified_ns != then.modified_ns {
            out.push("modification time".to_string());
        }
        #[cfg(unix)]
        {
            if self.inode != then.inode {
                out.push(format!("inode {} -> {}", then.inode, self.inode));
            }
            if self.device != then.device {
                out.push(format!(
                    "device {} -> {} (a pool re-import or reboot changes it)",
                    then.device, self.device
                ));
            }
        }
        out.join(", ")
    }
}

/// Which pages of a page-aligned range were in the page cache.
#[cfg(target_os = "linux")]
pub struct CachedPages {
    pub start: u64,
    pub cached: Vec<bool>,
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).unwrap_or(4096).max(512)
}

pub struct Disk {
    pub file: File,
    pub path: PathBuf,
    #[cfg(test)]
    pub fault: Option<crate::tests::Fault>,
}

impl Disk {
    /// Keep reading the same open file, even if its pathname is replaced.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
            #[cfg(test)]
            fault: self.fault.clone(),
        })
    }

    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve {}", path.display()))?;
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        ensure!(
            file.metadata()?.is_file(),
            "{} is not a regular file",
            path.display()
        );
        Ok(Self {
            file,
            path,
            #[cfg(test)]
            fault: None,
        })
    }

    pub fn stamp(&self) -> Result<Stamp> {
        let m = self.file.metadata()?;
        Ok(Stamp {
            size: m.len(),
            modified_ns: m.modified()?.duration_since(UNIX_EPOCH)?.as_nanos(),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                m.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                m.ino()
            },
        })
    }

    pub fn check_path(&self) -> Result<()> {
        ensure!(
            same_file::Handle::from_file(self.file.try_clone()?)?
                == same_file::Handle::from_path(&self.path)?,
            "file path was replaced: {}",
            self.path.display()
        );
        Ok(())
    }

    /// Other processes holding this file open, found through `/proc/*/fd`. Best effort:
    /// without root, other users' processes cannot be inspected and are reported on stderr.
    #[cfg(target_os = "linux")]
    pub fn other_openers(&self) -> Result<Vec<u32>> {
        use std::os::unix::fs::MetadataExt;
        let me = self.file.metadata()?;
        let own = std::process::id();
        let (mut pids, mut hidden) = (Vec::new(), 0usize);
        for entry in std::fs::read_dir("/proc")?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own {
                continue;
            }
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                hidden += 1;
                continue;
            };
            let open = fds.flatten().any(|fd| {
                std::fs::metadata(fd.path())
                    .is_ok_and(|m| m.dev() == me.dev() && m.ino() == me.ino())
            });
            if open {
                pids.push(pid);
            }
        }
        if hidden > 0 {
            eprintln!(
                "warning: {hidden} processes could not be inspected for open handles on {}; \
                 run as root or confirm manually that nodeos is stopped",
                self.path.display()
            );
        }
        Ok(pids)
    }

    /// Exact positional reads: no buffering or read-ahead into the damaged record.
    pub fn read(&self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        if len > MAX_BYTES || offset.checked_add(len).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read exceeds safety limit",
            ));
        }
        #[cfg(test)]
        if let Some(f) = &self.fault
            && f.unreadable(offset, len)
        {
            return Err(io::Error::from_raw_os_error(f.errno));
        }
        let mut bytes = vec![0; len as usize];
        let mut done = 0;
        while done < bytes.len() {
            #[cfg(unix)]
            let n = {
                use std::os::unix::fs::FileExt;
                self.file.read_at(&mut bytes[done..], offset + done as u64)
            };
            #[cfg(windows)]
            let n = {
                use std::os::windows::fs::FileExt;
                self.file
                    .seek_read(&mut bytes[done..], offset + done as u64)
            };
            match n {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "short positional read",
                    ));
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(bytes)
    }

    /// Page-cache residency of `offset..offset + len`, page by page (Linux `mincore`). The
    /// mapping is never touched, so damaged pages are not read. `None` when unavailable.
    #[cfg(target_os = "linux")]
    pub fn cached_pages(&self, offset: u64, len: u64) -> Option<CachedPages> {
        use std::os::fd::AsRawFd;
        let page = page_size();
        let start = offset - offset % page;
        let end = offset.checked_add(len)?.next_multiple_of(page);
        let map_len = usize::try_from(end - start).ok()?;
        if map_len == 0 {
            return Some(CachedPages {
                start,
                cached: Vec::new(),
            });
        }
        let mut residency = vec![0u8; map_len / page as usize];
        // SAFETY: a read-only shared mapping of our own descriptor, inspected with mincore
        // and unmapped before returning; no byte of it is ever dereferenced.
        unsafe {
            let addr = libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                libc::off_t::try_from(start).ok()?,
            );
            if addr == libc::MAP_FAILED {
                return None;
            }
            let rc = libc::mincore(addr, map_len, residency.as_mut_ptr());
            libc::munmap(addr, map_len);
            if rc != 0 {
                return None;
            }
        }
        Some(CachedPages {
            start,
            cached: residency.iter().map(|b| b & 1 == 1).collect(),
        })
    }

    /// Drop from the page cache the pages that `before` recorded as not cached, so a scan
    /// does not evict a live node's working set with cold log data. Best effort.
    #[cfg(target_os = "linux")]
    pub fn uncache(&self, before: &CachedPages) {
        use std::os::fd::AsRawFd;
        let page = page_size();
        let mut i = 0;
        while i < before.cached.len() {
            if before.cached[i] {
                i += 1;
                continue;
            }
            let j = (i..before.cached.len())
                .find(|&k| before.cached[k])
                .unwrap_or(before.cached.len());
            let (Ok(offset), Ok(len)) = (
                libc::off_t::try_from(before.start + i as u64 * page),
                libc::off_t::try_from((j - i) as u64 * page),
            ) else {
                return;
            };
            // SAFETY: an advisory call on our own descriptor; it never changes file contents.
            unsafe {
                libc::posix_fadvise(
                    self.file.as_raw_fd(),
                    offset,
                    len,
                    libc::POSIX_FADV_DONTNEED,
                );
            }
            i = j;
        }
    }

    /// Filesystem-preferred I/O size (`st_blksize`). On ZFS this is the file's data block
    /// (record) size; on ext4, xfs, and btrfs it is the 4 KiB page/block size.
    pub fn block_size(&self) -> Option<u64> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.file.metadata().ok().map(|m| m.blksize())
        }
        #[cfg(not(unix))]
        None
    }

    /// ONE positional write covering a whole unreadable extent, so the filesystem replaces
    /// complete records without reading the damaged ones. Never retry a short write.
    #[cfg(unix)]
    pub fn write_extent(&self, offset: u64, bytes: &[u8]) -> Result<()> {
        let expected_len = bytes.len();
        #[cfg(test)]
        let bytes = if self.fault.as_ref().is_some_and(|f| f.short_write) {
            &bytes[..bytes.len() / 2]
        } else {
            bytes
        };
        let n = {
            use std::os::unix::fs::FileExt;
            self.file.write_at(bytes, offset)?
        };
        self.file.sync_all()?;
        #[cfg(test)]
        if let Some(f) = &self.fault {
            f.after_write(offset, n);
        }
        ensure!(
            n == expected_len,
            "SHORT WRITE: {n}/{} bytes; do not retry automatically; inspect the journal",
            expected_len
        );
        Ok(())
    }
}

pub fn media_error(e: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(e.raw_os_error(), Some(libc::EIO) | Some(libc::ENODATA))
    }
    #[cfg(not(unix))]
    {
        let _ = e;
        false
    } // In-place repair is deliberately disabled outside Unix.
}

pub fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn create_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "create new {} (existing files are never overwritten)",
                path.display()
            )
        })?;
    f.write_all(bytes)?;
    f.sync_all()?;
    sync_dir(parent_dir(path))
}

/// Create a NEW directory and make its entry durable in the parent.
pub fn create_dir(path: &Path) -> Result<()> {
    std::fs::create_dir(path).with_context(|| {
        format!(
            "create new directory {}; choose an unused path",
            path.display()
        )
    })?;
    sync_dir(parent_dir(path))
}

pub fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
