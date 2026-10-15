//! Scaling smoke tests: the index must handle a real mod list in seconds.
//!
//! The full-scale benchmark is `cargo run --release -p openvfs-mods --example bench_index`;
//! these are the CI-safe versions — small, deterministic, and asserted only
//! for *completeness*, not absolute time (see `openvfs/tests/it/scaling.rs`
//! for the timing-ratio style used when asymptotics need guarding).

use std::hint::black_box;
use std::sync::Arc;

use openvfs::prelude::*;

use crate::common::{Rng, env_knob, populate, tree};
use openvfs_mods::{Layer, MergedIndex};

/// `mods` in-memory layers of `files` files each, spread over a few dirs and
/// deliberately case-mixed, with every mod also providing the same shared set
/// so conflicts exist.
fn make_layers(mods: usize, files: usize, shared: usize) -> Vec<Layer> {
    (0..mods)
        .map(|m| {
            let fs: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
            let mut contents = tree(&[("/Data/", ""), ("/Shared/", "")]);
            for i in 0..files {
                contents.insert(
                    format!("/Data/{m}f{i}.txt"),
                    crate::common::Entry::File(format!("m{m} f{i}").into_bytes()),
                );
            }
            for s in 0..shared {
                contents.insert(
                    format!("/Shared/s{s}.txt"),
                    crate::common::Entry::File(format!("from mod {m}").into_bytes()),
                );
            }
            populate(&*fs, &contents);
            Layer::new(format!("mod{m}"), fs)
        })
        .collect()
}

#[test]
fn a_real_sized_index_builds_and_answers() {
    // Defaults are CI-safe; scale up with OPENVFS_MODS_TEST_* to reproduce.
    let mods = usize::try_from(env_knob("OPENVFS_MODS_TEST_MODS", 30)).unwrap();
    let files = usize::try_from(env_knob("OPENVFS_MODS_TEST_FILES", 60)).unwrap();
    let shared = usize::try_from(env_knob("OPENVFS_MODS_TEST_SHARED", 20)).unwrap();

    let index = MergedIndex::build(make_layers(mods, files, shared)).unwrap();
    assert_eq!(index.len(), 2 + mods * files + shared); // /Data /Shared + files

    // Lookups across the mod list, random but reproducible.
    let mut rng = Rng::new(0x5EED);
    for _ in 0..(mods * files).min(4_000) {
        let (m, f) = (rng.below(mods), rng.below(files));
        let path = VPath::new(format!("/data/{m}F{f}.TXT")).unwrap(); // folded on purpose
        let winner = index.winner(&path).unwrap();
        assert_eq!(winner.name(), format!("mod{m}"));
        assert_eq!(winner.path().as_str(), format!("/Data/{m}f{f}.txt"));
    }

    // Every shared file conflicts with all mods below the winner.
    let report = index.conflicts();
    assert_eq!(report.file_conflicts().count(), shared);
    for (layer, count) in report.file_counts().iter().enumerate() {
        assert_eq!(count.won, usize::from(layer == 0) * shared);
        assert_eq!(count.lost, usize::from(layer != 0) * shared);
    }
    black_box(report.to_string());
}
