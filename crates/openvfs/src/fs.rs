//! The backend contract — [`FileSystem`] — and the value types it speaks.

use std::io::{self, Read, Seek, Write};
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Result, VfsError};
use crate::path::VPath;

/// Error messages shared by every backend, so they read identically everywhere.
pub(crate) mod msg {
    pub(crate) const REMOVE_ROOT: &str = "cannot remove the root directory";
    pub(crate) const RENAME_ROOT: &str = "cannot rename the root directory";
    pub(crate) const INTO_ITSELF: &str = "cannot move or copy a directory inside itself";
    pub(crate) const SAME_FILE: &str = "source and destination are the same file";
}

/// What kind of node a path points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

/// Information about a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// File or directory.
    pub file_type: FileType,
    /// Size in bytes (always `0` for directories).
    pub len: u64,
    /// Last modification time, if the backend tracks it.
    pub modified: Option<SystemTime>,
    /// `true` when the node cannot be written through this file system.
    pub read_only: bool,
}

impl Metadata {
    /// Metadata for a file of `len` bytes.
    #[must_use]
    pub fn file(len: u64) -> Self {
        Self { file_type: FileType::File, len, modified: None, read_only: false }
    }

    /// Metadata for a directory.
    #[must_use]
    pub fn dir() -> Self {
        Self { file_type: FileType::Dir, len: 0, modified: None, read_only: false }
    }

    /// Sets the modification time.
    #[must_use]
    pub fn with_modified(mut self, modified: Option<SystemTime>) -> Self {
        self.modified = modified;
        self
    }

    /// Sets the read-only flag.
    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// `true` for regular files.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.file_type == FileType::File
    }

    /// `true` for directories.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Dir
    }
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's own name (last path component).
    pub name: String,
    /// The full virtual path of the entry.
    pub path: VPath,
    /// File or directory.
    pub file_type: FileType,
}

impl DirEntry {
    /// `true` for directories.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Dir
    }

    /// `true` for regular files.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.file_type == FileType::File
    }
}

/// How to open a file. Mirrors [`std::fs::OpenOptions`] with a by-value builder.
///
/// ```
/// use openvfs::OpenOptions;
///
/// let opts = OpenOptions::new().write(true).create(true);
/// assert!(opts.is_write());
/// assert!(OpenOptions::new().validate().is_err()); // no access mode at all
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // a bag of independent flags is the point
pub struct OpenOptions {
    /// Allow reading.
    pub read: bool,
    /// Allow writing at the cursor.
    pub write: bool,
    /// Every write goes to the end of the file.
    pub append: bool,
    /// Empty the file on open.
    pub truncate: bool,
    /// Create the file if it is missing.
    pub create: bool,
    /// Create the file, failing if it already exists.
    pub create_new: bool,
}

impl OpenOptions {
    /// All flags off.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only access to an existing file.
    #[must_use]
    pub fn reading() -> Self {
        Self::new().read(true)
    }

    /// Create-or-truncate, then write — like [`std::fs::File::create`].
    #[must_use]
    pub fn writing() -> Self {
        Self::new().write(true).create(true).truncate(true)
    }

    /// Create-if-missing, then append.
    #[must_use]
    pub fn appending() -> Self {
        Self::new().append(true).create(true)
    }

    /// Sets [`OpenOptions::read`].
    #[must_use]
    pub fn read(mut self, on: bool) -> Self {
        self.read = on;
        self
    }

    /// Sets [`OpenOptions::write`].
    #[must_use]
    pub fn write(mut self, on: bool) -> Self {
        self.write = on;
        self
    }

    /// Sets [`OpenOptions::append`].
    #[must_use]
    pub fn append(mut self, on: bool) -> Self {
        self.append = on;
        self
    }

    /// Sets [`OpenOptions::truncate`].
    #[must_use]
    pub fn truncate(mut self, on: bool) -> Self {
        self.truncate = on;
        self
    }

    /// Sets [`OpenOptions::create`].
    #[must_use]
    pub fn create(mut self, on: bool) -> Self {
        self.create = on;
        self
    }

    /// Sets [`OpenOptions::create_new`].
    #[must_use]
    pub fn create_new(mut self, on: bool) -> Self {
        self.create_new = on;
        self
    }

    /// `true` if the handle may modify the file.
    #[must_use]
    pub fn is_write(&self) -> bool {
        self.write || self.append
    }

    /// Rejects flag combinations that make no sense (same rules as `std`).
    pub fn validate(&self) -> Result<()> {
        if !self.read && !self.is_write() {
            return Err(VfsError::InvalidInput("open needs read, write or append access"));
        }
        if (self.truncate || self.create || self.create_new) && !self.is_write() {
            return Err(VfsError::InvalidInput("create/truncate require write access"));
        }
        if self.truncate && self.append {
            return Err(VfsError::InvalidInput("truncate and append are mutually exclusive"));
        }
        Ok(())
    }

    pub(crate) fn to_std(self) -> std::fs::OpenOptions {
        let mut o = std::fs::OpenOptions::new();
        o.read(self.read)
            .write(self.write)
            .append(self.append)
            .truncate(self.truncate)
            .create(self.create)
            .create_new(self.create_new);
        o
    }
}

/// An open file: the [`std::io`] traits plus the two file-specific operations
/// every serious consumer eventually needs.
pub trait VfsFile: Read + Write + Seek + Send {
    /// Truncates or zero-extends the file to exactly `size` bytes.
    /// Requires a handle opened with write access.
    fn set_len(&mut self, size: u64) -> io::Result<()>;

    /// Flushes data *and* metadata to durable storage. For volatile backends
    /// (memory) this is the same as [`Write::flush`].
    fn sync_all(&mut self) -> io::Result<()> {
        self.flush()
    }
}

/// Lets `Result<Box<dyn VfsFile>>` be debug-printed and `unwrap_err`'d.
impl std::fmt::Debug for dyn VfsFile + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VfsFile { .. }")
    }
}

impl VfsFile for std::fs::File {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        std::fs::File::set_len(self, size)
    }

    fn sync_all(&mut self) -> io::Result<()> {
        std::fs::File::sync_all(self)
    }
}

impl<T: VfsFile + ?Sized> VfsFile for Box<T> {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        (**self).set_len(size)
    }

    fn sync_all(&mut self) -> io::Result<()> {
        (**self).sync_all()
    }
}

/// The contract every backend implements.
///
/// Seven primitives are required; [`FileSystem::remove_dir_all`] and
/// [`FileSystem::copy_file`] have portable default implementations that a
/// backend may override with something faster or atomic. Everything else
/// (`create_dir_all`, recursive copy, walking, atomic writes, …) lives in
/// [`crate::ops`] and [`crate::Vfs`], written once on top of this trait.
///
/// # Semantics
///
/// Every bundled backend passes the same conformance and differential test
/// suites, which pin down this behavior:
///
/// * `create_dir` and `open(.., create)` require the parent to exist;
/// * `remove_dir` only removes *empty* directories; the root can never be removed;
/// * `rename` replaces an existing *file* at the destination, refuses to
///   overwrite a directory or to move a directory inside itself, and renaming
///   a path onto itself is a no-op;
/// * `copy_file` refuses to copy a file onto itself (instead of truncating it);
/// * a path that runs *through* a file (`/file.txt/x`) is
///   [`VfsError::NotADirectory`]; a path that simply does not exist is
///   [`VfsError::NotFound`];
/// * errors report the path that was passed in (see [`VfsError`]).
///
/// Implementations must be thread-safe: a `FileSystem` is typically shared
/// behind an `Arc` and used from many threads at once.
pub trait FileSystem: Send + Sync {
    /// Returns information about `path`.
    fn metadata(&self, path: &VPath) -> Result<Metadata>;

    /// Lists the direct children of the directory at `path`, in any order.
    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>>;

    /// Creates a single directory. The parent must exist.
    fn create_dir(&self, path: &VPath) -> Result<()>;

    /// Deletes a file.
    fn remove_file(&self, path: &VPath) -> Result<()>;

    /// Deletes an empty directory.
    fn remove_dir(&self, path: &VPath) -> Result<()>;

    /// Moves a file or directory.
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()>;

    /// Opens a file with the given options.
    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>>;

    /// Deletes a directory and everything below it.
    ///
    /// The default walks the tree; backends override it to be atomic
    /// ([`crate::MemoryFs`], [`crate::OverlayFs`]) or to avoid following
    /// host symlinks ([`crate::PhysicalFs`]).
    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        if !self.metadata(path)?.is_dir() {
            return Err(VfsError::NotADirectory(path.clone()));
        }
        for entry in self.read_dir(path)? {
            if entry.is_dir() {
                self.remove_dir_all(&entry.path)?;
            } else {
                self.remove_file(&entry.path)?;
            }
        }
        self.remove_dir(path)
    }

    /// Copies a file within this file system, replacing `to` if it is a file.
    /// Returns the number of bytes copied.
    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        if self.metadata(from)?.is_dir() {
            return Err(VfsError::IsADirectory(from.clone()));
        }
        if from == to {
            return Err(VfsError::InvalidInput(msg::SAME_FILE));
        }
        crate::ops::stream_copy(self, from, self, to)
    }

    /// `true` if `path` exists. Errors other than "missing" also yield `false`;
    /// use [`crate::Vfs::try_exists`] to tell them apart.
    fn exists(&self, path: &VPath) -> bool {
        self.metadata(path).is_ok()
    }

    /// `true` if the whole file system rejects writes.
    fn is_read_only(&self) -> bool {
        false
    }
}

impl<T: FileSystem + ?Sized> FileSystem for Arc<T> {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        (**self).metadata(path)
    }
    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        (**self).read_dir(path)
    }
    fn create_dir(&self, path: &VPath) -> Result<()> {
        (**self).create_dir(path)
    }
    fn remove_file(&self, path: &VPath) -> Result<()> {
        (**self).remove_file(path)
    }
    fn remove_dir(&self, path: &VPath) -> Result<()> {
        (**self).remove_dir(path)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        (**self).rename(from, to)
    }
    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        (**self).open(path, options)
    }
    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        (**self).remove_dir_all(path)
    }
    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        (**self).copy_file(from, to)
    }
    fn exists(&self, path: &VPath) -> bool {
        (**self).exists(path)
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
}

/// Builds the right "missing" error for `path`: [`VfsError::NotADirectory`]
/// if the nearest existing ancestor is a file, [`VfsError::NotFound`] otherwise.
pub(crate) fn classify_missing(path: &VPath, mut kind_of: impl FnMut(&VPath) -> Option<FileType>) -> VfsError {
    for ancestor in path.ancestors().skip(1) {
        match kind_of(&ancestor) {
            Some(FileType::File) => return VfsError::NotADirectory(path.clone()),
            Some(FileType::Dir) => break,
            None => {}
        }
    }
    VfsError::NotFound(path.clone())
}
