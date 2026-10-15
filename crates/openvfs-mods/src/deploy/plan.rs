//! The deployment plan: the ordered op list that turns the game directory
//! as it *is* into the merged tree a [`MergedIndex`] describes.
//!
//! Planning is a three-pass diff over a fresh scan of the game directory:
//!
//! 1. **Retire** — everything a previous manifest deployed whose path is no
//!    longer served by a mod is removed, and the original it displaced is
//!    restored;
//! 2. **Place** — in folded-path order (so parents always precede their
//!    children), wanted directories are created and wanted files are linked
//!    or copied, after moving whatever occupies the path into the backup;
//! 3. **Clean** — directories a previous deployment created and nothing
//!    wants any more are removed, deepest first.
//!
//! The plan is a pure value: [`crate::Deployer::deploy`] can journal it,
//! replay it, and — since every [`DeployOp`] is idempotent — resume it after
//! a crash without re-deciding anything.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use openvfs::{FileType, VPath};
use serde::{Deserialize, Serialize};

use super::state::{self, DeployMethod, EntryKind, Manifest, ManifestEntry, ScannedNode};
use crate::error::{Error, Result};
use crate::index::{MergedIndex, Provider};

/// How deployed files are materialized in the game directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeployMode {
    /// Hardlink wherever the filesystem allows it (NTFS supports this
    /// without privileges), copying on cross-volume or unsupported hosts.
    /// Zero extra space, but see the hardlink caveat in the
    /// [`crate::deploy`] module docs.
    #[default]
    Link,
    /// Always copy. Slower and uses real space, but a game writing in place
    /// can never reach a mod's own bytes — the safe choice for directories
    /// the game is known to write into.
    Copy,
}

/// What the planner does with game-directory files the merged tree does
/// not know about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForeignPolicy {
    /// Leave them in place; they are listed in the plan's
    /// [`DeploymentPlan::foreign`] and carried into the manifest so that
    /// capture does not mistake them for fresh game writes.
    #[default]
    Keep,
    /// Refuse to build a plan at all, naming every foreign file.
    Error,
    // Deliberately no `Remove` variant: deleting a file the manager cannot
    // attribute is never safe — it might be the user's own data. `Keep` is
    // the default for the same reason.
}

/// Knobs for [`crate::Deployer::plan`]; all-default is the common case.
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Link or copy, globally.
    pub mode: DeployMode,
    /// What to do with files the merged tree does not want.
    pub foreign: ForeignPolicy,
    /// Folded-path prefixes (e.g. `"/save"`) under which files are always
    /// copied even in [`DeployMode::Link`] — use it for subtrees the game
    /// writes into, so an in-place write can never mutate a mod file.
    pub copy_subtrees: Vec<String>,
    /// The profile's overwrite directory. Recorded in the manifest (capture
    /// can default to it) and excluded from scans when nested inside the
    /// game directory.
    pub overwrite_dir: Option<PathBuf>,
}

impl PlanOptions {
    /// All defaults: link mode, keep foreign files.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets [`PlanOptions::mode`].
    #[must_use]
    pub fn mode(mut self, mode: DeployMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets [`PlanOptions::foreign`].
    #[must_use]
    pub fn foreign(mut self, policy: ForeignPolicy) -> Self {
        self.foreign = policy;
        self
    }

    /// Forces copies under the folded prefix `path` (`"/SAVES"` folds too).
    #[must_use]
    pub fn copy_under(mut self, path: impl AsRef<str>) -> Self {
        self.copy_subtrees.push(path.as_ref().to_lowercase());
        self
    }

    /// Sets [`PlanOptions::overwrite_dir`].
    #[must_use]
    pub fn overwrite_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.overwrite_dir = Some(dir.into());
        self
    }

    /// The method a file at `folded` gets: copy when globally copied or
    /// under a forced-copy subtree, else link.
    pub(crate) fn method_for(&self, folded: &str) -> DeployMethod {
        let copy = self.mode == DeployMode::Copy
            || self.copy_subtrees.iter().any(|s| {
                let s = s.trim_end_matches('/');
                folded == s || folded.starts_with(&format!("{s}/"))
            });
        if copy { DeployMethod::Copy } else { DeployMethod::Link }
    }
}

/// A single, idempotent step of a [`DeploymentPlan`].
///
/// Ops serialize into the journal, so every variant carries absolute host
/// paths — a plan is bound to the directory layout it was computed for.
/// *Idempotent* means running an op twice (or on the half-done result of a
/// crashed run) converges to the same state: `Mkdir` tolerates existing
/// directories, `Link` is a no-op when the link already points at the
/// source, `Backup`/`Restore` are satisfied when the move already happened,
/// and the removals tolerate a missing target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeployOp {
    /// Create one directory at `path`; parents already exist by ordering.
    Mkdir {
        /// Absolute host path to create.
        path: PathBuf,
    },
    /// Move whatever is at `from` aside to `to` (inside the state dir's
    /// backup area), preserving it for a later restore. Renames on the same
    /// volume — the original inode is kept — and copies across volumes.
    Backup {
        /// The displaced game-dir path.
        from: PathBuf,
        /// Where it goes inside `backup/`.
        to: PathBuf,
    },
    /// Make `link` a hardlink to `source`; falls back to copying when the
    /// host refuses (cross-volume, no hardlink support).
    Link {
        /// The mod file (or backup-restored file) serving the content.
        source: PathBuf,
        /// The game-dir path to create.
        link: PathBuf,
    },
    /// Copy `source` to `link` unconditionally — never a shared inode.
    Copy {
        /// The file serving the content.
        source: PathBuf,
        /// The game-dir path to create.
        link: PathBuf,
    },
    /// Delete the file at `path` — always a file *we* placed or replaced.
    /// `relink_from` lets [`crate::Deployer::rollback`] re-materialize it:
    /// the source is never touched by removal.
    RemoveFile {
        /// The game-dir path to delete.
        path: PathBuf,
        /// Where the bytes still live, for rollback's relink.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relink_from: Option<PathBuf>,
    },
    /// Move a backup back over its original path — the reverse of
    /// [`DeployOp::Backup`].
    Restore {
        /// The preserved original inside `backup/`.
        backup: PathBuf,
        /// The game-dir path it returns to.
        to: PathBuf,
    },
    /// Remove an empty directory an earlier deployment created — never
    /// scheduled for pre-existing or non-empty directories.
    RemoveDir {
        /// The directory to remove if empty.
        path: PathBuf,
    },
}

/// One file the deployed tree is expected to contain — the manifest's
/// template. [`EntryKind::Deployed`] files were placed by the deployment;
/// [`EntryKind::Base`] files are the game's own, recognized and left alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFile {
    /// Path relative to the game directory in the spelling it is (or will
    /// be) deployed with, `/`-rooted.
    pub path: String,
    /// `path` folded — the stable identity.
    pub folded: String,
    /// Placed by us, or pre-existing.
    pub kind: EntryKind,
    /// The method `Deployed` entries plan to use (`Link` may still fall
    /// back to `Copy` at execution; the manifest records what happened).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<DeployMethod>,
    /// The mod file behind a `Deployed` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// The displaced original under `backup/`, when the path was occupied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// The complete, ordered deployment description: ops to run plus the file
/// list the manifest will record once they have run.
///
/// Built by [`crate::Deployer::plan`] / [`crate::Deployer::plan_index`];
/// consumed by [`crate::Deployer::deploy`] and serialized into the journal
/// so [`crate::Deployer::resume`] can continue without the profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentPlan {
    /// The game directory this plan deploys into.
    pub game_dir: PathBuf,
    /// The overwrite dir recorded for the manifest, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_dir: Option<PathBuf>,
    /// The ordered, idempotent steps.
    pub ops: Vec<DeployOp>,
    /// Every file the deployed tree should contain afterwards.
    pub files: Vec<PlannedFile>,
    /// Directories the deployment creates or keeps responsibility for
    /// (deployed spelling, parents first) — what undeploy may remove.
    #[serde(default)]
    pub created_dirs: Vec<String>,
    /// Foreign files observed and kept ([`ForeignPolicy::Keep`]).
    #[serde(default)]
    pub foreign: Vec<String>,
    /// Desired files that cannot exist on disk because an ancestor of
    /// theirs deploys as a file — recorded, never attempted.
    #[serde(default)]
    pub unreachable: Vec<String>,
}

impl DeploymentPlan {
    /// `true` when the game dir already matches the merged tree.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Number of ops.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }
}

/// Mutable scratch the three planning passes share.
struct Planner<'a> {
    index: &'a MergedIndex,
    roots: &'a [PathBuf],
    game_dir: &'a Path,
    backup_root: PathBuf,
    options: &'a PlanOptions,
    /// Index of the base-game layer (the last one).
    base_layer: usize,
    /// Folded rel path → what is on disk.
    disk: BTreeMap<String, ScannedNode>,
    /// Previous deployment, when the state dir holds one.
    prev: Option<Manifest>,
    /// Folded path → index into `prev.entries`, for `Deployed` entries.
    prev_deployed: BTreeMap<String, usize>,
    /// Backup-relative paths already spoken for (previous backups + this plan's).
    claimed_backups: BTreeSet<String>,
    /// Folded dir → the real spelling used on disk (existing or to-create).
    dir_spellings: BTreeMap<String, VPath>,
    /// Folded paths scheduled into `backup/` — the node (and its whole
    /// subtree, when a directory was moved) no longer stands in the game
    /// dir, so `finish` must not call its members foreign.
    moved_nodes: BTreeSet<String>,
    plan: DeploymentPlan,
}

/// Computes the diff. `roots` aligns with `index.layers()`; its last entry
/// is the base game's directory and also `game_dir`.
pub(crate) fn compute(
    index: &MergedIndex,
    roots: &[PathBuf],
    game_dir: &Path,
    state_dir: &Path,
    options: &PlanOptions,
) -> Result<DeploymentPlan> {
    if !game_dir.is_dir() {
        return Err(Error::MissingDirectory { role: "base game", path: game_dir.to_path_buf() });
    }
    if index.layers().len() != roots.len() {
        return Err(Error::Deploy {
            reason: format!("index covers {} layers but {} roots were given", index.layers().len(), roots.len()),
        });
    }
    let prev = Manifest::load(state_dir)?;
    if let Some(prev) = &prev {
        // A manifest describing a different game dir means the state dir is
        // being reused — refuse rather than corrupt someone else's record.
        let same = match (prev.game_dir.canonicalize(), game_dir.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => prev.game_dir == game_dir,
        };
        if !same {
            return Err(Error::Deploy {
                reason: format!("state dir manifest describes {}, not {}", prev.game_dir.display(), game_dir.display()),
            });
        }
    }
    let mut exclude = vec![state_dir.to_path_buf()];
    if let Some(overwrite) = &options.overwrite_dir {
        exclude.push(overwrite.clone());
    }
    let disk = state::scan_dir(game_dir, &exclude)?;
    let prev_deployed = prev
        .as_ref()
        .map(|m| {
            m.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.kind == EntryKind::Deployed)
                .map(|(i, e)| (e.folded.clone(), i))
                .collect()
        })
        .unwrap_or_default();
    let claimed_backups =
        prev.as_ref().map(|m| m.deployed().filter_map(|e| e.backup.clone()).collect()).unwrap_or_default();
    let mut planner = Planner {
        index,
        roots,
        game_dir,
        backup_root: Manifest::backup_root(state_dir),
        options,
        base_layer: roots.len() - 1,
        disk,
        prev,
        prev_deployed,
        claimed_backups,
        dir_spellings: BTreeMap::new(),
        moved_nodes: BTreeSet::new(),
        plan: DeploymentPlan {
            game_dir: game_dir.to_path_buf(),
            overwrite_dir: options.overwrite_dir.clone(),
            ops: Vec::new(),
            files: Vec::new(),
            created_dirs: Vec::new(),
            foreign: Vec::new(),
            unreachable: Vec::new(),
        },
    };
    planner.retire_stale_deployed();
    planner.place_desired()?;
    planner.sweep_dirs();
    planner.finish()
}

impl Planner<'_> {
    /// `true` when `folded` is served by a mod layer's file (not the base).
    fn is_mod_served(&self, folded: &str) -> bool {
        let Ok(path) = VPath::new(folded) else { return false };
        self.index.winner(&path).is_some_and(|w| w.is_file() && w.layer() != self.base_layer)
    }

    /// The previous manifest's `Deployed` entry for `folded`, if any.
    fn prev_entry(&self, folded: &str) -> Option<&ManifestEntry> {
        let i = *self.prev_deployed.get(folded)?;
        self.prev.as_ref()?.entries.get(i)
    }

    /// Pass 1: deployed files no longer served by a mod come out; their
    /// backups go back in. An occupant whose bytes no longer match the
    /// recorded deployment — a game write, a user edit — is preserved under
    /// `backup/stray/` instead of being silently deleted (the same rule
    /// undeploy's capture step follows).
    fn retire_stale_deployed(&mut self) {
        let Some(prev) = &self.prev else { return };
        let stale: Vec<ManifestEntry> = prev.deployed().filter(|e| !self.is_mod_served(&e.folded)).cloned().collect();
        for entry in stale {
            match self.disk.get(&entry.folded).cloned() {
                // The deployed file, untouched: remove it. The journal
                // records `relink_from` so rollback can put it back.
                Some(node) if node.kind == FileType::File => {
                    let host = state::host_path(self.game_dir, &node.path);
                    let clean =
                        state::sha256_file(&host).is_ok_and(|(size, sha)| size == entry.size && sha == entry.sha256);
                    if clean {
                        self.plan.ops.push(DeployOp::RemoveFile { path: host, relink_from: entry.source.clone() });
                    } else {
                        self.stray_node(&node);
                    }
                }
                // A directory grew where a file deployed: foreign content —
                // preserve it before the restore wants the path back.
                Some(node) => self.stray_node(&node),
                None => {} // already gone
            }
            if let Some(backup) = entry.backup {
                let from = self.backup_root.join(backup.trim_start_matches('/'));
                // The backup rel path *is* the original's spelling — restore
                // there, not at the deployed spelling.
                let to = state::host_path_str(self.game_dir, &backup);
                self.plan.ops.push(DeployOp::Restore { backup: from, to });
            }
        }
    }

    /// Pass 2: every wanted node, in folded order (parents before children).
    fn place_desired(&mut self) -> Result<()> {
        // Snapshot the winners first: the loop bodies mutate `self`.
        let entries: Vec<(String, Provider)> =
            self.index.iter().map(|e| (e.folded_path().to_owned(), e.winner().clone())).collect();
        for (folded, winner) in entries {
            if winner.is_dir() {
                self.desired_dir(&folded, winner.path());
            } else {
                self.desired_file(&folded, &winner)?;
            }
        }
        Ok(())
    }

    /// The real rel path a wanted dir gets on disk: an existing folded-equal
    /// dir keeps its spelling; a created one takes the winner's. `None` when
    /// an ancestor deploys as a file — the subtree is then unreachable.
    fn resolve_dir(&self, folded: &str, real: &VPath) -> Option<VPath> {
        if let Some(node) = self.disk.get(folded)
            && node.kind == FileType::Dir
        {
            return Some(node.path.clone());
        }
        let parent = real.parent().unwrap_or_else(VPath::root);
        let base = if parent.is_root() {
            VPath::root()
        } else {
            self.dir_spellings.get(&parent.as_str().to_lowercase())?.clone()
        };
        base.join(real.file_name()?).ok()
    }

    /// A wanted directory: exists, blocked by a file, or missing.
    fn desired_dir(&mut self, folded: &str, real: &VPath) {
        match self.disk.get(folded).map(|n| n.kind) {
            Some(FileType::Dir) => {
                let path = self.disk[folded].path.clone();
                let still_ours = self.prev.as_ref().is_some_and(|m| m.dirs.iter().any(|d| d.to_lowercase() == folded));
                if still_ours {
                    // Still wanted: it stays ours to remove on undeploy.
                    self.plan.created_dirs.push(path.to_string());
                }
                self.dir_spellings.insert(folded.to_owned(), path);
            }
            Some(FileType::File) => {
                let node = self.disk[folded].clone();
                self.backup_node(&node);
                match self.resolve_dir(folded, real) {
                    Some(spelling) => {
                        self.mkdir(&spelling);
                        self.dir_spellings.insert(folded.to_owned(), spelling);
                    }
                    None => self.plan.unreachable.push(real.to_string()),
                }
            }
            None => match self.resolve_dir(folded, real) {
                Some(spelling) => {
                    self.mkdir(&spelling);
                    self.dir_spellings.insert(folded.to_owned(), spelling);
                }
                None => self.plan.unreachable.push(real.to_string()),
            },
        }
    }

    /// A wanted file: either the game's own (left in place, recorded `Base`)
    /// or a mod's (placed by link/copy after freeing the path).
    fn desired_file(&mut self, folded: &str, winner: &Provider) -> Result<()> {
        if winner.layer() == self.base_layer {
            // The game's own file already serves itself — record it so
            // verify and capture can recognize it.
            if let Some(node) = self.disk.get(folded).filter(|n| n.kind == FileType::File) {
                self.plan.files.push(PlannedFile {
                    path: node.path.to_string(),
                    folded: folded.to_owned(),
                    kind: EntryKind::Base,
                    method: None,
                    source: None,
                    backup: None,
                });
            }
            return Ok(());
        }

        let real = winner.path();
        let source = state::host_path(&self.roots[winner.layer()], real);
        let mut method = self.options.method_for(folded);
        let parent_folded = real.parent().map_or_else(|| "/".to_owned(), |p| p.as_str().to_lowercase());
        let target = if parent_folded == "/" {
            VPath::root().join(real.file_name().unwrap_or("file")).unwrap_or_else(|_| real.clone())
        } else {
            let Some(dir) = self.dir_spellings.get(&parent_folded) else {
                // Pathological mod set: an ancestor is itself served as a file.
                self.plan.unreachable.push(real.to_string());
                return Ok(());
            };
            if let Ok(target) = dir.join(real.file_name().unwrap_or("file")) {
                target
            } else {
                self.plan.unreachable.push(real.to_string());
                return Ok(());
            }
        };
        // What the previous deployment said about this path — values copied
        // out so the ops below can mutate `self` freely.
        let prev = self.prev_entry(folded).cloned();
        let prev_source = prev.as_ref().and_then(|e| e.source.clone());
        let mut backup = prev.as_ref().and_then(|e| e.backup.clone());
        let mut deployed_at = target.clone();
        let mut place = true;

        match self.disk.get(folded).cloned() {
            None => {}
            Some(node) if node.kind == FileType::Dir => {
                // A whole directory standing where a file belongs is moved
                // into backup too (its contents travel with it) — and the
                // manifest must know, or undeploy could never put it back.
                backup = Some(self.backup_node(&node));
            }
            Some(node) => {
                let host = state::host_path(self.game_dir, &node.path);
                let host_sum = state::sha256_file(&host)?;
                let matches_prev = prev.as_ref().is_some_and(|e| host_sum == (e.size, e.sha256.clone()));
                if matches_prev && Self::sum_matches(&host_sum, &source) {
                    // Still exactly what we deployed — keep it (link or copy,
                    // the recorded method says which), unless the plan now
                    // forces a copy of what was deployed as a link.
                    if method == DeployMethod::Copy && prev.as_ref().and_then(|e| e.method) == Some(DeployMethod::Link)
                    {
                        // falls through: re-place as a real copy
                    } else {
                        place = false;
                        deployed_at = node.path.clone();
                        method = prev.as_ref().and_then(|e| e.method).unwrap_or(method);
                    }
                } else if prev.is_some() && matches_prev {
                    // Our own deployed content, but the winner's source has
                    // changed (a mod update): regenerate, nothing to preserve.
                    self.plan.ops.push(DeployOp::RemoveFile { path: host, relink_from: prev_source.clone() });
                } else if Self::sum_matches(&host_sum, &source) {
                    // An unrelated real file with identical bytes already
                    // serves the path — leave it, record it as `Base`.
                    self.plan.files.push(PlannedFile {
                        path: node.path.to_string(),
                        folded: folded.to_owned(),
                        kind: EntryKind::Base,
                        method: None,
                        source: None,
                        backup: None,
                    });
                    return Ok(());
                } else if prev.is_some() {
                    // An occupant where we once deployed, matching neither the
                    // old deployment nor the new source: a stray write (the
                    // game, or the user). Preserve it under `stray/` — the
                    // real original, if any, is already in the backup.
                    self.stray_node(&node);
                } else {
                    // A genuine original being shadowed: back it up in place.
                    backup = Some(self.backup_node(&node));
                }
            }
        }
        if place {
            let link = state::host_path(self.game_dir, &target);
            self.plan.ops.push(if method == DeployMethod::Copy {
                DeployOp::Copy { source: source.clone(), link }
            } else {
                DeployOp::Link { source: source.clone(), link }
            });
        }
        self.plan.files.push(PlannedFile {
            path: deployed_at.to_string(),
            folded: folded.to_owned(),
            kind: EntryKind::Deployed,
            method: Some(method),
            source: Some(source),
            backup,
        });
        Ok(())
    }

    /// Pass 3: directories a previous deployment created and the merged
    /// tree no longer wants are removed if empty, deepest first.
    fn sweep_dirs(&mut self) {
        let Some(prev) = &self.prev else { return };
        let mut doomed: Vec<String> =
            prev.dirs.iter().filter(|d| !self.dir_spellings.contains_key(&d.to_lowercase())).cloned().collect();
        // Deepest first so a parent is empty when its turn comes.
        doomed.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        for rel in doomed {
            let host = state::host_path_str(self.game_dir, &rel);
            if host.is_dir() {
                self.plan.ops.push(DeployOp::RemoveDir { path: host });
            }
        }
    }

    /// Foreign check + plan assembly.
    fn finish(mut self) -> Result<DeploymentPlan> {
        let desired: BTreeSet<String> = self.index.iter().map(|e| e.folded_path().to_owned()).collect();
        let foreign: Vec<String> = self
            .disk
            .iter()
            .filter(|(folded, node)| {
                node.kind == FileType::File && !desired.contains(folded.as_str()) && !self.was_moved(folded)
            })
            .map(|(_, node)| node.path.to_string())
            .collect();
        self.plan.foreign = foreign;
        if self.options.foreign == ForeignPolicy::Error && !self.plan.foreign.is_empty() {
            log::info!("plan refused: {} foreign file(s) under --foreign error", self.plan.foreign.len());
            return Err(Error::Deploy {
                reason: format!("foreign files in game directory: {}", self.plan.foreign.join(", ")),
            });
        }
        Ok(self.plan)
    }

    /// Creates a `Mkdir` op and records the dir as deployment-owned.
    fn mkdir(&mut self, rel: &VPath) {
        self.plan.ops.push(DeployOp::Mkdir { path: state::host_path(self.game_dir, rel) });
        self.plan.created_dirs.push(rel.to_string());
    }

    /// `true` when `folded` — or a folded ancestor of it — was scheduled
    /// into `backup/` and so no longer stands in the game directory.
    fn was_moved(&self, folded: &str) -> bool {
        let mut f = folded;
        loop {
            if self.moved_nodes.contains(f) {
                return true;
            }
            match f.rfind('/') {
                Some(0) | None => return false,
                Some(i) => f = &f[..i],
            }
        }
    }

    /// Schedules `node`'s move into the backup area and returns the claimed
    /// backup-relative path. The node's own rel path is preferred; when a
    /// previous manifest already owns it (the true original), the occupant
    /// goes to `backup/stray/` instead — history is never overwritten.
    fn backup_node(&mut self, node: &ScannedNode) -> String {
        self.moved_nodes.insert(node.path.as_str().to_lowercase());
        let rel = self.claim_backup(&node.path);
        let to = self.backup_root.join(rel.trim_start_matches('/'));
        log::debug!("plan: backup {} -> backup/{rel}", node.path);
        self.plan.ops.push(DeployOp::Backup { from: state::host_path(self.game_dir, &node.path), to });
        rel
    }

    /// Claims a backup slot for `path`: its own rel path, or a name under
    /// `backup/stray/` when the slot already preserves something.
    fn claim_backup(&mut self, path: &VPath) -> String {
        let rel = path.to_string();
        if self.claimed_backups.insert(rel.clone()) {
            return rel;
        }
        let base = format!("{}{}", state::STRAY_DIR, path); // "stray" + "/a/b"
        if self.claimed_backups.insert(base.clone()) {
            return base;
        }
        for n in 1u32.. {
            let cand = format!("{base}.{n}");
            if self.claimed_backups.insert(cand.clone()) {
                return cand;
            }
        }
        unreachable!("backup slot names are unlimited")
    }

    /// Schedules `node`'s move into `backup/stray/` — for occupants that are
    /// neither the recorded original nor our own deployed bytes. Returns
    /// nothing: a stray is preserved but is nobody's "original", so it never
    /// lands in an entry's `backup` slot.
    fn stray_node(&mut self, node: &ScannedNode) {
        self.moved_nodes.insert(node.path.as_str().to_lowercase());
        let base = format!("{}{}", state::STRAY_DIR, node.path);
        let mut rel = base.clone();
        let mut n = 1u32;
        while !self.claimed_backups.insert(rel.clone()) {
            rel = format!("{base}.{n}");
            n += 1;
        }
        let to = self.backup_root.join(rel.trim_start_matches('/'));
        log::debug!("plan: stray {} -> backup/{rel} (preserved, never auto-restored)", node.path);
        self.plan.ops.push(DeployOp::Backup { from: state::host_path(self.game_dir, &node.path), to });
    }

    /// `true` when `path`'s streamed checksum equals `sum`.
    fn sum_matches(sum: &(u64, String), path: &Path) -> bool {
        state::sha256_file(path).is_ok_and(|other| other == *sum)
    }
}
