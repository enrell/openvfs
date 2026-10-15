//! Scaling contracts: operations whose cost must not depend on how much *unrelated* data exists.
//!
//! These guard the asymptotics, not the absolute speed. Each test times the same operation on a small
//! and on a ~100x larger input and fails only if the large one is slower by a wide margin; the wide
//! margin (and taking the best of many interleaved rounds) keeps them stable on a loaded CI machine,
//! while a regression to linear behaviour is off by one to two orders of magnitude and cannot hide.
//! For absolute numbers use `cargo run --release -p openvfs --example bench`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use openvfs::prelude::*;

/// How many times slower `large` is than `small`, comparing the best of many rounds of each.
fn slowdown<A, B>(mut small: impl FnMut() -> A, mut large: impl FnMut() -> B) -> f64 {
    fn timed<R>(op: &mut impl FnMut() -> R, batch: u32) -> Duration {
        let started = Instant::now();
        for _ in 0..batch {
            black_box(op());
        }
        started.elapsed()
    }

    // Size the batch so one round of the *large* case takes about 2 ms.
    let mut batch = 1_u32;
    while timed(&mut large, batch) < Duration::from_millis(2) && batch < 1 << 20 {
        batch *= 2;
    }
    let (mut best_small, mut best_large) = (Duration::MAX, Duration::MAX);
    for _ in 0..40 {
        best_small = best_small.min(timed(&mut small, batch));
        best_large = best_large.min(timed(&mut large, batch));
    }
    best_large.as_secs_f64() / best_small.as_secs_f64().max(1e-9)
}

/// `files` files spread over `files / 100` directories under `/big`, plus an empty `/small`.
fn populated(files: usize) -> Vfs {
    let vfs = Vfs::memory();
    vfs.create_dir("/small").unwrap();
    for dir in 0..files.div_ceil(100) {
        vfs.create_dir_all(format!("/big/d{dir}")).unwrap();
    }
    for i in 0..files {
        vfs.write(format!("/big/d{}/f{i}", i / 100), "x").unwrap();
    }
    vfs
}

/// Listing a directory costs its *children*, not the size of the subtree below them.
#[test]
fn memory_read_dir_does_not_scan_the_subtree() {
    let (few, many) = (populated(1_000), populated(100_000));
    // `/` has two children (`small` and `big`) in both trees; `big` holds 100x more below it in one.
    let ratio = slowdown(|| few.read_dir("/").unwrap(), || many.read_dir("/").unwrap());
    assert!(ratio < 30.0, "read_dir('/') got {ratio:.0}x slower with 100x more nodes below (linear = ~100x)");
}

/// Looking at a directory is not slowed down by a huge sibling subtree either.
#[test]
fn memory_lookups_do_not_depend_on_unrelated_tree_size() {
    let (few, many) = (populated(1_000), populated(100_000));
    let ratio = slowdown(|| few.read_dir("/small").unwrap(), || many.read_dir("/small").unwrap());
    assert!(ratio < 30.0, "read_dir('/small') got {ratio:.0}x slower with 100x more nodes elsewhere");
    let ratio = slowdown(|| few.metadata("/big/d0/f0").unwrap(), || many.metadata("/big/d0/f0").unwrap());
    assert!(ratio < 30.0, "metadata got {ratio:.0}x slower with 100x more nodes elsewhere");
}

/// Resolving a path costs the path's depth, not the number of mount points.
#[test]
fn mount_resolution_does_not_scan_the_mount_table() {
    fn table(mounts: usize) -> Vfs {
        let table = MountFs::new();
        for i in 0..mounts {
            let fs = Vfs::memory();
            fs.write("/f", "x").unwrap();
            table.mount_arc(format!("/m{i}"), fs.backend().clone()).unwrap();
        }
        Vfs::new(table)
    }
    let (few, many) = (table(16), table(4_096));
    let ratio = slowdown(|| few.metadata("/m15/f").unwrap(), || many.metadata("/m4095/f").unwrap());
    assert!(ratio < 12.0, "metadata got {ratio:.0}x slower with 256x more mounts (linear = ~256x)");
    let ratio = slowdown(|| few.read("/m15/f").unwrap(), || many.read("/m4095/f").unwrap());
    assert!(ratio < 12.0, "read got {ratio:.0}x slower with 256x more mounts");
}
