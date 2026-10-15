//! `OverlayFs`: copy-on-write semantics, change tracking, and the algebraic
//! properties of `changes` / `commit` / `reset` over random histories.

use crate::common;

use std::sync::Arc;

use common::{Entry, Rng, TempDir, Tree, assert_trees_eq, env_knob, model, names, populate, snapshot, tree};
use openvfs::prelude::*;

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

/// A lower layer with a little project in it, and an overlay on top.
fn setup() -> (Vfs, Arc<OverlayFs>, Vfs) {
    let lower = Vfs::memory();
    populate(
        &**lower.backend(),
        &tree(&[
            ("/src/", ""),
            ("/src/util/", ""),
            ("/src/main.rs", "fn main() {}"),
            ("/src/util/mod.rs", "// util"),
            ("/README.md", "# demo"),
        ]),
    );
    let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), lower.backend().clone()));
    (Vfs::from(overlay.clone()), overlay, lower)
}

fn change_strings(overlay: &OverlayFs) -> Vec<String> {
    overlay.changes().unwrap().iter().map(ToString::to_string).collect()
}

// --- semantics ---------------------------------------------------------------

#[test]
fn reads_fall_through_and_listings_merge() {
    let (vfs, _, _) = setup();
    vfs.write("/NEW.md", "added").unwrap();
    assert_eq!(names(&**vfs.backend(), "/"), ["NEW.md", "README.md", "src"]);
    assert_eq!(vfs.read_to_string("/src/main.rs").unwrap(), "fn main() {}");
}

#[test]
fn writes_never_reach_the_lower_layer() {
    let (vfs, _, lower) = setup();
    let before = snapshot(&**lower.backend());
    vfs.write("/src/main.rs", "fn main() { changed(); }").unwrap();
    vfs.append("/README.md", "\nmore").unwrap();
    vfs.set_len("/src/util/mod.rs", 2).unwrap();
    vfs.rename("/src", "/code").unwrap();
    vfs.remove_dir_all("/code").unwrap();
    assert_trees_eq(&before, &snapshot(&**lower.backend()), "lower must be untouched");
}

#[test]
fn copy_up_preserves_existing_bytes() {
    let (vfs, _, _) = setup();
    vfs.append("/README.md", "\nmore").unwrap();
    assert_eq!(vfs.read_to_string("/README.md").unwrap(), "# demo\nmore");
    let mut f = vfs.open("/src/main.rs", OpenOptions::new().write(true)).unwrap();
    std::io::Write::write_all(&mut f, b"FN").unwrap();
    drop(f);
    assert_eq!(vfs.read_to_string("/src/main.rs").unwrap(), "FN main() {}");
}

#[test]
fn deletes_are_whiteouts() {
    let (vfs, overlay, lower) = setup();
    vfs.remove_file("/README.md").unwrap();
    assert!(!vfs.exists("/README.md"));
    assert!(lower.exists("/README.md"));
    assert_eq!(names(&**vfs.backend(), "/"), ["src"]);
    assert_eq!(overlay.whiteouts(), [p("/README.md")]);
    vfs.write("/README.md", "fresh").unwrap();
    assert_eq!(vfs.read_to_string("/README.md").unwrap(), "fresh", "re-creation must not resurrect old bytes");
}

#[test]
fn recreated_directory_is_opaque() {
    let (vfs, _, _) = setup();
    vfs.remove_dir_all("/src").unwrap();
    assert!(!vfs.exists("/src/main.rs"));
    vfs.create_dir("/src").unwrap();
    assert!(vfs.read_dir("/src").unwrap().is_empty(), "old lower children must stay hidden");
    vfs.write("/src/lib.rs", "").unwrap();
    assert_eq!(names(&**vfs.backend(), "/src"), ["lib.rs"]);
}

#[test]
fn whiteouts_are_compacted() {
    let (vfs, overlay, _) = setup();
    vfs.remove_file("/src/util/mod.rs").unwrap();
    vfs.remove_file("/src/main.rs").unwrap();
    assert_eq!(overlay.whiteouts().len(), 2);
    vfs.remove_dir_all("/src").unwrap();
    assert_eq!(overlay.whiteouts(), [p("/src")], "one whiteout covers the subtree");
}

#[test]
fn rename_of_lower_directory_copies_up() {
    let (vfs, overlay, lower) = setup();
    vfs.rename("/src", "/code").unwrap();
    assert!(!vfs.exists("/src"));
    assert_eq!(vfs.read_to_string("/code/util/mod.rs").unwrap(), "// util");
    assert!(lower.exists("/src/main.rs"));
    assert_eq!(
        change_strings(&overlay),
        ["A /code/", "A /code/main.rs", "A /code/util/", "A /code/util/mod.rs", "D /src/"]
    );
}

#[test]
fn metadata_of_lower_files_is_writable_through_the_overlay() {
    let lower = Vfs::memory();
    lower.write("/ro.txt", "x").unwrap();
    let overlay = Vfs::new(OverlayFs::new(MemoryFs::new(), ReadOnlyFs::new(lower.backend().clone())));
    assert!(!overlay.metadata("/ro.txt").unwrap().read_only);
    overlay.write("/ro.txt", "y").unwrap();
    assert_eq!(overlay.read_to_string("/ro.txt").unwrap(), "y");
    assert_eq!(lower.read_to_string("/ro.txt").unwrap(), "x");

    let frozen = Vfs::new(OverlayFs::new(ReadOnlyFs::new(MemoryFs::new()), lower.backend().clone()));
    assert!(frozen.metadata("/ro.txt").unwrap().read_only, "a read-only upper makes everything read-only");
    assert!(frozen.backend().is_read_only());
    assert!(matches!(frozen.write("/ro.txt", "z"), Err(VfsError::ReadOnly(_))));
}

#[test]
fn readers_opened_before_copy_up_keep_the_lower_bytes() {
    let (vfs, _, _) = setup();
    let mut old = vfs.open_read("/README.md").unwrap();
    vfs.write("/README.md", "rewritten").unwrap();
    let mut seen = String::new();
    std::io::Read::read_to_string(&mut old, &mut seen).unwrap();
    assert_eq!(seen, "# demo", "an open lower handle is a stable snapshot");
}

#[test]
fn multiple_lowers_respect_priority() {
    let base = Vfs::memory();
    populate(
        &**base.backend(),
        &tree(&[("/textures/", ""), ("/textures/grass.png", "base grass"), ("/textures/stone.png", "base stone")]),
    );
    let hd_mod = Vfs::memory();
    populate(
        &**hd_mod.backend(),
        &tree(&[("/textures/", ""), ("/textures/grass.png", "HD"), ("/textures/sky.png", "sky")]),
    );

    let game = Vfs::new(OverlayFs::with_layers(
        Arc::new(MemoryFs::new()),
        vec![hd_mod.backend().clone(), base.backend().clone()],
    ));
    assert_eq!(game.read_to_string("/textures/grass.png").unwrap(), "HD");
    assert_eq!(game.read_to_string("/textures/stone.png").unwrap(), "base stone");
    assert_eq!(names(&**game.backend(), "/textures"), ["grass.png", "sky.png", "stone.png"]);
    game.remove_file("/textures/grass.png").unwrap();
    assert!(!game.exists("/textures/grass.png"), "a whiteout hides the path in every lower");
}

// --- change tracking -----------------------------------------------------------

#[test]
fn changes_report_exact_differences_in_replay_order() {
    let (vfs, overlay, _) = setup();
    assert!(overlay.changes().unwrap().is_empty());
    vfs.write("/src/main.rs", "fn main() { v2(); }").unwrap();
    vfs.remove_file("/src/util/mod.rs").unwrap();
    vfs.create_dir_all("/docs/api").unwrap();
    vfs.write("/docs/api/index.md", "hi").unwrap();
    assert_eq!(
        overlay.changes().unwrap(),
        [
            Change::Added { path: p("/docs"), kind: FileType::Dir },
            Change::Added { path: p("/docs/api"), kind: FileType::Dir },
            Change::Added { path: p("/docs/api/index.md"), kind: FileType::File },
            Change::Modified { path: p("/src/main.rs") },
            Change::Removed { path: p("/src/util/mod.rs"), kind: FileType::File },
        ]
    );
}

#[test]
fn rewriting_identical_bytes_is_not_a_change() {
    let (vfs, overlay, _) = setup();
    vfs.write("/README.md", "# demo").unwrap();
    vfs.write("/src/main.rs", "something else").unwrap();
    vfs.write("/src/main.rs", "fn main() {}").unwrap(); // changed back
    assert!(overlay.is_dirty().unwrap());
    assert!(overlay.changes().unwrap().is_empty());
}

#[test]
fn same_length_edits_are_detected() {
    let (vfs, overlay, _) = setup();
    vfs.write("/README.md", "# DEMO").unwrap();
    assert_eq!(change_strings(&overlay), ["M /README.md"]);
}

#[test]
fn large_file_modifications_are_detected_by_streaming_compare() {
    let lower = Vfs::memory();
    let big: Vec<u8> = (0..1_000_000u32).map(|i| i.to_le_bytes()[0]).collect();
    lower.write("/big.bin", &big).unwrap();
    let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), lower.backend().clone()));
    let vfs = Vfs::from(overlay.clone());
    let mut changed = big.clone();
    changed[999_000] ^= 0xFF; // one byte, deep in the last compare chunk
    vfs.write("/big.bin", &changed).unwrap();
    assert_eq!(change_strings(&overlay), ["M /big.bin"]);
    vfs.write("/big.bin", &big).unwrap();
    assert!(overlay.changes().unwrap().is_empty());
}

#[test]
fn type_changes_are_remove_then_add() {
    let (vfs, overlay, _) = setup();
    vfs.remove_file("/README.md").unwrap();
    vfs.create_dir("/README.md").unwrap();
    vfs.write("/README.md/inner", "x").unwrap();
    assert_eq!(change_strings(&overlay), ["D /README.md", "A /README.md/", "A /README.md/inner"]);
}

#[test]
fn opaque_directory_reports_removed_lower_children() {
    let (vfs, overlay, _) = setup();
    vfs.remove_dir_all("/src").unwrap();
    vfs.create_dir("/src").unwrap();
    vfs.write("/src/main.rs", "fn main() {}").unwrap(); // same bytes as before, but in an opaque dir
    assert_eq!(change_strings(&overlay), ["D /src/util/"]);
}

// --- commit / reset --------------------------------------------------------------

#[test]
fn commit_replays_onto_disk_and_reset_cleans_up() {
    let dir = TempDir::new("overlay-commit");
    std::fs::create_dir_all(dir.path().join("keep")).unwrap();
    std::fs::write(dir.path().join("keep/a.txt"), "old").unwrap();
    std::fs::write(dir.path().join("gone.txt"), "bye").unwrap();

    let disk = Arc::new(PhysicalFs::new(dir.path()).unwrap());
    let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), disk.clone()));
    let vfs = Vfs::from(overlay.clone());

    vfs.write("/keep/a.txt", "new").unwrap();
    vfs.remove_file("/gone.txt").unwrap();
    vfs.create_dir_all("/made/here").unwrap();
    vfs.write("/made/here/b.txt", "b").unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("keep/a.txt")).unwrap(), "old");

    let applied = overlay.commit(&*disk).unwrap();
    assert_eq!(applied.len(), 5);
    overlay.reset().unwrap();

    assert_eq!(std::fs::read_to_string(dir.path().join("keep/a.txt")).unwrap(), "new");
    assert!(!dir.path().join("gone.txt").exists());
    assert_eq!(std::fs::read_to_string(dir.path().join("made/here/b.txt")).unwrap(), "b");
    assert!(overlay.changes().unwrap().is_empty());
    assert!(!overlay.is_dirty().unwrap());
    assert!(overlay.whiteouts().is_empty());
    assert_eq!(vfs.read_to_string("/keep/a.txt").unwrap(), "new");
}

#[test]
fn commit_onto_read_only_target_fails_and_keeps_changes() {
    let (vfs, overlay, lower) = setup();
    vfs.write("/x", "1").unwrap();
    let target = ReadOnlyFs::new(lower.backend().clone());
    assert!(matches!(overlay.commit(&target), Err(VfsError::ReadOnly(_))));
    assert_eq!(change_strings(&overlay), ["A /x"]);
}

#[test]
fn commit_twice_is_idempotent() {
    let (vfs, overlay, lower) = setup();
    vfs.write("/src/main.rs", "v2").unwrap();
    vfs.remove_dir_all("/src/util").unwrap();
    let copy = MemoryFs::new();
    populate(&copy, &snapshot(&**lower.backend()));
    overlay.commit(&copy).unwrap();
    overlay.commit(&copy).unwrap();
    assert_trees_eq(&snapshot(&**vfs.backend()), &snapshot(&copy), "double commit");
}

// --- properties over random histories ------------------------------------------------

fn random_overlay_case(seed: u64, lower: Arc<dyn FileSystem>) -> (Arc<OverlayFs>, Tree) {
    let mut rng = Rng::new(seed);
    let initial = model::random_tree(&mut rng, 30);
    populate(&*lower, &initial);
    let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), lower));
    let vfs = Vfs::from(overlay.clone());
    for _ in 0..80 {
        let _ = model::apply(&vfs, &model::random_op(&mut rng));
    }
    (overlay, initial)
}

#[test]
fn property_changes_describe_the_merged_view_exactly() {
    for seed in 0..env_knob("OPENVFS_PROP_CASES", 150) {
        let lower: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
        let (overlay, initial) = random_overlay_case(seed, lower.clone());
        let merged = snapshot(&*overlay);
        let ctx = |what: &str| format!("{what}, seed {seed}");

        // 1. The lower layer is never written.
        assert_trees_eq(&initial, &snapshot(&*lower), &ctx("lower modified"));

        // 2. Every reported change is real and correctly classified.
        for change in overlay.changes().unwrap() {
            let key = change.path().to_string();
            match &change {
                Change::Added { kind, .. } => {
                    let now = merged.get(&key).unwrap_or_else(|| panic!("{}: added path missing", ctx(&key)));
                    assert_eq!(*kind == FileType::Dir, *now == Entry::Dir, "{}", ctx(&key));
                }
                Change::Modified { .. } => {
                    let (Some(Entry::File(old)), Some(Entry::File(new))) = (initial.get(&key), merged.get(&key)) else {
                        panic!("{}: modified must be a file on both sides", ctx(&key));
                    };
                    assert_ne!(old, new, "{}: modified but identical", ctx(&key));
                }
                Change::Removed { kind, .. } => {
                    let before =
                        initial.get(&key).unwrap_or_else(|| panic!("{}: removed path never existed", ctx(&key)));
                    assert_eq!(*kind == FileType::Dir, *before == Entry::Dir, "{}", ctx(&key));
                }
            }
        }

        // 3. Replaying the changes onto a copy of the lower reproduces the merged view.
        let copy = MemoryFs::new();
        populate(&copy, &initial);
        overlay.commit(&copy).unwrap();
        assert_trees_eq(&merged, &snapshot(&copy), &ctx("commit replay"));

        // 4. Reset restores the lower view exactly.
        overlay.reset().unwrap();
        assert_trees_eq(&initial, &snapshot(&*overlay), &ctx("after reset"));
        assert!(overlay.changes().unwrap().is_empty() && !overlay.is_dirty().unwrap(), "{}", ctx("clean"));
    }
}

#[test]
fn property_commit_to_disk_then_reset_is_a_no_op_for_the_view() {
    for seed in 0..env_knob("OPENVFS_PROP_CASES_DISK", 25) {
        let dir = TempDir::new("prop-disk");
        let disk: Arc<dyn FileSystem> = Arc::new(PhysicalFs::new(dir.path()).unwrap());
        let (overlay, _) = random_overlay_case(1000 + seed, disk.clone());
        let merged = snapshot(&*overlay);
        overlay.commit(&*disk).unwrap();
        assert!(overlay.changes().unwrap().is_empty(), "after commit the lower already matches, seed {seed}");
        overlay.reset().unwrap();
        assert_trees_eq(&merged, &snapshot(&*overlay), &format!("view changed by commit+reset, seed {seed}"));
        assert_trees_eq(&merged, &snapshot(&*disk), &format!("disk differs, seed {seed}"));
    }
}
