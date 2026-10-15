//! Fault injection for the *layer* side: a [`FileSystem`] wrapper whose read
//! operations — `metadata` and `read_dir`, the calls index builds and
//! profiles live on — can be told to fail at a chosen call count.
//!
//! Writes pass straight through (the deploy path is exercised by
//! `deploy_faults`, not here); this wrapper exists to prove that a layer
//! dying mid-scan produces a clean `Err` — never a half-built index or a
//! half-written plan.
//!
//! ```text
//! let flaky = FlakyFs::wrap(PhysicalFs::new(dir)?);
//! flaky.fail_read_dir_at(3); // the 4th read_dir (0-based) errors, then heals
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use openvfs::{DirEntry, FileSystem, Metadata, OpenOptions, VPath, VfsError, VfsFile};

/// `fail_*_at(NEVER)` leaves the operation alone — the default.
pub const NEVER: usize = usize::MAX;

/// One counted operation kind.
struct Counter {
    performed: AtomicUsize,
    fail_at: AtomicUsize,
}

impl Default for Counter {
    /// Never fails until armed.
    fn default() -> Self {
        Self { performed: AtomicUsize::new(0), fail_at: AtomicUsize::new(NEVER) }
    }
}

impl Counter {
    /// Counts one call; `false` if it must fail.
    fn allow(&self) -> bool {
        let n = self.performed.fetch_add(1, Ordering::SeqCst);
        n != self.fail_at.load(Ordering::SeqCst)
    }

    /// Arms the counter: call `n` (0-based, counted from now) fails.
    fn fail_at(&self, n: usize) {
        self.performed.store(0, Ordering::SeqCst);
        self.fail_at.store(n, Ordering::SeqCst);
    }
}

/// A [`FileSystem`] whose `metadata`/`read_dir` fail on demand. Wraps an
/// `Arc<dyn FileSystem>` so it can sit anywhere a layer can.
#[derive(Clone)]
pub struct FlakyFs {
    inner: Arc<dyn FileSystem>,
    metadata: Arc<Counter>,
    read_dir: Arc<Counter>,
}

impl FlakyFs {
    /// Wraps `inner`; nothing fails until a `fail_*_at` call.
    pub fn wrap(inner: Arc<dyn FileSystem>) -> Self {
        Self { inner, metadata: Arc::new(Counter::default()), read_dir: Arc::new(Counter::default()) }
    }

    /// The `n`-th `metadata` call (0-based, counted from now) fails.
    pub fn fail_metadata_at(&self, n: usize) {
        self.metadata.fail_at(n);
    }

    /// The `n`-th `read_dir` call (0-based, counted from now) fails.
    pub fn fail_read_dir_at(&self, n: usize) {
        self.read_dir.fail_at(n);
    }

    /// Every future `read_dir` fails (`NEVER` resets via [`FlakyFs::heal`]).
    pub fn fail_all_read_dir(&self) {
        self.read_dir.performed.store(0, Ordering::SeqCst);
        self.read_dir.fail_at.store(0, Ordering::SeqCst);
    }

    /// Restores normal operation.
    pub fn heal(&self) {
        self.metadata.fail_at(NEVER);
        self.read_dir.fail_at(NEVER);
    }
}

/// The error injected — a plain I/O failure, what a dying disk looks like.
fn injected(path: &VPath) -> VfsError {
    VfsError::Io { path: Some(path.clone()), source: std::io::Error::other("injected layer fault") }
}

impl FileSystem for FlakyFs {
    fn metadata(&self, path: &VPath) -> openvfs::Result<Metadata> {
        if !self.metadata.allow() {
            return Err(injected(path));
        }
        self.inner.metadata(path)
    }

    fn read_dir(&self, path: &VPath) -> openvfs::Result<Vec<DirEntry>> {
        if !self.read_dir.allow() {
            return Err(injected(path));
        }
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.inner.create_dir(path)
    }

    fn remove_file(&self, path: &VPath) -> openvfs::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.inner.remove_dir(path)
    }

    fn rename(&self, from: &VPath, to: &VPath) -> openvfs::Result<()> {
        self.inner.rename(from, to)
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> openvfs::Result<Box<dyn VfsFile>> {
        self.inner.open(path, options)
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}
