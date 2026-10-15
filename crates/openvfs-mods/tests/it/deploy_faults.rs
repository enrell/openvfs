//! Fault injection for deployment — the "crash anywhere" matrix.
//!
//! For every `DeployOp` kind and every position in a plan that produces all
//! of them, the apply is abandoned at both journaled edges of the step
//! ([`OpPhase::Before`] / [`OpPhase::After`], via [`Deployer::deploy_with`])
//! and right before it ([`Deployer::deploy_partial`]); then `resume` must
//! converge to the completed deployment's tree byte-for-byte and `rollback`
//! must restore the pre-deploy tree byte-for-byte.
//!
//! The second half corrupts the journal itself (torn lines, orphan `done`
//! records, missing plan records) and fails the *layer* file systems
//! ([`FlakyFs`]) — planning errors must be clean `Err`s that leave no
//! partial state behind.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openvfs::{FileSystem, PhysicalFs, VPath};
use openvfs_mods::deploy::{
    DeployOp, Deployer, DeploymentPlan, ForeignPolicy, Journal, JournalRecord, Manifest, OpPhase, PlanOptions,
};
use openvfs_mods::{Error, Layer, MergedIndex, ModEntry, Profile, Result};

use crate::common::faulty::FlakyFs;
use crate::common::{Entry, TempDir, Tree, assert_trees_eq, host_tree, populate_host, tree};

fn scaffold() -> (TempDir, Profile, Deployer) {
    let root = TempDir::new("faults");
    let profile = Profile::new("faults", root.path().join("game"), root.path().join("overwrite"));
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

fn options() -> PlanOptions {
    PlanOptions::new().copy_under("/save")
}

/// The richest scenario the planner can express: phase A deploys mod `ma`
/// (created dir, shadowed file), phase B swaps it for `mb` — yielding a
/// plan containing **all seven** `DeployOp` kinds.
fn plan_b(profile: &Profile, deployer: &Deployer) -> DeploymentPlan {
    deployer.plan(profile, options()).unwrap()
}

/// Builds a fresh scaffold, deploys phase A, returns everything the caller
/// needs to plan phase B.
fn phase_a_deployed() -> (TempDir, Profile, Deployer) {
    let (root, mut profile, deployer) = scaffold();
    populate_host(
        &profile.game_dir,
        &tree(&[
            ("/data/", ""),
            ("/data/shared.txt", "base shared"),
            ("/data/game.txt", "base"),
            ("/readme.txt", "readme"),
        ]),
    );
    let ma = write_mod(
        &root,
        "ma",
        &[
            ("/data/", ""),
            ("/data/shared.txt", "ma wins"),
            ("/data/m_a.txt", "ma"),
            ("/old/", ""),
            ("/old/stale.txt", "stale"),
        ],
    );
    add(&mut profile, "ma", &ma);
    deployer.deploy(&deployer.plan(&profile, options()).unwrap()).unwrap();
    (root, profile, deployer)
}

/// Turns the phase-A deployment into phase B: `ma` disabled, `mb` added.
/// `mb` also provides a *file* at `/old` — the folded path where phase A
/// created a directory — which is what makes the planner emit `RemoveDir`
/// (a created dir survives in the index via the base layer's own listing,
/// so only a file winning its folded path ever sweeps it).
fn phase_b(root: &TempDir, profile: &mut Profile) {
    assert!(profile.set_enabled("ma", false));
    let mb = write_mod(
        root,
        "mb",
        &[
            ("/new/", ""),
            ("/new/n.txt", "new"),
            ("/data/", ""),
            ("/data/m_b.txt", "mb"),
            ("/data/game.txt", "mb wins"),
            ("/save/", ""),
            ("/save/s.sav", "sav"),
            ("/old", "mb file"),
        ],
    );
    add(profile, "mb", &mb);
}

/// How a deployment is abandoned mid-apply.
#[derive(Debug, Clone, Copy)]
enum Crash {
    /// Stop before op k runs (`deploy_partial(k)`) — no journal record for it.
    BeforeStep(usize),
    /// `pending` journaled for seq k, op never ran (hook fails at `Before`).
    PendingNoRun(u64),
    /// Op k ran, `done` never journaled (hook fails at `After`).
    RanNoDone(u64),
}

/// Applies `crash`; `BeforeStep` stops "successfully" (partial apply), the
/// hook shapes fail with an injected error.
fn apply_crash(deployer: &Deployer, plan: &DeploymentPlan, crash: Crash) -> Result<()> {
    match crash {
        Crash::BeforeStep(k) => deployer.deploy_partial(plan, k).map(|_| ()),
        Crash::PendingNoRun(k) => deployer
            .deploy_with(plan, |phase, seq, _| {
                if phase == OpPhase::Before && seq == k {
                    return Err(Error::Deploy { reason: "injected crash".into() });
                }
                Ok(())
            })
            .map(|_| ()),
        Crash::RanNoDone(k) => deployer
            .deploy_with(plan, |phase, seq, _| {
                if phase == OpPhase::After && seq == k {
                    return Err(Error::Deploy { reason: "injected crash".into() });
                }
                Ok(())
            })
            .map(|_| ()),
    }
}

/// Every crash position: `0..=len` partials plus both hook edges per op.
fn crash_matrix(len: usize) -> Vec<Crash> {
    let mut crashes: Vec<Crash> = (0..=len).map(Crash::BeforeStep).collect();
    for k in 0..len as u64 {
        crashes.push(Crash::PendingNoRun(k));
        crashes.push(Crash::RanNoDone(k));
    }
    crashes
}

/// `true` when the crash left a deployment actually in flight (every shape
/// except "stopped after the last op", which is just a finished deploy).
fn in_flight(crash: Crash, plan_len: usize) -> bool {
    !matches!(crash, Crash::BeforeStep(k) if k == plan_len)
}

fn op_kind(op: &DeployOp) -> &'static str {
    match op {
        DeployOp::Mkdir { .. } => "mkdir",
        DeployOp::Backup { .. } => "backup",
        DeployOp::Link { .. } => "link",
        DeployOp::Copy { .. } => "copy",
        DeployOp::RemoveFile { .. } => "rm",
        DeployOp::Restore { .. } => "restore",
        DeployOp::RemoveDir { .. } => "rmdir",
    }
}

#[test]
fn the_rich_plan_contains_every_op_kind() {
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);
    let kinds: BTreeSet<&str> = plan.ops.iter().map(op_kind).collect();
    assert_eq!(
        kinds,
        ["mkdir", "backup", "link", "copy", "rm", "restore", "rmdir"].into_iter().collect(),
        "the scenario must exercise every DeployOp kind: {kinds:?}"
    );
}

/// A completed deployment for comparison (and the pre-B tree), computed once.
fn reference_trees() -> (TempDir, Tree, Tree) {
    let (root, mut profile, deployer) = phase_a_deployed();
    let pre_b = host_tree(&profile.game_dir);
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);
    deployer.deploy(&plan).unwrap();
    let post_b = host_tree(&profile.game_dir);
    (root, pre_b, post_b)
}

/// The phase-B plan length, from a throwaway scaffold.
fn plan_b_len() -> usize {
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    plan_b(&profile, &deployer).len()
}

#[test]
fn resume_converges_from_every_crash_position() {
    let (_keep, _pre_b, control) = reference_trees();
    for crash in crash_matrix(plan_b_len()) {
        let (root, mut profile, deployer) = phase_a_deployed();
        phase_b(&root, &mut profile);
        let plan = plan_b(&profile, &deployer);

        let outcome = apply_crash(&deployer, &plan, crash);
        if !in_flight(crash, plan.len()) {
            assert!(outcome.is_ok(), "crash {crash:?}: {outcome:?}");
            assert!(!Journal::exists(deployer.state_dir()), "crash {crash:?}: finished deploy must clear the journal");
            assert_trees_eq(&control, &host_tree(&profile.game_dir), &format!("completed deploy at {crash:?}"));
            continue;
        }
        assert!(Journal::exists(deployer.state_dir()), "crash {crash:?}: nothing left to resume");
        let report = deployer.resume().unwrap_or_else(|e| panic!("resume after {crash:?}: {e}"));
        assert!(!Journal::exists(deployer.state_dir()), "crash {crash:?}: journal left behind: {report:?}");
        assert!(deployer.verify().unwrap().is_empty(), "crash {crash:?}: {:?}", deployer.verify().unwrap());
        assert_trees_eq(&control, &host_tree(&profile.game_dir), &format!("resume after {crash:?}"));
    }
}

#[test]
fn rollback_restores_from_every_crash_position() {
    let (_keep, pre_b, control) = reference_trees();
    for crash in crash_matrix(plan_b_len()) {
        let (root, mut profile, deployer) = phase_a_deployed();
        phase_b(&root, &mut profile);
        let plan = plan_b(&profile, &deployer);

        let outcome = apply_crash(&deployer, &plan, crash);
        if !in_flight(crash, plan.len()) {
            // A completed deployment is not the journal's business any
            // more: rollback is a no-op and `undeploy` is the undo.
            assert!(outcome.is_ok(), "crash {crash:?}: {outcome:?}");
            deployer.rollback().unwrap();
            assert_trees_eq(&control, &host_tree(&profile.game_dir), "rollback of a finished deploy");
            continue;
        }
        deployer.rollback().unwrap_or_else(|e| panic!("rollback after {crash:?}: {e}"));
        assert!(!Journal::exists(deployer.state_dir()), "crash {crash:?}: journal left behind");
        assert_trees_eq(&pre_b, &host_tree(&profile.game_dir), &format!("rollback after {crash:?}"));
        // The phase-A manifest survived and still describes the tree.
        assert!(deployer.verify().unwrap().is_empty(), "crash {crash:?}: {:?}", deployer.verify().unwrap());
    }
}

// --- journal corruption -----------------------------------------------------

/// Appends raw JSONL to the journal (a corrupting third party).
fn journal_push(state_dir: &Path, line: &str) {
    let path = Journal::path(state_dir);
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(line.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
}

/// Truncates the journal's tail, a crash mid-append.
fn journal_truncate(state_dir: &Path, drop_bytes: u64) {
    let path = Journal::path(state_dir);
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len > drop_bytes);
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(len - drop_bytes).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn a_torn_last_journal_line_is_dropped_and_resumed() {
    let (_keep, _pre_b, control) = reference_trees();
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);

    deployer.deploy_partial(&plan, 3).unwrap();
    // Lose the tail mid-record: the `done` for step 2 is half-written.
    journal_truncate(deployer.state_dir(), 5);
    let report = deployer.resume().unwrap();
    assert!(report.skipped < plan.len(), "some steps must re-run: {report:?}");
    assert_trees_eq(&control, &host_tree(&profile.game_dir), "resume after torn journal");
    assert!(deployer.verify().unwrap().is_empty());
}

#[test]
fn a_torn_plan_record_cannot_resume_but_rolls_back() {
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);
    let pre_b = host_tree(&profile.game_dir);

    deployer.deploy_partial(&plan, 0).unwrap(); // plan record only
    journal_truncate(deployer.state_dir(), 10); // …torn in half.

    let err = deployer.resume().unwrap_err();
    assert!(err.to_string().contains("no plan record"), "{err}");
    deployer.rollback().unwrap();
    assert!(!Journal::exists(deployer.state_dir()));
    assert_trees_eq(&pre_b, &host_tree(&profile.game_dir), "rollback with a torn plan record");
}

#[test]
fn a_done_without_pending_is_rerun_not_trusted() {
    let (_keep, _pre_b, control) = reference_trees();
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);
    let last = plan.len() as u64 - 1;

    // Journal as if every step but the last finished, then claim — without
    // a pending record — that the last one is done too. `done` can only be
    // trusted with its `pending`, so resume must run the step anyway.
    deployer.deploy_partial(&plan, usize::try_from(last).unwrap()).unwrap();
    let line = serde_json::to_string(&JournalRecord::Done { seq: last }).unwrap();
    journal_push(deployer.state_dir(), &line);

    deployer.resume().unwrap();
    assert_trees_eq(&control, &host_tree(&profile.game_dir), "orphan done must not skip real work");
    assert!(deployer.verify().unwrap().is_empty());
}

#[test]
fn a_stray_seq_beyond_the_plan_does_not_wedge_resume() {
    let (_keep, _pre_b, control) = reference_trees();
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);

    deployer.deploy_partial(&plan, 0).unwrap();
    let line = serde_json::to_string(&JournalRecord::Done { seq: u64::MAX }).unwrap();
    journal_push(deployer.state_dir(), &line);

    deployer.resume().unwrap();
    assert_trees_eq(&control, &host_tree(&profile.game_dir), "resume with a stray done seq");
    assert!(deployer.verify().unwrap().is_empty());
}

#[test]
fn an_empty_journal_rolls_back_instead_of_resuming() {
    let (_root, _profile, deployer) = phase_a_deployed();
    let journal = Journal::open(deployer.state_dir()).unwrap(); // creates the empty file
    drop(journal);

    let err = deployer.resume().unwrap_err();
    assert!(err.to_string().contains("no plan record"), "{err}");
    deployer.rollback().unwrap(); // clears the dead journal
    assert!(!Journal::exists(deployer.state_dir()));
}

#[test]
fn pending_records_without_a_plan_still_roll_back() {
    let (root, profile, deployer) = phase_a_deployed();
    let game = profile.game_dir.clone();

    // A crash so early the plan record was never written: simulate the
    // journaled pending ops *and* their on-disk effects by hand.
    let moved_from = game.join("data/game.txt");
    let moved_to = deployer.state_dir().join("backup/data/game.txt");
    std::fs::create_dir_all(moved_to.parent().unwrap()).unwrap();
    std::fs::rename(&moved_from, &moved_to).unwrap(); // the Backup really ran
    let injected = game.join("data/injected.txt");
    std::fs::write(&injected, "injected").unwrap(); // the Link really ran
    let source = root.path().join("mods/ma/data/m_a.txt");

    let mut journal = Journal::open(deployer.state_dir()).unwrap();
    journal
        .append(&JournalRecord::Pending {
            seq: 0,
            op: DeployOp::Backup { from: moved_from.clone(), to: moved_to.clone() },
        })
        .unwrap();
    journal.append(&JournalRecord::Pending { seq: 1, op: DeployOp::Link { source, link: injected.clone() } }).unwrap();
    drop(journal);

    let err = deployer.resume().unwrap_err();
    assert!(err.to_string().contains("no plan record"), "{err}");

    deployer.rollback().unwrap();
    // The backup moved back out, and the link was removed — the ops'
    // embedded records were enough to unwind without a plan.
    assert_eq!(std::fs::read_to_string(&moved_from).unwrap(), "base");
    assert!(!injected.exists(), "the injected link must be removed");
    let tree = host_tree(&game);
    assert_eq!(tree.get("/data/game.txt"), Some(&Entry::File(b"base".to_vec())));
}

#[test]
fn a_corrupt_middle_journal_line_stops_everything() {
    let (_keep, _pre_b, control) = reference_trees();
    let (root, mut profile, deployer) = phase_a_deployed();
    phase_b(&root, &mut profile);
    let plan = plan_b(&profile, &deployer);

    deployer.deploy_partial(&plan, 2).unwrap();
    journal_push(deployer.state_dir(), "{not json}");
    let line = serde_json::to_string(&JournalRecord::Done { seq: 99 }).unwrap();
    journal_push(deployer.state_dir(), &line);

    // Only a torn *tail* is tolerated; corruption in the middle is fatal
    // until a human inspects it (see docs/recovery.md).
    let err = deployer.resume().unwrap_err();
    assert!(err.to_string().contains("corrupt journal"), "{err}");
    let err = deployer.rollback().unwrap_err();
    assert!(err.to_string().contains("corrupt journal"), "{err}");

    // The documented manual recovery: remove the journal, then redeploy —
    // the idempotent ops converge whatever half-state the crash left.
    std::fs::remove_file(Journal::path(deployer.state_dir())).unwrap();
    deployer.deploy(&plan).unwrap();
    assert_trees_eq(&control, &host_tree(&profile.game_dir), "manual recovery after corrupt journal");
}

// --- layer-side faults -------------------------------------------------------

/// Index builds over a layer that dies mid-scan fail cleanly at every
/// position — never a half-built index, never panic.
#[test]
fn index_build_fails_cleanly_at_every_read_dir_position() {
    let root = TempDir::new("layer-fault");
    let mod_dir = root.path().join("mod");
    populate_host(&mod_dir, &tree(&[("/a/", ""), ("/a/x.txt", "x"), ("/b/", ""), ("/b/y.txt", "y")]));
    let game_dir = root.path().join("game");
    populate_host(&game_dir, &tree(&[("/g.txt", "g")]));

    let flaky = FlakyFs::wrap(Arc::new(PhysicalFs::new(&mod_dir).unwrap()) as Arc<dyn FileSystem>);
    let game_flaky = FlakyFs::wrap(Arc::new(PhysicalFs::new(&game_dir).unwrap()) as Arc<dyn FileSystem>);
    let layers = || -> Vec<Layer> {
        vec![
            Layer::new("mod", Arc::new(flaky.clone()) as Arc<dyn FileSystem>),
            Layer::new("base game", Arc::new(game_flaky.clone()) as Arc<dyn FileSystem>),
        ]
    };

    // The build lists the mod's /, /a, /b — 3 calls through this wrapper.
    for k in 0..5 {
        flaky.heal();
        flaky.fail_read_dir_at(k);
        match MergedIndex::build(layers()) {
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("injected layer fault"), "unexpected error at k={k}: {e}");
                assert!(k < 3, "build failed after the injected call: k={k}");
            }
            Ok(index) => {
                assert!(k >= 3, "build unexpectedly succeeded at fail_at={k}: {index:?}");
            }
        }
    }

    // A failure in the *base* layer aborts the same clean way.
    flaky.heal();
    game_flaky.fail_read_dir_at(0);
    let err = MergedIndex::build(layers()).unwrap_err();
    assert!(err.to_string().contains("injected layer fault"), "{err}");
    game_flaky.heal();
    assert!(MergedIndex::build(layers()).is_ok());
}

/// `rebuild` keeps the old snapshot intact when the refresh fails — an
/// index is never observed half-built.
#[test]
fn failed_rebuild_keeps_the_last_snapshot() {
    let root = TempDir::new("rebuild-fault");
    let mod_dir = root.path().join("mod");
    populate_host(&mod_dir, &tree(&[("/a/", ""), ("/a/x.txt", "x")]));

    let flaky = FlakyFs::wrap(Arc::new(PhysicalFs::new(&mod_dir).unwrap()) as Arc<dyn FileSystem>);
    let mut index =
        MergedIndex::build(vec![Layer::new("mod", Arc::new(flaky.clone()) as Arc<dyn FileSystem>)]).unwrap();
    index.invalidate();
    assert!(index.is_stale());

    flaky.fail_read_dir_at(0);
    assert!(index.rebuild().is_err());
    assert!(index.is_stale(), "still stale after a failed rebuild");
    // The old snapshot still answers.
    assert!(index.winner(&VPath::new("/a/x.txt").unwrap()).is_some());

    flaky.heal();
    index.rebuild().unwrap();
    assert!(!index.is_stale());
}

/// A planning failure leaves no partial state: no journal, no manifest, no
/// backup, and the game dir untouched.
#[test]
fn failed_plans_leave_no_partial_state() {
    let (root, mut profile, deployer) = scaffold();
    populate_host(&profile.game_dir, &tree(&[("/d/", ""), ("/d/f.txt", "b")]));
    let m = write_mod(&root, "m", &[("/d/", ""), ("/d/m.txt", "m")]);
    add(&mut profile, "m", &m);

    // ForeignPolicy::Error aborts planning before anything is touched.
    // A file dropped in the game dir is a *base-layer* node — foreign means
    // "the index never saw it", so it must land after the index is built.
    let built = profile.build().unwrap();
    let index = built.index().unwrap();
    std::fs::write(profile.game_dir.join("d/foreign.txt"), "mine").unwrap();
    let after_foreign = host_tree(&profile.game_dir);
    let err =
        deployer.plan_index(&built, &profile.game_dir, &index, &options().foreign(ForeignPolicy::Error)).unwrap_err();
    assert!(err.to_string().contains("foreign"), "{err}");
    assert!(!Journal::exists(deployer.state_dir()));
    assert!(Manifest::load(deployer.state_dir()).unwrap().is_none());
    assert!(!Manifest::backup_root(deployer.state_dir()).exists());
    assert_trees_eq(&after_foreign, &host_tree(&profile.game_dir), "game dir after a failed plan");

    // A missing game dir fails the same way — clean Err, no state.
    let (r2, mut p2, d2) = scaffold();
    let m2 = write_mod(&r2, "m", &[("/x.txt", "x")]);
    add(&mut p2, "m", &m2);
    let err = d2.plan(&p2, options()).unwrap_err();
    assert!(err.to_string().contains("base game"), "{err}");
    assert!(!Journal::exists(d2.state_dir()));
}

/// `FlakyFs` itself: the armed call fails once, others pass, `heal` resets.
#[test]
fn flaky_fs_fails_only_the_armed_call() {
    let root = TempDir::new("flaky");
    let dir = root.path().join("d");
    populate_host(&dir, &tree(&[("/f.txt", "f")]));
    let flaky = FlakyFs::wrap(Arc::new(PhysicalFs::new(&dir).unwrap()) as Arc<dyn FileSystem>);

    let root_path = VPath::new("/").unwrap();
    flaky.fail_metadata_at(1);
    assert!(flaky.metadata(&root_path).is_ok());
    assert!(flaky.metadata(&root_path).is_err());
    assert!(flaky.metadata(&root_path).is_ok(), "only call 1 fails");

    flaky.fail_read_dir_at(0);
    assert!(flaky.read_dir(&root_path).is_err());
    assert!(flaky.read_dir(&root_path).is_ok());

    flaky.fail_metadata_at(0);
    flaky.heal();
    assert!(flaky.metadata(&root_path).is_ok());
}
