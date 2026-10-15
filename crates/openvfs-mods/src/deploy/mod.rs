//! Link-deployment: materializing the merged mod view as real files.
//!
//! The "same result" track of the mod manager: instead of serving a virtual
//! overlay to a patched launcher, [`Deployer`] plants the winning files
//! *into* the game directory itself — as hardlinks where the filesystem
//! allows, copies where it does not — so the unmodified game sees an
//! ordinary directory. [`Deployer::undeploy`] restores the directory
//! byte-for-byte, including the originals that deployment displaced.
//!
//! # State directory layout
//!
//! All bookkeeping lives in a caller-chosen *state directory* that **must
//! sit outside the game directory** (a profile data directory is the usual
//! choice):
//!
//! ```text
//! <state_dir>/
//!   journal.jsonl   write-ahead log; one JSON record per line
//!   manifest.json   the deployment record (files, hashes, backups)
//!   backup/         originals moved out of the game dir, rel paths kept
//!     stray/          unexpected occupants displaced from deployed paths
//! ```
//!
//! `state::scan_dir` still skips the state dir if a caller nests it inside
//! the game dir anyway, but keeping it outside is the supported layout —
//! an undeploy must never sweep the manager's own bookkeeping into the game.
//!
//! # Crash model
//!
//! [`Deployer::deploy`] writes the full [`DeploymentPlan`] to the journal,
//! then brackets every op with a synced `pending`/`done` pair. Every
//! [`DeployOp`] is idempotent, so a crash mid-apply leaves a state that
//! [`Deployer::resume`] simply finishes (re-running a pending op converges)
//! or [`Deployer::rollback`] unwinds in reverse. A *completed* deployment is
//! undone by [`Deployer::undeploy`], driven by the manifest rather than the
//! journal. See `docs/recovery.md` for the operator-facing recovery guide.
//!
//! [`Deployer::deploy_with`] is the deliberate seam for exercising that
//! model: a hook called at both edges of every step — after the `pending`
//! record is synced and again after the op ran — so tests (or a progress
//! UI that wants a veto) can abandon the apply at any point and leave a
//! journal indistinguishable from a real crash's.
//!
//! # The hardlink caveat — read before choosing [`DeployMode::Link`]
//!
//! A hardlink makes the game-dir entry *the same inode* as the mod's file.
//! A game that writes a file **in place** therefore mutates the mod itself;
//! the original mod bytes are gone. [`Deployer::capture`] detects this by
//! hash mismatch: it moves the mutated content to the overwrite dir and
//! names the corrupted mod file in its report — but the bytes are already
//! lost. For directories the game writes into, prefer
//! [`PlanOptions::copy_under`] or [`DeployMode::Copy`]. (Games that rewrite
//! via rename — write temp, rename over — are harmless: they replace the
//! link, not the inode, and capture then repairs the deployment.)
//!
//! A future implementation could use `ReFS` block cloning for copy-on-write
//! copies on supporting volumes; the manifest already records the method.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use openvfs::FileType;

mod plan;
mod state;

pub use plan::{DeployMode, DeployOp, DeploymentPlan, ForeignPolicy, PlanOptions, PlannedFile};
pub use state::{DeployMethod, EntryKind, ForeignFile, Journal, JournalRecord, Manifest, ManifestEntry};

use crate::MergedIndex;
use crate::error::{Error, Result};
use crate::profile::{BuiltProfile, Profile};

/// The deploy engine: plans and applies link/copy deployments of a
/// [`BuiltProfile`] into the game directory, journaling every step in one
/// [`state_dir`](Deployer::state_dir) so the work survives a crash.
///
/// Construct with [`Deployer::new`] — the state dir is created on first
/// deploy and is expected to live *outside* the game directory.
#[derive(Debug, Clone)]
pub struct Deployer {
    state_dir: PathBuf,
}

/// What a [`Deployer::verify`] run found wrong with the game directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// Folded, `/`-rooted path of the file.
    pub path: String,
    /// What kind of drift.
    pub kind: MismatchKind,
    /// The hash the manifest recorded (absent for `Foreign`).
    pub expected_sha256: Option<String>,
    /// The hash found on disk (`Missing`/`Foreign` have none).
    pub actual_sha256: Option<String>,
}

/// The kind of drift [`Mismatch`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchKind {
    /// A manifest file is gone from the game directory.
    Missing,
    /// A manifest file's bytes no longer match the recorded hash — the game
    /// wrote through it.
    Modified,
    /// A file the manifest does not know at all.
    Foreign,
    /// A directory sits where the manifest expects a file (or vice versa).
    WrongType,
}

/// One game write moved into the overwrite directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedFile {
    /// The manifest/scan path it was found at (`/`-rooted, real spelling).
    pub path: String,
    /// The host path it was moved to inside the overwrite dir.
    pub target: PathBuf,
}

/// A deployed file whose in-place write mutated the *mod's own file* —
/// the hardlink caveat made concrete. The mod file's original bytes are
/// gone; only re-installing the mod restores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptedFile {
    /// The deployed game-dir path.
    pub path: String,
    /// The mod source whose contents were destroyed.
    pub source: PathBuf,
}

/// Where in a journaled step the [`Deployer::deploy_with`] hook runs.
///
/// ```text
/// journal: pending(seq) ── synced ──> Before ──> apply op ──> After ──> journal: done(seq)
/// ```
///
/// An error at [`OpPhase::Before`] leaves the op never-run behind a
/// `pending` record; an error at [`OpPhase::After`] leaves the op fully
/// applied but unacknowledged — the two crash shapes a WAL can produce.
/// [`Deployer::resume`] and [`Deployer::rollback`] converge from either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpPhase {
    /// The `pending` record is journaled and synced; the op has not run.
    Before,
    /// The op's effects are on disk; its `done` record is not yet journaled.
    After,
}

/// Counts from [`Deployer::deploy`], [`resume`](Deployer::resume),
/// [`rollback`](Deployer::rollback) and [`undeploy`](Deployer::undeploy).
#[derive(Debug, Default, Clone)]
pub struct DeployReport {
    /// Files materialized as hardlinks.
    pub linked: usize,
    /// Files materialized as copies (forced, or link fallback).
    pub copied: usize,
    /// Files removed (stale deployments, displaced links).
    pub removed: usize,
    /// Originals moved back out of `backup/` into the game dir.
    pub restored: usize,
    /// Occupants moved into `backup/` for preservation.
    pub backed_up: usize,
    /// Directories created.
    pub dirs_created: usize,
    /// Empty directories removed.
    pub dirs_removed: usize,
    /// Ops found already satisfied (idempotent no-ops).
    pub skipped: usize,
    /// Foreign files the plan observed and kept.
    pub foreign: Vec<String>,
    /// Game writes moved to the overwrite dir (undeploy's capture step).
    pub captured: Vec<CapturedFile>,
    /// Manifest files whose pre-deploy bytes no longer exist anywhere —
    /// e.g. a base-game file the game rewrote in place. They are captured
    /// to the overwrite when one is known, but the original is gone.
    pub unrecoverable: Vec<String>,
}

/// What [`Deployer::capture`] collected after the game ran.
#[derive(Debug, Default, Clone)]
pub struct CaptureReport {
    /// Files moved into the overwrite directory.
    pub moved: Vec<CapturedFile>,
    /// Deployed files re-materialized from a still-pristine source.
    pub repaired: Vec<String>,
    /// Deployed files whose in-place writes corrupted the mod source itself.
    pub corrupted: Vec<CorruptedFile>,
    /// Manifest files gone for good (deleted or rewritten base files —
    /// the write itself is still captured and lands in `moved`).
    pub lost: Vec<String>,
    /// Foreign files seen at deploy time and left in place.
    pub kept_foreign: usize,
}

impl Deployer {
    /// A deployer keeping its journal, manifest and backups in `state_dir`.
    ///
    /// Keep the state dir **outside** the game directory (and outside the
    /// mods), on the same volume as the game dir for rename-fast backups.
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self { state_dir: state_dir.into() }
    }

    /// The state directory this deployer works in.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The plan to deploy `profile`'s merged tree into `profile.game_dir`.
    ///
    /// Builds the profile and its index internally; use
    /// [`Deployer::plan_index`] to reuse an existing [`MergedIndex`].
    pub fn plan(&self, profile: &Profile, options: PlanOptions) -> Result<DeploymentPlan> {
        let built = profile.build()?;
        let index = built.index()?;
        let mut options = options;
        options.overwrite_dir.get_or_insert_with(|| profile.overwrite_dir.clone());
        self.plan_index(&built, &profile.game_dir, &index, &options)
    }

    /// [`Deployer::plan`] against an already-built index — `index` must have
    /// been built over `built.layers()` so layer indices line up.
    pub fn plan_index(
        &self,
        built: &BuiltProfile,
        game_dir: &Path,
        index: &MergedIndex,
        options: &PlanOptions,
    ) -> Result<DeploymentPlan> {
        let mut roots: Vec<PathBuf> = built.mods().iter().map(|m| m.source_dir().to_path_buf()).collect();
        roots.push(game_dir.to_path_buf());
        let plan = plan::compute(index, &roots, game_dir, &self.state_dir, options)?;
        log::info!(
            "plan built: ops={} files={} dirs={} foreign={} unreachable={} game_dir={} state_dir={}",
            plan.ops.len(),
            plan.files.len(),
            plan.created_dirs.len(),
            plan.foreign.len(),
            plan.unreachable.len(),
            plan.game_dir.display(),
            self.state_dir.display(),
        );
        for foreign in &plan.foreign {
            log::debug!("foreign file kept: {foreign}");
        }
        for lost in &plan.unreachable {
            log::info!("unreachable (ancestor deploys as a file): {lost}");
        }
        Ok(plan)
    }

    /// Applies `plan`, journaling each step. Equivalent to
    /// [`Deployer::deploy_partial`] without a step limit.
    ///
    /// Fails fast if the state dir holds an unfinished journal — an earlier
    /// deploy must be [`resume`](Deployer::resume)d or
    /// [`rollback`](Deployer::rollback)ed first.
    pub fn deploy(&self, plan: &DeploymentPlan) -> Result<DeployReport> {
        self.deploy_partial(plan, usize::MAX)
    }

    /// Applies at most `max_ops` steps of `plan`, then stops *as if the
    /// process had crashed*: executed steps are journaled `done`, the rest
    /// stay `pending` — call [`Deployer::resume`] to finish or
    /// [`Deployer::rollback`] to undo. Outside tests this is still useful
    /// for progress reporting; `deploy` is `deploy_partial(plan, usize::MAX)`.
    pub fn deploy_partial(&self, plan: &DeploymentPlan, max_ops: usize) -> Result<DeployReport> {
        let journal = self.begin(plan)?;
        self.run(plan, journal, &BTreeSet::new(), max_ops, &mut |_, _, _| Ok(()))
    }

    /// [`Deployer::deploy`] with a hook inside every journaled step.
    ///
    /// `hook(phase, seq, op)` is called twice per step — see [`OpPhase`]:
    /// once after the `pending` record is synced and once after the op's
    /// effects hit disk but before its `done` record is written. Returning
    /// `Err` abandons the deployment exactly there, leaving a journal that
    /// [`Deployer::resume`] finishes or [`Deployer::rollback`] unwinds.
    ///
    /// This is the **fault-injection seam** used by the crash-recovery
    /// tests; it doubles as a progress/veto callback (log ops, abort on a
    /// condition). The hook runs *inside* the transaction — keep it cheap:
    /// an fsync sits on each side of it.
    pub fn deploy_with(
        &self,
        plan: &DeploymentPlan,
        mut hook: impl FnMut(OpPhase, u64, &DeployOp) -> Result<()>,
    ) -> Result<DeployReport> {
        let journal = self.begin(plan)?;
        self.run(plan, journal, &BTreeSet::new(), usize::MAX, &mut hook)
    }

    /// The shared start of `deploy_partial`/`deploy_with`: refuse to stack
    /// onto an in-flight journal, then write the plan record.
    fn begin(&self, plan: &DeploymentPlan) -> Result<Journal> {
        if Journal::exists(&self.state_dir) {
            return Err(Error::Deploy {
                reason: "an interrupted deployment is in flight — call resume() or rollback() first".into(),
            });
        }
        fs::create_dir_all(&self.state_dir)?;
        let mut journal = Journal::open(&self.state_dir)?;
        journal.append(&JournalRecord::Plan { plan: plan.clone() })?;
        log::info!("deploy started: ops={} state_dir={}", plan.ops.len(), self.state_dir.display());
        Ok(journal)
    }

    /// Finishes an interrupted deployment: re-runs every journaled `pending`
    /// step (each is idempotent) plus the ops the crash never journaled —
    /// the journal's `plan` record carries the full op list — then writes
    /// the manifest.
    pub fn resume(&self) -> Result<DeployReport> {
        let state = Journal::read(&self.state_dir)?;
        let Some(plan) = state.plan else {
            if state.steps.is_empty() && !Journal::exists(&self.state_dir) {
                return Err(Error::Deploy { reason: "no deployment in flight".into() });
            }
            return Err(Error::Deploy {
                reason: "journal has no plan record; the deployment cannot resume — use rollback()".into(),
            });
        };
        // A `done` only counts when its `pending` record survived too. A
        // done-without-pending can only come from corruption — a real crash
        // always leaves `pending` first — so the step's outcome is unknown
        // and the safest recovery is to run it again (idempotent, so the
        // journal self-heals). Stray seqs beyond the plan's range are
        // ignored rather than wedging the run.
        let done: BTreeSet<u64> =
            state.steps.iter().filter(|(_, (op, done))| *done && op.is_some()).map(|(seq, _)| *seq).collect();
        let orphans = state.steps.iter().filter(|(_, (op, done))| *done && op.is_none()).count();
        if orphans > 0 {
            log::warn!("resume: {orphans} journal done-record(s) without their pending — re-running those steps");
        }
        let journal = Journal::open_append(&self.state_dir)?;
        log::info!("resume: done={} remaining={}", done.len(), plan.ops.len().saturating_sub(done.len()));
        self.run(&plan, journal, &done, usize::MAX, &mut |_, _, _| Ok(()))
    }

    /// Undoes an *in-flight* deployment: every journaled step in reverse
    /// (pending steps may have partially run — the inverses are idempotent
    /// too), then the journal is cleared. The game directory returns to its
    /// pre-deploy state; a manifest from an earlier completed deployment is
    /// kept, since it still best describes that state.
    ///
    /// For a *completed* deployment use [`Deployer::undeploy`].
    pub fn rollback(&self) -> Result<DeployReport> {
        let state = Journal::read(&self.state_dir)?;
        let mut report = DeployReport::default();
        // Ops come from the plan record when present, else from the pending
        // lines themselves — a torn first write still unwinds cleanly.
        let planned = state.plan.as_ref().map(|p| p.ops.as_slice());
        let executed: BTreeSet<u64> = state.steps.keys().copied().collect();
        log::info!("rollback: undoing {} journaled step(s) state_dir={}", executed.len(), self.state_dir.display());
        for seq in executed.iter().rev() {
            let op = planned
                .and_then(|ops| usize::try_from(*seq).ok().and_then(|i| ops.get(i)))
                .or_else(|| state.steps.get(seq).and_then(|(op, _)| op.as_ref()));
            if let Some(op) = op {
                undo_op(op, &mut report);
            }
        }
        Journal::remove(&self.state_dir);
        log::info!(
            "rollback done: restored={} removed={} dirs_removed={}",
            report.restored,
            report.removed,
            report.dirs_removed
        );
        Ok(report)
    }

    /// Undoes a *completed* deployment: every manifest-deployed file whose
    /// hash still matches is removed, every displaced original is restored
    /// from `backup/`, created directories are swept, and the manifest is
    /// cleared. The game directory returns to its pre-deploy state.
    ///
    /// A deployed file whose hash *changed* is a game write, not garbage:
    /// it is moved to `overwrite_dir` (falling back to the manifest's
    /// recorded overwrite dir) — capture-then-remove, never silent deletion.
    /// A modified base-game file likewise keeps its new bytes in place
    /// (removing it would only lose data); it is reported `unrecoverable`.
    pub fn undeploy(&self, overwrite_dir: Option<&Path>) -> Result<DeployReport> {
        let Some(manifest) = Manifest::load(&self.state_dir)? else {
            return Err(Error::Deploy { reason: "no deployment manifest — nothing was deployed".into() });
        };
        if Journal::exists(&self.state_dir) {
            return Err(Error::Deploy {
                reason: "an interrupted deployment is in flight — resume() or rollback() first".into(),
            });
        }
        let game_dir = manifest.game_dir.clone();
        let mut report = DeployReport::default();
        let mut overwrite = Overwrite::new(overwrite_dir, manifest.overwrite_dir.as_deref());
        log::info!(
            "undeploy: entries={} game_dir={} state_dir={}",
            manifest.entries.len(),
            game_dir.display(),
            self.state_dir.display()
        );
        for entry in manifest.deployed() {
            let host = state::host_path_str(&game_dir, &entry.path);
            match fs::symlink_metadata(&host) {
                Ok(meta) if meta.is_file() || meta.is_symlink() => {
                    if state::sha256_file(&host).is_ok_and(|(_, sha)| sha == entry.sha256) {
                        fs::remove_file(&host)?;
                        report.removed += 1;
                    } else {
                        let target = overwrite.capture_target(&game_dir, &entry.path)?;
                        capture_move(&host, &target)?;
                        log::debug!("undeploy: captured modified deployed file {}", entry.path);
                        report.captured.push(CapturedFile { path: entry.path.clone(), target });
                        // If the write went through a hardlink, it reached the
                        // mod file too — the original bytes are gone.
                        if entry
                            .source
                            .as_deref()
                            .is_some_and(|s| state::sha256_file(s).is_ok_and(|(_, sha)| sha != entry.sha256))
                        {
                            report.unrecoverable.push(entry.path.clone());
                        }
                    }
                }
                Ok(_) => report.skipped += 1, // a directory now sits there — leave it
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    report.skipped += 1;
                    log::debug!("undeploy: {} already gone", entry.path);
                }
                Err(e) => return Err(e.into()),
            }
            if let Some(backup) = &entry.backup {
                let from = Manifest::backup_root(&self.state_dir).join(backup.trim_start_matches('/'));
                if from.exists() {
                    // The backup rel path carries the original's spelling —
                    // restore there, so even the casing returns untouched.
                    let to = state::host_path_str(&game_dir, backup);
                    state::move_any(&from, &to)?;
                    report.restored += 1;
                }
            }
        }
        // Base entries stay put; a modified one is a game write we cannot
        // undo — it is reported rather than silently deleted or overwritten.
        for entry in manifest.entries.iter().filter(|e| e.kind == EntryKind::Base) {
            let host = state::host_path_str(&game_dir, &entry.path);
            let changed = match fs::symlink_metadata(&host) {
                Ok(meta) if meta.is_file() => state::sha256_file(&host).map(|(_, sha)| sha != entry.sha256)?,
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => true,
                Err(e) => return Err(e.into()),
            };
            if changed {
                report.unrecoverable.push(entry.path.clone());
            }
        }
        // Directories we created go last, deepest first, and only if empty.
        let mut dirs = manifest.dirs.clone();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        for rel in dirs {
            let host = state::host_path_str(&game_dir, &rel);
            if fs::remove_dir(&host).is_ok() {
                report.dirs_removed += 1;
            }
        }
        // Anything left in `backup/` outside `stray/` is a displaced
        // original the manifest could not attach to a file entry — a file
        // displaced by a *created* directory. Restore it at its rel path,
        // never overwriting: a new occupant there is the user's own write.
        let backup_root = Manifest::backup_root(&self.state_dir);
        if backup_root.is_dir() {
            restore_orphans(&backup_root, &game_dir, &mut report)?;
        }
        Manifest::remove(&self.state_dir);
        Journal::remove(&self.state_dir);
        log::info!(
            "undeploy done: removed={} restored={} captured={} dirs_removed={} unrecoverable={}",
            report.removed,
            report.restored,
            report.captured.len(),
            report.dirs_removed,
            report.unrecoverable.len()
        );
        Ok(report)
    }

    /// Re-hashes every manifest entry against the game directory and lists
    /// what drifted: `Missing`, `Modified`, `WrongType` manifest files, then
    /// every on-disk file the manifest does not know as `Foreign`.
    pub fn verify(&self) -> Result<Vec<Mismatch>> {
        let Some(manifest) = Manifest::load(&self.state_dir)? else {
            return Err(Error::Deploy { reason: "no deployment manifest — nothing was deployed".into() });
        };
        let game_dir = &manifest.game_dir;
        let mut out = Vec::new();
        for entry in &manifest.entries {
            let host = state::host_path_str(game_dir, &entry.path);
            match fs::symlink_metadata(&host) {
                Ok(meta) if meta.is_file() => {
                    let (size, sha) = state::sha256_file(&host)?;
                    if sha != entry.sha256 || size != entry.size {
                        out.push(Mismatch {
                            path: entry.folded.clone(),
                            kind: MismatchKind::Modified,
                            expected_sha256: Some(entry.sha256.clone()),
                            actual_sha256: Some(sha),
                        });
                    }
                }
                Ok(_) => out.push(Mismatch {
                    path: entry.folded.clone(),
                    kind: MismatchKind::WrongType,
                    expected_sha256: Some(entry.sha256.clone()),
                    actual_sha256: None,
                }),
                Err(e) if e.kind() == io::ErrorKind::NotFound => out.push(Mismatch {
                    path: entry.folded.clone(),
                    kind: MismatchKind::Missing,
                    expected_sha256: Some(entry.sha256.clone()),
                    actual_sha256: None,
                }),
                Err(e) => return Err(e.into()),
            }
        }
        let mut exclude = vec![self.state_dir.clone()];
        if let Some(overwrite) = &manifest.overwrite_dir {
            exclude.push(overwrite.clone());
        }
        let disk = state::scan_dir(game_dir, &exclude)?;
        for (folded, node) in disk {
            if node.kind == FileType::File && manifest.entry(&folded).is_none() {
                out.push(Mismatch {
                    path: folded,
                    kind: MismatchKind::Foreign,
                    expected_sha256: None,
                    actual_sha256: None,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        let mut kinds = [0usize; 4];
        for m in &out {
            kinds[match m.kind {
                MismatchKind::Missing => 0,
                MismatchKind::Modified => 1,
                MismatchKind::Foreign => 2,
                MismatchKind::WrongType => 3,
            }] += 1;
        }
        log::info!(
            "verify: missing={} modified={} foreign={} wrong_type={} game_dir={}",
            kinds[0],
            kinds[1],
            kinds[2],
            kinds[3],
            game_dir.display()
        );
        Ok(out)
    }

    /// Collects the game's writes after it exits: anything on disk that is
    /// *not* the manifest — new files, deployed files whose hash changed —
    /// is moved into `overwrite_dir` at the same relative path, and the
    /// deployed entries are then repaired from their sources so the game
    /// dir matches the manifest again.
    ///
    /// * A *modified* deployed file moves to the overwrite; if its mod
    ///   source still hashes clean the deployment is re-linked/re-copied
    ///   (`repaired`), else the write went through the hardlink and mutated
    ///   the mod itself — reported in `corrupted` (see module docs).
    /// * A *modified* base-game file is captured too, but nothing can
    ///   restore its original bytes — it lands in `lost`.
    /// * Files recorded at deploy time as kept-foreign ([`ForeignPolicy::Keep`])
    ///   whose bytes are unchanged are left alone; foreign files that are
    ///   new or changed are captured like any other write.
    ///
    /// `overwrite_dir` falls back to the manifest's recorded directory.
    /// Stray backups (`backup/stray/`) are never touched.
    pub fn capture(&self, overwrite_dir: Option<&Path>) -> Result<CaptureReport> {
        let Some(manifest) = Manifest::load(&self.state_dir)? else {
            return Err(Error::Deploy { reason: "no deployment manifest — nothing was deployed".into() });
        };
        let game_dir = manifest.game_dir.clone();
        let mut overwrite = Overwrite::new(overwrite_dir, manifest.overwrite_dir.as_deref());
        let mut exclude = vec![self.state_dir.clone()];
        if let Some(known) = overwrite.known() {
            exclude.push(known.to_path_buf());
        }
        let disk = state::scan_dir(&game_dir, &exclude)?;
        let mut report = CaptureReport::default();
        log::info!(
            "capture: entries={} foreign_known={} game_dir={}",
            manifest.entries.len(),
            manifest.foreign.len(),
            game_dir.display()
        );

        for entry in &manifest.entries {
            let host = state::host_path_str(&game_dir, &entry.path);
            let meta = match fs::symlink_metadata(&host) {
                Ok(meta) => Some(meta),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let intact = match &meta {
                Some(meta) if meta.is_file() => match state::sha256_file(&host) {
                    Ok((size, sha)) => size == entry.size && sha == entry.sha256,
                    Err(e) => return Err(e.into()),
                },
                Some(_) | None => false, // a directory replaced it — a write to capture
            };
            if intact {
                continue;
            }
            // Move the write (if anything is there) into the overwrite. For
            // manifest entries this is a copy, not a rename: a deployed
            // hardlink must not drag its shared inode along.
            if meta.is_some() {
                let target = overwrite.capture_target(&game_dir, &entry.path)?;
                capture_move(&host, &target)?;
                report.moved.push(CapturedFile { path: entry.path.clone(), target });
            }
            // Then put the entry back the way the manifest wants it.
            match entry.kind {
                EntryKind::Deployed => {
                    let source = entry.source.clone().unwrap_or_default();
                    let source_ok = state::sha256_file(&source).is_ok_and(|(_, sha)| sha == entry.sha256);
                    if source_ok {
                        let force_copy = entry.method == Some(DeployMethod::Copy);
                        place_file(&source, &host, force_copy)?;
                        report.repaired.push(entry.path.clone());
                    } else {
                        log::warn!(
                            "capture: in-place write through a hardlink corrupted mod file {} ({})",
                            source.display(),
                            entry.path
                        );
                        report.corrupted.push(CorruptedFile { path: entry.path.clone(), source });
                    }
                }
                EntryKind::Base => report.lost.push(entry.path.clone()),
            }
        }
        // Whatever the manifest does not know is a game write (or a kept
        // foreign file — left alone while its bytes still match).
        for (folded, node) in disk {
            if node.kind != FileType::File || manifest.entry(&folded).is_some() {
                continue;
            }
            let host = state::host_path(&game_dir, &node.path);
            let known_kept = manifest.foreign.iter().find(|f| f.folded == folded);
            match known_kept {
                Some(kept) if state::sha256_file(&host).is_ok_and(|(s, h)| s == kept.size && h == kept.sha256) => {
                    report.kept_foreign += 1;
                }
                _ => {
                    let target = overwrite.capture_target(&game_dir, node.path.as_str())?;
                    state::move_any(&host, &target)?;
                    report.moved.push(CapturedFile { path: node.path.to_string(), target });
                }
            }
        }
        log::info!(
            "capture done: moved={} repaired={} corrupted={} lost={} kept_foreign={}",
            report.moved.len(),
            report.repaired.len(),
            report.corrupted.len(),
            report.lost.len(),
            report.kept_foreign
        );
        Ok(report)
    }

    /// The step loop shared by `deploy` and `resume`: journal `pending` +
    /// fsync, execute, journal `done` + fsync — with the `deploy_with` hook
    /// called at both edges of each step. Writes the manifest once every op
    /// the plan knows is done — after which the journal, a finished WAL, is
    /// removed (the handle must be dropped first: Windows refuses to delete
    /// an open file). `done` seqs beyond the plan's range (journal
    /// corruption) are ignored rather than wedging the completion check.
    fn run(
        &self,
        plan: &DeploymentPlan,
        mut journal: Journal,
        done: &BTreeSet<u64>,
        limit: usize,
        hook: &mut dyn FnMut(OpPhase, u64, &DeployOp) -> Result<()>,
    ) -> Result<DeployReport> {
        let mut report = DeployReport { foreign: plan.foreign.clone(), ..DeployReport::default() };
        // What each placement op actually did — the manifest's `method`.
        let mut placed = std::collections::BTreeMap::new();
        let mut ran = 0usize;
        for (seq, op) in plan.ops.iter().enumerate() {
            let seq = seq as u64;
            if done.contains(&seq) {
                report.skipped += 1;
                continue;
            }
            if ran >= limit {
                break;
            }
            journal.append(&JournalRecord::Pending { seq, op: op.clone() })?;
            hook(OpPhase::Before, seq, op)?;
            log::debug!("op {seq} apply: {}", describe(op));
            if let Some((link, method)) = apply_op(op, &mut report)? {
                placed.insert(link, method);
            }
            hook(OpPhase::After, seq, op)?;
            journal.append(&JournalRecord::Done { seq })?;
            ran += 1;
            log::debug!("op {seq} done: {}", describe(op));
        }
        // `done` may carry seqs the plan never had (corrupt journal) — count
        // only the plan's own unfinished ops, so they cannot wedge the run.
        let remaining = plan.ops.iter().enumerate().filter(|(i, _)| !done.contains(&(*i as u64))).count();
        if ran == remaining {
            self.write_manifest(plan, &placed)?;
            drop(journal);
            Journal::remove(&self.state_dir);
            log::info!(
                "deploy finished: linked={} copied={} removed={} backed_up={} skipped={} state_dir={}",
                report.linked,
                report.copied,
                report.removed,
                report.backed_up,
                report.skipped,
                self.state_dir.display()
            );
        }
        Ok(report)
    }

    /// Records what was deployed: hashes every planned file as it lies on
    /// disk and writes `manifest.json` (atomically, via a temp + rename).
    /// `placed` carries the method each placement op really used.
    fn write_manifest(
        &self,
        plan: &DeploymentPlan,
        placed: &std::collections::BTreeMap<PathBuf, DeployMethod>,
    ) -> Result<()> {
        let mut entries = Vec::with_capacity(plan.files.len());
        for file in &plan.files {
            let host = state::host_path_str(&plan.game_dir, &file.path);
            let meta = match fs::symlink_metadata(&host) {
                Ok(meta) => meta,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if file.kind == EntryKind::Base {
                        continue; // the game's own file vanished mid-apply — nothing to record
                    }
                    return Err(Error::Deploy {
                        reason: format!("deployed file missing after apply: {}", host.display()),
                    });
                }
                Err(e) => return Err(e.into()),
            };
            if !meta.is_file() {
                continue;
            }
            let (size, sha256) = state::sha256_file(&host)?;
            let method = match file.kind {
                // What execution recorded wins; a kept file carries its
                // previous (or planned) method instead.
                EntryKind::Deployed => Some(placed.get(&host).copied().or(file.method).unwrap_or(DeployMethod::Link)),
                EntryKind::Base => None,
            };
            entries.push(ManifestEntry {
                path: file.path.clone(),
                folded: file.folded.clone(),
                kind: file.kind,
                method,
                sha256,
                size,
                source: file.source.clone(),
                backup: file.backup.clone(),
            });
        }
        entries.sort_by(|a, b| a.folded.cmp(&b.folded));
        let foreign = plan
            .foreign
            .iter()
            .filter_map(|rel| {
                let host = state::host_path_str(&plan.game_dir, rel);
                state::sha256_file(&host).ok().map(|(size, sha256)| ForeignFile {
                    path: rel.clone(),
                    folded: rel.to_lowercase(),
                    sha256,
                    size,
                })
            })
            .collect();
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Manifest {
            version: 1,
            game_dir: plan.game_dir.clone(),
            overwrite_dir: plan.overwrite_dir.clone(),
            created,
            entries,
            dirs: plan.created_dirs.clone(),
            foreign,
        }
        .save(&self.state_dir)?;
        log::debug!(
            "manifest written: entries={} dirs={} foreign={} path={}",
            plan.files.len(),
            plan.created_dirs.len(),
            plan.foreign.len(),
            Manifest::path(&self.state_dir).display()
        );
        Ok(())
    }
}

/// Restores displaced originals left under `backup/` at the end of an
/// undeploy, member-wise: whole directories when their game-dir path is
/// free, entry-by-entry into a surviving directory, never over an existing
/// file. The `stray/` subtree is skipped — those are preserved non-originals
/// a human must sort out (see `docs/recovery.md`).
fn restore_orphans(backup_dir: &Path, game_dir: &Path, report: &mut DeployReport) -> Result<()> {
    for entry in fs::read_dir(backup_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "stray" {
            continue;
        }
        let from = entry.path();
        let to = game_dir.join(&name);
        let is_dir = fs::symlink_metadata(&from).is_ok_and(|m| m.is_dir());
        match (is_dir, fs::symlink_metadata(&to)) {
            (true, Ok(m)) if m.is_dir() => {
                // A directory still stands there — restore member-wise,
                // then drop the emptied backup directory.
                restore_orphans(&from, &to, report)?;
                let _ = fs::remove_dir(&from);
            }
            (_, Ok(_)) => {} // occupied: leave the backup in place
            (_, Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                state::move_any(&from, &to)?;
                report.restored += 1;
            }
            (_, Err(e)) => return Err(e.into()),
        }
    }
    Ok(())
}

/// A one-line description of an op for debug logging — the op kind and the
/// path it acts on, without dumping the serialized record.
fn describe(op: &DeployOp) -> String {
    match op {
        DeployOp::Mkdir { path } => format!("mkdir {}", path.display()),
        DeployOp::Backup { from, to } => format!("backup {} -> {}", from.display(), to.display()),
        DeployOp::Link { source, link } => format!("link {} -> {}", source.display(), link.display()),
        DeployOp::Copy { source, link } => format!("copy {} -> {}", source.display(), link.display()),
        DeployOp::RemoveFile { path, .. } => format!("remove {}", path.display()),
        DeployOp::Restore { backup, to } => format!("restore {} -> {}", backup.display(), to.display()),
        DeployOp::RemoveDir { path } => format!("rmdir {}", path.display()),
    }
}

/// Lazily-resolved overwrite directory: the explicit argument wins, then
/// the manifest's record; the error surfaces only if a capture actually
/// needs it.
struct Overwrite<'a> {
    param: Option<&'a Path>,
    recorded: Option<&'a Path>,
    resolved: Option<PathBuf>,
}

impl<'a> Overwrite<'a> {
    fn new(param: Option<&'a Path>, recorded: Option<&'a Path>) -> Overwrite<'a> {
        Overwrite { param, recorded, resolved: None }
    }

    /// The overwrite dir if either source knows it (no filesystem checks).
    fn known(&self) -> Option<&Path> {
        self.param.or(self.recorded)
    }

    /// The directory to move a captured game-dir rel path into.
    fn capture_target(&mut self, game_dir: &Path, rel: &str) -> Result<PathBuf> {
        if self.resolved.is_none() {
            let dir = self
                .known()
                .ok_or_else(|| Error::Deploy {
                    reason: "a game write needs an overwrite directory — none given and none recorded".into(),
                })?
                .to_path_buf();
            fs::create_dir_all(&dir)?;
            self.resolved = Some(dir);
        }
        let root = self.resolved.as_ref().expect("resolved above");
        let _ = game_dir; // the rel path already carries the game-dir view
        Ok(root.join(rel.trim_start_matches('/')))
    }
}

/// Executes one journaled op, idempotently. For placement ops, returns
/// `(link path, method used)` so the caller can record the *actual* method
/// (link, or the copy a failed link fell back to) in the manifest.
fn apply_op(op: &DeployOp, report: &mut DeployReport) -> Result<Option<(PathBuf, DeployMethod)>> {
    match op {
        DeployOp::Mkdir { path } => match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => report.skipped += 1,
            Ok(_) => {
                return Err(Error::Deploy { reason: format!("a file blocks the wanted directory {}", path.display()) });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(path)?;
                report.dirs_created += 1;
            }
            Err(e) => return Err(e.into()),
        },
        DeployOp::Backup { from, to } => match state::move_any(from, to) {
            Ok(()) => {
                if to.exists() && !from.exists() {
                    report.backed_up += 1;
                } else {
                    report.skipped += 1;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.skipped += 1,
            Err(e) => return Err(e.into()),
        },
        DeployOp::Link { source, link } => {
            let method = match place_file(source, link, false)? {
                Placed::Already => {
                    report.skipped += 1;
                    return Ok(None);
                }
                Placed::Linked => {
                    report.linked += 1;
                    DeployMethod::Link
                }
                Placed::Copied => {
                    report.copied += 1;
                    DeployMethod::Copy
                }
            };
            return Ok(Some((link.clone(), method)));
        }
        DeployOp::Copy { source, link } => {
            let method = match place_file(source, link, true)? {
                Placed::Already => {
                    report.skipped += 1;
                    return Ok(None);
                }
                Placed::Linked => {
                    report.linked += 1;
                    DeployMethod::Link
                }
                Placed::Copied => {
                    report.copied += 1;
                    DeployMethod::Copy
                }
            };
            return Ok(Some((link.clone(), method)));
        }
        DeployOp::RemoveFile { path, .. } => match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                return Err(Error::Deploy { reason: format!("a directory blocks removal of {}", path.display()) });
            }
            Ok(_) => {
                fs::remove_file(path)?;
                report.removed += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.skipped += 1,
            Err(e) => return Err(e.into()),
        },
        DeployOp::Restore { backup, to } => match state::move_any(backup, to) {
            Ok(()) => report.restored += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => report.skipped += 1,
            Err(e) => return Err(e.into()),
        },
        DeployOp::RemoveDir { path } => match fs::remove_dir(path) {
            Ok(()) => report.dirs_removed += 1,
            // Missing or non-empty (e.g. foreign files kept inside): tolerated.
            Err(_) => report.skipped += 1,
        },
    }
    Ok(None)
}

/// The inverse of [`apply_op`] for [`Deployer::rollback`]: every step the
/// journal recorded — `pending` ones included, since they may have run
/// partway — is undone in reverse order. Best-effort by construction:
/// inverses only ever *restore* content, so a step that never ran is a
/// harmless no-op.
fn undo_op(op: &DeployOp, report: &mut DeployReport) {
    match op {
        DeployOp::Mkdir { path } => {
            if fs::remove_dir(path).is_ok() {
                report.dirs_removed += 1;
            }
        }
        DeployOp::Backup { from, to } => {
            // Reverse the displacement. If `from` was reoccupied the backup
            // is left in place rather than destroying the new file.
            if to.exists() && !from.exists() && state::move_any(to, from).is_ok() {
                report.restored += 1;
            }
        }
        DeployOp::Link { link, .. } | DeployOp::Copy { link, .. } => {
            if fs::symlink_metadata(link).is_ok_and(|m| !m.is_dir()) && fs::remove_file(link).is_ok() {
                report.removed += 1;
            }
        }
        DeployOp::RemoveFile { path, relink_from } => {
            if let Some(source) = relink_from
                && !path.exists()
                && place_file(source, path, false).is_ok()
            {
                report.restored += 1;
            }
        }
        DeployOp::Restore { backup, to } => {
            // Back the restored file into backup again — unless something
            // already stands at `to` (then leave everything alone).
            if to.exists() && !backup.exists() && state::move_any(to, backup).is_ok() {
                report.backed_up += 1;
            }
        }
        DeployOp::RemoveDir { path } => {
            if fs::create_dir_all(path).is_ok() {
                report.dirs_created += 1;
            }
        }
    }
}

/// What [`place_file`] did.
enum Placed {
    /// The link already pointed at the source — a no-op.
    Already,
    /// A hardlink was created.
    Linked,
    /// A copy was made (forced, or the link fallback).
    Copied,
}

/// Materializes `source` at `link`: a hardlink unless `force_copy`, falling
/// back to a copy when the host refuses (`ERROR_NOT_SAME_DEVICE` across
/// volumes, but any link failure earns the fallback — a copy is never
/// wrong). Idempotent: a link already sharing the source's file identity is
/// left alone; any other file at `link` is replaced (the plan guarantees a
/// backup ran first).
fn place_file(source: &Path, link: &Path, force_copy: bool) -> io::Result<Placed> {
    place_file_with(source, link, force_copy, |s, l| fs::hard_link(s, l))
}

/// [`place_file`] with the link primitive behind a seam — tests inject a
/// failing linker to exercise the cross-volume (`ERROR_NOT_SAME_DEVICE`)
/// copy fallback without needing a second volume.
fn place_file_with(
    source: &Path,
    link: &Path,
    force_copy: bool,
    linker: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<Placed> {
    if let Ok(meta) = fs::symlink_metadata(link) {
        if meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::IsADirectory, format!("{} is a directory", link.display())));
        }
        if !force_copy && state::same_file(link, source) {
            return Ok(Placed::Already);
        }
        fs::remove_file(link)?;
    }
    // A failed link — cross-volume (`ERROR_NOT_SAME_DEVICE`), no link
    // support, quota — earns the copy fallback: a copy is never wrong.
    if !force_copy && linker(source, link).is_ok() {
        return Ok(Placed::Linked);
    }
    fs::copy(source, link)?;
    Ok(Placed::Copied)
}

/// Moves a game write into the overwrite dir. Plain files rename like
/// [`state::move_any`]; a deployed file is instead **copied then removed** —
/// it may share an inode with the mod source, and the captured file must
/// not keep sharing it.
fn capture_move(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(from)?.is_dir() {
        return state::move_any(from, to);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(to).is_ok() {
        state::remove_any(to)?;
    }
    fs::copy(from, to)?;
    fs::remove_file(from)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("openvfs-deploy-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn place_file_links_then_relinks_idempotently() {
        let dir = tempdir("place-link");
        let source = dir.join("mod.bin");
        let link = dir.join("game.bin");
        fs::write(&source, b"mod-bytes").unwrap();

        // First placement is a hardlink where the host allows it.
        let placed = place_file(&source, &link, false).unwrap();
        assert!(matches!(placed, Placed::Linked | Placed::Copied));
        assert_eq!(fs::read(&link).unwrap(), b"mod-bytes");

        // Re-running converges without an error — `Already` where file
        // identity is stable (unix), a relink where it is not (windows).
        let again = place_file(&source, &link, false).unwrap();
        if cfg!(unix) {
            assert!(matches!(again, Placed::Already), "the same inode must not be replaced");
        }
        assert_eq!(fs::read(&link).unwrap(), b"mod-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_file_force_copy_replaces_even_an_existing_file() {
        let dir = tempdir("place-copy");
        let source = dir.join("mod.bin");
        let link = dir.join("game.bin");
        fs::write(&source, b"mod-bytes").unwrap();
        fs::write(&link, b"old occupant").unwrap();

        let placed = place_file(&source, &link, true).unwrap();
        assert!(matches!(placed, Placed::Copied));
        assert_eq!(fs::read(&link).unwrap(), b"mod-bytes");

        // The copy is an independent file: writing through the link path
        // must not reach the source (the copy-mode guarantee).
        fs::write(&link, b"game wrote").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"mod-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_file_falls_back_to_copy_when_linking_fails() {
        // The cross-volume case, injected: `ERROR_NOT_SAME_DEVICE` (17).
        let dir = tempdir("place-fallback");
        let source = dir.join("mod.bin");
        let link = dir.join("game.bin");
        fs::write(&source, b"mod-bytes").unwrap();

        let placed = place_file_with(&source, &link, false, |_, _| Err(io::Error::from_raw_os_error(17))).unwrap();
        assert!(matches!(placed, Placed::Copied), "a failed link must fall back to a copy");
        assert_eq!(fs::read(&link).unwrap(), b"mod-bytes");
        // And it is an independent inode: a write through it stays put.
        fs::write(&link, b"game wrote").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"mod-bytes");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn place_file_refuses_a_directory_occupant() {
        let dir = tempdir("place-dir");
        let source = dir.join("mod.bin");
        let link = dir.join("game.bin");
        fs::write(&source, b"x").unwrap();
        fs::create_dir(&link).unwrap();
        assert!(place_file(&source, &link, false).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
