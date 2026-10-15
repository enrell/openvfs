//! Scale benchmark for `MergedIndex` — the numbers a mod manager feels.
//!
//! ```text
//! cargo run --release -p openvfs-mods --example bench_index
//! OPENVFS_MODS_MODS=500 OPENVFS_MODS_FILES=200 OPENVFS_MODS_LOOKUPS=100000 cargo run --release -p openvfs-mods --example bench_index
//! OPENVFS_MODS_BACKEND=mem cargo run --release -p openvfs-mods --example bench_index
//! ```
//!
//! Builds `MODS` mod directories of `FILES` files each (default 500 × 200 =
//! 100k files) — on disk under `%TEMP%` unless `OPENVFS_MODS_BACKEND=mem` —
//! then times the index build, a batch of random folded lookups, and the
//! conflict report. Every mod also provides `OPENVFS_MODS_SHARED` (default
//! 50) shared files, so conflicts exist. Population is what it is (creating
//! 100k real files is dominated by the OS and any antivirus); the index
//! times are what this benchmark is about.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openvfs::prelude::*;
use openvfs_mods::{Layer, MergedIndex};

/// `SplitMix64`, like the test suite's `Rng`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap_or(0)
    }
}

fn knob(name: &str, default: u64) -> usize {
    usize::try_from(std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)).unwrap()
}

/// A scratch directory under `%TEMP%`, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("openvfs-mods-bench-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&path));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

/// `dirs` fan-out directories per mod; files are named `f{i}` inside `d{i % dirs}`.
const DIRS_PER_MOD: usize = 10;

/// Writes one mod directory on disk: `unique` files under `data/` named so
/// that every mod owns distinct paths (mods mostly *add* files), plus the
/// `shared` files under `shared/` that every mod provides (the conflicts).
fn populate_mod_disk(root: &Path, m: usize, unique: usize, shared: usize) -> PathBuf {
    let dir = root.join(format!("mod{m:04}"));
    for d in 0..DIRS_PER_MOD {
        std::fs::create_dir_all(dir.join(format!("data/d{d}"))).unwrap();
    }
    std::fs::create_dir_all(dir.join("shared")).unwrap();
    for i in 0..unique {
        std::fs::write(dir.join(format!("data/d{}/f{i}_m{m}.txt", i % DIRS_PER_MOD)), format!("mod {m} file {i}"))
            .unwrap();
    }
    for s in 0..shared {
        std::fs::write(dir.join(format!("shared/s{s}.txt")), format!("mod {m}")).unwrap();
    }
    dir
}

/// The same layout in a `MemoryFs` (no host I/O, same index work).
fn populate_mod_mem(m: usize, unique: usize, shared: usize) -> Arc<dyn FileSystem> {
    let vfs = Vfs::memory();
    for d in 0..DIRS_PER_MOD {
        vfs.create_dir_all(format!("/data/d{d}")).unwrap();
    }
    vfs.create_dir_all("/shared").unwrap();
    for i in 0..unique {
        vfs.write(format!("/data/d{}/f{i}_m{m}.txt", i % DIRS_PER_MOD), format!("mod {m} file {i}")).unwrap();
    }
    for s in 0..shared {
        vfs.write(format!("/shared/s{s}.txt"), format!("mod {m}")).unwrap();
    }
    vfs.backend().clone()
}

fn main() {
    let mods = knob("OPENVFS_MODS_MODS", 500);
    let files = knob("OPENVFS_MODS_FILES", 200);
    let lookups = knob("OPENVFS_MODS_LOOKUPS", 100_000);
    let shared = knob("OPENVFS_MODS_SHARED", 50).min(files);
    let backend = std::env::var("OPENVFS_MODS_BACKEND").unwrap_or_else(|_| "disk".into());
    let unique = files - shared;

    println!("openvfs-mods index benchmark: {mods} mods x {files} files ({shared} shared), backend={backend}");

    // --- populate ---------------------------------------------------------
    let started = Instant::now();
    let scratch = (backend != "mem").then(ScratchDir::new);
    let mut layers = Vec::with_capacity(mods);
    match scratch.as_ref() {
        Some(scratch) => {
            // Creating 100k small files is syscall-bound: parallelize the mods.
            let dirs: Vec<PathBuf> = std::thread::scope(|scope| {
                let threads = usize::min(16, mods);
                let chunk = mods.div_ceil(threads);
                let mut workers = Vec::new();
                for t in 0..threads {
                    let root = scratch.0.clone();
                    workers.push(scope.spawn(move || {
                        (t * chunk..((t + 1) * chunk).min(mods))
                            .map(|m| populate_mod_disk(&root, m, unique, shared))
                            .collect::<Vec<_>>()
                    }));
                }
                workers.into_iter().flat_map(|w| w.join().unwrap()).collect()
            });
            for (m, dir) in dirs.into_iter().enumerate() {
                let fs: Arc<dyn FileSystem> = Arc::new(PhysicalFs::read_only(dir).unwrap());
                layers.push(Layer::new(format!("mod{m:04}"), fs));
            }
        }
        None => {
            for m in 0..mods {
                layers.push(Layer::new(format!("mod{m:04}"), populate_mod_mem(m, unique, shared)));
            }
        }
    }
    let populated = started.elapsed();
    println!("{:<28} {:>10.2?}   ({} files)", "populate layers", populated, mods * files);

    // --- index build --------------------------------------------------------
    let started = Instant::now();
    let index = MergedIndex::build(layers).unwrap();
    let built = started.elapsed();
    println!("{:<28} {:>10.2?}   ({} nodes)", "MergedIndex::build", built, index.len());

    // --- folded lookups -----------------------------------------------------
    let started = Instant::now();
    let mut rng = Rng(0x5EED);
    let mut hits = 0_u64;
    for _ in 0..lookups {
        let (m, i) = (rng.below(mods), rng.below(unique.max(1)));
        // Deliberately weird casing: lookups fold, the index still answers.
        let path = VPath::new(format!("/DATA/D{}/F{i}_M{m}.TXT", i % DIRS_PER_MOD)).unwrap();
        hits += u64::from(black_box(index.real_path(&path)).is_some());
    }
    let looked_up = started.elapsed();
    println!("{:<28} {:>10.2?}   ({lookups} lookups, {hits} hits)", "real_path lookups", looked_up);

    // --- conflict report ----------------------------------------------------
    let started = Instant::now();
    let report = index.conflicts();
    let reported = started.elapsed();
    let file_conflicts = report.file_conflicts().count();
    println!("{:<28} {:>10.2?}   ({} conflicts, {file_conflicts} files)", "conflicts()", reported, report.len());
    for (layer, count) in report.file_counts().iter().enumerate() {
        if count.won + count.lost > 0 {
            black_box((layer, count.won, count.lost));
        }
    }

    if populated > Duration::from_secs(120) {
        println!("note: population dominated this run — try OPENVFS_MODS_BACKEND=mem or OPENVFS_MODS_MODS=50");
    }
}
