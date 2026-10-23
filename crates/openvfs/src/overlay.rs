//! Copy-on-write layering: a writable upper layer over read-only lowers.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::{Arc, RwLock};

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, FileType, Metadata, OpenOptions, VfsFile, classify_missing, msg};
use crate::ops;
use crate::path::VPath;
use crate::sync;

/// Stacks a writable *upper* layer on top of one or more *lower* layers.
///
/// * **Reads** fall through: the upper layer wins, then lowers in priority
///   order (`lowers[0]` first). Directory listings are merged.
/// * **Writes** never touch the lowers. Modifying a lower file first copies
///   it up ("copy-on-write").
/// * **Deletes** of lower content are recorded as *whiteouts*: the path is
///   hidden from the lower layers. Re-creating a deleted directory makes it
///   *opaque* — the lower directory's old children stay hidden.
///
/// Because lowers are never written, [`OverlayFs::changes`] can always tell
/// you exactly what differs from them, and [`OverlayFs::commit`] can apply
/// that change set somewhere — typically back onto the lower layer.
///
/// ```
/// use openvfs::{Change, MemoryFs, OverlayFs, Vfs};
/// use std::sync::Arc;
///
/// let base = Vfs::memory();
/// base.write("/config.toml", "debug = false")?;
///
/// let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), base.backend().clone()));
/// let vfs = Vfs::from(overlay.clone());
/// vfs.write("/config.toml", "debug = true")?;
///
/// assert_eq!(base.read_to_string("/config.toml")?, "debug = false"); // untouched
/// assert_eq!(overlay.changes()?, [Change::Modified { path: "/config.toml".parse()? }]);
/// # Ok::<(), openvfs::VfsError>(())
/// ```
///
/// # Consistency
///
/// Structural operations (create, remove, rename, copy, opening for write)
/// are **linearizable**: they are serialized by one lock, while reads,
/// listings and [`OverlayFs::changes`] share it. A listing therefore never
/// observes half of a rename, even one that had to copy a lower directory.
/// Bytes written through an already open handle go straight to the upper
/// layer and follow that backend's own guarantees.
///
/// Concurrent modification of the *lower* layers through other handles is not
/// coordinated with the overlay and gives unspecified (but memory-safe) results.
pub struct OverlayFs {
    upper: Arc<dyn FileSystem>,
    lowers: Vec<Arc<dyn FileSystem>>,
    /// The whiteouts, doubling as the lock that serializes structural changes.
    state: RwLock<Whiteouts>,
    /// `path -> metadata` for content that came from a lower layer (`None` =
    /// known missing). Lowers do not change while an overlay is live, so these
    /// entries are only dropped when a write touches the overlay. This is what
    /// keeps a view over a hundred mods usable: without it every lookup walks
    /// every layer again.
    meta_cache: RwLock<HashMap<VPath, Option<Metadata>>>,
    /// Merged listings, same invalidation policy.
    dir_cache: RwLock<HashMap<VPath, Arc<Vec<DirEntry>>>>,
}

/// One difference between an overlay and its lower layers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Change {
    /// The path is new.
    Added {
        /// The new path.
        path: VPath,
        /// What was created.
        kind: FileType,
    },
    /// A file exists in both, with different contents.
    Modified {
        /// The modified file.
        path: VPath,
    },
    /// The path (and, for directories, everything below it) is gone.
    Removed {
        /// The removed path.
        path: VPath,
        /// What was removed.
        kind: FileType,
    },
}

impl Change {
    /// The affected path.
    #[must_use]
    pub fn path(&self) -> &VPath {
        match self {
            Self::Added { path, .. } | Self::Modified { path } | Self::Removed { path, .. } => path,
        }
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slash = |kind: &FileType| if *kind == FileType::Dir { "/" } else { "" };
        match self {
            Self::Added { path, kind } => write!(f, "A {path}{}", slash(kind)),
            Self::Modified { path } => write!(f, "M {path}"),
            Self::Removed { path, kind } => write!(f, "D {path}{}", slash(kind)),
        }
    }
}

/// Paths hidden from the lower layers (with everything below them).
#[derive(Default)]
struct Whiteouts(BTreeSet<VPath>);

impl Whiteouts {
    /// `true` if `path` or one of its ancestors is whited out.
    fn hides(&self, path: &VPath) -> bool {
        !self.0.is_empty() && path.ancestors().any(|a| self.0.contains(&a))
    }

    /// `true` if `dir` itself or anything below it is whited out.
    fn touches(&self, dir: &VPath) -> bool {
        if self.0.contains(dir) {
            return true;
        }
        let prefix = dir.descendant_prefix();
        self.0.range(VPath::bound(prefix.clone())..).next().is_some_and(|h| h.as_str().starts_with(&prefix))
    }

    /// Hides `path`; whiteouts below it become redundant and are dropped.
    fn hide(&mut self, path: &VPath) {
        let prefix = path.descendant_prefix();
        let redundant: Vec<VPath> = self
            .0
            .range(VPath::bound(prefix.clone())..)
            .take_while(|h| h.as_str().starts_with(&prefix))
            .cloned()
            .collect();
        for h in redundant {
            self.0.remove(&h);
        }
        self.0.insert(path.clone());
    }
}

enum Location {
    Upper(Metadata),
    Lower(usize, Metadata),
}

impl Location {
    fn meta(&self) -> &Metadata {
        match self {
            Self::Upper(m) | Self::Lower(_, m) => m,
        }
    }

    fn is_dir(&self) -> bool {
        self.meta().is_dir()
    }
}

/// `metadata`, with "not there" mapped to `None` instead of an error.
fn probe(fs: &dyn FileSystem, path: &VPath) -> Result<Option<Metadata>> {
    match fs.metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Compares two files chunk by chunk, without loading either into memory.
fn same_contents(a: &mut dyn Read, b: &mut dyn Read) -> io::Result<bool> {
    fn fill(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match r.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
    let (mut buf_a, mut buf_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (n_a, n_b) = (fill(a, &mut buf_a)?, fill(b, &mut buf_b)?);
        if n_a != n_b || buf_a[..n_a] != buf_b[..n_b] {
            return Ok(false);
        }
        if n_a == 0 {
            return Ok(true);
        }
    }
}

impl OverlayFs {
    /// One upper layer over one lower layer.
    pub fn new(upper: impl FileSystem + 'static, lower: impl FileSystem + 'static) -> Self {
        Self::with_layers(Arc::new(upper), vec![Arc::new(lower)])
    }

    /// One upper layer over several lowers; `lowers[0]` has the highest priority.
    ///
    /// Lowers are expected to agree on which paths are directories.
    #[must_use]
    pub fn with_layers(upper: Arc<dyn FileSystem>, lowers: Vec<Arc<dyn FileSystem>>) -> Self {
        Self {
            upper,
            lowers,
            state: RwLock::new(Whiteouts::default()),
            meta_cache: RwLock::new(HashMap::new()),
            dir_cache: RwLock::new(HashMap::new()),
        }
    }

    /// The writable layer.
    #[must_use]
    pub fn upper(&self) -> &Arc<dyn FileSystem> {
        &self.upper
    }

    /// The read-only layers, highest priority first.
    #[must_use]
    pub fn lowers(&self) -> &[Arc<dyn FileSystem>] {
        &self.lowers
    }

    /// The current whiteouts, sorted (useful for debugging and persistence).
    #[must_use]
    pub fn whiteouts(&self) -> Vec<VPath> {
        sync::read(&self.state).0.iter().cloned().collect()
    }

    /// Drops every cached lookup. Called by anything that changes the view.
    fn invalidate(&self) {
        sync::write(&self.meta_cache).clear();
        sync::write(&self.dir_cache).clear();
    }

    /// Hides `path` without touching any layer: records a whiteout so lower
    /// content at `path` becomes invisible. Unlike [`OverlayFs::remove_file`]
    /// and friends, nothing is deleted — this exists to *replay* whiteouts a
    /// caller persisted from [`OverlayFs::whiteouts`]. If `path` exists in the
    /// upper it stays visible; prefer `remove_*` for an actual delete.
    pub fn hide(&self, path: &VPath) {
        sync::write(&self.state).hide(path);
    }

    /// Cheap check for pending writes. May be `true` while [`OverlayFs::changes`]
    /// is empty — e.g. after a file was copied up but rewritten unchanged.
    pub fn is_dirty(&self) -> Result<bool> {
        let w = sync::read(&self.state);
        Ok(!w.0.is_empty() || !self.upper.read_dir(&VPath::root())?.is_empty())
    }

    /// Computes every difference between the merged view and the lowers.
    ///
    /// The list is a consistent snapshot, sorted by path, with parents before
    /// children and a `Removed` before an `Added` of the same path — so it can
    /// be replayed top-down (see [`OverlayFs::commit`]). Untouched subtrees are
    /// skipped without being read, and files that were rewritten with identical
    /// bytes are not reported.
    pub fn changes(&self) -> Result<Vec<Change>> {
        let w = sync::read(&self.state);
        let mut out = Vec::new();
        self.diff_dir(&w, &VPath::root(), &mut out)?;
        Ok(out)
    }

    /// Replays [`OverlayFs::changes`] onto `target`, streaming file contents,
    /// and returns the applied changes. Usually followed by [`OverlayFs::reset`].
    ///
    /// For an exact result, `target` should hold what the lowers hold — with a
    /// single lower, pass the lower itself to "save" the overlay.
    ///
    /// The replay is **idempotent but not atomic**: if it fails midway,
    /// `target` holds a prefix of the change set and the overlay is unchanged,
    /// so calling `commit` again finishes the job.
    pub fn commit(&self, target: &dyn FileSystem) -> Result<Vec<Change>> {
        let w = sync::read(&self.state);
        let mut changes = Vec::new();
        self.diff_dir(&w, &VPath::root(), &mut changes)?;
        for change in &changes {
            match change {
                Change::Removed { path, .. } => match target.metadata(path) {
                    Ok(meta) if meta.is_dir() => target.remove_dir_all(path)?,
                    Ok(_) => target.remove_file(path)?,
                    Err(VfsError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                },
                Change::Added { path, kind: FileType::Dir } => match target.create_dir(path) {
                    Ok(()) => {}
                    Err(VfsError::AlreadyExists(_)) if target.metadata(path)?.is_dir() => {}
                    Err(e) => return Err(e),
                },
                Change::Added { path, kind: FileType::File } | Change::Modified { path } => {
                    let mut reader = self.open_in(&w, path, OpenOptions::reading())?;
                    let mut writer = target.open(path, &OpenOptions::writing())?;
                    io::copy(&mut reader, &mut writer)
                        .and_then(|_| writer.flush())
                        .map_err(|e| VfsError::from_io(path, e))?;
                }
            }
        }
        Ok(changes)
    }

    /// Discards every pending change: empties the upper layer and forgets whiteouts.
    pub fn reset(&self) -> Result<()> {
        let mut w = sync::write(&self.state);
        for entry in self.upper.read_dir(&VPath::root())? {
            if entry.is_dir() {
                self.upper.remove_dir_all(&entry.path)?;
            } else {
                self.upper.remove_file(&entry.path)?;
            }
        }
        w.0.clear();
        Ok(())
    }

    // --- lookup (all under the state lock) ---------------------------------

    /// The lowers as they were, ignoring whiteouts.
    fn base_lookup(&self, path: &VPath) -> Result<Option<(usize, Metadata)>> {
        for (layer, lower) in self.lowers.iter().enumerate() {
            if let Some(meta) = probe(&**lower, path)? {
                return Ok(Some((layer, meta)));
            }
        }
        Ok(None)
    }

    /// The lowers as currently visible through the overlay.
    fn lower_lookup(&self, w: &Whiteouts, path: &VPath) -> Result<Option<(usize, Metadata)>> {
        if w.hides(path) { Ok(None) } else { self.base_lookup(path) }
    }

    fn locate(&self, w: &Whiteouts, path: &VPath) -> Result<Option<Location>> {
        if let Some(meta) = probe(&*self.upper, path)? {
            return Ok(Some(Location::Upper(meta)));
        }
        Ok(self.lower_lookup(w, path)?.map(|(layer, meta)| Location::Lower(layer, meta)))
    }

    fn in_upper(&self, path: &VPath) -> Result<bool> {
        Ok(probe(&*self.upper, path)?.is_some())
    }

    fn missing(&self, w: &Whiteouts, path: &VPath) -> VfsError {
        classify_missing(path, |p| self.locate(w, p).ok().flatten().map(|loc| loc.meta().file_type))
    }

    /// The parent of `path` must be a directory in the merged view; errors name `path`.
    fn require_parent_dir(&self, w: &Whiteouts, path: &VPath) -> Result<VPath> {
        let parent = path.parent().unwrap_or_else(VPath::root);
        match self.locate(w, &parent)? {
            Some(loc) if loc.is_dir() => Ok(parent),
            Some(_) => Err(VfsError::NotADirectory(path.clone())),
            None => Err(self.missing(w, path)),
        }
    }

    /// Union of the lower listings for `dir`, ignoring whiteouts.
    fn base_read_dir(&self, dir: &VPath) -> Result<BTreeMap<String, FileType>> {
        let mut merged = BTreeMap::new();
        for lower in self.lowers.iter().rev() {
            if probe(&**lower, dir)?.is_some_and(|m| m.is_dir()) {
                for entry in lower.read_dir(dir)? {
                    merged.insert(entry.name, entry.file_type);
                }
            }
        }
        Ok(merged)
    }

    fn metadata_in(&self, w: &Whiteouts, path: &VPath) -> Result<Metadata> {
        match self.locate(w, path)? {
            Some(Location::Upper(meta)) => Ok(meta),
            // Lower files are writable *through the overlay* (copy-up).
            Some(Location::Lower(_, meta)) => Ok(meta.with_read_only(self.upper.is_read_only())),
            None => Err(self.missing(w, path)),
        }
    }

    fn read_dir_in(&self, w: &Whiteouts, path: &VPath) -> Result<Vec<DirEntry>> {
        let in_upper = match self.locate(w, path)? {
            Some(loc) if loc.is_dir() => matches!(loc, Location::Upper(_)),
            Some(_) => return Err(VfsError::NotADirectory(path.clone())),
            None => return Err(self.missing(w, path)),
        };
        let mut merged = BTreeMap::new();
        if !w.hides(path) {
            let lower = self.base_read_dir(path)?;
            merged.extend(lower.into_iter().filter(|(name, _)| !w.0.contains(&path.child(name))));
        }
        if in_upper {
            merged.extend(self.upper.read_dir(path)?.into_iter().map(|e| (e.name, e.file_type)));
        }
        Ok(merged.into_iter().map(|(name, file_type)| DirEntry { path: path.child(&name), name, file_type }).collect())
    }

    // --- copy-on-write ------------------------------------------------------

    /// Mirrors `dir` and its ancestors into the upper layer.
    fn ensure_upper_dir(&self, dir: &VPath) -> Result<()> {
        let mut missing = Vec::new();
        for ancestor in dir.ancestors() {
            if self.in_upper(&ancestor)? {
                break;
            }
            missing.push(ancestor);
        }
        for d in missing.into_iter().rev() {
            match self.upper.create_dir(&d) {
                Ok(()) | Err(VfsError::AlreadyExists(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Copies `from` (in `source`) to `to` in the upper layer *atomically*:
    /// the bytes go to a fresh temporary sibling that is renamed into place,
    /// so a failure midway never leaves a partial file shadowing `to`.
    fn copy_into_upper(&self, source: &dyn FileSystem, from: &VPath, to: &VPath) -> Result<()> {
        let parent = to.parent().unwrap_or_else(VPath::root);
        self.ensure_upper_dir(&parent)?;
        let name = to.file_name().unwrap_or("root");
        let mut tmp = None;
        for n in 0u32.. {
            let candidate = parent.child(&format!(".{name}.{}-{n}.openvfs-copyup", std::process::id()));
            if !self.in_upper(&candidate)? {
                tmp = Some(candidate);
                break;
            }
        }
        let tmp = tmp.expect("an unused temporary name exists");
        let result = ops::stream_copy(source, from, &*self.upper, &tmp).and_then(|_| self.upper.rename(&tmp, to));
        if result.is_err() {
            let _ = self.upper.remove_file(&tmp); // best effort
        }
        result.map_err(|e| e.with_path(to))
    }

    // --- mutations (callers hold the state lock for writing) ----------------

    fn create_dir_in(&self, w: &Whiteouts, path: &VPath) -> Result<()> {
        if path.is_root() || self.locate(w, path)?.is_some() {
            return Err(VfsError::AlreadyExists(path.clone()));
        }
        let parent = self.require_parent_dir(w, path)?;
        self.ensure_upper_dir(&parent)?;
        // If `path` was whited out it stays hidden: the new directory is opaque.
        self.upper.create_dir(path)
    }

    fn remove_file_in(&self, w: &mut Whiteouts, path: &VPath) -> Result<()> {
        match self.locate(w, path)? {
            None => return Err(self.missing(w, path)),
            Some(loc) if loc.is_dir() => return Err(VfsError::IsADirectory(path.clone())),
            Some(_) => {}
        }
        if self.in_upper(path)? {
            self.upper.remove_file(path)?;
        }
        if self.lower_lookup(w, path)?.is_some() {
            w.hide(path);
        }
        Ok(())
    }

    fn remove_dir_in(&self, w: &mut Whiteouts, path: &VPath, recursive: bool) -> Result<()> {
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        match self.locate(w, path)? {
            None => return Err(self.missing(w, path)),
            Some(loc) if !loc.is_dir() => return Err(VfsError::NotADirectory(path.clone())),
            Some(_) => {}
        }
        if !recursive && !self.read_dir_in(w, path)?.is_empty() {
            return Err(VfsError::DirectoryNotEmpty(path.clone()));
        }
        if self.in_upper(path)? {
            self.upper.remove_dir_all(path)?;
        }
        // One whiteout hides the whole lower subtree: O(upper subtree), not O(tree).
        if self.lower_lookup(w, path)?.is_some() {
            w.hide(path);
        }
        Ok(())
    }

    fn rename_in(&self, w: &mut Whiteouts, from: &VPath, to: &VPath) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(VfsError::InvalidInput(msg::RENAME_ROOT));
        }
        let Some(src) = self.locate(w, from)? else {
            return Err(self.missing(w, from));
        };
        if from == to {
            return Ok(());
        }
        if src.is_dir() && to.starts_with(from) {
            return Err(VfsError::InvalidInput(msg::INTO_ITSELF));
        }
        let to_parent = self.require_parent_dir(w, to)?;
        if let Some(dst) = self.locate(w, to)? {
            match (src.is_dir(), dst.is_dir()) {
                (_, true) => return Err(VfsError::AlreadyExists(to.clone())),
                (true, false) => return Err(VfsError::NotADirectory(to.clone())),
                // An existing file at `to` is replaced atomically below (an upper
                // file wins over any lower one), so nothing is deleted up front.
                (false, false) => {}
            }
        }
        let from_in_lower = self.lower_lookup(w, from)?.is_some();

        if !src.is_dir() {
            match src {
                Location::Upper(_) => {
                    self.ensure_upper_dir(&to_parent)?;
                    self.upper.rename(from, to)?;
                }
                Location::Lower(layer, _) => self.copy_into_upper(&*self.lowers[layer], from, to)?,
            }
            if from_in_lower {
                w.hide(from);
            }
            return Ok(());
        }

        if !from_in_lower {
            // Fast path: the directory lives only in the upper layer.
            self.ensure_upper_dir(&to_parent)?;
            return self.upper.rename(from, to);
        }
        // Lower content can't be moved: copy it up under the new name, then hide
        // the old one. On failure, roll the partial copy back (`to` was free).
        if let Err(e) = self.copy_tree_in(w, from, to) {
            if self.locate(w, to).is_ok_and(|loc| loc.is_some()) {
                let _ = self.remove_dir_in(w, to, true);
            }
            return Err(e);
        }
        self.remove_dir_in(w, from, true)
    }

    fn copy_file_in(&self, w: &Whiteouts, from: &VPath, to: &VPath) -> Result<u64> {
        let src = match self.locate(w, from)? {
            None => return Err(self.missing(w, from)),
            Some(loc) if loc.is_dir() => return Err(VfsError::IsADirectory(from.clone())),
            Some(loc) => loc,
        };
        if from == to {
            return Err(VfsError::InvalidInput(msg::SAME_FILE));
        }
        let parent = self.require_parent_dir(w, to)?;
        if self.locate(w, to)?.is_some_and(|loc| loc.is_dir()) {
            return Err(VfsError::IsADirectory(to.clone()));
        }
        self.ensure_upper_dir(&parent)?;
        match src {
            Location::Upper(_) => self.upper.copy_file(from, to),
            Location::Lower(layer, _) => ops::stream_copy(&*self.lowers[layer], from, &*self.upper, to),
        }
    }

    /// Copies a merged subtree into the upper layer. `to` must not exist.
    fn copy_tree_in(&self, w: &Whiteouts, from: &VPath, to: &VPath) -> Result<()> {
        if !self.locate(w, from)?.is_some_and(|loc| loc.is_dir()) {
            return self.copy_file_in(w, from, to).map(drop);
        }
        self.create_dir_in(w, to)?;
        for entry in self.read_dir_in(w, from)? {
            self.copy_tree_in(w, &entry.path, &to.child(&entry.name))?;
        }
        Ok(())
    }

    fn open_in(&self, w: &Whiteouts, path: &VPath, options: OpenOptions) -> Result<Box<dyn VfsFile>> {
        let location = self.locate(w, path)?;
        if location.as_ref().is_some_and(Location::is_dir) {
            return Err(VfsError::IsADirectory(path.clone()));
        }
        if !options.is_write() {
            return match location {
                Some(Location::Upper(_)) => self.upper.open(path, &options),
                Some(Location::Lower(layer, _)) => self.lowers[layer].open(path, &options),
                None => Err(self.missing(w, path)),
            };
        }
        match location {
            Some(Location::Upper(_)) => self.upper.open(path, &options),
            Some(Location::Lower(..)) if options.create_new => Err(VfsError::AlreadyExists(path.clone())),
            Some(Location::Lower(layer, _)) => {
                if options.truncate {
                    self.ensure_upper_dir(&path.parent().unwrap_or_else(VPath::root))?; // old bytes not needed
                } else {
                    self.copy_into_upper(&*self.lowers[layer], path, path)?; // copy-up
                }
                self.upper.open(path, &OpenOptions { create: true, create_new: false, ..options })
            }
            None if options.create || options.create_new => {
                let parent = self.require_parent_dir(w, path)?;
                self.ensure_upper_dir(&parent)?;
                self.upper.open(path, &options)
            }
            None => Err(self.missing(w, path)),
        }
    }

    // --- diffing --------------------------------------------------------------

    fn diff_dir(&self, w: &Whiteouts, dir: &VPath, out: &mut Vec<Change>) -> Result<()> {
        let now: BTreeMap<String, FileType> =
            self.read_dir_in(w, dir)?.into_iter().map(|e| (e.name, e.file_type)).collect();
        let before = self.base_read_dir(dir)?;
        let names: BTreeSet<&String> = now.keys().chain(before.keys()).collect();

        for name in names {
            let path = dir.child(name);
            match (now.get(name), before.get(name)) {
                (Some(&kind), None) => self.push_added(w, &path, kind, out)?,
                (None, Some(&kind)) => out.push(Change::Removed { path, kind }),
                (Some(FileType::File), Some(FileType::File)) => {
                    if self.in_upper(&path)? && self.differs_from_base(&path)? {
                        out.push(Change::Modified { path });
                    }
                }
                (Some(FileType::Dir), Some(FileType::Dir)) => {
                    // Untouched subtrees are skipped without being read.
                    if self.in_upper(&path)? || w.touches(&path) {
                        self.diff_dir(w, &path, out)?;
                    }
                }
                (Some(&now_kind), Some(&before_kind)) => {
                    out.push(Change::Removed { path: path.clone(), kind: before_kind });
                    self.push_added(w, &path, now_kind, out)?;
                }
                (None, None) => unreachable!("name came from one of the two listings"),
            }
        }
        Ok(())
    }

    fn push_added(&self, w: &Whiteouts, path: &VPath, kind: FileType, out: &mut Vec<Change>) -> Result<()> {
        out.push(Change::Added { path: path.clone(), kind });
        if kind == FileType::Dir {
            for entry in self.read_dir_in(w, path)? {
                self.push_added(w, &entry.path, entry.file_type, out)?;
            }
        }
        Ok(())
    }

    fn differs_from_base(&self, path: &VPath) -> Result<bool> {
        let Some((layer, base_meta)) = self.base_lookup(path)? else { return Ok(true) };
        if self.upper.metadata(path)?.len != base_meta.len {
            return Ok(true);
        }
        let mut upper = self.upper.open(path, &OpenOptions::reading())?;
        let mut lower = self.lowers[layer].open(path, &OpenOptions::reading())?;
        same_contents(&mut upper, &mut lower).map(|same| !same).map_err(|e| VfsError::from_io(path, e))
    }
}

impl fmt::Debug for OverlayFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OverlayFs")
            .field("lowers", &self.lowers.len())
            .field("whiteouts", &sync::read(&self.state).0.len())
            .finish_non_exhaustive()
    }
}

impl FileSystem for OverlayFs {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        if let Some(hit) = sync::read(&self.meta_cache).get(path).cloned() {
            return match hit {
                Some(meta) => Ok(meta),
                None => Err(VfsError::NotFound(path.clone())),
            };
        }
        let state = sync::read(&self.state);
        let located = self.locate(&state, path);
        drop(state);
        let result = match located {
            // Upper content is writable: never cache it.
            Ok(Some(Location::Upper(meta))) => return Ok(meta),
            Ok(Some(Location::Lower(_, meta))) => Ok(meta.with_read_only(self.upper.is_read_only())),
            Ok(None) => Err(self.missing(&Whiteouts::default(), path)),
            Err(e) => Err(e),
        };
        let entry = match &result {
            Ok(meta) => Some(meta.clone()),
            Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) => None,
            Err(_) => return result,
        };
        sync::write(&self.meta_cache).insert(path.clone(), entry);
        result
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        if let Some(hit) = sync::read(&self.dir_cache).get(path).cloned() {
            return Ok((*hit).clone());
        }
        let entries = self.read_dir_in(&sync::read(&self.state), path)?;
        sync::write(&self.dir_cache).insert(path.clone(), Arc::new(entries.clone()));
        Ok(entries)
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        let r = self.create_dir_in(&sync::write(&self.state), path);
        self.invalidate();
        r
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        let r = self.remove_file_in(&mut sync::write(&self.state), path);
        self.invalidate();
        r
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        let r = self.remove_dir_in(&mut sync::write(&self.state), path, false);
        self.invalidate();
        r
    }

    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        let r = self.remove_dir_in(&mut sync::write(&self.state), path, true);
        self.invalidate();
        r
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let r = self.rename_in(&mut sync::write(&self.state), from, to);
        self.invalidate();
        r
    }

    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        let r = self.copy_file_in(&sync::write(&self.state), from, to);
        self.invalidate();
        r
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        options.validate()?;
        if options.is_write() {
            let r = self.open_in(&sync::write(&self.state), path, *options);
            self.invalidate();
            r
        } else {
            self.open_in(&sync::read(&self.state), path, *options)
        }
    }

    fn is_read_only(&self) -> bool {
        self.upper.is_read_only()
    }
}
