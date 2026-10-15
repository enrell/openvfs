//! Composite operations written once, on top of the [`FileSystem`] primitives.
//!
//! Each function works with *any* backend — and across backends, since
//! source and destination are passed separately where it matters.

use std::fmt;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, OpenOptions, msg};
use crate::path::VPath;

/// Reads a whole file into memory.
pub fn read_all(fs: &dyn FileSystem, path: &VPath) -> Result<Vec<u8>> {
    let mut file = fs.open(path, &OpenOptions::reading())?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).map_err(|e| VfsError::from_io(path, e))?;
    Ok(buf)
}

/// Replaces the contents of `path` with `data`, creating the file if needed.
///
/// Readers may observe the file empty or half-written while this runs; use
/// [`write_atomic`] when that matters.
pub fn write_all(fs: &dyn FileSystem, path: &VPath, data: &[u8]) -> Result<()> {
    put(fs, path, data, OpenOptions::writing())
}

/// Appends `data` to `path`, creating the file if needed.
pub fn append_all(fs: &dyn FileSystem, path: &VPath, data: &[u8]) -> Result<()> {
    put(fs, path, data, OpenOptions::appending())
}

fn put(fs: &dyn FileSystem, path: &VPath, data: &[u8], options: OpenOptions) -> Result<()> {
    let mut file = fs.open(path, &options)?;
    file.write_all(data).and_then(|()| file.flush()).map_err(|e| VfsError::from_io(path, e))
}

/// Replaces the contents of `path` so that readers see either the old or the
/// new bytes, never a mix: the data is written to a temporary sibling, synced,
/// then renamed over `path`. On failure the temporary file is removed and
/// `path` is left untouched.
///
/// Atomicity relies on the backend's `rename` replacing files atomically,
/// which holds for [`crate::MemoryFs`], [`crate::OverlayFs`] and (POSIX and
/// NTFS) host directories.
pub fn write_atomic(fs: &dyn FileSystem, path: &VPath, data: &[u8]) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(VfsError::IsADirectory(path.clone()));
    };
    if fs.metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(VfsError::IsADirectory(path.clone()));
    }
    let tmp = parent.child(&format!(".{name}.{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = fs.open(&tmp, &OpenOptions::new().write(true).create_new(true))?;
        file.write_all(data).and_then(|()| file.sync_all()).map_err(|e| VfsError::from_io(&tmp, e))?;
        drop(file);
        fs.rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs.remove_file(&tmp);
    }
    result.map_err(|e| e.with_path(path))
}

/// Creates `path` and every missing ancestor. Existing directories are fine;
/// an existing *file* at `path` is [`VfsError::AlreadyExists`], and one at an
/// ancestor is [`VfsError::NotADirectory`].
pub fn create_dir_all(fs: &dyn FileSystem, path: &VPath) -> Result<()> {
    let mut chain: Vec<VPath> = path.ancestors().collect();
    chain.pop(); // the root always exists
    for dir in chain.into_iter().rev() {
        let step = match fs.metadata(&dir) {
            Ok(meta) if meta.is_dir() => Ok(()),
            Ok(_) if dir == *path => Err(VfsError::AlreadyExists(dir)),
            Ok(_) => Err(VfsError::NotADirectory(dir)),
            Err(VfsError::NotFound(_)) => match fs.create_dir(&dir) {
                Ok(()) => Ok(()),
                // Tolerate a concurrent creator.
                Err(VfsError::AlreadyExists(_)) if fs.metadata(&dir).is_ok_and(|m| m.is_dir()) => Ok(()),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        step.map_err(|e| e.with_path(path))?;
    }
    Ok(())
}

/// Deletes a file, or a directory and everything below it.
pub fn remove_tree(fs: &dyn FileSystem, path: &VPath) -> Result<()> {
    if fs.metadata(path)?.is_file() { fs.remove_file(path) } else { fs.remove_dir_all(path) }
}

/// Copies one file to another location, possibly on another backend, and
/// returns the number of bytes copied. Within one backend this delegates to
/// [`FileSystem::copy_file`] (which may be atomic or server-side); across
/// backends the bytes are streamed.
pub fn copy_file(src_fs: &dyn FileSystem, src: &VPath, dst_fs: &dyn FileSystem, dst: &VPath) -> Result<u64> {
    if std::ptr::addr_eq(src_fs, dst_fs) {
        return src_fs.copy_file(src, dst);
    }
    if src_fs.metadata(src)?.is_dir() {
        return Err(VfsError::IsADirectory(src.clone()));
    }
    stream_copy(src_fs, src, dst_fs, dst)
}

/// Streams `src` into `dst` (created or truncated). No same-file check.
pub(crate) fn stream_copy<S, D>(src_fs: &S, src: &VPath, dst_fs: &D, dst: &VPath) -> Result<u64>
where
    S: FileSystem + ?Sized,
    D: FileSystem + ?Sized,
{
    let mut reader = src_fs.open(src, &OpenOptions::reading())?;
    let mut writer = dst_fs.open(dst, &OpenOptions::writing())?;
    let copied = io::copy(&mut reader, &mut writer).map_err(|e| VfsError::from_io(dst, e))?;
    writer.flush().map_err(|e| VfsError::from_io(dst, e))?;
    Ok(copied)
}

/// Recursively copies a file or directory. Existing destination directories
/// are merged into; existing destination files are overwritten.
pub fn copy_tree(src_fs: &dyn FileSystem, src: &VPath, dst_fs: &dyn FileSystem, dst: &VPath) -> Result<()> {
    if src_fs.metadata(src)?.is_file() {
        return copy_file(src_fs, src, dst_fs, dst).map(drop);
    }
    if std::ptr::addr_eq(src_fs, dst_fs) && dst.starts_with(src) {
        return Err(VfsError::InvalidInput(msg::INTO_ITSELF));
    }
    match dst_fs.create_dir(dst) {
        Ok(()) => {}
        Err(VfsError::AlreadyExists(_)) if dst_fs.metadata(dst)?.is_dir() => {}
        Err(e) => return Err(e),
    }
    for entry in src_fs.read_dir(src)? {
        copy_tree(src_fs, &entry.path, dst_fs, &dst.child(&entry.name))?;
    }
    Ok(())
}

type EntryFilter = Box<dyn FnMut(&DirEntry) -> bool + Send>;

/// A depth-first, pre-order walk with entries sorted by name at each level.
///
/// Created by [`crate::Vfs::walk`]. The starting directory itself is not
/// yielded; walking a file yields nothing. A directory that cannot be listed
/// yields one `Err` and the walk carries on with its siblings. The walk is
/// lazy: each directory is listed only when the iterator reaches it.
///
/// ```
/// use openvfs::Vfs;
///
/// let vfs = Vfs::memory();
/// vfs.create_dir_all("/src/target/debug")?;
/// vfs.write("/src/lib.rs", "")?;
///
/// let found: Vec<String> = vfs
///     .walk("/")?
///     .max_depth(3)
///     .filter_entry(|e| e.name != "target") // prune whole subtrees
///     .map(|e| e.map(|e| e.path.to_string()))
///     .collect::<Result<_, _>>()?;
/// assert_eq!(found, ["/src", "/src/lib.rs"]);
/// # Ok::<(), openvfs::VfsError>(())
/// ```
pub struct Walk {
    fs: Arc<dyn FileSystem>,
    pending_root: Option<VPath>,
    stack: Vec<(Result<DirEntry>, usize)>,
    max_depth: usize,
    filter: Option<EntryFilter>,
}

impl Walk {
    pub(crate) fn new(fs: Arc<dyn FileSystem>, root: &VPath) -> Result<Self> {
        let is_dir = fs.metadata(root)?.is_dir();
        Ok(Self {
            fs,
            pending_root: is_dir.then(|| root.clone()),
            stack: Vec::new(),
            max_depth: usize::MAX,
            filter: None,
        })
    }

    /// Descends at most `depth` levels: `1` yields only the direct children.
    #[must_use]
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Skips entries for which `keep` returns `false`; a skipped directory is
    /// not descended into, which makes this the way to prune subtrees.
    #[must_use]
    pub fn filter_entry(mut self, keep: impl FnMut(&DirEntry) -> bool + Send + 'static) -> Self {
        self.filter = Some(Box::new(keep));
        self
    }

    fn push_children(&mut self, dir: &VPath, depth: usize) {
        match self.fs.read_dir(dir) {
            Ok(mut entries) => {
                // Reverse order, so popping the stack yields ascending names.
                entries.sort_unstable_by(|a, b| b.name.cmp(&a.name));
                self.stack.extend(entries.into_iter().map(|e| (Ok(e), depth)));
            }
            Err(e) => self.stack.push((Err(e), depth)),
        }
    }
}

impl Iterator for Walk {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(root) = self.pending_root.take()
            && self.max_depth > 0
        {
            self.push_children(&root, 1);
        }
        loop {
            let (item, depth) = self.stack.pop()?;
            let entry = match item {
                Ok(entry) => entry,
                Err(e) => return Some(Err(e)),
            };
            if let Some(keep) = &mut self.filter
                && !keep(&entry)
            {
                continue;
            }
            if entry.is_dir() && depth < self.max_depth {
                let dir = entry.path.clone();
                self.push_children(&dir, depth + 1);
            }
            return Some(Ok(entry));
        }
    }
}

impl fmt::Debug for Walk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Walk")
            .field("pending", &self.stack.len())
            .field("max_depth", &self.max_depth)
            .field("filtered", &self.filter.is_some())
            .finish_non_exhaustive()
    }
}
