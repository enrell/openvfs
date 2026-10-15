//! Shared test infrastructure: the few helpers the suites need, in the same
//! style as `openvfs/tests/it/common`.

#![allow(dead_code, clippy::cast_possible_truncation, clippy::cast_lossless)] // test helpers cast freely

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use openvfs::{DirEntry, FileSystem, FileType, Metadata, OpenOptions, VPath, VfsError, VfsFile, ops};

/// Arms the process-wide deadline (once): if the test binary is still running
/// when it passes, a test is hung — abort with a clear message instead of
/// waiting for CI to time out. `OPENVFS_TEST_DEADLINE_SECS` (default 180).
pub fn arm_suite_deadline() {
    static ARMED: Once = Once::new();
    ARMED.call_once(|| {
        let limit = Duration::from_secs(env_knob("OPENVFS_TEST_DEADLINE_SECS", 180));
        std::thread::spawn(move || {
            std::thread::sleep(limit);
            let message = format!(
                "\nthe test binary was still running {limit:?} after it started: a test is hung (deadlock?). Aborting.\n"
            );
            let _ = std::io::stderr().write_all(message.as_bytes());
            std::process::exit(101);
        });
    });
}

/// A host directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        arm_suite_deadline();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("openvfs-mods-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Reads an integer knob from the environment (scales randomized runs in CI).
pub fn env_knob(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// `SplitMix64`: small, fast, and makes every randomized test reproducible.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        arm_suite_deadline();
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// One node of a [`Tree`].
#[derive(Clone, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File(Vec<u8>),
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir => f.write_str("Dir"),
            Self::File(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) if bytes.len() <= 48 => write!(f, "File({text:?})"),
                _ => write!(f, "File({} bytes)", bytes.len()),
            },
        }
    }
}

/// Every node of a file system except the root, keyed by path. Sorted, so
/// parents always come before their children.
pub type Tree = BTreeMap<String, Entry>;

/// Builds a tree literal: `tree(&[("/a/", ""), ("/a/f", "data")])` — a
/// trailing slash marks a directory.
pub fn tree(entries: &[(&str, &str)]) -> Tree {
    entries
        .iter()
        .map(|(path, data)| match path.strip_suffix('/') {
            Some(dir) => (dir.to_owned(), Entry::Dir),
            None => ((*path).to_owned(), Entry::File(data.as_bytes().to_vec())),
        })
        .collect()
}

/// Writes `tree` into an (empty) file system.
pub fn populate(fs: &dyn FileSystem, tree: &Tree) {
    for (path, entry) in tree {
        let path = VPath::new(path).unwrap();
        match entry {
            Entry::Dir => fs.create_dir(&path).unwrap(),
            Entry::File(bytes) => ops::write_all(fs, &path, bytes).unwrap(),
        }
    }
}

/// Writes `tree` under a host directory (the on-disk twin of [`populate`]).
pub fn populate_host(root: &Path, tree: &Tree) {
    for (path, entry) in tree {
        let host = root.join(path.trim_start_matches('/'));
        match entry {
            Entry::Dir => std::fs::create_dir_all(&host).unwrap(),
            Entry::File(bytes) => {
                std::fs::create_dir_all(host.parent().unwrap()).unwrap();
                std::fs::write(&host, bytes).unwrap();
            }
        }
    }
}

/// Captures a whole file system using only the trait primitives.
pub fn snapshot(fs: &dyn FileSystem) -> Tree {
    fn visit(fs: &dyn FileSystem, dir: &VPath, out: &mut Tree) {
        for entry in fs.read_dir(dir).unwrap_or_else(|e| panic!("snapshot: read_dir({dir}): {e}")) {
            if entry.is_dir() {
                out.insert(entry.path.to_string(), Entry::Dir);
                visit(fs, &entry.path, out);
            } else {
                let bytes =
                    ops::read_all(fs, &entry.path).unwrap_or_else(|e| panic!("snapshot: read({}): {e}", entry.path));
                out.insert(entry.path.to_string(), Entry::File(bytes));
            }
        }
    }
    let mut out = Tree::new();
    visit(fs, &VPath::root(), &mut out);
    out
}

/// Panics with a readable diff when two trees differ.
#[track_caller]
pub fn assert_trees_eq(expected: &Tree, actual: &Tree, context: &str) {
    if expected == actual {
        return;
    }
    let mut report = String::new();
    for (path, entry) in expected {
        match actual.get(path) {
            None => {
                let _ = writeln!(report, "  missing    {path}: {entry:?}");
            }
            Some(other) if other != entry => {
                let _ = writeln!(report, "  differs    {path}: expected {entry:?}, got {other:?}");
            }
            Some(_) => {}
        }
    }
    for (path, entry) in actual {
        if !expected.contains_key(path) {
            let _ = writeln!(report, "  unexpected {path}: {entry:?}");
        }
    }
    panic!("trees differ ({context}):\n{report}");
}

/// Lists the names in a directory (sorted), for terse assertions.
pub fn names(fs: &dyn FileSystem, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = fs.read_dir(&VPath::new(dir).unwrap()).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    names
}

/// A backend that behaves like a mod manifest: it knows files and their
/// implied parents, but **never lists directories** — `read_dir` returns the
/// *files* below the queried directory (with their full deep paths), so an
/// index has to materialize the parent directories itself.
///
/// Read-only and single-threaded by construction; mutation methods fail.
pub struct SparseFs {
    files: BTreeMap<VPath, Vec<u8>>,
}

impl SparseFs {
    /// A sparse backend holding exactly `tree`'s files (dirs are implied).
    pub fn new(tree: &Tree) -> Self {
        let files = tree
            .iter()
            .filter_map(|(path, entry)| match entry {
                Entry::File(bytes) => Some((VPath::new(path).unwrap(), bytes.clone())),
                Entry::Dir => None,
            })
            .collect();
        Self { files }
    }

    fn kind_of(&self, path: &VPath) -> Option<FileType> {
        if self.files.contains_key(path) {
            return Some(FileType::File);
        }
        let prefix = format!("{}/", if path.is_root() { String::new() } else { path.to_string() });
        self.files.keys().any(|f| f.as_str().starts_with(&prefix)).then_some(FileType::Dir)
    }
}

impl FileSystem for SparseFs {
    fn metadata(&self, path: &VPath) -> openvfs::Result<Metadata> {
        match self.kind_of(path) {
            Some(FileType::File) => Ok(Metadata::file(self.files[path].len() as u64).with_read_only(true)),
            Some(FileType::Dir) => Ok(Metadata::dir().with_read_only(true)),
            None => Err(VfsError::NotFound(path.clone())),
        }
    }

    fn read_dir(&self, path: &VPath) -> openvfs::Result<Vec<DirEntry>> {
        match self.kind_of(path) {
            Some(FileType::Dir) => Ok(self
                .files
                .iter()
                .filter(|(file, _)| file.starts_with(path))
                .map(|(file, _)| DirEntry {
                    name: file.file_name().unwrap().to_owned(),
                    path: file.clone(),
                    file_type: FileType::File,
                })
                .collect()),
            Some(FileType::File) => Err(VfsError::NotADirectory(path.clone())),
            None => Err(VfsError::NotFound(path.clone())),
        }
    }

    fn create_dir(&self, path: &VPath) -> openvfs::Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn remove_file(&self, path: &VPath) -> openvfs::Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn remove_dir(&self, path: &VPath) -> openvfs::Result<()> {
        Err(VfsError::ReadOnly(path.clone()))
    }

    fn rename(&self, from: &VPath, _to: &VPath) -> openvfs::Result<()> {
        Err(VfsError::ReadOnly(from.clone()))
    }

    fn open(&self, path: &VPath, _options: &OpenOptions) -> openvfs::Result<Box<dyn VfsFile>> {
        Err(VfsError::ReadOnly(path.clone()))
    }
}
