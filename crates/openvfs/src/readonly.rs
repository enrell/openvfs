//! A wrapper that turns any backend read-only.

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, Metadata, OpenOptions, VfsFile};
use crate::path::VPath;

/// Forwards reads to the wrapped backend and rejects every write with
/// [`VfsError::ReadOnly`]. Metadata reports `read_only: true`.
///
/// ```
/// use openvfs::{MemoryFs, ReadOnlyFs, Vfs, VfsError};
///
/// let vfs = Vfs::new(ReadOnlyFs::new(MemoryFs::new()));
/// assert!(matches!(vfs.write("/x", "nope"), Err(VfsError::ReadOnly(_))));
/// ```
#[derive(Debug, Clone)]
pub struct ReadOnlyFs<F> {
    inner: F,
}

impl<F: FileSystem> ReadOnlyFs<F> {
    /// Wraps `inner`.
    pub fn new(inner: F) -> Self {
        Self { inner }
    }

    /// The wrapped backend.
    pub fn inner(&self) -> &F {
        &self.inner
    }

    /// Unwraps the backend.
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: FileSystem> FileSystem for ReadOnlyFs<F> {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        self.inner.metadata(path).map(|m| m.with_read_only(true))
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn rename(&self, from: &VPath, _to: &VPath) -> Result<()> {
        Err(VfsError::ReadOnly(from.clone()))
    }

    fn copy_file(&self, _from: &VPath, to: &VPath) -> Result<u64> {
        Err(VfsError::ReadOnly(to.clone()))
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        if options.is_write() {
            return Err(VfsError::ReadOnly(path.clone()));
        }
        self.inner.open(path, options)
    }

    fn exists(&self, path: &VPath) -> bool {
        self.inner.exists(path)
    }

    fn is_read_only(&self) -> bool {
        true
    }
}
