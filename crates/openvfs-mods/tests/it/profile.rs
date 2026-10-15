//! `Profile`: ordering, enable/disable, JSON, and the end-to-end build.

use std::path::Path;

use openvfs::prelude::*;

use crate::common::{TempDir, populate_host, tree};
use openvfs_mods::{ModEntry, Profile};

fn mod_entry(name: &str, source_dir: &Path, enabled: bool) -> ModEntry {
    ModEntry { name: name.to_owned(), source_dir: source_dir.to_path_buf(), enabled }
}

/// A profile scaffold on disk: `game/`, `mods/` and `overwrite/` under one
/// temp root. Returns the profile and the root (which must outlive it).
fn scaffold(profile_name: &str) -> (TempDir, Profile) {
    let root = TempDir::new("profile");
    let profile = Profile::new(profile_name, root.path().join("game"), root.path().join("overwrite"));
    (root, profile)
}

fn write_mod(root: &TempDir, name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = root.path().join("mods").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    populate_host(&dir, &tree(files));
    dir
}

#[test]
fn ordering_and_enable_disable() {
    let (root, mut profile) = scaffold("ops");
    let (a, b, c) = (write_mod(&root, "a", &[]), write_mod(&root, "b", &[]), write_mod(&root, "c", &[]));
    profile.add_mod(mod_entry("a", &a, true));
    profile.add_mod(mod_entry("c", &c, true));
    profile.insert_mod(1, mod_entry("b", &b, true)); // between a and c
    assert_eq!(profile.mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);

    profile.move_mod(2, 0); // c to the top — highest priority now
    assert_eq!(profile.mods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["c", "a", "b"]);

    assert!(profile.set_enabled("a", false));
    assert!(!profile.set_enabled("ghost", true));
    assert_eq!(profile.enabled_mods().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["c", "b"]);

    assert_eq!(profile.remove_mod("b").unwrap().source_dir, b);
    assert_eq!(profile.mods.len(), 2);
}

#[test]
fn json_round_trip() {
    let (root, mut profile) = scaffold("säve-日本語");
    let a = write_mod(&root, "Alpha", &[]);
    profile.add_mod(mod_entry("Alpha HD", &a, false));
    let json = profile.to_json().unwrap();

    let loaded = Profile::from_json(&json).unwrap();
    assert_eq!(loaded.name, "säve-日本語");
    assert_eq!(loaded.mods.len(), 1);
    assert_eq!(loaded.mods[0].name, "Alpha HD");
    assert!(!loaded.mods[0].enabled, "enabled survives the round trip");

    // Via the file too.
    let file = root.path().join("profile.json");
    profile.save(&file).unwrap();
    let reloaded = Profile::load(&file).unwrap();
    assert_eq!(reloaded.mods[0].source_dir, a);
    assert!(Profile::from_json("{ not json").is_err());
}

#[test]
fn build_merges_mods_over_the_game() {
    let (root, mut profile) = scaffold("e2e");
    populate_host(
        &profile.game_dir,
        &tree(&[("/data/", ""), ("/data/game.txt", "base"), ("/data/shared.txt", "base shared")]),
    );
    let top = write_mod(&root, "top", &[("/data/", ""), ("/data/shared.txt", "mod wins"), ("/data/mod.txt", "m")]);
    let low = write_mod(&root, "low", &[("/data/", ""), ("/data/low.txt", "l"), ("/data/shared.txt", "low loses")]);
    let off = write_mod(&root, "off", &[("/data/", ""), ("/data/hidden.txt", "invisible")]);
    profile.add_mod(mod_entry("top", &top, true));
    profile.add_mod(mod_entry("low", &low, true));
    profile.add_mod(mod_entry("off", &off, false)); // disabled: contributes nothing

    let built = profile.build().unwrap();
    assert!(built.skipped().is_empty());
    assert_eq!(built.mods().iter().map(openvfs_mods::LoadedMod::name).collect::<Vec<_>>(), ["top", "low"]);

    let vfs = built.vfs();
    assert_eq!(vfs.read_to_string("/data/shared.txt").unwrap(), "mod wins", "mods[0] beats mods[1] beats game");
    assert_eq!(vfs.read_to_string("/data/game.txt").unwrap(), "base");
    assert_eq!(vfs.read_to_string("/data/low.txt").unwrap(), "l");
    assert!(!vfs.exists("/data/hidden.txt"), "disabled mods are absent from the view");
}

#[test]
fn overwrite_captures_writes_and_changes_reports_them() {
    let (root, mut profile) = scaffold("writes");
    populate_host(&profile.game_dir, &tree(&[("/data/", ""), ("/data/f.txt", "base")]));
    let m = write_mod(&root, "m", &[("/data/", ""), ("/data/m.txt", "m")]);
    profile.add_mod(mod_entry("m", &m, true));

    let built = profile.build().unwrap();
    let vfs = built.vfs();
    vfs.write("/data/f.txt", "the game rewrote it").unwrap();
    vfs.write("/data/new.txt", "a brand new file").unwrap();
    vfs.remove_file("/data/m.txt").unwrap();

    // Everything landed in the overwrite dir — mod and game are untouched.
    assert_eq!(std::fs::read_to_string(root.path().join("overwrite/data/new.txt")).unwrap(), "a brand new file");
    assert_eq!(std::fs::read_to_string(root.path().join("overwrite/data/f.txt")).unwrap(), "the game rewrote it");
    assert_eq!(std::fs::read_to_string(m.join("data/m.txt")).unwrap(), "m");
    assert_eq!(std::fs::read_to_string(profile.game_dir.join("data/f.txt")).unwrap(), "base");

    let changes = built.overlay().changes().unwrap().iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(changes.contains(&"A /data/new.txt".to_owned()), "{changes:?}");
    assert!(changes.contains(&"M /data/f.txt".to_owned()), "{changes:?}");
    assert!(changes.contains(&"D /data/m.txt".to_owned()), "{changes:?}");
}

#[test]
fn missing_enabled_mod_is_skipped_not_fatal() {
    let (root, mut profile) = scaffold("skip");
    populate_host(&profile.game_dir, &tree(&[("/f", "base")]));
    let present = write_mod(&root, "present", &[("/g", "g")]);
    profile.add_mod(mod_entry("ghost", &root.path().join("mods/ghost"), true));
    profile.add_mod(mod_entry("present", &present, true));

    let built = profile.build().unwrap();
    assert_eq!(built.skipped().len(), 1);
    assert_eq!(built.skipped()[0].name, "ghost");
    assert!(!built.skipped()[0].reason.is_empty());
    assert_eq!(built.mods().len(), 1, "only the present mod is layered");
    assert_eq!(built.vfs().read_to_string("/g").unwrap(), "g");
}

#[test]
fn missing_game_dir_is_a_hard_error() {
    let (_root, profile) = scaffold("nobase");
    let err = profile.build().unwrap_err();
    assert!(err.to_string().contains("base game"), "{err}");
}

#[test]
fn built_profile_builds_a_matching_index() {
    let (root, mut profile) = scaffold("indexed");
    populate_host(&profile.game_dir, &tree(&[("/data/", ""), ("/data/f.txt", "base")]));
    // The mod spells it differently — the index still finds the real file.
    let m = write_mod(&root, "m", &[("/DATA/", ""), ("/DATA/F.TXT", "mod")]);
    profile.add_mod(mod_entry("m", &m, true));

    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    let winner = index.winner(&VPath::new("/data/f.txt").unwrap()).unwrap();
    assert_eq!(winner.name(), "m");
    assert_eq!(index.real_path(&VPath::new("/data/f.txt").unwrap()).unwrap().as_str(), "/DATA/F.TXT");
    // The same merged view the overlay serves: mod shadows game, folded.
    assert_eq!(built.vfs().read_to_string("/DATA/F.TXT").unwrap(), "mod");
}

#[test]
fn vfs_shortcut_matches_build() {
    let (root, profile) = scaffold("shortcut");
    populate_host(&profile.game_dir, &tree(&[("/f", "x")]));
    assert_eq!(profile.vfs().unwrap().read_to_string("/f").unwrap(), "x");
    drop(root);
}
