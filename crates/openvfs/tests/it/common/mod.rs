//! Shared test infrastructure.
//!
//! * [`TempDir`] — host directories cleaned up on drop;
//! * [`Rng`] — a tiny seeded PRNG, so every randomized test is reproducible;
//! * [`Tree`] / [`snapshot`] / [`populate`] — whole-file-system comparisons;
//! * [`fixtures`] — every backend configuration under test;
//! * [`model`] — random operations and their canonical outcomes;
//! * [`faulty`] — fault injection.

#![allow(dead_code, clippy::cast_possible_truncation)] // each test binary uses a different subset

pub mod faulty;
pub mod fixtures;
pub mod model;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use openvfs::{FileSystem, OpenOptions, VPath, ops};

/// Arms the process-wide deadline (once): if the test binary is still running when it passes, a test
/// is hung, most likely on a deadlock, so abort with a clear message instead of waiting for CI to
/// time out hours later. `within` bounds an individual test; this bounds all the others.
///
/// The limit is `OPENVFS_TEST_DEADLINE_SECS` (default 180: the whole binary normally takes seconds).
/// Raise it for the scaled-up randomized runs.
pub fn arm_suite_deadline() {
    static ARMED: Once = Once::new();
    ARMED.call_once(|| {
        let limit = Duration::from_secs(env_knob("OPENVFS_TEST_DEADLINE_SECS", 180));
        std::thread::spawn(move || {
            std::thread::sleep(limit);
            // Straight to the real stderr: `eprintln!` would land in the output-capture buffer the
            // libtest harness gave the test that armed us, and nobody would ever see it.
            let message = format!(
                "\nthe test binary was still running {limit:?} after it started: a test is hung (deadlock?).\n\
                 The tests listed above as \"running for over 60 seconds\" are the suspects. Aborting.\n"
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
        let path = std::env::temp_dir().join(format!("openvfs-{tag}-{}-{n}", std::process::id()));
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

/// `SplitMix64`: small, fast, and good enough to drive property tests.
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

    /// `true` with probability `percent`%.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    pub fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
}

/// Reads an integer knob from the environment (used to scale randomized suites in CI).
pub fn env_knob(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
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

/// Runs `f` on another thread and fails the test if it does not finish in
/// time — turns a deadlock into a clear failure instead of a hung CI job.
#[track_caller]
pub fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    arm_suite_deadline();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(limit) {
        Ok(value) => {
            worker.join().unwrap();
            value
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("worker exited without sending"),
        },
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("operation did not finish within {limit:?} (deadlock?)"),
    }
}

/// Lists the names in a directory (sorted), for terse assertions.
pub fn names(fs: &dyn FileSystem, dir: &str) -> Vec<String> {
    let mut names: Vec<String> = fs.read_dir(&VPath::new(dir).unwrap()).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    names
}

/// Opens with explicit options (shorthand used across tests).
pub fn open(fs: &dyn FileSystem, path: &str, options: OpenOptions) -> openvfs::Result<Box<dyn openvfs::VfsFile>> {
    fs.open(&VPath::new(path).unwrap(), &options)
}
