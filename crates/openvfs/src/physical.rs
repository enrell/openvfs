//! A file system backed by a directory on the host.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::{Result, VfsError};
use crate::fs::{DirEntry, FileSystem, FileType, Metadata, Missing, OpenOptions, VfsFile, classify_missing, msg};
use crate::path::VPath;

/// How [`PhysicalFs`] treats symbolic links (and, on Windows, junctions).
///
/// The virtual model has no links, so the policy decides how host links
/// project into it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Symlinks {
    /// Links are invisible in listings, and any path that names or goes
    /// through one fails with [`VfsError::PermissionDenied`]. Nothing can be
    /// read, written or deleted *through* a link, so link loops and escapes are
    /// impossible by construction. The safe default.
    #[default]
    Deny,
    /// Links are followed while they resolve inside the root. Links that point
    /// outside, and dangling links, fail with [`VfsError::PathEscapesRoot`] and
    /// are hidden from listings. Recursive deletes still remove the *link*,
    /// never the target. Beware: a link to an ancestor makes the tree infinite
    /// (use [`crate::Walk::max_depth`]).
    FollowWithinRoot,
}

/// Exposes one host directory as the root `/` of a virtual file system.
///
/// Virtual paths can never leave the root:
///
/// * `..` is already clamped by [`VPath`];
/// * components the host would interpret specially are rejected with
///   [`VfsError::InvalidPath`] — drive prefixes, and on Windows also device
///   names (`CON`, `NUL`, `COM1`…), reserved characters (`<>:"|?*`), and
///   names ending in `.` or space (which Windows silently strips);
/// * symlinks are handled by the configured [`Symlinks`] policy.
///
/// Devices, sockets and FIFOs under the root are invisible.
///
/// This confinement protects against hostile *paths*. It is not a defense
/// against a concurrent, hostile process swapping directories for symlinks
/// between the check and the use (TOCTOU); use OS-level sandboxing for that.
///
/// ```no_run
/// use openvfs::{PhysicalFs, Symlinks, Vfs};
///
/// let assets = PhysicalFs::builder("./assets").read_only(true).symlinks(Symlinks::Deny).build()?;
/// let config = Vfs::new(assets).read_to_string("/config.toml")?;
/// # Ok::<(), openvfs::VfsError>(())
/// ```
#[derive(Debug, Clone)]
pub struct PhysicalFs {
    root: PathBuf,
    read_only: bool,
    symlinks: Symlinks,
    /// `path -> resolved host path + metadata`, or which way it is missing.
    /// Only populated for read-only instances: their host tree does not change
    /// while a mount is live, and the layers of a game's virtual view are
    /// exactly that. A writable instance keeps every lookup uncached.
    meta_cache: Cache<Result<(PathBuf, Metadata), Missing>>,
    /// `dir -> entries`. Same policy as `meta_cache`.
    dir_cache: Cache<Arc<Vec<DirEntry>>>,
    /// `dir -> lowercase names of everything in it` (links included), `None` when it could not be
    /// read. Same policy as `meta_cache`. It only rules names out: a name that is in the set still
    /// goes to the host, which decides what it is.
    names_cache: Cache<Option<Arc<HashSet<String>>>>,
}

/// What walking the components of a path on the host turned up.
enum Walk {
    /// The path exists; this is its own metadata (not a link).
    Found(fs::Metadata),
    /// A component does not exist. `under_file`: the nearest one that does is a file.
    Missing { under_file: bool },
    /// Not walked (links are followed), or the host failed in some other way.
    Unknown,
}

impl Walk {
    fn found(self) -> Option<fs::Metadata> {
        match self {
            Self::Found(meta) => Some(meta),
            Self::Missing { .. } | Self::Unknown => None,
        }
    }
}

/// What [`PhysicalFs::stat_below_parent`] found.
enum Below {
    /// The parent is missing or a file, so `path` is too; no host call was made.
    Derived(VfsError),
    /// The host was asked about `path` itself.
    Probed(Result<(PathBuf, Metadata)>),
}

/// A per-path cache kept by read-only instances; `None` for writable ones.
type Cache<V> = Option<Arc<RwLock<HashMap<VPath, V>>>>;

/// Configures a [`PhysicalFs`]; created by [`PhysicalFs::builder`].
#[derive(Debug, Clone)]
#[must_use]
pub struct PhysicalFsBuilder {
    root: PathBuf,
    read_only: bool,
    symlinks: Symlinks,
}

impl PhysicalFsBuilder {
    /// Rejects every write with [`VfsError::ReadOnly`].
    pub fn read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }

    /// Sets the [`Symlinks`] policy (default: [`Symlinks::Deny`]).
    pub fn symlinks(mut self, policy: Symlinks) -> Self {
        self.symlinks = policy;
        self
    }

    /// Opens the root, which must be an existing directory.
    pub fn build(self) -> Result<PhysicalFs> {
        let host_err =
            |kind: io::ErrorKind, msg: String| VfsError::Io { path: None, source: io::Error::new(kind, msg) };
        let root = self.root.canonicalize().map_err(|e| host_err(e.kind(), format!("{}: {e}", self.root.display())))?;
        if !root.is_dir() {
            return Err(host_err(io::ErrorKind::NotADirectory, format!("{} is not a directory", root.display())));
        }
        let cache = self.read_only;
        Ok(PhysicalFs {
            root,
            read_only: self.read_only,
            symlinks: self.symlinks,
            meta_cache: cache.then(|| Arc::new(RwLock::new(HashMap::new()))),
            dir_cache: cache.then(|| Arc::new(RwLock::new(HashMap::new()))),
            names_cache: cache.then(|| Arc::new(RwLock::new(HashMap::new()))),
        })
    }
}

impl PhysicalFs {
    /// Uses `root` (an existing directory) as `/`, writable, with [`Symlinks::Deny`].
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        Self::builder(root).build()
    }

    /// Like [`PhysicalFs::new`], but every write fails with [`VfsError::ReadOnly`].
    pub fn read_only(root: impl AsRef<Path>) -> Result<Self> {
        Self::builder(root).read_only(true).build()
    }

    /// Starts configuring a host-backed file system rooted at `root`.
    pub fn builder(root: impl AsRef<Path>) -> PhysicalFsBuilder {
        PhysicalFsBuilder { root: root.as_ref().to_path_buf(), read_only: false, symlinks: Symlinks::default() }
    }

    /// The canonical host directory acting as `/`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The active symlink policy.
    #[must_use]
    pub fn symlink_policy(&self) -> Symlinks {
        self.symlinks
    }

    /// Maps a virtual path to the host path it is stored at, enforcing every
    /// confinement rule.
    pub fn host_path(&self, path: &VPath) -> Result<PathBuf> {
        self.resolve(path).map(|(host, _)| host)
    }

    /// [`host_path`](Self::host_path), plus the target's own metadata whenever the
    /// confinement check already had to fetch it (it saves a system call, and those
    /// dominate the cost of every operation).
    fn resolve(&self, path: &VPath) -> Result<(PathBuf, Walk)> {
        let mut host = self.root.clone();
        for component in path.components() {
            if !is_valid_component(component) {
                return Err(VfsError::InvalidPath(path.to_string()));
            }
            host.push(component);
        }
        let mut walk = Walk::Unknown;
        match self.symlinks {
            Symlinks::Deny => walk = self.deny_links(path)?,
            Symlinks::FollowWithinRoot => self.ensure_inside(&host, path)?,
        }
        Ok((host, walk))
    }

    /// Fails if any existing component of `path` is a link. Otherwise returns the
    /// metadata of `path` itself, or says where the walk ended without it.
    ///
    /// Costs one `lstat` per component. A component that is not a link has the same
    /// metadata whether or not links are followed, so the last one doubles as `stat`.
    fn deny_links(&self, path: &VPath) -> Result<Walk> {
        let mut probe = self.root.clone();
        let mut last: Option<fs::Metadata> = None;
        for component in path.components() {
            probe.push(component);
            match probe.symlink_metadata() {
                Ok(meta) if meta.file_type().is_symlink() => return Err(VfsError::PermissionDenied(path.clone())),
                Ok(meta) => last = Some(meta),
                // The rest does not exist (yet), and the nearest ancestor that does is `last`.
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
                    return Ok(Walk::Missing { under_file: last.as_ref().is_some_and(fs::Metadata::is_file) });
                }
                Err(_) => return Ok(Walk::Unknown), // let the host say what is wrong
            }
        }
        Ok(last.map_or(Walk::Unknown, Walk::Found))
    }

    /// Resolves the deepest existing ancestor of `host` and checks it is
    /// still under the root. A dangling symlink counts as an escape, because
    /// creating a file through it would land wherever it points.
    fn ensure_inside(&self, host: &Path, path: &VPath) -> Result<()> {
        let escape = || VfsError::PathEscapesRoot(path.clone());
        let mut probe = Some(host);
        while let Some(candidate) = probe {
            match candidate.canonicalize() {
                Ok(real) if real.starts_with(&self.root) => return Ok(()),
                Ok(_) => return Err(escape()),
                Err(_) if candidate.symlink_metadata().is_ok() => return Err(escape()),
                Err(_) => probe = candidate.parent(),
            }
        }
        Ok(())
    }

    fn writable(&self, path: &VPath) -> Result<()> {
        if self.read_only { Err(VfsError::ReadOnly(path.clone())) } else { Ok(()) }
    }

    /// Translates a host error, normalizing "missing" errors across platforms
    /// (Windows reports a path through a file as not-found, Unix as ENOTDIR).
    fn host_err(&self, path: &VPath, err: io::Error) -> VfsError {
        match err.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => self.classify(path),
            _ => VfsError::from_io(path, err),
        }
    }

    fn classify(&self, path: &VPath) -> VfsError {
        classify_missing(path, |p| self.stat(p).ok().map(|(_, meta)| meta.file_type))
    }

    fn stat(&self, path: &VPath) -> Result<(PathBuf, Metadata)> {
        if let Some(cache) = &self.meta_cache {
            if let Some(hit) = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(path).cloned() {
                return hit.map_err(|missing| missing.error(path));
            }
            let result = match self.stat_below_parent(path) {
                Some(Below::Derived(e)) => return Err(e),
                Some(Below::Probed(result)) => result,
                None => self.stat_uncached(path),
            };
            let Some(entry) = Missing::cacheable(&result) else { return result };
            cache.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(path.clone(), entry);
            return result;
        }
        self.stat_uncached(path)
    }

    fn read_dir_uncached(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        let (host, meta) = self.stat(path)?;
        if !meta.is_dir() {
            return Err(VfsError::NotADirectory(path.clone()));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&host).map_err(|e| self.host_err(path, e))? {
            let entry = entry.map_err(|e| VfsError::from_io(path, e))?;
            // Names that cannot be expressed as a virtual path component are skipped.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if name.contains('\\') || !is_valid_component(&name) {
                continue;
            }
            let Some(file_type) = self.entry_type(&entry) else { continue };
            entries.push(DirEntry { path: path.child(&name), name, file_type });
        }
        // Hosts list in arbitrary order (ext4, APFS); every other backend is sorted, and callers such as
        // `copy_tree` fail at a different point when the order differs.
        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// `false` only when the host directory `dir` certainly has no entry called `name`.
    ///
    /// Names that are not plain ASCII are always "maybe": the host may fold them in ways the
    /// lowercase set does not.
    fn may_contain(&self, dir: &VPath, dir_host: &Path, name: &str) -> bool {
        let Some(cache) = &self.names_cache else { return true };
        if !name.is_ascii() {
            return true;
        }
        let known = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(dir).cloned();
        let names = known.unwrap_or_else(|| {
            let scanned = fs::read_dir(dir_host).ok().map(|entries| {
                Arc::new(
                    entries
                        .filter_map(|e| e.ok()?.file_name().to_str().map(str::to_lowercase))
                        .collect::<HashSet<String>>(),
                )
            });
            cache.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(dir.clone(), scanned.clone());
            scanned
        });
        names.is_none_or(|names| names.contains(&name.to_lowercase()))
    }

    /// The cheap lookup for a cached instance: the parent decides.
    ///
    /// The parent is itself looked up through the cache, so it was already checked for links on the
    /// way in. If it is missing or a file, so is `path` — no system call, and nothing worth caching:
    /// the answer is the parent's, one lookup away. If it is a directory, `path` costs one `lstat`
    /// instead of one per component plus a second failing `stat`.
    /// This matters when many layers are probed for many paths: most layers lack the directory, and
    /// the answer is the same for every file in it. Caching each of those misses instead would add an
    /// entry per path per layer.
    ///
    /// `None` when the shortcut does not apply (root, or links are followed).
    fn stat_below_parent(&self, path: &VPath) -> Option<Below> {
        if self.symlinks != Symlinks::Deny {
            return None;
        }
        let parent = path.parent()?;
        let name = path.file_name()?;
        let (parent_host, parent_meta) = match self.stat(&parent) {
            Ok(found) => found,
            Err(VfsError::NotFound(_)) => return Some(Below::Derived(VfsError::NotFound(path.clone()))),
            Err(VfsError::NotADirectory(_)) => return Some(Below::Derived(VfsError::NotADirectory(path.clone()))),
            Err(e) => return Some(Below::Probed(Err(e.with_path(path)))),
        };
        if !parent_meta.is_dir() {
            return Some(Below::Derived(VfsError::NotADirectory(path.clone())));
        }
        if !is_valid_component(name) {
            return Some(Below::Probed(Err(VfsError::InvalidPath(path.to_string()))));
        }
        // Most layers have the directory but not the file; asking the host about each one is a
        // failing system call per layer per file, so the directory's names are read once instead.
        if !self.may_contain(&parent, &parent_host, name) {
            return Some(Below::Derived(VfsError::NotFound(path.clone())));
        }
        let host = parent_host.join(name);
        let meta = match host.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Some(Below::Probed(Err(VfsError::PermissionDenied(path.clone()))));
            }
            Ok(meta) => meta,
            Err(e) => return Some(Below::Probed(Err(self.host_err(path, e)))),
        };
        if !meta.is_dir() && !meta.is_file() {
            return Some(Below::Probed(Err(VfsError::NotFound(path.clone())))); // devices, sockets, FIFOs are out of scope
        }
        Some(Below::Probed(Ok((host, self.convert(&meta)))))
    }

    fn stat_uncached(&self, path: &VPath) -> Result<(PathBuf, Metadata)> {
        let (host, walk) = self.resolve(path)?;
        let meta = match walk {
            Walk::Found(meta) => meta,
            // Already known, so the host is not asked again and the ancestors are not re-examined
            // to tell the two apart: this is what every miss in an empty layer costs.
            Walk::Missing { under_file: true } => return Err(VfsError::NotADirectory(path.clone())),
            Walk::Missing { under_file: false } => return Err(VfsError::NotFound(path.clone())),
            Walk::Unknown => fs::metadata(&host).map_err(|e| self.host_err(path, e))?,
        };
        if !meta.is_dir() && !meta.is_file() {
            return Err(VfsError::NotFound(path.clone())); // devices, sockets, FIFOs are out of scope
        }
        Ok((host, self.convert(&meta)))
    }

    fn convert(&self, meta: &fs::Metadata) -> Metadata {
        let base = if meta.is_dir() { Metadata::dir() } else { Metadata::file(meta.len()) };
        base.with_modified(meta.modified().ok()).with_read_only(self.read_only || meta.permissions().readonly())
    }

    /// The parent of `path` must be an existing directory; errors name `path`.
    fn require_parent_dir(&self, path: &VPath) -> Result<()> {
        let parent = path.parent().unwrap_or_else(VPath::root);
        match self.stat(&parent) {
            Ok((_, meta)) if meta.is_dir() => Ok(()),
            Ok(_) => Err(VfsError::NotADirectory(path.clone())),
            Err(VfsError::NotFound(_) | VfsError::NotADirectory(_)) => Err(self.classify(path)),
            Err(e) => Err(e.with_path(path)),
        }
    }

    /// The listing type of a host entry under the active policy, or `None` to hide it.
    fn entry_type(&self, entry: &fs::DirEntry) -> Option<FileType> {
        let mut file_type = entry.file_type().ok()?;
        if file_type.is_symlink() {
            if self.symlinks == Symlinks::Deny {
                return None;
            }
            let target = entry.path().canonicalize().ok()?; // dangling links are hidden
            if !target.starts_with(&self.root) {
                return None;
            }
            file_type = fs::metadata(&target).ok()?.file_type();
        }
        if file_type.is_dir() {
            Some(FileType::Dir)
        } else if file_type.is_file() {
            Some(FileType::File)
        } else {
            None
        }
    }
}

/// `true` if the host would treat `name` as exactly one ordinary path segment.
fn is_valid_component(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    let single = matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None));
    single && (!cfg!(windows) || is_valid_windows_name(name))
}

/// Windows file-name rules (checked on every platform by the unit tests).
fn is_valid_windows_name(name: &str) -> bool {
    // COM/LPT with superscript digits (¹²³) are devices too.
    const DEVICES: [&str; 32] = [
        "CON",
        "PRN",
        "AUX",
        "NUL",
        "CONIN$",
        "CONOUT$",
        "COM0",
        "COM1",
        "COM2",
        "COM3",
        "COM4",
        "COM5",
        "COM6",
        "COM7",
        "COM8",
        "COM9",
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT0",
        "LPT1",
        "LPT2",
        "LPT3",
        "LPT4",
        "LPT5",
        "LPT6",
        "LPT7",
        "LPT8",
        "LPT9",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
    ];
    if name.ends_with(['.', ' ']) {
        return false; // Windows strips these, aliasing `a.` to `a`
    }
    if name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control()) {
        return false;
    }
    // `NUL.txt` and `nul .tar.gz` still address the device.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    !DEVICES.iter().any(|device| device.eq_ignore_ascii_case(stem))
}

/// `true` if both host paths resolve to the same entry (e.g. case aliases on
/// case-insensitive file systems).
fn same_entry(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

impl FileSystem for PhysicalFs {
    fn metadata(&self, path: &VPath) -> Result<Metadata> {
        self.stat(path).map(|(_, meta)| meta)
    }

    fn read_dir(&self, path: &VPath) -> Result<Vec<DirEntry>> {
        if let Some(cache) = &self.dir_cache {
            if let Some(hit) = cache.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(path).cloned() {
                return Ok((*hit).clone());
            }
            let entries = self.read_dir_uncached(path)?;
            cache
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(path.clone(), Arc::new(entries.clone()));
            return Ok(entries);
        }
        self.read_dir_uncached(path)
    }

    fn create_dir(&self, path: &VPath) -> Result<()> {
        self.writable(path)?;
        if path.is_root() {
            return Err(VfsError::AlreadyExists(path.clone()));
        }
        let host = self.host_path(path)?;
        fs::create_dir(&host).map_err(|e| self.host_err(path, e))
    }

    fn remove_file(&self, path: &VPath) -> Result<()> {
        self.writable(path)?;
        let (host, meta) = self.stat(path)?;
        if meta.is_dir() {
            return Err(VfsError::IsADirectory(path.clone()));
        }
        fs::remove_file(&host).map_err(|e| self.host_err(path, e))
    }

    fn remove_dir(&self, path: &VPath) -> Result<()> {
        self.writable(path)?;
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        let (host, meta) = self.stat(path)?;
        if !meta.is_dir() {
            return Err(VfsError::NotADirectory(path.clone()));
        }
        fs::remove_dir(&host).map_err(|e| self.host_err(path, e))
    }

    fn remove_dir_all(&self, path: &VPath) -> Result<()> {
        self.writable(path)?;
        if path.is_root() {
            return Err(VfsError::InvalidInput(msg::REMOVE_ROOT));
        }
        let (host, meta) = self.stat(path)?;
        if !meta.is_dir() {
            return Err(VfsError::NotADirectory(path.clone()));
        }
        // std's implementation never follows links and is hardened against races.
        fs::remove_dir_all(&host).map_err(|e| self.host_err(path, e))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        self.writable(from)?;
        if from.is_root() || to.is_root() {
            return Err(VfsError::InvalidInput(msg::RENAME_ROOT));
        }
        let (src, src_meta) = self.stat(from)?;
        if from == to {
            return Ok(());
        }
        if src_meta.is_dir() && to.starts_with(from) {
            return Err(VfsError::InvalidInput(msg::INTO_ITSELF));
        }
        self.require_parent_dir(to)?;
        let dst = self.host_path(to)?;
        // A case-only rename on a case-insensitive host targets the source itself.
        if !same_entry(&src, &dst) {
            match self.stat(to) {
                Ok((_, existing)) if existing.is_dir() => return Err(VfsError::AlreadyExists(to.clone())),
                Ok(_) if src_meta.is_dir() => return Err(VfsError::NotADirectory(to.clone())),
                Ok(_) | Err(VfsError::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        fs::rename(&src, &dst).map_err(|e| self.host_err(to, e))
    }

    fn copy_file(&self, from: &VPath, to: &VPath) -> Result<u64> {
        self.writable(to)?;
        let (src, src_meta) = self.stat(from)?;
        if src_meta.is_dir() {
            return Err(VfsError::IsADirectory(from.clone()));
        }
        if from == to {
            return Err(VfsError::InvalidInput(msg::SAME_FILE));
        }
        self.require_parent_dir(to)?;
        match self.stat(to) {
            Ok((_, existing)) if existing.is_dir() => return Err(VfsError::IsADirectory(to.clone())),
            Ok(_) | Err(VfsError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        let dst = self.host_path(to)?;
        if same_entry(&src, &dst) {
            return Err(VfsError::InvalidInput(msg::SAME_FILE)); // never truncate the source
        }
        fs::copy(&src, &dst).map_err(|e| self.host_err(to, e))
    }

    fn open(&self, path: &VPath, options: &OpenOptions) -> Result<Box<dyn VfsFile>> {
        options.validate()?;
        if options.is_write() {
            self.writable(path)?;
        }
        let (host, walk) = self.resolve(path)?;
        match walk.found().map_or_else(|| fs::metadata(&host), Ok) {
            Ok(meta) if meta.is_dir() => return Err(VfsError::IsADirectory(path.clone())),
            Ok(meta) if !meta.is_file() => return Err(VfsError::NotFound(path.clone())),
            _ => {}
        }
        let file = options.to_std().open(&host).map_err(|e| self.host_err(path, e))?;
        Ok(Box::new(file))
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_name_rules() {
        for ok in ["a", "file.txt", ".hidden", "a..b", "ação", "日本語", "console", "COM10", "my LPT1"] {
            assert!(is_valid_windows_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "CON",
            "con",
            "Nul",
            "nul.txt",
            "NUL .tar",
            "aux",
            "PRN",
            "COM1",
            "com9.log",
            "LPT1",
            "CONIN$",
            "COM\u{b9}",
            "LPT\u{b2}",
            "a:b",
            "x?",
            "pipe|",
            "quote\"",
            "star*",
            "lt<",
            "gt>",
            "tab\t",
            "dot.",
            "space ",
            "...",
        ] {
            assert!(!is_valid_windows_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn single_component_rule() {
        assert!(is_valid_component("plain"));
        assert!(!is_valid_component("."));
        assert!(!is_valid_component(".."));
        assert!(!is_valid_component(""));
    }
}
