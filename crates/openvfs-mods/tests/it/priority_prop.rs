//! Priority-ordering properties: for every folded path provided by more
//! than one enabled layer, the **highest-priority provider wins** — in the
//! index's winner, in the bytes the overlay serves, and in the file the
//! deployment plants in the game directory. Three views, one winner.
//!
//! File contents are `"{mod}:{folded-path}"` (see `common::scenario`), so a
//! served byte string always names its provider — a shadowed winner is
//! impossible to miss.
//!
//! The file-vs-directory clashes are pinned separately at the bottom:
//! coherent sets can't express them, so the documented winner rule gets
//! its own deterministic cases.
//!
//! Knobs:
//! * `OPENVFS_MODS_PROP_CASES` — random load orders (default 16);
//! * `OPENVFS_MODS_PROP_SEED`  — replay one.

use std::path::{Path, PathBuf};

use openvfs_mods::deploy::Manifest;
use openvfs_mods::deploy::{Deployer, PlanOptions};
use openvfs_mods::{ModEntry, Profile};

use crate::common::{Rng, TempDir, env_knob, populate_host, scenario, tree};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("OPENVFS_MODS_PROP_SEED") {
        return vec![seed.parse().expect("OPENVFS_MODS_PROP_SEED must be an integer")];
    }
    (0..env_knob("OPENVFS_MODS_PROP_CASES", 16)).map(|i| 0x9210_0000 + i).collect()
}

/// `game_dir.join` for a manifest's `/`-rooted rel path.
fn host(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for component in rel.trim_start_matches('/').split('/') {
        if !component.is_empty() {
            out.push(component);
        }
    }
    out
}

/// The entry names in `dir`, sorted — the case-insensitive host's own view.
fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_str().unwrap().to_owned()).collect();
    names.sort();
    names
}

/// The shared property, checked at every contested path of a seeded load
/// order. Returns the number of contested files examined.
fn run_case(seed: u64) -> usize {
    let mut rng = Rng::new(seed);
    let root = TempDir::new("priority");

    let (n_dirs, n_files) = (3 + rng.below(4), 6 + rng.below(9));
    let ns = scenario::namespace(&mut rng, n_dirs, n_files);
    let game_dir = root.path().join("game");
    populate_host(&game_dir, &scenario::layer_tree(&mut rng, &ns, "base game", 85, 20));

    let mod_count = 2 + rng.below(4);
    let mut profile = Profile::new(format!("prop{seed}"), &game_dir, root.path().join("overwrite"));
    for m in 0..mod_count {
        let dir = root.path().join("mods").join(format!("mod{m}"));
        populate_host(&dir, &scenario::layer_tree(&mut rng, &ns, &format!("mod{m}"), 55, 10));
        profile.add_mod(ModEntry {
            name: format!("mod{m}"),
            source_dir: dir,
            enabled: !rng.chance(25), // ~1 in 4 mods sits disabled in the list
        });
    }

    let deployer = Deployer::new(root.path().join("state"));
    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    let vfs = built.vfs();
    deployer.deploy(&deployer.plan_index(&built, &game_dir, &index, &PlanOptions::new()).unwrap()).unwrap();
    let manifest = Manifest::load(deployer.state_dir()).unwrap().unwrap();

    let mut contested = 0usize;
    for entry in index.iter() {
        if entry.providers().len() < 2 || !entry.winner().is_file() {
            continue;
        }
        contested += 1;
        let winner = entry.winner();
        let expected = scenario::content(winner.name(), entry.folded_path());

        // 1. The index: providers are priority-ordered, the winner is first.
        assert_eq!(winner.name(), entry.providers()[0].name(), "seed {seed}: {}", entry.folded_path());

        // 2. The served view: the overlay reads the winner's bytes.
        let served = vfs.read(winner.path().as_str()).unwrap();
        assert_eq!(served, expected, "seed {seed}: overlay serves the loser at {}", entry.folded_path());

        // 3. The deployment: the game-dir file holds the winner's bytes.
        let record = manifest
            .entry(entry.folded_path())
            .unwrap_or_else(|| panic!("seed {seed}: contested {} missing from the manifest", entry.folded_path()));
        let on_disk = std::fs::read(host(&game_dir, &record.path)).unwrap();
        assert_eq!(on_disk, expected, "seed {seed}: deployed {} holds the loser's bytes", entry.folded_path());

        // …and the shadowed providers really were different — the property
        // has teeth, not a coincidence of identical contents.
        for loser in entry.losers() {
            assert_ne!(
                scenario::content(loser.name(), entry.folded_path()),
                expected,
                "seed {seed}: indistinguishable providers at {}",
                entry.folded_path()
            );
        }
    }
    contested
}

#[test]
fn the_highest_priority_provider_wins_everywhere() {
    let mut total = 0usize;
    for seed in seeds() {
        total += run_case(seed);
    }
    assert!(total > 0, "the property ran without ever contesting a file");
}

// --- directory-vs-file clashes: the documented winner rule -------------------

/// High-priority mod serves a **file** at `/x` where the game itself has a
/// **directory** `/x/` with content (and a lower mod provides one too). The
/// file wins the folded path — the directory is displaced into backup,
/// contents and all, and undeploy brings it back.
#[test]
fn file_over_directory_same_spelling() {
    let root = TempDir::new("prio-clash");
    let game_dir = root.path().join("game");
    populate_host(&game_dir, &tree(&[("/g.txt", "g"), ("/x/", ""), ("/x/old.txt", "dir content")]));
    let high = root.path().join("mods/high");
    populate_host(&high, &tree(&[("/x", "file wins")]));
    let low = root.path().join("mods/low");
    populate_host(&low, &tree(&[("/x/", ""), ("/x/b", "low dir file")]));

    let mut profile = Profile::new("clash", &game_dir, root.path().join("ov"));
    for (name, dir) in [("high", &high), ("low", &low)] {
        profile.add_mod(ModEntry { name: name.into(), source_dir: dir.clone(), enabled: true });
    }
    let deployer = Deployer::new(root.path().join("state"));
    let built = profile.build().unwrap();
    let index = built.index().unwrap();

    // The folded `/x` winner is the file; `/x/b` is the low dir's own.
    let winner = index.winner(&openvfs::VPath::new("/x").unwrap()).unwrap();
    assert!(winner.is_file() && winner.name() == "high");

    let plan = deployer.plan_index(&built, &game_dir, &index, &PlanOptions::new()).unwrap();
    assert!(plan.unreachable.contains(&"/x/b".to_owned()), "{plan:?}");
    deployer.deploy(&plan).unwrap();

    assert_eq!(std::fs::read_to_string(game_dir.join("x")).unwrap(), "file wins");
    assert_eq!(dir_names(&game_dir), ["g.txt", "x"], "only the file stands at /x");
    // The displaced directory is preserved in backup — undeploy-safe.
    assert!(deployer.verify().unwrap().is_empty(), "{:?}", deployer.verify().unwrap());
    deployer.undeploy(Some(&profile.overwrite_dir)).unwrap();
    assert!(game_dir.join("x").is_dir(), "the dir returns on undeploy");
    assert_eq!(std::fs::read_to_string(game_dir.join("x/old.txt")).unwrap(), "dir content");
}

/// Same clash with **mismatched casing**: the file wins under its own
/// spelling and the loser dir's content is unreachable in the deployment.
#[test]
fn file_over_directory_different_spelling() {
    let root = TempDir::new("prio-clash-case");
    let game_dir = root.path().join("game");
    populate_host(&game_dir, &tree(&[("/g.txt", "g")]));
    let high = root.path().join("mods/high");
    populate_host(&high, &tree(&[("/X", "file wins")]));
    let low = root.path().join("mods/low");
    populate_host(&low, &tree(&[("/x/", ""), ("/x/b", "dir content")]));

    let mut profile = Profile::new("clash", &game_dir, root.path().join("ov"));
    for (name, dir) in [("high", &high), ("low", &low)] {
        profile.add_mod(ModEntry { name: name.into(), source_dir: dir.clone(), enabled: true });
    }
    let deployer = Deployer::new(root.path().join("state"));
    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    let winner = index.winner(&openvfs::VPath::new("/x").unwrap()).unwrap();
    assert!(winner.is_file() && winner.name() == "high");

    deployer.deploy(&deployer.plan_index(&built, &game_dir, &index, &PlanOptions::new()).unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(game_dir.join("X")).unwrap(), "file wins");
    // The host is case-insensitive — `x` resolves to the deployed `X` — so
    // check the actual listing: only the file's name stands, no dir.
    assert_eq!(dir_names(&game_dir), ["X", "g.txt"]);
}

/// The mirror: a **directory** at the higher priority wins the folded
/// path; the lower mod's file at that path is shadowed entirely. And when
/// the directory wins over a *file* standing in the game dir, the file is
/// displaced into backup — the manifest has no file entry to hang it on,
/// so undeploy's leftover-backup sweep puts it back.
#[test]
fn directory_over_file() {
    let root = TempDir::new("prio-clash-dir");
    let game_dir = root.path().join("game");
    populate_host(&game_dir, &tree(&[("/g.txt", "g"), ("/x", "game file")]));
    let high = root.path().join("mods/high");
    populate_host(&high, &tree(&[("/x/", ""), ("/x/a", "dir wins")]));
    let low = root.path().join("mods/low");
    populate_host(&low, &tree(&[("/x", "shadowed file")]));

    let mut profile = Profile::new("clash", &game_dir, root.path().join("ov"));
    for (name, dir) in [("high", &high), ("low", &low)] {
        profile.add_mod(ModEntry { name: name.into(), source_dir: dir.clone(), enabled: true });
    }
    let deployer = Deployer::new(root.path().join("state"));
    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    let winner = index.winner(&openvfs::VPath::new("/x").unwrap()).unwrap();
    assert!(winner.is_dir() && winner.name() == "high");

    deployer.deploy(&deployer.plan_index(&built, &game_dir, &index, &PlanOptions::new()).unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(game_dir.join("x/a")).unwrap(), "dir wins");
    assert!(game_dir.join("x").is_dir(), "the created dir displaced the game file");
    assert!(deployer.verify().unwrap().is_empty());
    deployer.undeploy(Some(&profile.overwrite_dir)).unwrap();
    assert_eq!(std::fs::read_to_string(game_dir.join("x")).unwrap(), "game file");
}
