//! A [`FileSystem`] adapter that resolves every path component
//! case-insensitively.
//!
//! [`CaseInsensitiveFs`] wraps another backend and answers
//! `metadata("/TEXTURES/A.dds")` from a file really stored at
//! `textures/a.dds`: each component is matched against the containing
//! directory's listing ignoring case (the same [`VPath::folded`] comparison
//! the rest of the crate uses), then the operation runs against the real
//! spelling. Listings show the real casing, never the caller's.
//!
//! Component lookup uses a per-directory cache, so steady-state metadata or
//! open calls cost one map lookup per component — not one `read_dir` each.
//!
//! # Semantics
//!
//! * The longest resolvable prefix folds onto real entries; the remaining
//!   tail keeps the caller's spelling — that is what a `create` or
//!   `create_dir` will make on disk.
//! * Mutations issued *through this adapter* invalidate the affected
//!   directory caches (and whole subtrees for renames/removals), so its own
//!   writes stay visible. Writes reaching the wrapped backend through other
//!   handles are not tracked — same "mods don't move under a locked profile"
//!   assumption as [`crate::MergedIndex`]; call [`CaseInsensitiveFs::clear_cache`]
//!   after touching the backend directly.
//! * If the wrapped backend genuinely contains case twins (`A` and `a` in
//!   one directory), resolution picks whichever listing entry folds last —
//!   the ambiguity is real and no adapter can resolve it.
//! * Errors still report the path that was passed in.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use openvfs::{DirEntry, FileSystem, Metadata, OpenOptions, Result, VPath, VfsError, VfsFile};

type DirMap = Arc<BTreeMap<String, String>>;

/// See the module documentation.
pub struct CaseInsensitiveFs {
    inner: Arc<dyn FileSystem>,
    /// Resolved dir → folded entry name → real entry name.
    cache: RwLock<BTreeMap<VPath, DirMap>>,
}

impl CaseInsensitiveFs {
    /// Wraps `inner` with case-insensitive path resolution.
    pub fn new(inner: Arc<dyn FileSystem>) -> Self {
        Self { inner, cache: RwLock::new(BTreeMap::new()) }
    }

    /// The wrapped file system.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn FileSystem> {
        &self.inner
    }

    /// Drops every cached directory map — after changing the wrapped backend
    /// through a handle other than this adapter.
    pub fn clear_cache(&self) {
        lock_write(&self.cache).clear();
    }

    /// Maps `path` to the real, on-backend spelling. The longest prefix that
    /// resolves through listings (folded component = listed name) is
    /// rewritten to the real casing; the rest keeps the caller's spelling —
    /// exactly the tail a `create` is about to make.
    fn resolve(&self, path: &VPath) -> Result<VPath> {
        let mut real = VPath::root();
        let mut it = path.components().peekable();
        while let Some(component) = it.next() {
            let folded = component.to_lowercase();
            if let Some(name) = self.dir_map(&real)?.and_then(|map| map.get(&folded).cloned()) {
                real = real.join(&name)?;
            } else {
                // Unresolvable from here on: keep the caller's spelling.
                real = real.join(component)?;
                for rest in it {
                    real = real.join(rest)?;
                }
                return Ok(real);
            }
        }
        Ok(real)
    }

    /// The folded-name → real-name map for `dir`, from cache or freshly
    /// listed. `Ok(None)` when `dir` is missing or not a directory — the
    /// caller then leaves the remaining path untouched for the backend's own
    /// `NotFound`/`NotADirectory` to report.
    fn dir_map(&self, dir: &VPath) -> Result<Option<DirMap>> {
        if let Some(map) = lock_read(&self.cache).get(dir).cloned() {
            return Ok(Some(map));
        }
        let entries = match self.inner.read_dir(dir) {
            Ok(entries) => entries,
            Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let map: DirMap = Arc::new(entries.into_iter().map(|e| (e.name.to_lowercase(), e.name)).collect());
        lock_write(&self.cache).insert(dir.clone(), Arc::clone(&map));
        Ok(Some(map))
    }

    /// Forgets the contents of `path`'s parent (`path` itself is a name in
    /// it), plus `path`'s own subtree — what a mutation at `path` can change.
    fn invalidate_around(&self, path: &VPath) {
        let mut cache = lock_write(&self.cache);
        if let Some(parent) = path.parent() {
            cache.remove(&parent);
        }
        cache.retain(|key, _| !key.starts_with(path));
    }

    /// Re-reports the error against the path the caller passed.
    fn remap(path: &VPath, err: VfsError) -> VfsError {
        let p = || path.clone();
        match err {
            VfsError::NotFound(_) => VfsError::NotFound(p()),
            VfsError::AlreadyExists(_) => VfsError::AlreadyExists(p()),
            VfsError::NotADirectory(_) => VfsError::NotADirectory(p()),
            VfsError::IsADirectory(_) => VfsError::IsADirectory(p()),
            VfsError::DirectoryNotEmpty(_) => VfsError::DirectoryNotEmpty(p()),
            VfsError::ReadOnly(_) => VfsError::ReadOnly(p()),
            VfsError::PermissionDenied(_) => VfsError::PermissionDenied(p()),
            VfsError::StorageFull(_) => VfsError::StorageFull(p()),
            VfsError::PathEscapesRoot(_) => VfsError::PathEscapesRoot(p()),
            other => other,
        }
    }

    /// Resolves `path`, then runs `op` on the real path, remapping errors.
    fn at<T>(&self, path: &VPath, op: impl FnOnce(&VPath) -> Result<T>) -> Result<T> {
        let real = self.resolve(path)?;
        op(&real).map_err(|e| Self::remap(path, e))
    }

    /// [`CaseInsensitiveFs::at`], plus invalidation of the listings the
    /// mutation could have changed. Cache keys are *resolved* dirs, so the
    /// invalidation must run on `real`, not on `path`.
    fn mutating_at<T>(&self, path: &VPath, op: impl FnOnce(&VPath) -> Result<T>) -> Result<T> {
        let real = self.resolve(path)?;
        let out = op(&real).map_err(|e| Self::remap(path, e))?;
        self.invalidate_around(&real);
        Ok(out)
    }
}

impl fmt::Debug for CaseInsensitiveFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaseInsensitiveFs").field("cached_dirs", &lock_read(&self.cache).len()).finish_non_exhaustive()
    }
}

impl FileSystem for CaseInsensitiveFs {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        self.at(path, |real| self.inner.metadata(real))
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        // Entries come back with the backend's real spelling — the point.
        self.at(path, |real| self.inner.read_dir(real))
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        self.mutating_at(path, |real| self.inner.create_dir(real))
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        self.mutating_at(path, |real| self.inner.remove_file(real))
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        self.mutating_at(path, |real| self.inner.remove_dir(real))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let real_from = self.resolve(from)?;
        let real_to = self.resolve(to)?;
        self.inner
            .rename(&real_from, &real_to)
            .map_err(|e| Self::remap(if e.path().is_some_and(|p| *p == real_to) { to } else { from }, e))?;
        self.invalidate_around(&real_from);
        self.invalidate_around(&real_to);
        Ok(())
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        let real = self.resolve(path)?;
        let file = self.inner.open(&real, options).map_err(|e| Self::remap(path, e))?;
        if options.is_write() {
            self.invalidate_around(&real); // a create changes the parent's listing
        }
        Ok(file)
    }

    fn exists(&self, path: &VPath) -> bool {
        self.resolve(path).is_ok_and(|real| self.inner.exists(&real))
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}

/// Lock helpers, as in `openvfs::sync`: a panicking writer must not wedge the
/// cache — every critical section leaves it structurally valid.
fn lock_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}
