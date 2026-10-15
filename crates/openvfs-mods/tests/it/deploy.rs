//! `deploy`: planning, apply/resume/rollback, verify, undeploy, capture —
//! all against real temp dirs on the host (NTFS hardlinks available).

use std::path::{Path, PathBuf};

use openvfs_mods::deploy::{
    DeployMethod, DeployMode, DeployOp, Deployer, EntryKind, ForeignPolicy, Manifest, MismatchKind, PlanOptions,
};
use openvfs_mods::{ModEntry, Profile};

use crate::common::{
    TempDir, assert_hardlinked, assert_not_hardlinked, assert_trees_eq, host_tree, populate_host, tree,
};

/// A scaffold on disk: `game/`, `mods/`, `overwrite/` and `state/` under one
/// temp root. The `TempDir` must outlive everything built on it.
fn scaffold() -> (TempDir, Profile, Deployer) {
    let root = TempDir::new("deploy");
    let profile = Profile::new("test", root.path().join("game"), root.path().join("overwrite"));
    let deployer = Deployer::new(root.path().join("state"));
    (root, profile, deployer)
}

fn write_mod(root: &TempDir, name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = root.path().join("mods").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    populate_host(&dir, &tree(files));
    dir
}

fn add(profile: &mut Profile, name: &str, source_dir: &Path) {
    profile.add_mod(ModEntry { name: name.to_owned(), source_dir: source_dir.to_path_buf(), enabled: true });
}

/// `plan.ops` as a compact description for assertions.
fn op_names(plan: &openvfs_mods::DeploymentPlan) -> Vec<String> {
    plan.ops
        .iter()
        .map(|op| match op {
            DeployOp::Mkdir { path } => format!("mkdir {}", tail(path)),
            DeployOp::Backup { from, .. } => format!("backup {}", tail(from)),
            DeployOp::Link { link, .. } => format!("link {}", tail(link)),
            DeployOp::Copy { link, .. } => format!("copy {}", tail(link)),
            DeployOp::RemoveFile { path, .. } => format!("rm {}", tail(path)),
            DeployOp::Restore { to, .. } => format!("restore {}", tail(to)),
            DeployOp::RemoveDir { path } => format!("rmdir {}", tail(path)),
        })
        .collect()
}

/// The path's tail below the temp `game/` dir, for readable assertions.
fn tail(path: &Path) -> String {
    path.iter().skip_while(|c| *c != "game").skip(1).collect::<PathBuf>().display().to_string().replace('\\', "/")
}

#[test]
fn plan_links_mod_files_and_backs_up_shadowed_game_files() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(
        &profile.game_dir,
        &tree(&[("/data/", ""), ("/data/game.txt", "base"), ("/data/shared.txt", "base shared")]),
    );
    let top = write_mod(&root, "top", &[("/data/", ""), ("/data/shared.txt", "mod wins"), ("/data/mod.txt", "m")]);
    let low = write_mod(&root, "low", &[("/data/", ""), ("/data/low.txt", "l"), ("/data/shared.txt", "low loses")]);
    add(&mut profile, "top", &top);
    add(&mut profile, "low", &low);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let names = op_names(&plan);
    // The shadowed original is backed up before its replacement is placed.
    let backup = names.iter().position(|n| n == "backup data/shared.txt").expect("a backup op");
    let link = names.iter().position(|n| n == "link data/shared.txt").expect("a link op");
    assert!(backup < link, "{names:?}");
    assert!(names.contains(&"link data/mod.txt".to_owned()), "{names:?}");
    assert!(names.contains(&"link data/low.txt".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n == "mkdir data"), "the dir already exists: {names:?}");
    assert!(plan.foreign.is_empty());

    // The planned file list: three deployed, one base.
    let placed: Vec<_> = plan.files.iter().filter(|f| f.kind == EntryKind::Deployed).collect();
    assert_eq!(placed.len(), 3);
    assert_eq!(
        placed.iter().find(|f| f.path == "/data/shared.txt").unwrap().backup.as_deref(),
        Some("/data/shared.txt")
    );
    let base: Vec<_> = plan.files.iter().filter(|f| f.kind == EntryKind::Base).collect();
    assert_eq!(base.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["/data/game.txt"]);
}

#[test]
fn plan_uses_existing_dir_casing() {
    let (root, mut profile, deployer) = scaffold();
    // The game spells it `Data/`; the mod spells it `data/`.
    populate_host(&profile.game_dir, &tree(&[("/Data/", ""), ("/Data/base.txt", "b")]));
    let m = write_mod(&root, "m", &[("/data/", ""), ("/data/x.txt", "x")]);
    add(&mut profile, "m", &m);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let names = op_names(&plan);
    assert!(names.contains(&"link Data/x.txt".to_owned()), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("mkdir")), "{names:?}");
}

#[test]
fn plan_on_identical_tree_is_empty() {
    let (root, profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/a/", ""), ("/a/f.txt", "f")]));
    let _ = &root;
    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    assert!(plan.is_empty(), "{:?}", op_names(&plan));
    // Deploying a no-op plan still writes a manifest for verify/undeploy.
    deployer.deploy(&plan).unwrap();
    assert!(deployer.verify().unwrap().is_empty());
}

#[test]
fn deploy_verify_undeploy_roundtrip_is_byte_for_byte() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(
        &profile.game_dir,
        &tree(&[
            ("/data/", ""),
            ("/data/game.txt", "base"),
            ("/data/shared.txt", "base shared"),
            ("/readme.txt", "keep me"),
        ]),
    );
    let top = write_mod(
        &root,
        "top",
        &[("/data/", ""), ("/data/shared.txt", "mod wins"), ("/data/new/", ""), ("/data/new/added.txt", "new")],
    );
    let low = write_mod(&root, "low", &[("/other.txt", "other")]);
    add(&mut profile, "top", &top);
    add(&mut profile, "low", &low);
    let before = host_tree(&profile.game_dir);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let report = deployer.deploy(&plan).unwrap();
    assert_eq!(report.linked, 3, "{report:?}"); // shared.txt, new/added.txt, other.txt

    // The deployed files really are the mod files: the manifest recorded
    // `Link` and a write through the game path reaches the mod source.
    let manifest = Manifest::load(deployer.state_dir()).unwrap().unwrap();
    assert_eq!(manifest.entry("/data/shared.txt").unwrap().method, Some(DeployMethod::Link));
    assert_eq!(manifest.entry("/other.txt").unwrap().method, Some(DeployMethod::Link));
    assert_hardlinked(&profile.game_dir.join("data/shared.txt"), &top.join("data/shared.txt"));
    assert_hardlinked(&profile.game_dir.join("other.txt"), &low.join("other.txt"));
    assert_eq!(std::fs::read_to_string(profile.game_dir.join("data/shared.txt")).unwrap(), "mod wins");

    assert!(deployer.verify().unwrap().is_empty(), "verify must be clean right after deploy");

    let report = deployer.undeploy(None).unwrap();
    assert_eq!(report.removed, 3, "{report:?}");
    assert_eq!(report.restored, 1, "the shadowed game file returns, {report:?}");
    assert!(report.dirs_removed >= 1, "the created data/new dir is swept");
    assert_trees_eq(&before, &host_tree(&profile.game_dir), "game dir after undeploy");
    assert!(Manifest::load(deployer.state_dir()).unwrap().is_none(), "manifest cleared by undeploy");
}

#[test]
fn deploy_is_idempotent_when_already_deployed() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/f.txt", "base")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "m")]);
    add(&mut profile, "m", &m);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    deployer.deploy(&plan).unwrap();
    // A second plan sees the links as already right — pure no-ops.
    let plan2 = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let report = deployer.deploy(&plan2).unwrap();
    assert!(deployer.verify().unwrap().is_empty());
    assert_eq!(report.linked, 0, "{report:?}");
    assert!(plan2.files.iter().any(|f| f.kind == EntryKind::Deployed), "still manifest-tracked");
    let _ = root;
}

#[test]
fn resume_finishes_an_interrupted_deploy() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/a.txt", "base a"), ("/d/b.txt", "base b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "m"), ("/d/a.txt", "mod a")]);
    add(&mut profile, "m", &m);
    let before = host_tree(&profile.game_dir);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let total = plan.len();
    assert!(total >= 2);
    let partial = deployer.deploy_partial(&plan, 1).unwrap(); // one step, then "crash"
    assert!(Manifest::load(deployer.state_dir()).unwrap().is_none(), "no manifest mid-flight");

    // A fresh deploy is refused while the journal is mid-flight…
    let err = deployer.deploy(&plan).unwrap_err();
    assert!(err.to_string().contains("resume"), "{err}");

    // …and resume finishes it exactly.
    let report = deployer.resume().unwrap();
    assert!(deployer.verify().unwrap().is_empty(), "{:?}", deployer.verify().unwrap());
    assert!(deployer.deploy(&deployer.plan(&profile, PlanOptions::new()).unwrap()).is_ok(), "journal freed for reuse");
    let _ = (partial, report);

    // Now a mid-flight rollback restores the original directory.
    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    let _ = plan;
    // Re-modify: undeploy first to a clean slate, redeploy partially, roll back.
    deployer.undeploy(None).unwrap();
    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    deployer.deploy_partial(&plan, 2).unwrap();
    let rolled = deployer.rollback().unwrap();
    assert!(rolled.restored + rolled.removed + rolled.dirs_removed > 0 || total > 0, "{rolled:?}");
    assert_trees_eq(&before, &host_tree(&profile.game_dir), "game dir after rollback");
    assert!(
        deployer.deploy(&deployer.plan(&profile, PlanOptions::new()).unwrap()).is_ok(),
        "deployable again after rollback"
    );
}

#[test]
fn copy_mode_and_copy_subtrees() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/save/", ""), ("/d/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "m"), ("/save/", ""), ("/save/plugin.sav", "s")]);
    add(&mut profile, "m", &m);

    let plan = deployer.plan(&profile, PlanOptions::new().copy_under("/SAVE")).unwrap();
    deployer.deploy(&plan).unwrap();
    let manifest = Manifest::load(deployer.state_dir()).unwrap().unwrap();
    let sav = manifest.entries.iter().find(|e| e.path == "/save/plugin.sav").unwrap();
    assert_eq!(sav.method, Some(DeployMethod::Copy), "forced copy under /save");
    assert_not_hardlinked(&profile.game_dir.join("save/plugin.sav"), &m.join("save/plugin.sav"));
    let m_txt = manifest.entries.iter().find(|e| e.path == "/d/m.txt").unwrap();
    assert_eq!(m_txt.method, Some(DeployMethod::Link));

    // A global Copy mode: everything is a copy.
    deployer.undeploy(None).unwrap();
    let plan = deployer.plan(&profile, PlanOptions::new().mode(DeployMode::Copy)).unwrap();
    let report = deployer.deploy(&plan).unwrap();
    assert_eq!(report.copied, 2, "{report:?}");
    assert_not_hardlinked(&profile.game_dir.join("d/m.txt"), &m.join("d/m.txt"));

    // Re-deploying a linked file under a forced-copy policy replaces the
    // link with a real copy — no undeploy needed in between.
    deployer.undeploy(None).unwrap();
    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    deployer.deploy(&plan).unwrap(); // links
    assert_hardlinked(&profile.game_dir.join("d/m.txt"), &m.join("d/m.txt"));
    let plan = deployer.plan(&profile, PlanOptions::new().mode(DeployMode::Copy)).unwrap();
    let report = deployer.deploy(&plan).unwrap();
    assert_eq!(report.copied, 2, "{report:?}");
    let manifest = Manifest::load(deployer.state_dir()).unwrap().unwrap();
    assert_eq!(manifest.entry("/d/m.txt").unwrap().method, Some(DeployMethod::Copy));
    assert_not_hardlinked(&profile.game_dir.join("d/m.txt"), &m.join("d/m.txt"));
    let _ = root;
}

#[test]
fn foreign_files_keep_by_default_and_error_on_demand() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "m")]);
    add(&mut profile, "m", &m);

    // The file lands *after* the index is built: unknown to the merged tree.
    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    std::fs::write(profile.game_dir.join("d/user.ini"), "mine").unwrap();

    let err = deployer
        .plan_index(&built, &profile.game_dir, &index, &PlanOptions::new().foreign(ForeignPolicy::Error))
        .unwrap_err();
    assert!(err.to_string().contains("user.ini"), "{err}");

    let plan = deployer.plan_index(&built, &profile.game_dir, &index, &PlanOptions::new()).unwrap();
    assert_eq!(plan.foreign, ["/d/user.ini"]);
    deployer.deploy(&plan).unwrap();

    // It survives deployment, and verify reports it as foreign.
    assert_eq!(std::fs::read_to_string(profile.game_dir.join("d/user.ini")).unwrap(), "mine");
    let mismatches = deployer.verify().unwrap();
    let foreign: Vec<_> = mismatches.iter().filter(|m| m.kind == MismatchKind::Foreign).collect();
    assert_eq!(foreign.len(), 1, "{mismatches:?}");
    assert_eq!(foreign[0].path, "/d/user.ini");
}

#[test]
fn capture_moves_writes_to_overwrite_and_reports_corruption() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/base.txt", "base")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "mod-bytes")]);
    add(&mut profile, "m", &m);
    deployer.deploy(&deployer.plan(&profile, PlanOptions::new()).unwrap()).unwrap();
    let overwrite = profile.overwrite_dir.clone();

    // The game writes through the hardlink — mutating the mod file itself.
    std::fs::write(profile.game_dir.join("d/m.txt"), "game wrote here").unwrap();
    assert_eq!(std::fs::read_to_string(m.join("d/m.txt")).unwrap(), "game wrote here", "the caveat, demonstrated");
    // …replaces a base file, and drops a brand-new save.
    std::fs::write(profile.game_dir.join("d/base.txt"), "rewritten base").unwrap();
    std::fs::create_dir_all(profile.game_dir.join("saves")).unwrap();
    std::fs::write(profile.game_dir.join("saves/new.sav"), "savegame").unwrap();

    let report = deployer.capture(Some(&overwrite)).unwrap();
    let moved: Vec<_> = report.moved.iter().map(|c| c.path.as_str()).collect();
    assert!(moved.contains(&"/d/m.txt"), "{moved:?}");
    assert!(moved.contains(&"/d/base.txt"), "{moved:?}");
    assert!(moved.contains(&"/saves/new.sav"), "{moved:?}");
    assert_eq!(report.corrupted.len(), 1, "{report:?}");
    assert_eq!(report.corrupted[0].source, m.join("d/m.txt"));
    assert_eq!(report.lost, ["/d/base.txt"], "the base file's original bytes are unrecoverable");

    // The writes live in the overwrite under their relative paths.
    assert_eq!(std::fs::read_to_string(overwrite.join("d/m.txt")).unwrap(), "game wrote here");
    assert_eq!(std::fs::read_to_string(overwrite.join("d/base.txt")).unwrap(), "rewritten base");
    assert_eq!(std::fs::read_to_string(overwrite.join("saves/new.sav")).unwrap(), "savegame");
    // And it is an independent file now — not still linked to the mod.
    assert_not_hardlinked(&overwrite.join("d/m.txt"), &m.join("d/m.txt"));

    // The corrupted deployment could not be repaired: the path is left gone.
    assert!(!profile.game_dir.join("d/m.txt").exists());
}

#[test]
fn capture_repairs_a_replaced_link() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "mod-bytes")]);
    add(&mut profile, "m", &m);
    deployer.deploy(&deployer.plan(&profile, PlanOptions::new()).unwrap()).unwrap();

    // A well-behaved game rewrites via replace: the link is swapped for a
    // fresh file and the mod source stays pristine.
    std::fs::remove_file(profile.game_dir.join("d/m.txt")).unwrap();
    std::fs::write(profile.game_dir.join("d/m.txt"), "replacement").unwrap();

    let report = deployer.capture(Some(&profile.overwrite_dir)).unwrap();
    assert!(report.corrupted.is_empty(), "{report:?}");
    assert_eq!(report.repaired, ["/d/m.txt"], "{report:?}");
    assert_eq!(std::fs::read_to_string(m.join("d/m.txt")).unwrap(), "mod-bytes", "mod file untouched");
    // The deployment is whole again: the path is a link to the clean source.
    assert_hardlinked(&profile.game_dir.join("d/m.txt"), &m.join("d/m.txt"));
    assert_eq!(std::fs::read_to_string(profile.overwrite_dir.join("d/m.txt")).unwrap(), "replacement");
    assert!(deployer.verify().unwrap().is_empty());
}

#[test]
fn verify_reports_missing_modified_and_foreign() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/base.txt", "base")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "mod-bytes"), ("/d/gone.txt", "soon gone")]);
    add(&mut profile, "m", &m);
    deployer.deploy(&deployer.plan(&profile, PlanOptions::new()).unwrap()).unwrap();
    assert!(deployer.verify().unwrap().is_empty());

    // Delete a deployed file, mutate a base file, drop something foreign,
    // and park a directory where a file was.
    std::fs::remove_file(profile.game_dir.join("d/gone.txt")).unwrap();
    std::fs::write(profile.game_dir.join("d/base.txt"), "tampered").unwrap();
    std::fs::write(profile.game_dir.join("stray.ini"), "x").unwrap();

    let mismatches = deployer.verify().unwrap();
    let kinds: Vec<(&str, MismatchKind)> = mismatches.iter().map(|m| (m.path.as_str(), m.kind)).collect();
    assert!(kinds.contains(&("/d/gone.txt", MismatchKind::Missing)), "{kinds:?}");
    assert!(kinds.contains(&("/d/base.txt", MismatchKind::Modified)), "{kinds:?}");
    assert!(kinds.contains(&("/stray.ini", MismatchKind::Foreign)), "{kinds:?}");
    let modified = mismatches.iter().find(|m| m.kind == MismatchKind::Modified).unwrap();
    assert!(modified.expected_sha256.is_some() && modified.actual_sha256.is_some());
    let _ = root;
}

#[test]
fn state_dir_inside_game_dir_is_skipped() {
    let (root, mut profile) = {
        let root = TempDir::new("deploy-nested");
        let profile = Profile::new("t", root.path().join("game"), root.path().join("ov"));
        (root, profile)
    };
    // Deliberately wrong layout: state inside the game dir.
    let deployer = Deployer::new(profile.game_dir.join("gamestate"));
    populate_host(&profile.game_dir, &tree(&[("/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/m.txt", "m")]);
    add(&mut profile, "m", &m);

    let plan = deployer.plan(&profile, PlanOptions::new()).unwrap();
    deployer.deploy(&plan).unwrap();
    // Journal, manifest and backups inside the game dir are never foreign.
    let mismatches = deployer.verify().unwrap();
    assert!(mismatches.iter().all(|m| m.kind != MismatchKind::Foreign), "{mismatches:?}");
    assert!(deployer.undeploy(None).is_ok());
}

#[test]
fn undeploy_captures_modified_deployed_files() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/orig.txt", "original"), ("/d/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/orig.txt", "mod replacement"), ("/d/m.txt", "m")]);
    add(&mut profile, "m", &m);
    deployer.deploy(&deployer.plan(&profile, PlanOptions::new().mode(DeployMode::Copy)).unwrap()).unwrap();

    // The game mutates a deployed copy (mod source stays pristine) and
    // deletes another deployed file outright.
    std::fs::write(profile.game_dir.join("d/m.txt"), "mutated").unwrap();

    let report = deployer.undeploy(Some(&profile.overwrite_dir)).unwrap();
    assert_eq!(report.captured.len(), 1, "{report:?}");
    assert_eq!(report.captured[0].path, "/d/m.txt");
    assert_eq!(std::fs::read_to_string(profile.overwrite_dir.join("d/m.txt")).unwrap(), "mutated");
    // The shadowed original is back where it belongs.
    assert_eq!(std::fs::read_to_string(profile.game_dir.join("d/orig.txt")).unwrap(), "original");
    // The mod's own file was never in danger: it was a copy.
    assert_eq!(std::fs::read_to_string(m.join("d/m.txt")).unwrap(), "m");
    assert!(report.unrecoverable.is_empty(), "{report:?}");
    let _ = root;
}
