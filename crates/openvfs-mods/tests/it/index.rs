//! `MergedIndex`: folded lookups, priority order, implied directories,
//! snapshot consistency.

use std::sync::Arc;

use openvfs::prelude::*;

use crate::common::{SparseFs, populate, tree};
use openvfs_mods::{Layer, MergedIndex};

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

/// An in-memory layer holding `contents`, called `name`.
fn mem_layer(name: &str, contents: &[(&str, &str)]) -> Layer {
    let fs: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*fs, &tree(contents));
    Layer::new(name, fs)
}

/// The layer names providing `path`, winners first — the terse assertion.
fn providers(index: &MergedIndex, path: &str) -> Vec<String> {
    index.providers(&p(path)).unwrap().iter().map(|pr| pr.name().to_owned()).collect()
}

#[test]
fn folded_lookups_find_the_real_spelling() {
    let index =
        MergedIndex::build(vec![mem_layer("textures-mod", &[("/textures/", ""), ("/textures/A.DDS", "a")])]).unwrap();
    let winner = index.winner(&p("/TEXTURES/a.dds")).expect("folded lookup must hit");
    assert_eq!(winner.name(), "textures-mod");
    assert_eq!(winner.path(), &p("/textures/A.DDS"), "winner keeps the real casing");
    assert_eq!(index.real_path(&p("/TeXtUrEs/A.DdS")), Some(&p("/textures/A.DDS")));
    assert!(index.winner(&p("/other")).is_none());
}

#[test]
fn highest_priority_layer_wins() {
    let index = MergedIndex::build(vec![
        mem_layer("top", &[("/f.txt", "top"), ("/top-only", "t")]),
        mem_layer("mid", &[("/f.txt", "mid"), ("/g.txt", "mid")]),
        mem_layer("base", &[("/f.txt", "base"), ("/g.txt", "base"), ("/h.txt", "base")]),
    ])
    .unwrap();
    assert_eq!(providers(&index, "/f.txt"), ["top", "mid", "base"]);
    assert_eq!(providers(&index, "/g.txt"), ["mid", "base"]);
    assert_eq!(providers(&index, "/h.txt"), ["base"]);
    assert_eq!(providers(&index, "/top-only"), ["top"]);
    assert_eq!(index.winner(&p("/F.TXT")).unwrap().name(), "top");
}

#[test]
fn read_dir_merges_directories_with_mixed_case() {
    // One mod spells it `Textures/`, the other `textures/` — in the folded
    // namespace that is ONE directory containing both mods' files.
    let index = MergedIndex::build(vec![
        mem_layer("a", &[("/Textures/", ""), ("/Textures/x.dds", "x")]),
        mem_layer("b", &[("/textures/", ""), ("/textures/y.dds", "y")]),
    ])
    .unwrap();
    let root = index.read_dir(&p("/")).unwrap();
    assert_eq!(root.len(), 1, "the two spellings must merge into one dir");
    assert_eq!(root[0].name, "Textures", "the winner's spelling is shown");
    assert!(root[0].is_dir());

    let mut children: Vec<String> = index.read_dir(&p("/TEXTURES")).unwrap().into_iter().map(|e| e.name).collect();
    children.sort();
    assert_eq!(children, ["x.dds", "y.dds"]);
}

#[test]
fn real_path_prefers_the_highest_priority_spelling() {
    // Layer 0's dir is only implied by its file (a manifest-style sparse
    // backend); layer 1 lists the dir explicitly. Layer 0 still wins —
    // implied or not, it is higher priority.
    let index = MergedIndex::build(vec![
        Layer::new("upper", Arc::new(SparseFs::new(&tree(&[("/DATA/f", "x")])))),
        mem_layer("lower", &[("/data/", ""), ("/data/g", "y")]),
    ])
    .unwrap();
    assert_eq!(index.real_path(&p("/data")), Some(&p("/DATA")));
    assert!(!index.winner(&p("/data")).unwrap().listed(), "layer 0's provider is implied");
    assert!(index.providers(&p("/data")).unwrap()[1].listed(), "layer 1's provider is explicit");
}

#[test]
fn parents_implied_by_files_are_directories() {
    // A "manifest" backend: lists files with deep paths, never the dirs.
    let sparse = SparseFs::new(&tree(&[("/a/b/c/f.txt", "deep")]));
    let index = MergedIndex::build(vec![Layer::new("sparse", Arc::new(sparse))]).unwrap();
    for dir in ["/a", "/a/b", "/a/b/c"] {
        let meta = index.metadata(&p(dir)).unwrap_or_else(|| panic!("{dir} must exist as an implied dir"));
        assert!(meta.is_dir(), "{dir} must be a directory");
        assert!(!index.winner(&p(dir)).unwrap().listed(), "{dir} is implied, not listed");
    }
    // …and they show up in listings like real dirs.
    assert_eq!(index.read_dir(&p("/")).unwrap()[0].name, "a");
    assert_eq!(index.read_dir(&p("/a/b")).unwrap()[0].name, "c");
}

#[test]
fn index_metadata_is_type_only_and_read_only() {
    let index = MergedIndex::build(vec![mem_layer("m", &[("/f.txt", "contents")])]).unwrap();
    let meta = index.metadata(&p("/F.TXT")).unwrap();
    assert!(meta.is_file());
    assert!(meta.read_only, "an index never serves writes");
    assert_eq!(meta.len, 0, "sizes are not tracked; stat the real file instead");
    assert!(index.metadata(&p("/")).unwrap().is_dir(), "the merged root always exists");
    assert!(index.metadata(&p("/nope")).is_none());
}

#[test]
fn read_dir_on_a_file_is_none() {
    let index = MergedIndex::build(vec![mem_layer("m", &[("/f.txt", "x")])]).unwrap();
    assert!(index.read_dir(&p("/f.txt")).is_none());
    assert!(index.read_dir(&p("/missing")).is_none());
    assert!(index.read_dir(&p("/")).unwrap().iter().any(|e| e.name == "f.txt"));
}

#[test]
fn snapshot_stays_until_rebuild() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*fs, &tree(&[("/old.txt", "o")]));
    let mut index = MergedIndex::build(vec![Layer::new("m", Arc::clone(&fs))]).unwrap();
    assert!(!index.is_stale());

    // A layer changes behind the index's back: the snapshot still answers.
    populate(&*fs, &tree(&[("/new.txt", "n")]));
    assert!(index.winner(&p("/new.txt")).is_none(), "no watching: the snapshot is stale but stable");

    index.invalidate();
    assert!(index.is_stale());
    index.rebuild().unwrap();
    assert!(!index.is_stale());
    assert_eq!(index.winner(&p("/new.txt")).unwrap().name(), "m");
}

#[test]
fn iteration_is_sorted_by_folded_path() {
    let contents = ["/A/", "/a/", "/B/", "/B/f", "/a/f", "/A/g", "/C"].map(|p| (p, "x"));
    let index = MergedIndex::build(vec![mem_layer("m", &contents)]).unwrap();
    let folded: Vec<&str> = index.iter().map(|e| e.folded_path()).collect();
    let mut sorted = folded.clone();
    sorted.sort_unstable();
    assert_eq!(folded, sorted);
    // Folded nodes: dirs /a /b, files /a/f /a/g /b/f /c — six unique entries.
    assert_eq!(index.len(), 6);
}

#[test]
fn layers_keep_their_file_systems() {
    let index = MergedIndex::build(vec![mem_layer("m", &[("/f", "x")])]).unwrap();
    assert_eq!(index.layers()[0].name(), "m");
    assert!(index.layers()[0].fs().exists(&p("/f")));
}

#[test]
fn entry_view_exposes_winner_and_losers() {
    let index = MergedIndex::build(vec![mem_layer("hi", &[("/f", "hi")]), mem_layer("lo", &[("/f", "lo")])]).unwrap();
    let entry = index.iter().find(|e| e.folded_path() == "/f").unwrap();
    assert_eq!(entry.winner().name(), "hi");
    assert_eq!(entry.losers().iter().map(openvfs_mods::Provider::name).collect::<Vec<_>>(), ["lo"]);
    assert_eq!(entry.providers().len(), 2);
}
