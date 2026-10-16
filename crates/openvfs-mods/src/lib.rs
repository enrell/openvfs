//! # openvfs-mods — a mod manager core on top of `openvfs`
//!
//! Priority-ordered mods whose files merge into one case-insensitive view,
//! then materialize into the game directory as real files — the model
//! popularized by mod-manager tools, reimplemented clean-room on the
//! public `openvfs` API only.
//!
//! The pipeline, end to end:
//!
//! 1. **[`Profile`]** — a named, ordered mod list (`mods[0]` wins) plus the
//!    game and overwrite directories, saved as hand-editable JSON.
//!    [`Profile::build`] assembles it into an [`OverlayFs`](openvfs::OverlayFs)
//!    — mods over the game, under a writable *overwrite* upper that captures
//!    every write — and reports skipped mods whose dirs are missing.
//! 2. **[`MergedIndex`] / [`ConflictReport`]** — a snapshot of the merged
//!    namespace keyed by *folded* paths (so `Textures/` and `textures/`
//!    really merge), and who wins each path more than one layer provides.
//! 3. **[`Deployer`]** — diffs the merged tree against the real game dir
//!    and materializes it with hardlinks (or copies), journaled so a crash
//!    is resumable and rollback-able; see [`deploy`] and `docs/recovery.md`.
//! 4. **After the game runs** — [`Deployer::capture`] collects its writes
//!    into the overwrite dir and repairs deployment drift;
//!    [`Deployer::verify`] just reports it.
//! 5. **[`Deployer::undeploy`]** — removes the deployed files, restores
//!    displaced originals from `backup/`, returns the game dir
//!    byte-for-byte.
//!
//! ```no_run
//! # fn main() -> openvfs_mods::Result<()> {
//! use openvfs_mods::{Deployer, ModEntry, PlanOptions, Profile};
//!
//! let mut profile = Profile::new("my game", "C:/games/foo", "D:/profiles/foo/overwrite");
//! profile.add_mod(ModEntry { name: "HD textures".into(), source_dir: "D:/mods/hd".into(), enabled: true });
//!
//! let deployer = Deployer::new("D:/profiles/foo/state"); // outside the game dir
//! let report = deployer.deploy(&deployer.plan(&profile, PlanOptions::new())?)?;
//! // …run the game…
//! deployer.capture(Some(&profile.overwrite_dir))?;
//! deployer.undeploy(Some(&profile.overwrite_dir))?;
//! # let _ = report; Ok(())
//! # }
//! ```
//!
//! Logging goes through the [`log`] facade — `info!` at operation
//! boundaries, `debug!` per-op, `warn!` on recovery-relevant surprises;
//! binaries (see the `openvfs-mods` CLI) wire up the output.
//!
//! * [`CaseInsensitiveFs`] — a [`FileSystem`](openvfs::FileSystem) adapter
//!   resolving lookups case-insensitively, the other half of folded paths.
//! * A hardlink caveat applies to in-place game writes — see the [`deploy`]
//!   module docs before choosing `DeployMode::Link`.

#![warn(missing_docs)]

mod ci;
mod conflicts;
pub mod deploy;
mod error;
mod index;
pub mod install;
mod profile;

pub use ci::CaseInsensitiveFs;
pub use conflicts::{ConflictEntry, ConflictReport, FileCount};
pub use deploy::{
    CaptureReport, CapturedFile, CorruptedFile, DeployMethod, DeployMode, DeployOp, DeployReport, Deployer,
    DeploymentPlan, EntryKind, ForeignFile, ForeignPolicy, Journal, JournalRecord, Manifest, ManifestEntry, Mismatch,
    MismatchKind, OpPhase, PlanOptions, PlannedFile,
};
pub use error::{Error, Result};
pub use index::{IndexEntry, Layer, MergedIndex, Provider};
pub use install::{InstallOptions, Installed, install_mod};
pub use profile::{BuiltProfile, LoadedMod, ModEntry, Profile, SkippedMod};
