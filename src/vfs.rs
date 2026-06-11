//! The virtual file system the disk engine runs on (ADR-0009).
//!
//! The engine never touches `std::fs` directly: every byte it reads or
//! writes goes through [`Vfs`], so M1.2 can substitute a fault-injecting
//! in-memory implementation and crash the "disk" at any point. Production
//! code must be byte-for-byte agnostic to which `Vfs` it runs on.
//!
//! Positional I/O is unix-only for now ([`FileVfs`] builds on
//! `std::os::unix::fs::FileExt`); a portable backend can arrive when a
//! platform needs one.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Positional I/O over a single backing "file" (ADR-0009).
///
/// Offsets are absolute; there is no cursor. `sync` must not return until
/// everything previously written is durable.
// The prompt-for-M1.2 fault injector needs exactly these five operations;
// `is_empty` would be a misleading eighth wheel on an I/O handle.
#[allow(clippy::len_without_is_empty)]
pub trait Vfs {
    /// Fill `buf` exactly from the bytes at `off`, erroring on short reads.
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write all of `data` at `off`, extending the file if needed.
    fn write_all_at(&mut self, off: u64, data: &[u8]) -> io::Result<()>;

    /// Make every prior write durable before returning.
    fn sync(&mut self) -> io::Result<()>;

    /// Current length in bytes.
    fn len(&self) -> io::Result<u64>;

    /// Truncate or zero-extend to exactly `len` bytes.
    fn set_len(&mut self, len: u64) -> io::Result<()>;
}

/// The production [`Vfs`]: a real file accessed via unix positional I/O.
#[cfg(unix)]
#[derive(Debug)]
pub struct FileVfs {
    file: File,
}

#[cfg(unix)]
impl FileVfs {
    /// Open `path` for read/write, creating it if missing, and fsync the
    /// parent directory once so the file's *existence* is as durable as
    /// its future contents. An existing file is NOT truncated — the engine
    /// decides what an existing file means.
    pub fn create(path: impl AsRef<Path>) -> io::Result<FileVfs> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let parent = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        File::open(parent)?.sync_all()?;
        Ok(FileVfs { file })
    }

    /// Open an existing file for read/write; errors if it does not exist.
    pub fn open(path: impl AsRef<Path>) -> io::Result<FileVfs> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(FileVfs { file })
    }
}

#[cfg(unix)]
impl Vfs for FileVfs {
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, off)
    }

    fn write_all_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.file, data, off)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}
