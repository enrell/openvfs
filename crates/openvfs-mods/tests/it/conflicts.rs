//! `ConflictReport`: winners, ordered losers, per-mod won/lost counts.

use std::sync::Arc;

use openvfs::prelude::*;

use crate::common::{populate, tree};
use openvfs_mods::{FileCount, Layer, MergedIndex};

fn mem_layer(name: &str, contents: &[(&str, &str)]) -> Layer {
    let fs: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*fs, &tree(contents));
    Layer::new(name, fs)
}

fn three_mod_index() -> MergedIndex {
    MergedIndex::build(vec![
        mem_layer("top", &[("/data/", ""), ("/data/f.txt", "top's f"), ("/top.txt", "t")]),
        mem_layer("mid", &[("/DATA/", ""), ("/DATA/f.txt", "mid's f"), ("/data/", ""), ("/data/g.txt", "mid's g")]),
        mem_layer("base", &[("/data/", ""), ("/data/F.TXT", "base f"), ("/data/g.txt", "base g"), ("/base.txt", "b")]),
    ])
    .unwrap()
}

#[test]
fn contested_files_name_winner_and_ordered_losers() {
    let report = three_mod_index().conflicts();
    let f = report.entries().iter().find(|e| e.folded_path() == "/data/f.txt").unwrap();
    assert_eq!(f.winner().name(), "top");
    assert_eq!(f.path().as_str(), "/data/f.txt", "the winner's spelling, not the folded one");
    assert_eq!(f.losers().iter().map(openvfs_mods::Provider::name).collect::<Vec<_>>(), ["mid", "base"]);

    let g = report.entries().iter().find(|e| e.folded_path() == "/data/g.txt").unwrap();
    assert_eq!(g.winner().name(), "mid");
    assert_eq!(g.losers()[0].name(), "base");

    // Uncontested files produce no entries.
    assert!(report.entries().iter().all(|e| e.folded_path() != "/top.txt"));
}

#[test]
fn shared_directories_conflict_too() {
    // All three mods list a `data` directory (with different spellings).
    let report = three_mod_index().conflicts();
    let dir = report.entries().iter().find(|e| e.folded_path() == "/data").unwrap();
    assert_eq!(dir.file_type(), FileType::Dir);
    assert_eq!(dir.providers().len(), 3);
    // …but it is not a *file* conflict.
    assert!(report.file_conflicts().all(|e| e.file_type() == FileType::File));
    assert_eq!(report.file_conflicts().count(), 2);
}

#[test]
fn per_mod_file_counts() {
    let report = three_mod_index().conflicts();
    assert_eq!(
        report.file_counts(),
        [
            FileCount { won: 1, lost: 0 }, // "top" serves f.txt
            FileCount { won: 1, lost: 1 }, // "mid" serves g.txt, loses f.txt
            FileCount { won: 0, lost: 2 }, // "base" loses both
        ]
    );
    // An empty report has no counts and nothing to display.
    let quiet = MergedIndex::build(vec![mem_layer("only", &[("/f", "x")])]).unwrap().conflicts();
    assert!(quiet.is_empty());
    assert_eq!(quiet.file_counts(), [FileCount::default()]);
}

#[test]
fn display_names_every_conflict_and_the_counts() {
    let report = three_mod_index().conflicts().to_string();
    assert!(report.contains("/data/f.txt"), "{report}");
    assert!(report.contains("\"top\" over \"mid\", \"base\""), "{report}");
    assert!(report.contains("\"top\": 1 files won, 0 lost"), "{report}");
    assert!(report.contains("\"base\": 0 files won, 2 lost"), "{report}");
}
