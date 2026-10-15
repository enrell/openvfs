//! Composing several backends into a single namespace.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, FileType, Metadata, OpenOptions, VfsFile, msg};
use crate::ops;
use crate::path::VPath;
use crate::sync;

type Table = BTreeMap<VPath, Arc<dyn FileSystem>>;

/// A mount table: each backend serves the subtree below its mount point.
///
/// A path is served by the mount with the *longest* matching prefix, exactly
/// like Unix mounts. Ancestors of mount points always exist as (virtual)
/// directories, even when no backend is mounted at `/`, and mount points
/// shadow whatever the parent backend has at that path. Errors from a
/// mounted backend are reported in outer paths.
///
/// Renames across mounts fail with [`VfsError::CrossDevice`] (the [`crate::Vfs`]
/// layer falls back to copy + delete); copies across mounts are streamed.
///
/// ```
/// use openvfs::{MemoryFs, MountFs, ReadOnlyFs, Vfs};
///
/// let mounts = MountFs::new();
/// mounts.mount("/", MemoryFs::new())?;
/// mounts.mount("/assets", ReadOnlyFs::new(MemoryFs::new()))?;
///
/// let vfs = Vfs::new(mounts);
/// vfs.write("/notes.txt", "goes to the first MemoryFs")?;
/// assert!(vfs.write("/assets/x.png", "").is_err()); // read-only mount
/// # Ok::<(), openvfs::VfsError>(())
/// ```
#[derive(Default)]
pub struct MountFs {
    table: RwLock<Table>,
}

/// Where a path lands after consulting the mount table.
struct Route {
    fs: Arc<dyn FileSystem>,
    mount: VPath,
    inner: VPath,
}

impl Route {
    fn wrap<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|e| e.rebase(&self.mount))
    }
}

impl MountFs {
    /// An empty mount table. `/` exists as an empty virtual directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mounts `fs` at `at`.
    ///
    /// # Errors
    ///
    /// [`VfsError::AlreadyExists`] if something is already mounted exactly there.
    pub fn mount(&self, at: impl AsRef<str>, fs: impl FileSystem + 'static) -> Result<()> {
        self.mount_arc(at, Arc::new(fs))
    }

    /// Mounts an already shared backend.
    pub fn mount_arc(&self, at: impl AsRef<str>, fs: Arc<dyn FileSystem>) -> Result<()> {
        let at = VPath::new(at)?;
        let mut table = sync::write(&self.table);
        if table.contains_key(&at) {
            return Err(VfsError::AlreadyExists(at));
        }
        table.insert(at, fs);
        Ok(())
    }

    /// Removes the mount at `at` and hands the backend back.
    pub fn unmount(&self, at: impl AsRef<str>) -> Result<Arc<dyn FileSystem>> {
        let at = VPath::new(at)?;
        sync::write(&self.table).remove(&at).ok_or(VfsError::NotFound(at))
    }

    /// All mount points, sorted.
    #[must_use]
    pub fn mount_points(&self) -> Vec<VPath> {
        sync::read(&self.table).keys().cloned().collect()
    }

    fn route(&self, path: &VPath) -> Option<Route> {
        let table = sync::read(&self.table);
        path.ancestors().find_map(|mount| {
            table.get(&mount).map(|fs| Route {
                fs: Arc::clone(fs),
                inner: path.strip_prefix(&mount).expect("mount is an ancestor of path"),
                mount,
            })
        })
    }

    fn route_or_not_found(&self, path: &VPath) -> Result<Route> {
        self.route(path).ok_or_else(|| VfsError::NotFound(path.clone()))
    }

    /// `true` if `path` must exist as a directory because a mount lives at or
    /// strictly below it (the root always qualifies).
    fn is_structural(&self, path: &VPath) -> bool {
        path.is_root() || {
            let table = sync::read(&self.table);
            table.contains_key(path) || mounts_below(&table, path).next().is_some()
        }
    }

    /// `true` if a mount lives strictly below `path`.
    fn has_mounts_below(&self, path: &VPath) -> bool {
        path.is_root() || mounts_below(&sync::read(&self.table), path).next().is_some()
    }

    /// Names of the first components of every mount point strictly below `path`.
    fn virtual_children(&self, path: &VPath) -> BTreeSet<String> {
        mounts_below(&sync::read(&self.table), path)
            .filter_map(|m| m.strip_prefix(path))
            .filter_map(|rest| rest.components().next().map(str::to_owned))
            .collect()
    }

    fn reject_structural(&self, path: &VPath) -> Result<()> {
        if self.is_structural(path) {
            Err(VfsError::InvalidInput("cannot remove or move a mount point or its ancestors"))
        } else {
            Ok(())
        }
    }
}

/// Every mount point strictly below `path`.
///
/// The table is ordered by raw bytes, so those mount points are one contiguous
/// range starting at `path/`: the lookup costs `O(log m + results)` instead of
/// scanning all `m` mount points.
fn mounts_below<'t>(table: &'t Table, path: &VPath) -> impl Iterator<Item = &'t VPath> + use<'t> {
    let prefix = path.descendant_prefix();
    table
        .range(VPath::bound(prefix.clone())..)
        .map(|(mount, _)| mount)
        .take_while(move |mount| mount.as_str().starts_with(&prefix))
        .filter(|mount| !mount.is_root()) // for `/`, the range starts at the root mount itself
}

impl fmt::Debug for MountFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountFs").field("mounts", &self.mount_points()).finish()
    }
}

impl FileSystem for MountFs {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        let route = self.route(path);
        if self.has_mounts_below(path) {
            // Prefer the real directory's metadata when a backend has one.
            if let Some(r) = &route
                && let Ok(meta) = r.fs.metadata(&r.inner)
                && meta.is_dir()
            {
                return Ok(meta);
            }
            return Ok(Metadata::dir());
        }
        let r = route.ok_or_else(|| VfsError::NotFound(path.clone()))?;
        r.wrap(r.fs.metadata(&r.inner))
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        let is_virtual = self.has_mounts_below(path);
        let mut merged: BTreeMap<String, DirEntry> = BTreeMap::new();
        match self.route(path) {
            Some(r) => match r.fs.read_dir(&r.inner) {
                Ok(entries) => {
                    for e in entries {
                        let path = path.child(&e.name);
                        merged.insert(e.name.clone(), DirEntry { path, ..e });
                    }
                }
                // The backend has nothing (or a file) here; the mounts below still show up.
                Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) if is_virtual => {}
                Err(e) => return Err(e.rebase(&r.mount)),
            },
            None if !is_virtual => return Err(VfsError::NotFound(path.clone())),
            None => {}
        }
        for name in self.virtual_children(path) {
            let entry = DirEntry { path: path.child(&name), name: name.clone(), file_type: FileType::Dir };
            merged.insert(name, entry);
        }
        Ok(merged.into_values().collect())
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        if self.is_structural(path) {
            return Err(VfsError::AlreadyExists(path.clone()));
        }
        let r = self.route_or_not_found(path)?;
        r.wrap(r.fs.create_dir(&r.inner))
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        if self.is_structural(path) {
            return Err(VfsError::IsADirectory(path.clone()));
        }
        let r = self.route_or_not_found(path)?;
        r.wrap(r.fs.remove_file(&r.inner))
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        self.reject_structural(path)?;
        let r = self.route_or_not_found(path)?;
        r.wrap(r.fs.remove_dir(&r.inner))
    }

    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        self.reject_structural(path)?;
        let r = self.route_or_not_found(path)?;
        r.wrap(r.fs.remove_dir_all(&r.inner))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(VfsError::InvalidInput(msg::RENAME_ROOT));
        }
        self.reject_structural(from)?;
        if self.is_structural(to) {
            return Err(VfsError::AlreadyExists(to.clone()));
        }
        let src = self.route_or_not_found(from)?;
        let dst = self.route_or_not_found(to)?;
        if src.mount != dst.mount {
            return Err(VfsError::CrossDevice { from: from.clone(), to: to.clone() });
        }
        src.wrap(src.fs.rename(&src.inner, &dst.inner))
    }

    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        // Same check order as every other backend: source, identity, destination.
        if self.is_structural(from) {
            return Err(VfsError::IsADirectory(from.clone()));
        }
        let src = self.route_or_not_found(from)?;
        if src.wrap(src.fs.metadata(&src.inner))?.is_dir() {
            return Err(VfsError::IsADirectory(from.clone()));
        }
        if from == to {
            return Err(VfsError::InvalidInput(msg::SAME_FILE));
        }
        if self.is_structural(to) {
            return Err(VfsError::IsADirectory(to.clone()));
        }
        let dst = self.route_or_not_found(to)?;
        if src.mount == dst.mount {
            return src.wrap(src.fs.copy_file(&src.inner, &dst.inner));
        }
        ops::stream_copy(self, from, self, to)
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        if self.is_structural(path) {
            return Err(VfsError::IsADirectory(path.clone()));
        }
        let r = self.route_or_not_found(path)?;
        r.wrap(r.fs.open(&r.inner, options))
    }
}
