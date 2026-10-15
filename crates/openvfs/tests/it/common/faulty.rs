//! Fault injection: a wrapper whose mutating operations fail on demand.
//!
//! Two fault models:
//! * **dead disk** ([`FaultyFs::fail_from`]): operation N and every later one
//!   fails — nothing, not even cleanup, can succeed afterwards;
//! * **transient** ([`FaultyFs::fail_once_at`]): only operation N fails —
//!   recovery and cleanup code gets a chance to run.
//!
//! "Operations" are mutating trait calls plus every write, `set_len` and
//! `sync_all` on handles opened for writing. Reads never fail.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openvfs::{DirEntry, FileSystem, Metadata, OpenOptions, VPath, VfsError, VfsFile};

pub const NEVER: usize = usize::MAX;

#[derive(Default)]
struct Plan {
    performed: AtomicUsize,
    fail_from: AtomicUsize,
    fail_only: AtomicUsize,
}

impl Plan {
    fn new() -> Self {
        Self { performed: AtomicUsize::new(0), fail_from: AtomicUsize::new(NEVER), fail_only: AtomicUsize::new(NEVER) }
    }

    /// Counts one operation; `false` if it must fail.
    fn allow(&self) -> bool {
        let n = self.performed.fetch_add(1, Ordering::SeqCst);
        n < self.fail_from.load(Ordering::SeqCst) && n != self.fail_only.load(Ordering::SeqCst)
    }
}

pub struct FaultyFs<F> {
    inner: F,
    plan: Arc<Plan>,
}

impl<F: FileSystem> FaultyFs<F> {
    /// Never fails until configured.
    pub fn new(inner: F) -> Self {
        Self { inner, plan: Arc::new(Plan::new()) }
    }

    /// Dead disk: counting from now, operation `n` (0-based) and all later ones fail.
    pub fn fail_from(&self, n: usize) {
        self.reconfigure(n, NEVER);
    }

    /// Transient: counting from now, only operation `n` (0-based) fails.
    pub fn fail_once_at(&self, n: usize) {
        self.reconfigure(NEVER, n);
    }

    pub fn heal(&self) {
        self.reconfigure(NEVER, NEVER);
    }

    fn reconfigure(&self, from: usize, only: usize) {
        self.plan.performed.store(0, Ordering::SeqCst);
        self.plan.fail_from.store(from, Ordering::SeqCst);
        self.plan.fail_only.store(only, Ordering::SeqCst);
    }

    fn tick(&self, path: &VPath) -> openvfs::Result<()> {
        if self.plan.allow() { Ok(()) } else { Err(VfsError::Io { path: Some(path.clone()), source: injected() }) }
    }
}

fn injected() -> io::Error {
    io::Error::other("injected fault")
}

impl<F: FileSystem> FileSystem for FaultyFs<F> {
    fn metadata(&self, path: &VPath) -> openvfs::Result<Metadata> {
        self.inner.metadata(path)
    }
    fn read_dir(&self, path: &VPath) -> openvfs::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn create_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.tick(path)?;
        self.inner.create_dir(path)
    }
    fn remove_file(&self, path: &VPath) -> openvfs::Result<()> {
        self.tick(path)?;
        self.inner.remove_file(path)
    }
    fn remove_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.tick(path)?;
        self.inner.remove_dir(path)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> openvfs::Result<()> {
        self.tick(from)?;
        self.inner.rename(from, to)
    }
    fn open(&self, path: &VPath, options: &OpenOptions) -> openvfs::Result<Box<dyn VfsFile>> {
        if !options.is_write() {
            return self.inner.open(path, options);
        }
        self.tick(path)?;
        let inner = self.inner.open(path, options)?;
        Ok(Box::new(FaultyFile { inner, plan: Arc::clone(&self.plan) }))
    }
    // remove_dir_all / copy_file use the trait defaults, so they are built from
    // the faulty primitives above and can fail at any intermediate step.
}

struct FaultyFile {
    inner: Box<dyn VfsFile>,
    plan: Arc<Plan>,
}

impl Read for FaultyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for FaultyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.plan.allow() { self.inner.write(buf) } else { Err(injected()) }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for FaultyFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl VfsFile for FaultyFile {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        if self.plan.allow() { self.inner.set_len(size) } else { Err(injected()) }
    }
    fn sync_all(&mut self) -> io::Result<()> {
        if self.plan.allow() { self.inner.sync_all() } else { Err(injected()) }
    }
}
