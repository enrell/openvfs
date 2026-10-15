//! A thread-safe, purely in-memory file system with an optional byte quota.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, FileType, Metadata, OpenOptions, VfsFile, classify_missing, msg};
use crate::path::VPath;
use crate::sync;

/// A file system that lives entirely in RAM.
///
/// Nodes are kept in an ordered map keyed by their full path. Because a
/// directory's descendants share a string prefix (`/a/…`), they form one
/// contiguous range of the map, which makes listing, emptiness checks, subtree
/// renames and recursive deletes simple range scans — all performed under a
/// single lock, so every operation is **atomic** and **linearizable**.
///
/// File contents sit behind their own lock, so an open handle keeps reading
/// and writing without contending on the tree lock. As on Unix, every handle
/// to the same file sees the same bytes, and a handle stays usable after its
/// file has been removed.
///
/// A quota ([`MemoryFs::with_quota`]) bounds the total file bytes, which makes
/// it safe to accept writes from untrusted input; exceeding it fails with
/// [`VfsError::StorageFull`] (or an [`io::ErrorKind::StorageFull`] I/O error
/// from a handle). Bytes are released when the last handle to a removed file
/// is dropped.
///
/// ```
/// use openvfs::{MemoryFs, Vfs, VfsError};
///
/// let vfs = Vfs::new(MemoryFs::with_quota(8));
/// vfs.write("/small", "12345678")?;
/// assert!(matches!(vfs.write("/big", "123456789"), Err(VfsError::StorageFull(_))));
/// # Ok::<(), VfsError>(())
/// ```
pub struct MemoryFs {
    tree: RwLock<Tree>,
    usage: Arc<Usage>,
}

type Tree = BTreeMap<VPath, Node>;
type SharedData = Arc<RwLock<FileData>>;

enum Node {
    Dir { modified: SystemTime },
    File(SharedData),
}

impl Node {
    fn file_type(&self) -> FileType {
        match self {
            Self::Dir { .. } => FileType::Dir,
            Self::File(_) => FileType::File,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, Self::Dir { .. })
    }
}

/// Byte accounting shared by a file system and every file it ever created.
struct Usage {
    used: AtomicU64,
    limit: u64,
}

impl Usage {
    /// Atomically claims `bytes`, or returns `false` if that would exceed the limit.
    fn reserve(&self, bytes: u64) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(total) = used.checked_add(bytes).filter(|&total| total <= self.limit) else {
                return false;
            };
            match self.used.compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    fn release(&self, bytes: u64) {
        if bytes > 0 {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

struct FileData {
    bytes: Vec<u8>,
    modified: SystemTime,
    usage: Arc<Usage>,
}

impl FileData {
    fn new(usage: Arc<Usage>) -> Self {
        Self { bytes: Vec::new(), modified: SystemTime::now(), usage }
    }

    /// Grows (zero-filling) or shrinks the contents, charging the quota.
    fn resize(&mut self, len: usize) -> io::Result<()> {
        let old = self.bytes.len();
        if len > old {
            if !self.usage.reserve((len - old) as u64) {
                return Err(io::Error::new(io::ErrorKind::StorageFull, "memory quota exceeded"));
            }
            self.bytes.resize(len, 0);
        } else if len < old {
            self.usage.release((old - len) as u64);
            if len == 0 {
                self.bytes = Vec::new(); // actually give the memory back
            } else {
                self.bytes.truncate(len);
            }
        }
        self.modified = SystemTime::now();
        Ok(())
    }

    fn replace(&mut self, data: &[u8]) -> io::Result<()> {
        self.resize(data.len())?;
        self.bytes.copy_from_slice(data);
        Ok(())
    }
}

impl Drop for FileData {
    fn drop(&mut self) {
        self.usage.release(self.bytes.len() as u64);
    }
}

impl MemoryFs {
    /// An empty, unbounded file system containing only `/`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(u64::MAX)
    }

    /// An empty file system whose files may hold at most `max_bytes` in total.
    #[must_use]
    pub fn with_quota(max_bytes: u64) -> Self {
        Self::with_limit(max_bytes)
    }

    fn with_limit(limit: u64) -> Self {
        let mut tree = Tree::new();
        tree.insert(VPath::root(), Node::Dir { modified: SystemTime::now() });
        Self { tree: RwLock::new(tree), usage: Arc::new(Usage { used: AtomicU64::new(0), limit }) }
    }

    /// Bytes currently held by files (including removed files still open).
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.usage.used.load(Ordering::Acquire)
    }

    /// The configured quota, if any.
    #[must_use]
    pub fn quota(&self) -> Option<u64> {
        (self.usage.limit != u64::MAX).then_some(self.usage.limit)
    }

    /// Number of nodes (files and directories, including `/`).
    #[must_use]
    pub fn node_count(&self) -> usize {
        sync::read(&self.tree).len()
    }

    /// The shared contents of the file at `path`, if it is a file.
    fn lookup_file(&self, path: &VPath) -> Result<Option<SharedData>> {
        match sync::read(&self.tree).get(path) {
            Some(Node::File(data)) => Ok(Some(Arc::clone(data))),
            Some(Node::Dir { .. }) => Err(VfsError::IsADirectory(path.clone())),
            None => Ok(None),
        }
    }

    /// Inserts an empty file, re-checking under the write lock in case
    /// another thread created it in the meantime.
    fn create_file(&self, path: &VPath, exclusive: bool) -> Result<SharedData> {
        let mut tree = sync::write(&self.tree);
        match tree.get(path) {
            Some(Node::File(_)) if exclusive => Err(VfsError::AlreadyExists(path.clone())),
            Some(Node::File(data)) => Ok(Arc::clone(data)),
            Some(Node::Dir { .. }) => Err(VfsError::IsADirectory(path.clone())),
            None => {
                check_parent(&tree, path)?;
                let data = Arc::new(RwLock::new(FileData::new(Arc::clone(&self.usage))));
                tree.insert(path.clone(), Node::File(Arc::clone(&data)));
                touch(&mut tree, path.parent());
                Ok(data)
            }
        }
    }
}

impl Default for MemoryFs {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MemoryFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryFs")
            .field("nodes", &self.node_count())
            .field("used_bytes", &self.used_bytes())
            .field("quota", &self.quota())
            .finish()
    }
}

/// Every strict descendant of `dir`, in path order.
fn descendants<'a>(tree: &'a Tree, dir: &VPath) -> impl Iterator<Item = (&'a VPath, &'a Node)> + use<'a> {
    let prefix = dir.descendant_prefix();
    tree.range(VPath::bound(prefix.clone())..)
        .take_while(move |(k, _)| k.as_str().starts_with(prefix.as_str()))
        .filter(|(k, _)| !k.is_root())
}

/// Only the direct children of `dir`, in path order.
///
/// The map is ordered by raw bytes, so a child's subtree (`/a/…`) is contiguous
/// but *not* adjacent to the child: siblings such as `/a b` and `/a-b` sort
/// between `/a` and `/a/x`. The scan therefore walks key by key, and the moment
/// it meets a node deeper than a child, it **jumps** past that child's whole
/// subtree instead of walking it. Listing costs `O(children · log n)` however
/// large the subtrees are; filtering [`descendants`] would cost `O(subtree)`.
fn children<'a>(tree: &'a Tree, dir: &VPath) -> Children<'a> {
    let prefix = dir.descendant_prefix();
    Children { tree, next: Bound::Included(VPath::bound(prefix.clone())), prefix }
}

struct Children<'a> {
    tree: &'a Tree,
    prefix: String,
    next: Bound<VPath>,
}

impl<'a> Iterator for Children<'a> {
    type Item = (&'a VPath, &'a Node);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (key, node) = self.tree.range((self.next.clone(), Bound::Unbounded)).next()?;
            let rest = key.as_str().strip_prefix(self.prefix.as_str())?; // left the directory's range: done
            if let Some(slash) = rest.find('/') {
                // Deeper than a child. Its ancestor, a child of ours, sorts first and was already
                // reached, so skip to the first key after that ancestor's subtree: `"0"` is `"/"` + 1.
                let child = &key.as_str()[..self.prefix.len() + slash];
                self.next = Bound::Included(VPath::bound(format!("{child}0")));
                continue;
            }
            self.next = Bound::Excluded(key.clone());
            if !key.is_root() {
                return Some((key, node)); // listing `/` starts at the root's own key
            }
        }
    }
}

fn classify(tree: &Tree, path: &VPath) -> VfsError {
    classify_missing(path, |p| tree.get(p).map(Node::file_type))
}

fn check_parent(tree: &Tree, path: &VPath) -> Result<()> {
    match path.parent() {
        None => Err(VfsError::AlreadyExists(path.clone())),
        Some(parent) if matches!(tree.get(&parent), Some(Node::Dir { .. })) => Ok(()),
        Some(_) => Err(classify(tree, path)),
    }
}

/// Bumps a directory's modification time, as a real file system does when
/// entries are added or removed.
fn touch(tree: &mut Tree, dir: Option<VPath>) {
    if let Some(dir) = dir
        && let Some(Node::Dir { modified }) = tree.get_mut(&dir)
    {
        *modified = SystemTime::now();
    }
}

impl FileSystem for MemoryFs {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        let tree = sync::read(&self.tree);
        match tree.get(path) {
            Some(Node::Dir { modified }) => Ok(Metadata::dir().with_modified(Some(*modified))),
            Some(Node::File(data)) => {
                let data = sync::read(data);
                Ok(Metadata::file(data.bytes.len() as u64).with_modified(Some(data.modified)))
            }
            None => Err(classify(&tree, path)),
        }
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        let tree = sync::read(&self.tree);
        match tree.get(path) {
            Some(Node::Dir { .. }) => Ok(children(&tree, path)
                .map(|(child, node)| DirEntry {
                    name: child.file_name().unwrap_or_default().to_owned(),
                    path: child.clone(),
                    file_type: node.file_type(),
                })
                .collect()),
            Some(Node::File(_)) => Err(VfsError::NotADirectory(path.clone())),
            None => Err(classify(&tree, path)),
        }
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        let mut tree = sync::write(&self.tree);
        if tree.contains_key(path) {
            return Err(VfsError::AlreadyExists(path.clone()));
        }
        check_parent(&tree, path)?;
        tree.insert(path.clone(), Node::Dir { modified: SystemTime::now() });
        touch(&mut tree, path.parent());
        Ok(())
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        let mut tree = sync::write(&self.tree);
        match tree.get(path) {
            Some(Node::File(_)) => {
                tree.remove(path);
                touch(&mut tree, path.parent());
                Ok(())
            }
            Some(Node::Dir { .. }) => Err(VfsError::IsADirectory(path.clone())),
            None => Err(classify(&tree, path)),
        }
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        let mut tree = sync::write(&self.tree);
        match tree.get(path) {
            Some(Node::Dir { .. }) if descendants(&tree, path).next().is_some() => {
                Err(VfsError::DirectoryNotEmpty(path.clone()))
            }
            Some(Node::Dir { .. }) => {
                tree.remove(path);
                touch(&mut tree, path.parent());
                Ok(())
            }
            Some(Node::File(_)) => Err(VfsError::NotADirectory(path.clone())),
            None => Err(classify(&tree, path)),
        }
    }

    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        let mut tree = sync::write(&self.tree);
        match tree.get(path) {
            Some(Node::Dir { .. }) => {
                let doomed: Vec<VPath> =
                    std::iter::once(path.clone()).chain(descendants(&tree, path).map(|(k, _)| k.clone())).collect();
                for key in doomed {
                    tree.remove(&key);
                }
                touch(&mut tree, path.parent());
                Ok(())
            }
            Some(Node::File(_)) => Err(VfsError::NotADirectory(path.clone())),
            None => Err(classify(&tree, path)),
        }
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(VfsError::InvalidInput(msg::RENAME_ROOT));
        }
        let mut tree = sync::write(&self.tree);
        let src_is_dir = match tree.get(from) {
            Some(node) => node.is_dir(),
            None => return Err(classify(&tree, from)),
        };
        if from == to {
            return Ok(());
        }
        if src_is_dir && to.starts_with(from) {
            return Err(VfsError::InvalidInput(msg::INTO_ITSELF));
        }
        check_parent(&tree, to)?;
        match tree.get(to) {
            Some(node) if node.is_dir() => return Err(VfsError::AlreadyExists(to.clone())),
            Some(_) if src_is_dir => return Err(VfsError::NotADirectory(to.clone())),
            _ => {}
        }

        let mut moved = vec![from.clone()];
        moved.extend(descendants(&tree, from).map(|(k, _)| k.clone()));
        for old in moved {
            let node = tree.remove(&old).expect("key was collected under the same lock");
            let new = old.rebase(from, to).expect("every moved key lives under `from`");
            tree.insert(new, node);
        }
        touch(&mut tree, from.parent());
        touch(&mut tree, to.parent());
        Ok(())
    }

    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        let mut tree = sync::write(&self.tree);
        let src_data = match tree.get(from) {
            Some(Node::File(data)) => Arc::clone(data),
            Some(Node::Dir { .. }) => return Err(VfsError::IsADirectory(from.clone())),
            None => return Err(classify(&tree, from)),
        };
        if from == to {
            return Err(VfsError::InvalidInput(msg::SAME_FILE));
        }
        if matches!(tree.get(to), Some(Node::Dir { .. })) {
            return Err(VfsError::IsADirectory(to.clone()));
        }
        check_parent(&tree, to)?;
        let src = sync::read(&src_data);
        let quota = |e| VfsError::from_io(to, e);
        match tree.get(to) {
            Some(Node::Dir { .. }) => unreachable!("checked above, under the same lock"),
            // Rewrite in place, so existing handles to `to` see the new bytes.
            Some(Node::File(dst)) => sync::write(dst).replace(&src.bytes).map_err(quota)?,
            None => {
                let mut data = FileData::new(Arc::clone(&self.usage));
                data.replace(&src.bytes).map_err(quota)?;
                tree.insert(to.clone(), Node::File(Arc::new(RwLock::new(data))));
            }
        }
        touch(&mut tree, to.parent());
        Ok(src.bytes.len() as u64)
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        options.validate()?;
        let data = match self.lookup_file(path)? {
            Some(_) if options.create_new => return Err(VfsError::AlreadyExists(path.clone())),
            Some(data) => data,
            None if options.create || options.create_new => self.create_file(path, options.create_new)?,
            None => return Err(classify(&sync::read(&self.tree), path)),
        };
        if options.truncate {
            sync::write(&data).resize(0).map_err(|e| VfsError::from_io(path, e))?;
        }
        Ok(Box::new(MemFile {
            data,
            pos: 0,
            readable: options.read,
            writable: options.is_write(),
            append: options.append,
        }))
    }
}

/// An open handle onto a [`MemoryFs`] file.
struct MemFile {
    data: SharedData,
    pos: u64,
    readable: bool,
    writable: bool,
    append: bool,
}

fn denied(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, msg)
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, "offset exceeds addressable memory")
}

impl Read for MemFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.readable {
            return Err(denied("file was not opened for reading"));
        }
        let data = sync::read(&self.data);
        let start = usize::try_from(self.pos).unwrap_or(usize::MAX).min(data.bytes.len());
        let n = buf.len().min(data.bytes.len() - start);
        buf[..n].copy_from_slice(&data.bytes[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for MemFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.writable {
            return Err(denied("file was not opened for writing"));
        }
        let mut data = sync::write(&self.data);
        if self.append {
            self.pos = data.bytes.len() as u64;
        }
        let start = usize::try_from(self.pos).map_err(|_| too_large())?;
        let end = start.checked_add(buf.len()).ok_or_else(too_large)?;
        if data.bytes.len() < end {
            data.resize(end)?; // writing past EOF leaves a zero-filled gap, like a sparse file
        }
        data.bytes[start..end].copy_from_slice(buf);
        data.modified = SystemTime::now();
        self.pos = end as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for MemFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = sync::read(&self.data).bytes.len() as u64;
        let target = match pos {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(delta) => len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        self.pos = target
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek to a negative or overflowing position"))?;
        Ok(self.pos)
    }
}

impl VfsFile for MemFile {
    fn set_len(&mut self, size: u64) -> io::Result<()> {
        if !self.writable {
            return Err(denied("file was not opened for writing"));
        }
        let size = usize::try_from(size).map_err(|_| too_large())?;
        sync::write(&self.data).resize(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(raw: &str) -> VPath {
        VPath::new(raw).unwrap()
    }

    /// The obvious `O(subtree)` definition the skip-scan must agree with.
    fn by_filtering<'a>(tree: &'a Tree, dir: &VPath) -> Vec<&'a VPath> {
        let skip = dir.descendant_prefix().len();
        descendants(tree, dir).filter(|(k, _)| !k.as_str()[skip..].contains('/')).map(|(k, _)| k).collect()
    }

    #[test]
    fn children_skip_scan_agrees_with_filtering_the_subtree() {
        // Names chosen to sort right next to a directory's subtree: ' ' and '-' sort before '/',
        // '0' and '1' after it, and "x0" is the very key the scan jumps to after "x/…".
        let fs = MemoryFs::new();
        let dirs = [
            "/a", "/a b", "/a-b", "/a0", "/a/x", "/a/x0", "/a/x/y", "/a/x/y/z", "/a/ x", "/a/x-", "/a/x1", "/a/x1/q",
            "/ç", "/ç/ç", "/0", "/0/0",
        ];
        for dir in dirs {
            fs.create_dir(&p(dir)).unwrap();
        }
        for file in ["/a/f", "/a/x/f", "/a0/f", "/z", "/a/x0/f", "/0/1"] {
            fs.create_file(&p(file), true).unwrap();
        }

        let tree = sync::read(&fs.tree);
        let mut checked = 0;
        for dir in tree.iter().filter(|(_, node)| node.is_dir()).map(|(k, _)| k) {
            let got: Vec<_> = children(&tree, dir).map(|(k, _)| k).collect();
            assert_eq!(got, by_filtering(&tree, dir), "children of {dir}");
            checked += 1;
        }
        assert_eq!(checked, dirs.len() + 1, "every directory, plus the root, was compared");
    }

    #[test]
    fn children_skip_scan_agrees_on_random_trees() {
        // Components that sort adjacent to one another (and to the '/' separator).
        const NAMES: [&str; 9] = ["a", "a b", "a-b", "a0", "a1", "b", "0", "x", "ç"];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % bound as u64).unwrap()
        };

        for _ in 0..200 {
            let fs = MemoryFs::new();
            for _ in 0..40 {
                let depth = 1 + next(4);
                let mut path = VPath::root();
                for level in 0..depth {
                    path = path.join(NAMES[next(NAMES.len())]).unwrap();
                    // Make intermediate levels directories; the last one a file or a directory.
                    let last = level + 1 == depth;
                    let _ =
                        if last && next(2) == 0 { fs.create_file(&path, true).map(drop) } else { fs.create_dir(&path) };
                }
            }
            let tree = sync::read(&fs.tree);
            for dir in tree.iter().filter(|(_, node)| node.is_dir()).map(|(k, _)| k) {
                let got: Vec<_> = children(&tree, dir).map(|(k, _)| k).collect();
                assert_eq!(got, by_filtering(&tree, dir), "children of {dir}");
            }
        }
    }
}
