//! The ergonomic front door: [`Vfs`].

use std::fmt;
use std::io;
use std::sync::Arc;

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, Metadata, OpenOptions, VfsFile};
use crate::memory::MemoryFs;
use crate::ops::{self, Walk};
use crate::path::VPath;

/// A cheap-to-clone handle over any [`FileSystem`], with a `std::fs`-like API.
///
/// Every method takes paths as `impl AsRef<str>` (so `&str`, `String` and
/// [`VPath`] all work) and normalizes them before reaching the backend.
/// Clones share the same backend.
///
/// ```
/// use openvfs::Vfs;
///
/// let vfs = Vfs::memory();
/// vfs.create_dir_all("/projects/demo/src")?;
/// vfs.write("/projects/demo/src/main.rs", "fn main() {}")?;
///
/// let names: Vec<_> = vfs.walk("/projects")?
///     .map(|entry| entry.map(|e| e.path.to_string()))
///     .collect::<Result<_, _>>()?;
/// assert_eq!(names, ["/projects/demo", "/projects/demo/src", "/projects/demo/src/main.rs"]);
/// # Ok::<(), openvfs::VfsError>(())
/// ```
#[derive(Clone)]
pub struct Vfs {
    fs: Arc<dyn FileSystem>,
}

impl Vfs {
    /// Wraps a backend.
    pub fn new(fs: impl FileSystem + 'static) -> Self {
        Self { fs: Arc::new(fs) }
    }

    /// Wraps an already shared backend (keep your own `Arc` to reach
    /// backend-specific APIs such as [`crate::OverlayFs::changes`]).
    #[must_use]
    pub fn from_arc(fs: Arc<dyn FileSystem>) -> Self {
        Self { fs }
    }

    /// A fresh, empty in-memory file system.
    #[must_use]
    pub fn memory() -> Self {
        Self::new(MemoryFs::new())
    }

    /// The underlying backend.
    #[must_use]
    pub fn backend(&self) -> &Arc<dyn FileSystem> {
        &self.fs
    }

    /// Information about `path`.
    pub fn metadata(&self, path: impl AsRef<str>) -> Result<Metadata> {
        self.fs.metadata(&VPath::new(path)?)
    }

    /// `true` if `path` exists. Any error counts as "no"; see [`Vfs::try_exists`].
    pub fn exists(&self, path: impl AsRef<str>) -> bool {
        VPath::new(path).is_ok_and(|p| self.fs.exists(&p))
    }

    /// `Ok(true)`/`Ok(false)` when existence can be determined, and the error
    /// otherwise (e.g. [`VfsError::PermissionDenied`]).
    pub fn try_exists(&self, path: impl AsRef<str>) -> Result<bool> {
        match self.fs.metadata(&VPath::new(path)?) {
            Ok(_) => Ok(true),
            Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `true` if `path` exists and is a file.
    pub fn is_file(&self, path: impl AsRef<str>) -> bool {
        self.metadata(path).is_ok_and(|m| m.is_file())
    }

    /// `true` if `path` exists and is a directory.
    pub fn is_dir(&self, path: impl AsRef<str>) -> bool {
        self.metadata(path).is_ok_and(|m| m.is_dir())
    }

    /// Direct children of a directory, sorted by name.
    pub fn read_dir(&self, path: impl AsRef<str>) -> Result<Vec<DirEntry>> {
        let mut entries = self.fs.read_dir(&VPath::new(path)?)?;
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Recursively walks a directory; see [`Walk`].
    pub fn walk(&self, path: impl AsRef<str>) -> Result<Walk> {
        Walk::new(Arc::clone(&self.fs), &VPath::new(path)?)
    }

    /// Creates one directory; the parent must exist.
    pub fn create_dir(&self, path: impl AsRef<str>) -> Result<()> {
        self.fs.create_dir(&VPath::new(path)?)
    }

    /// Creates a directory and all missing parents.
    pub fn create_dir_all(&self, path: impl AsRef<str>) -> Result<()> {
        ops::create_dir_all(&*self.fs, &VPath::new(path)?)
    }

    /// Opens a file with explicit options.
    pub fn open(&self, path: impl AsRef<str>, options: OpenOptions) -> Result<Box<dyn VfsFile>> {
        self.fs.open(&VPath::new(path)?, &options)
    }

    /// Opens an existing file for reading.
    pub fn open_read(&self, path: impl AsRef<str>) -> Result<Box<dyn VfsFile>> {
        self.open(path, OpenOptions::reading())
    }

    /// Creates (or truncates) a file for writing.
    pub fn create(&self, path: impl AsRef<str>) -> Result<Box<dyn VfsFile>> {
        self.open(path, OpenOptions::writing())
    }

    /// Reads a whole file.
    pub fn read(&self, path: impl AsRef<str>) -> Result<Vec<u8>> {
        ops::read_all(&*self.fs, &VPath::new(path)?)
    }

    /// Reads a whole UTF-8 file.
    pub fn read_to_string(&self, path: impl AsRef<str>) -> Result<String> {
        let path = VPath::new(path)?;
        let bytes = ops::read_all(&*self.fs, &path)?;
        String::from_utf8(bytes)
            .map_err(|e| VfsError::Io { path: Some(path), source: io::Error::new(io::ErrorKind::InvalidData, e) })
    }

    /// Writes `contents` to a file, creating or truncating it. The parent must exist.
    pub fn write(&self, path: impl AsRef<str>, contents: impl AsRef<[u8]>) -> Result<()> {
        ops::write_all(&*self.fs, &VPath::new(path)?, contents.as_ref())
    }

    /// Like [`Vfs::write`], but readers never observe a partial file; see [`ops::write_atomic`].
    pub fn write_atomic(&self, path: impl AsRef<str>, contents: impl AsRef<[u8]>) -> Result<()> {
        ops::write_atomic(&*self.fs, &VPath::new(path)?, contents.as_ref())
    }

    /// Appends `contents` to a file, creating it if needed.
    pub fn append(&self, path: impl AsRef<str>, contents: impl AsRef<[u8]>) -> Result<()> {
        ops::append_all(&*self.fs, &VPath::new(path)?, contents.as_ref())
    }

    /// Truncates or zero-extends an existing file to `len` bytes.
    pub fn set_len(&self, path: impl AsRef<str>, len: u64) -> Result<()> {
        let path = VPath::new(path)?;
        let mut file = self.fs.open(&path, &OpenOptions::new().write(true))?;
        file.set_len(len).map_err(|e| VfsError::from_io(&path, e))
    }

    /// Deletes a file.
    pub fn remove_file(&self, path: impl AsRef<str>) -> Result<()> {
        self.fs.remove_file(&VPath::new(path)?)
    }

    /// Deletes an empty directory.
    pub fn remove_dir(&self, path: impl AsRef<str>) -> Result<()> {
        self.fs.remove_dir(&VPath::new(path)?)
    }

    /// Deletes a file or a whole directory tree.
    pub fn remove_dir_all(&self, path: impl AsRef<str>) -> Result<()> {
        ops::remove_tree(&*self.fs, &VPath::new(path)?)
    }

    /// Moves a file or directory.
    ///
    /// Moves between mounts fall back to copy + delete, with the same rules as
    /// a plain rename (a directory is never merged into an existing one). That
    /// fallback is not atomic.
    pub fn rename(&self, from: impl AsRef<str>, to: impl AsRef<str>) -> Result<()> {
        let (from, to) = (VPath::new(from)?, VPath::new(to)?);
        match self.fs.rename(&from, &to) {
            Err(VfsError::CrossDevice { .. }) => {
                let src_is_dir = self.fs.metadata(&from)?.is_dir();
                match self.fs.metadata(&to) {
                    Ok(dst) if dst.is_dir() => return Err(VfsError::AlreadyExists(to)),
                    Ok(_) if src_is_dir => return Err(VfsError::NotADirectory(to)),
                    Ok(_) | Err(VfsError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
                ops::copy_tree(&*self.fs, &from, &*self.fs, &to)?;
                ops::remove_tree(&*self.fs, &from)
            }
            other => other,
        }
    }

    /// Copies one file, returning the number of bytes copied.
    pub fn copy(&self, from: impl AsRef<str>, to: impl AsRef<str>) -> Result<u64> {
        ops::copy_file(&*self.fs, &VPath::new(from)?, &*self.fs, &VPath::new(to)?)
    }

    /// Recursively copies a file or directory.
    pub fn copy_dir_all(&self, from: impl AsRef<str>, to: impl AsRef<str>) -> Result<()> {
        ops::copy_tree(&*self.fs, &VPath::new(from)?, &*self.fs, &VPath::new(to)?)
    }
}

impl<F: FileSystem + 'static> From<Arc<F>> for Vfs {
    fn from(fs: Arc<F>) -> Self {
        Self { fs }
    }
}

impl fmt::Debug for Vfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vfs").finish_non_exhaustive()
    }
}
