//! Dependency-free micro-benchmarks for the hot paths of every backend.
//!
//! ```text
//! cargo run --release -p open-vfs --example bench
//! OPENVFS_BENCH_FILTER=mount cargo run --release -p open-vfs --example bench
//! ```
//!
//! Each case reports the **median** and the **best** of ten timed batches, in microseconds per
//! operation. The point is to see how cost *scales* (tree size, depth, mount count), so most cases
//! come in pairs or triples that differ in exactly one dimension. Absolute numbers depend on the
//! machine and, for `PhysicalFs`, on the host file system and any antivirus filter driver.

#![allow(clippy::cast_precision_loss)] // batch counts and timings are far below 2^52

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use openvfs::prelude::*;

struct Bench {
    filter: Option<String>,
    budget: Duration,
}

impl Bench {
    fn from_env() -> Self {
        let millis = std::env::var("OPENVFS_BENCH_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
        Self { filter: std::env::var("OPENVFS_BENCH_FILTER").ok(), budget: Duration::from_millis(millis) }
    }

    fn run<R>(&self, name: &str, mut op: impl FnMut() -> R) {
        if self.filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            return;
        }
        // Grow the batch until one batch takes a tenth of the budget, so timer noise is negligible.
        let mut batch = 1_u64;
        loop {
            let started = Instant::now();
            for _ in 0..batch {
                black_box(op());
            }
            if started.elapsed() >= self.budget / 10 || batch >= 1 << 24 {
                break;
            }
            batch *= 2;
        }
        let mut micros: Vec<f64> = (0..10)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..batch {
                    black_box(op());
                }
                started.elapsed().as_secs_f64() * 1e6 / batch as f64
            })
            .collect();
        micros.sort_by(f64::total_cmp);
        println!("{name:<54} {:>11.2} µs/op   (best {:>10.2})", micros[micros.len() / 2], micros[0]);
    }
}

/// `files` files spread over `files / 100` directories under `/big`.
fn populate(vfs: &Vfs, files: usize) {
    for dir in 0..files.div_ceil(100) {
        vfs.create_dir_all(format!("/big/d{dir}")).unwrap();
    }
    for i in 0..files {
        vfs.write(format!("/big/d{}/f{i}", i / 100), "x").unwrap();
    }
}

fn memory(bench: &Bench) {
    println!("\n== MemoryFs");
    // Listing `/` must cost the same however much sits *below* its two children.
    for nodes in [1_000, 10_000, 100_000] {
        let vfs = Vfs::memory();
        vfs.create_dir("/small").unwrap();
        populate(&vfs, nodes);
        bench.run(&format!("read_dir('/')       2 entries, {nodes:>6} nodes below"), || vfs.read_dir("/").unwrap());
        bench.run(&format!("read_dir('/small')  0 entries, {nodes:>6} nodes elsewhere"), || {
            vfs.read_dir("/small").unwrap()
        });
        bench.run(&format!("read_dir('/big')  {:>3} entries, {nodes:>6} nodes below", nodes.div_ceil(100)), || {
            vfs.read_dir("/big").unwrap()
        });
    }

    let vfs = Vfs::memory();
    vfs.create_dir_all("/a/b/c/d/e/f/g/h").unwrap();
    vfs.write("/a/b/c/d/e/f/g/h/file", vec![7_u8; 4096]).unwrap();
    bench.run("metadata, depth 8", || vfs.metadata("/a/b/c/d/e/f/g/h/file").unwrap());
    bench.run("read 4 KiB file, depth 8", || vfs.read("/a/b/c/d/e/f/g/h/file").unwrap());
    bench.run("write 4 KiB file (replace)", || vfs.write("/a/b/c/d/e/f/g/h/file", [7_u8; 4096]).unwrap());

    let vfs = Vfs::memory();
    populate(&vfs, 1_000);
    let mut forward = true;
    bench.run("rename directory holding 1000 files", || {
        let (from, to) = if forward { ("/big", "/moved") } else { ("/moved", "/big") };
        forward = !forward;
        vfs.rename(from, to).unwrap();
    });
}

fn mount(bench: &Bench) {
    println!("\n== MountFs");
    // Resolution must not depend on how many *other* mounts exist.
    for mounts in [1, 64, 1_024] {
        let table = MountFs::new();
        for i in 0..mounts {
            let fs = Vfs::memory();
            fs.write("/f", "x").unwrap();
            table.mount_arc(format!("/m{i}"), fs.backend().clone()).unwrap();
        }
        let vfs = Vfs::new(table);
        let target = format!("/m{}/f", mounts - 1);
        bench.run(&format!("metadata through {mounts:>5} mounts (last)"), || vfs.metadata(&target).unwrap());
        bench.run(&format!("metadata through {mounts:>5} mounts (missing)"), || vfs.metadata("/nowhere/f").is_err());
    }
}

fn overlay(bench: &Bench) {
    println!("\n== OverlayFs");
    let lower = Vfs::memory();
    populate(&lower, 1_000);
    lower.write("/lower-only", "x").unwrap();
    let vfs = Vfs::new(OverlayFs::new(MemoryFs::new(), lower.backend().clone()));
    vfs.write("/upper-only", "x").unwrap();
    bench.run("metadata, lower-only file", || vfs.metadata("/lower-only").unwrap());
    bench.run("metadata, upper-only file", || vfs.metadata("/upper-only").unwrap());
    bench.run("read_dir('/big'), 10 lower dirs merged", || vfs.read_dir("/big").unwrap());
    bench.run("read 1-byte lower file", || vfs.read("/lower-only").unwrap());
    let mut flip = false;
    bench.run("write + remove (whiteout churn)", || {
        flip = !flip;
        vfs.write("/big/d0/f0", if flip { "a" } else { "b" }).unwrap();
    });
}

fn physical(bench: &Bench) {
    println!("\n== PhysicalFs (host: {})", std::env::consts::OS);
    let root = scratch_dir();
    let deep = root.join("a/b/c/d/e/f/g/h");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("file"), vec![7_u8; 4096]).unwrap();
    for i in 0..1_000 {
        std::fs::write(root.join(format!("e{i}")), "x").unwrap();
    }

    let file = deep.join("file");
    bench.run("std::fs::metadata, depth 8 (baseline)", || std::fs::metadata(&file).unwrap());
    for (label, policy) in [("Deny", Symlinks::Deny), ("FollowWithinRoot", Symlinks::FollowWithinRoot)] {
        let vfs = Vfs::new(PhysicalFs::builder(&root).symlinks(policy).build().unwrap());
        bench.run(&format!("metadata, depth 8, {label}"), || vfs.metadata("/a/b/c/d/e/f/g/h/file").unwrap());
        bench.run(&format!("metadata, depth 1, {label}"), || vfs.metadata("/e0").unwrap());
        bench.run(&format!("read 4 KiB file, depth 8, {label}"), || vfs.read("/a/b/c/d/e/f/g/h/file").unwrap());
        bench.run(&format!("read_dir, 1000 entries, {label}"), || vfs.read_dir("/").unwrap());
    }
    drop(std::fs::remove_dir_all(&root));
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openvfs-bench-{}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Path::new(&dir).is_dir());
    dir
}

fn main() {
    let bench = Bench::from_env();
    println!("openvfs micro-benchmarks (median of 10 batches; OPENVFS_BENCH_FILTER / OPENVFS_BENCH_MS to adjust)");
    memory(&bench);
    mount(&bench);
    overlay(&bench);
    physical(&bench);
}
