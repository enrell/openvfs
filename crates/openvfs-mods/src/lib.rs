//! # openvfs-mods — a mod manager core on top of `openvfs`
//!
//! The pieces a mod manager needs before it can deploy anything, expressed
//! entirely through the public `openvfs` API:
//!
//! * [`MergedIndex`] — one snapshot of the merged namespace over an ordered
//!   list of named layers (mods plus the base game), keyed by *folded* paths,
//!   so mods that mix `Textures/` and `textures/` really merge;
//! * [`ConflictReport`] — who wins each path provided by more than one layer,
//!   and how many files each mod wins or loses;
//! * [`CaseInsensitiveFs`] — a [`FileSystem`](openvfs::FileSystem) adapter
//!   that resolves lookups case-insensitively;
//! * [`Profile`] — a named, ordered mod list plus game and overwrite
//!   directories, saved as JSON, that assembles into an
//!   [`OverlayFs`](openvfs::OverlayFs).
//!
//! * [`Deployer`] — the deployment engine: diffs the merged tree against the
//!   real game directory and materializes it with hardlinks/copies, under a
//!   journaled, resumable, restorable transaction (see [`deploy`]).
//!
//! The model is the one popularized by mod-manager tools — priority-ordered
//! mods whose files merge into a single view, with an *overwrite* directory
//! capturing every write the game makes — reimplemented clean-room on the
//! documented behavior only.

#![warn(missing_docs)]

mod ci;
mod conflicts;
pub mod deploy;
mod error;
mod index;
mod profile;

pub use ci::CaseInsensitiveFs;
pub use conflicts::{ConflictEntry, ConflictReport, FileCount};
pub use deploy::{
    CaptureReport, CapturedFile, CorruptedFile, DeployMethod, DeployMode, DeployOp, DeployReport, Deployer,
    DeploymentPlan, EntryKind, ForeignFile, ForeignPolicy, Journal, JournalRecord, Manifest, ManifestEntry, Mismatch,
    MismatchKind, PlanOptions, PlannedFile,
};
pub use error::{Error, Result};
pub use index::{IndexEntry, Layer, MergedIndex, Provider};
pub use profile::{BuiltProfile, LoadedMod, ModEntry, Profile, SkippedMod};
