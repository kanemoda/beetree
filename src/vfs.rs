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

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::rc::Rc;

/// Positional I/O over a single backing "file" (ADR-0009).
///
/// Offsets are absolute; there is no cursor. `sync` must not return until
/// everything previously written is durable.
// The fault injector ([`FaultyVfs`]) needs exactly these five operations;
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

// ---------------------------------------------------------------------
// FaultyVfs: the M1.2 crash-injection model (ADR-0010).

/// What a crash does to one un-synced operation when a crash image is
/// built (ADR-0010): dropped entirely, applied in full, applied as a
/// zero-fill of its full extent (the metadata-before-data failure: the
/// file grows but the payload never lands), or torn to a byte prefix.
///
/// `Tear(seed)` keeps `seed % (len + 1)` prefix bytes of a write. For
/// `set_len`, which the model treats as atomic, `Zero` and `Tear` both
/// mean `Apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fate {
    /// The operation never reached the platter.
    Drop,
    /// The operation became fully durable.
    Apply,
    /// The full extent became durable as zeros (length without payload).
    Zero,
    /// A byte-length prefix became durable (`seed % (len + 1)` bytes).
    Tear(u64),
}

/// One entry of the public op log: what the engine asked of the device,
/// in order. Tests use it to locate sync boundaries and crash points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsOp {
    /// `write_all_at(off, len bytes)`.
    Write {
        /// Absolute file offset of the write.
        off: u64,
        /// Length of the write in bytes.
        len: u64,
    },
    /// `set_len(len)`.
    SetLen {
        /// The requested file length.
        len: u64,
    },
    /// A `sync()` call; `failed` is true for an injected sync failure.
    Sync {
        /// Whether this sync failed (fsyncgate injection).
        failed: bool,
    },
}

/// The full-fidelity internal log (payload bytes included), from which
/// any historical crash image can be replayed.
#[derive(Debug, Clone)]
enum LogEntry {
    Write {
        off: u64,
        data: Vec<u8>,
    },
    SetLen {
        len: u64,
    },
    SyncOk,
    /// An injected sync failure: `fates` records which subset of the
    /// window became durable anyway (fsyncgate: unknowable to the caller).
    SyncFailed {
        fates: Vec<Fate>,
    },
}

#[derive(Debug, Default)]
struct FaultyState {
    /// What the running engine reads: every write applied (the page
    /// cache view). Survives nothing.
    view: Vec<u8>,
    /// Bytes guaranteed to survive a crash.
    durable: Vec<u8>,
    /// The durable baseline the device started with (`from_image`):
    /// crash-image replay must begin here, not at an empty file — the
    /// baseline predates the op log.
    initial: Vec<u8>,
    /// Complete operation history, payloads included.
    log: Vec<LogEntry>,
    /// Index into `log` of the first op after the last sync — the start
    /// of the current sync window.
    window_start: usize,
    /// Armed sync failure: (syncs to let through first, fates for the
    /// window of the failing one).
    fail_sync: Option<(u32, Vec<Fate>)>,
}

/// An in-memory [`Vfs`] modeling a device with a volatile write cache,
/// for crash-injection testing (M1.2; ADR-0010). Test infrastructure,
/// deliberately kept dependency-free so it can live in the production
/// crate without entering any production code path.
///
/// Writes and `set_len`s accumulate in a sync window; `sync()` makes the
/// window durable. [`FaultyVfs::crash_image_at`] reconstructs what the
/// file could look like if the machine died at any point in history: the
/// durable bytes plus an arbitrary subset of that point's un-synced
/// window, each op dropped, applied, zero-filled, or torn ([`Fate`]).
/// [`FaultyVfs::fail_next_sync`] models a failed fsync: the sync errors,
/// an arbitrary subset of the window becomes durable anyway, and the rest
/// is discarded forever.
///
/// `Clone` yields a HANDLE to the same underlying device, not a copy —
/// tests keep one handle while the engine owns another. Within one sync
/// window, overlapping writes panic: the commit protocol never issues
/// them, and the model turns that simplification into an engine check
/// (which also justifies ignoring reordering; ADR-0010).
#[derive(Debug, Clone, Default)]
pub struct FaultyVfs {
    state: Rc<RefCell<FaultyState>>,
}

impl FaultyVfs {
    /// An empty device.
    pub fn new() -> FaultyVfs {
        FaultyVfs::default()
    }

    /// A device whose durable contents are exactly `image` (e.g. a crash
    /// image from a previous run), with a fresh, empty op log.
    pub fn from_image(image: Vec<u8>) -> FaultyVfs {
        let vfs = FaultyVfs::new();
        {
            let mut s = vfs.state.borrow_mut();
            s.view = image.clone();
            s.durable = image.clone();
            s.initial = image;
        }
        vfs
    }

    /// Number of entries in the op log.
    pub fn log_len(&self) -> usize {
        self.state.borrow().log.len()
    }

    /// The public op log: offsets and lengths only, in issue order.
    pub fn op_log(&self) -> Vec<VfsOp> {
        self.state
            .borrow()
            .log
            .iter()
            .map(|e| match e {
                LogEntry::Write { off, data } => VfsOp::Write {
                    off: *off,
                    len: data.len() as u64,
                },
                LogEntry::SetLen { len } => VfsOp::SetLen { len: *len },
                LogEntry::SyncOk => VfsOp::Sync { failed: false },
                LogEntry::SyncFailed { .. } => VfsOp::Sync { failed: true },
            })
            .collect()
    }

    /// How many un-synced ops are pending at log position `pos` (the
    /// window a crash at `pos` would tear).
    pub fn window_len_at(&self, pos: usize) -> usize {
        let s = self.state.borrow();
        assert!(pos <= s.log.len(), "log position out of range");
        s.log[..pos]
            .iter()
            .rev()
            .take_while(|e| !matches!(e, LogEntry::SyncOk | LogEntry::SyncFailed { .. }))
            .count()
    }

    /// The file as it could be found after a crash at log position `pos`
    /// (ops `[0, pos)` happened): everything synced by then, plus the
    /// pending window with `fates[i]` applied to its i-th op (missing
    /// entries default to [`Fate::Apply`]). Pure replay; the live state
    /// is untouched.
    pub fn crash_image_at(&self, pos: usize, fates: &[Fate]) -> Vec<u8> {
        let s = self.state.borrow();
        assert!(pos <= s.log.len(), "log position out of range");
        let mut img = s.initial.clone();
        let mut wstart = 0;
        for (i, entry) in s.log[..pos].iter().enumerate() {
            match entry {
                LogEntry::SyncOk => {
                    apply_window(&mut img, &s.log[wstart..i], &[], true);
                    wstart = i + 1;
                }
                LogEntry::SyncFailed { fates } => {
                    apply_window(&mut img, &s.log[wstart..i], fates, false);
                    wstart = i + 1;
                }
                _ => {}
            }
        }
        apply_window(&mut img, &s.log[wstart..pos], fates, false);
        img
    }

    /// Arm an fsyncgate failure for the NEXT sync: it returns an error,
    /// `fates` decides which subset of the pending window becomes durable
    /// anyway, and the remaining pending ops are discarded forever (they
    /// will not be written by any later sync).
    pub fn fail_next_sync(&self, fates: Vec<Fate>) {
        self.fail_nth_sync(0, fates);
    }

    /// Like [`FaultyVfs::fail_next_sync`], but lets `skip` syncs succeed
    /// first (e.g. `skip = 1` targets a commit's superblock sync, letting
    /// its data sync through).
    pub fn fail_nth_sync(&self, skip: u32, fates: Vec<Fate>) {
        self.state.borrow_mut().fail_sync = Some((skip, fates));
    }
}

/// Apply one window of ops onto `buf`, honoring fates (`all` short-cuts
/// to full application for a successful sync).
fn apply_window(buf: &mut Vec<u8>, window: &[LogEntry], fates: &[Fate], all: bool) {
    let mut op_index = 0;
    for entry in window {
        let fate = if all {
            Fate::Apply
        } else {
            fates.get(op_index).copied().unwrap_or(Fate::Apply)
        };
        match entry {
            LogEntry::Write { off, data } => {
                match fate {
                    Fate::Drop => {}
                    Fate::Apply => write_at(buf, *off, data),
                    Fate::Zero => write_at(buf, *off, &vec![0u8; data.len()]),
                    Fate::Tear(seed) => {
                        let keep = (seed % (data.len() as u64 + 1)) as usize;
                        write_at(buf, *off, &data[..keep]);
                    }
                }
                op_index += 1;
            }
            LogEntry::SetLen { len } => {
                // set_len is atomic in the model (ADR-0010): Zero and
                // Tear degrade to Apply.
                if fate != Fate::Drop {
                    buf.resize(*len as usize, 0);
                }
                op_index += 1;
            }
            LogEntry::SyncOk | LogEntry::SyncFailed { .. } => {
                unreachable!("sync entries delimit windows; they never appear inside one")
            }
        }
    }
}

/// Copy `data` to `buf[off..]`, zero-filling any gap (sparse-file
/// semantics, as positional file writes behave).
fn write_at(buf: &mut Vec<u8>, off: u64, data: &[u8]) {
    let off = off as usize;
    let end = off + data.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[off..end].copy_from_slice(data);
}

impl Vfs for FaultyVfs {
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let s = self.state.borrow();
        let end = off as usize + buf.len();
        if end > s.view.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past end of file",
            ));
        }
        buf.copy_from_slice(&s.view[off as usize..end]);
        Ok(())
    }

    fn write_all_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        // The model's one simplification, turned into an engine check:
        // within a sync window, writes must never overlap byte-wise.
        // Disjoint writes commute, which is what justifies modeling a
        // crash as a per-op subset with no reordering (ADR-0010).
        let (a1, a2) = (off, off + data.len() as u64);
        for entry in &s.log[s.window_start..] {
            if let LogEntry::Write { off: o, data: d } = entry {
                let (b1, b2) = (*o, *o + d.len() as u64);
                assert!(
                    a2 <= b1 || b2 <= a1,
                    "FaultyVfs model violation: un-synced writes [{a1}, {a2}) and \
                     [{b1}, {b2}) overlap — the commit protocol must never do this \
                     (ADR-0010)"
                );
            }
        }
        write_at(&mut s.view, off, data);
        s.log.push(LogEntry::Write {
            off,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut s = self.state.borrow_mut();
        let s = &mut *s;
        if let Some((skip, fates)) = s.fail_sync.take() {
            if skip == 0 {
                // fsyncgate: the device durably applied SOME subset, the
                // caller gets an error, and the discarded ops are gone
                // for good (marked clean, never retried).
                let window = s.log[s.window_start..].to_vec();
                let mut durable = std::mem::take(&mut s.durable);
                apply_window(&mut durable, &window, &fates, false);
                s.durable = durable;
                s.log.push(LogEntry::SyncFailed { fates });
                s.window_start = s.log.len();
                return Err(io::Error::other("injected sync failure"));
            }
            s.fail_sync = Some((skip - 1, fates));
        }
        let window = s.log[s.window_start..].to_vec();
        let mut durable = std::mem::take(&mut s.durable);
        apply_window(&mut durable, &window, &[], true);
        s.durable = durable;
        s.log.push(LogEntry::SyncOk);
        s.window_start = s.log.len();
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.state.borrow().view.len() as u64)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut s = self.state.borrow_mut();
        s.view.resize(len as usize, 0);
        s.log.push(LogEntry::SetLen { len });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_reads_see_unsynced_writes() {
        let mut v = FaultyVfs::new();
        v.write_all_at(4, b"abc").unwrap();
        assert_eq!(v.len().unwrap(), 7);
        let mut buf = [0u8; 7];
        v.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"\0\0\0\0abc");
        assert!(v.read_exact_at(5, &mut [0u8; 3]).is_err());
    }

    #[test]
    fn crash_fates_drop_apply_zero_tear() {
        let mut v = FaultyVfs::new();
        v.write_all_at(0, b"xxxx").unwrap();
        v.sync().unwrap();
        v.write_all_at(4, b"abcd").unwrap();

        let end = v.log_len();
        assert_eq!(v.crash_image_at(end, &[Fate::Drop]), b"xxxx");
        assert_eq!(v.crash_image_at(end, &[Fate::Apply]), b"xxxxabcd");
        assert_eq!(v.crash_image_at(end, &[Fate::Zero]), b"xxxx\0\0\0\0");
        assert_eq!(v.crash_image_at(end, &[Fate::Tear(2)]), b"xxxxab");
        // Tear wraps modulo len + 1; missing fates default to Apply.
        assert_eq!(v.crash_image_at(end, &[Fate::Tear(5)]), b"xxxx");
        assert_eq!(v.crash_image_at(end, &[]), b"xxxxabcd");
        // A crash before the first write sees only durable bytes.
        assert_eq!(v.crash_image_at(0, &[]), b"");
    }

    #[test]
    fn synced_bytes_survive_any_fate() {
        let mut v = FaultyVfs::new();
        v.write_all_at(0, b"keep").unwrap();
        v.sync().unwrap();
        assert_eq!(v.crash_image_at(v.log_len(), &[Fate::Drop]), b"keep");
    }

    #[test]
    fn failed_sync_discards_what_it_did_not_apply() {
        let mut v = FaultyVfs::new();
        v.write_all_at(0, b"aa").unwrap();
        v.fail_next_sync(vec![Fate::Drop]);
        assert!(v.sync().is_err());
        // The dropped write is gone for good: a later successful sync
        // must not resurrect it (fsyncgate: marked clean, never retried).
        v.write_all_at(2, b"bb").unwrap();
        v.sync().unwrap();
        assert_eq!(v.crash_image_at(v.log_len(), &[]), b"\0\0bb");
        // The live view still shows everything (page cache semantics).
        let mut buf = [0u8; 4];
        v.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"aabb");
    }

    #[test]
    fn fail_nth_sync_lets_earlier_syncs_through() {
        let mut v = FaultyVfs::new();
        v.fail_nth_sync(1, vec![Fate::Apply]);
        v.write_all_at(0, b"a").unwrap();
        v.sync().unwrap(); // skip = 1: this one succeeds
        v.write_all_at(1, b"b").unwrap();
        assert!(v.sync().is_err()); // armed failure fires here
        assert_eq!(v.crash_image_at(v.log_len(), &[]), b"ab");
    }

    #[test]
    #[should_panic(expected = "overlap")]
    fn overlapping_unsynced_writes_panic() {
        let mut v = FaultyVfs::new();
        v.write_all_at(0, b"aaaa").unwrap();
        v.write_all_at(2, b"bb").unwrap();
    }

    #[test]
    fn set_len_is_atomic_drop_or_apply() {
        let mut v = FaultyVfs::new();
        v.write_all_at(0, b"abcdef").unwrap();
        v.sync().unwrap();
        v.set_len(2).unwrap();
        let end = v.log_len();
        assert_eq!(v.crash_image_at(end, &[Fate::Drop]), b"abcdef");
        assert_eq!(v.crash_image_at(end, &[Fate::Apply]), b"ab");
        assert_eq!(v.crash_image_at(end, &[Fate::Tear(1)]), b"ab");
        assert_eq!(v.len().unwrap(), 2);
    }

    /// Regression (caught by the M1.2 crash harness on first run): a
    /// `from_image` device starts with durable bytes that predate its op
    /// log, and crash images must replay on top of THAT baseline — not an
    /// empty file.
    #[test]
    fn from_image_baseline_survives_crash_images() {
        let mut v = FaultyVfs::from_image(b"base".to_vec());
        assert_eq!(v.crash_image_at(0, &[]), b"base");
        v.set_len(2).unwrap();
        assert_eq!(v.crash_image_at(v.log_len(), &[Fate::Drop]), b"base");
        assert_eq!(v.crash_image_at(v.log_len(), &[Fate::Apply]), b"ba");
    }

    #[test]
    fn clone_is_a_handle_not_a_copy() {
        let mut v = FaultyVfs::new();
        let h = v.clone();
        v.write_all_at(0, b"z").unwrap();
        assert_eq!(h.log_len(), 1);
    }
}

// ---------------------------------------------------------------------
// CountingVfs: exact engine-level I/O accounting (M3.1).

/// A snapshot of engine-level I/O counters (SPEC "Observability"): every
/// DATA-PATH operation the engine issued through its [`Vfs`], counted
/// exactly — reads, writes, syncs, set_lens. `len()` (a pure metadata
/// query) is deliberately uncounted. This is the PRIMARY, contract-level
/// metric; OS-level numbers ([`proc_self_io`]) are the secondary check
/// (the page cache is not defeated in this build).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// `read_exact_at` calls.
    pub read_ops: u64,
    /// Bytes requested across all reads.
    pub read_bytes: u64,
    /// `write_all_at` calls.
    pub write_ops: u64,
    /// Bytes written across all writes.
    pub write_bytes: u64,
    /// `sync` calls.
    pub syncs: u64,
    /// `set_len` calls.
    pub set_lens: u64,
}

/// A transparent counting wrapper over any [`Vfs`]: byte-for-byte
/// identical behavior (property-tested), plus an [`IoStats`] tally.
/// `Cell` keeps counting possible from `read_exact_at(&self, ..)`.
#[derive(Debug)]
pub struct CountingVfs<V> {
    inner: V,
    stats: std::cell::Cell<IoStats>,
}

impl<V: Vfs> CountingVfs<V> {
    /// Wrap a [`Vfs`], starting all counters at zero.
    pub fn new(inner: V) -> CountingVfs<V> {
        CountingVfs {
            inner,
            stats: std::cell::Cell::new(IoStats::default()),
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> IoStats {
        self.stats.get()
    }
}

impl<V: Vfs> Vfs for CountingVfs<V> {
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut s = self.stats.get();
        s.read_ops += 1;
        s.read_bytes += buf.len() as u64;
        self.stats.set(s);
        self.inner.read_exact_at(off, buf)
    }

    fn write_all_at(&mut self, off: u64, data: &[u8]) -> io::Result<()> {
        let mut s = self.stats.get();
        s.write_ops += 1;
        s.write_bytes += data.len() as u64;
        self.stats.set(s);
        self.inner.write_all_at(off, data)
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut s = self.stats.get();
        s.syncs += 1;
        self.stats.set(s);
        self.inner.sync()
    }

    fn len(&self) -> io::Result<u64> {
        self.inner.len()
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut s = self.stats.get();
        s.set_lens += 1;
        self.stats.set(s);
        self.inner.set_len(len)
    }
}

/// A snapshot of `/proc/self/io` (linux-only): the SECONDARY, OS-level
/// I/O metric for M3.2 benchmarks. The engine-level [`IoStats`] is the
/// contract metric; these numbers include everything else the process
/// does and sit above a live page cache (SPEC "Observability").
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcIo {
    /// Bytes read by syscalls (`rchar`).
    pub rchar: u64,
    /// Bytes written by syscalls (`wchar`).
    pub wchar: u64,
    /// Read syscalls (`syscr`).
    pub syscr: u64,
    /// Write syscalls (`syscw`).
    pub syscw: u64,
    /// Bytes actually fetched from the storage layer (`read_bytes`).
    pub read_bytes: u64,
    /// Bytes actually sent to the storage layer (`write_bytes`).
    pub write_bytes: u64,
}

/// Read `/proc/self/io`. Observability tooling, not an engine code path:
/// the engine itself still only touches storage through [`Vfs`].
#[cfg(target_os = "linux")]
pub fn proc_self_io() -> io::Result<ProcIo> {
    let text = std::fs::read_to_string("/proc/self/io")?;
    let mut out = ProcIo::default();
    for line in text.lines() {
        let mut parts = line.split(": ");
        let (Some(field), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let value: u64 = value.trim().parse().map_err(io::Error::other)?;
        match field {
            "rchar" => out.rchar = value,
            "wchar" => out.wchar = value,
            "syscr" => out.syscr = value,
            "syscw" => out.syscw = value,
            "read_bytes" => out.read_bytes = value,
            "write_bytes" => out.write_bytes = value,
            _ => {}
        }
    }
    Ok(out)
}
