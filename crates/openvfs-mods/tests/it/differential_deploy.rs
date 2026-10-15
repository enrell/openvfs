//! The deployed-vs-virtual differential: **the game directory after
//! `deploy` must equal the merged `Vfs` view**, folded and byte-for-byte.
//!
//! The index and the deployment work in folded-path space (`/Textures` and
//! `/textures` are one path); the served overlay merges by exact spelling.
//! The comparison therefore folds both snapshots — the virtual side keeps
//! each folded path's *index winner's* spelling — and they must agree on
//! every node and every byte.
//!
//! The generated mod sets are *coherent* (see `common::scenario`): no two
//! layers disagree whether a folded path is a file or a directory. Outside
//! that space the virtual and deployed views legitimately differ — the
//! index docs call it out — and `priority_prop` pins the rule there.
//!
//! The roundtrip is pinned too: `undeploy` returns the game dir to its
//! pre-deploy tree byte-for-byte, foreign files included.
//!
//! Knobs:
//! * `OPENVFS_MODS_DIFF_CASES` — random scenarios (default 8);
//! * `OPENVFS_MODS_DIFF_SEED`  — replay one scenario's seed.

use openvfs::VPath;
use openvfs_mods::deploy::{DeployMode, Deployer, DeploymentPlan, PlanOptions};
use openvfs_mods::{BuiltProfile, MergedIndex, ModEntry, Profile};

use crate::common::{
    Entry, Rng, TempDir, Tree, assert_trees_eq, env_knob, folded, host_tree, populate_host, scenario, snapshot,
};

fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("OPENVFS_MODS_DIFF_SEED") {
        return vec![seed.parse().expect("OPENVFS_MODS_DIFF_SEED must be an integer")];
    }
    (0..env_knob("OPENVFS_MODS_DIFF_CASES", 8)).map(|i| 0xD1FF_0000 + i).collect()
}

/// The merged virtual view in folded space: `snapshot` of the overlay with
/// every path folded. Folded collisions are resolved to the index winner's
/// spelling — exactly what `deploy` materializes. Paths the index does not
/// know (foreign files that landed after it was built) pass through.
fn folded_virtual(built: &BuiltProfile, index: &MergedIndex) -> Tree {
    let snap = snapshot(&**built.overlay());
    let mut out = Tree::new();
    for (path, entry) in snap {
        let folded_key = path.to_lowercase();
        match out.entry(folded_key.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::btree_map::Entry::Occupied(existing) if *existing.get() == entry => {}
            std::collections::btree_map::Entry::Occupied(_) => {
                let winner = index
                    .real_path(&VPath::new(&folded_key).unwrap())
                    .unwrap_or_else(|| panic!("{path} is in the merged view but not the index"));
                if path == winner.as_str() {
                    out.insert(folded_key, entry);
                }
            }
        }
    }
    out
}

/// One randomized end-to-end case: build the mod set, snapshot the merged
/// view, deploy, compare folded trees; then undeploy and compare the
/// restored tree byte-for-byte with the original.
fn run_case(seed: u64) {
    let mut rng = Rng::new(seed);
    let root = TempDir::new("diff-deploy");

    // A shared namespace the layers draw from independently.
    let (n_dirs, n_files) = (3 + rng.below(5), 6 + rng.below(10));
    let ns = scenario::namespace(&mut rng, n_dirs, n_files);

    // The base game and 1–4 mods.
    let game_dir = root.path().join("game");
    let game_tree = scenario::layer_tree(&mut rng, &ns, "base", 85, 25);
    populate_host(&game_dir, &game_tree);

    let mod_count = 1 + rng.below(4);
    let mut profile = Profile::new(format!("diff{seed}"), &game_dir, root.path().join("overwrite"));
    for m in 0..mod_count {
        let dir = root.path().join("mods").join(format!("mod{m}"));
        let tree = scenario::layer_tree(&mut rng, &ns, &format!("mod{m}"), 55, 12);
        populate_host(&dir, &tree);
        profile.add_mod(ModEntry {
            name: format!("mod{m}"),
            source_dir: dir,
            // Disabled mods still sit in the list — they contribute nothing.
            enabled: !(rng.chance(20) && mod_count > 1),
        });
    }

    let deployer = Deployer::new(root.path().join("state"));
    let built = profile.build().unwrap();
    let index = built.index().unwrap();

    // Foreign files: dropped into the game dir *after* the index was built —
    // the deployment must keep them, and they are part of both trees.
    let mut foreign = Tree::new();
    if rng.chance(45) {
        for i in 0..=rng.below(2) {
            let path = format!("/user-{i}.ini");
            let bytes = format!("user write {i}").into_bytes();
            std::fs::write(game_dir.join(format!("user-{i}.ini")), &bytes).unwrap();
            foreign.insert(path, Entry::File(bytes));
        }
    }

    let options = if rng.chance(30) { PlanOptions::new().mode(DeployMode::Copy) } else { PlanOptions::new() };
    let plan: DeploymentPlan = deployer.plan_index(&built, &game_dir, &index, &options).unwrap();
    assert!(plan.unreachable.is_empty(), "a coherent mod set has no unreachable files: {plan:?}");

    // The expected tree: the merged virtual view, folded — taken after the
    // foreign files landed (the base layer reads live).
    let expected = folded_virtual(&built, &index);
    let before = host_tree(&game_dir);

    let report = deployer.deploy(&plan).unwrap();
    let _ = report;
    // The only drift verify may report: the injected foreign files (verify
    // names every file outside the manifest, kept or not).
    let drift = deployer.verify().unwrap();
    let mut found: Vec<&str> = drift.iter().map(|m| m.path.as_str()).collect();
    found.sort_unstable();
    let mut wanted: Vec<String> = foreign.keys().cloned().collect();
    wanted.sort_unstable();
    assert_eq!(
        found,
        wanted.iter().map(String::as_str).collect::<Vec<_>>(),
        "seed {seed}: drift is exactly the foreign set"
    );
    assert!(drift.iter().all(|m| m.kind == openvfs_mods::MismatchKind::Foreign), "seed {seed}: {drift:?}");
    let on_disk = folded(&host_tree(&game_dir));
    assert_trees_eq(&expected, &on_disk, &format!("seed {seed}: deployed vs merged view"));

    // Undeploy returns the game dir byte-for-byte — foreign files survive.
    deployer.undeploy(Some(&profile.overwrite_dir)).unwrap();
    assert_trees_eq(&before, &host_tree(&game_dir), &format!("seed {seed}: undeploy roundtrip"));
}

#[test]
fn deployed_tree_equals_merged_virtual_view() {
    for seed in seeds() {
        run_case(seed);
    }
}
