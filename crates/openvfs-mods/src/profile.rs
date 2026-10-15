//! A named, ordered mod list that assembles into an [`OverlayFs`].
//!
//! A [`Profile`] is the mod manager's persistent state: which mods exist,
//! where they live on disk, in which order they win, and which are enabled —
//! plus the two directories every view needs: the *game directory* (the
//! unmodded files, always the lowest layer) and the *overwrite directory*
//! (the writable upper layer that captures everything the game writes, so
//! neither mods nor game files are ever touched).
//!
//! [`Profile::build`] assembles that into an [`OverlayFs`] and hands back the
//! [`Arc`]s, so the caller keeps reaching [`OverlayFs::changes`],
//! [`OverlayFs::commit`] and friends. Profiles save as plain JSON for
//! hand-editing.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openvfs::{FileSystem, OverlayFs, PhysicalFs, Vfs};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::index::{Layer, MergedIndex};

/// One mod in a [`Profile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModEntry {
    /// Display name; also the layer name in indexes and reports.
    pub name: String,
    /// Host directory holding the mod's files, used as a virtual `/`.
    pub source_dir: PathBuf,
    /// Disabled mods keep their place in the order but contribute no files.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// A mod that was skipped by [`Profile::build`], with the reason.
#[derive(Debug, Clone)]
pub struct SkippedMod {
    /// The mod's name.
    pub name: String,
    /// The directory the profile expected it at.
    pub source_dir: PathBuf,
    /// Why it was skipped (e.g. `"directory does not exist"`).
    pub reason: String,
}

impl fmt::Display for SkippedMod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} ({}): {}", self.name, self.source_dir.display(), self.reason)
    }
}

/// A named, ordered mod list: `mods[0]` has the highest priority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// The profile's name.
    pub name: String,
    /// Host directory of the unmodded game files — always the lowest layer.
    pub game_dir: PathBuf,
    /// Host directory that captures every write (created on `build` if missing).
    pub overwrite_dir: PathBuf,
    /// Ordered mods: `mods[0]` wins conflicts, the last one loses to all.
    #[serde(default)]
    pub mods: Vec<ModEntry>,
}

impl Profile {
    /// An empty profile.
    #[must_use]
    pub fn new(name: impl Into<String>, game_dir: impl Into<PathBuf>, overwrite_dir: impl Into<PathBuf>) -> Self {
        Self { name: name.into(), game_dir: game_dir.into(), overwrite_dir: overwrite_dir.into(), mods: Vec::new() }
    }

    /// Appends a mod at the lowest priority.
    pub fn add_mod(&mut self, entry: ModEntry) {
        self.mods.push(entry);
    }

    /// Inserts a mod at `index` (`0` = highest priority), shifting the rest.
    /// Clamped to the list's bounds.
    pub fn insert_mod(&mut self, index: usize, entry: ModEntry) {
        self.mods.insert(index.min(self.mods.len()), entry);
    }

    /// Removes and returns the mod called `name`.
    pub fn remove_mod(&mut self, name: &str) -> Option<ModEntry> {
        let index = self.mods.iter().position(|m| m.name == name)?;
        Some(self.mods.remove(index))
    }

    /// Moves `mods[from]` to `to` (`0` = highest priority). Out-of-range
    /// indices are clamped; a missing `from` is a no-op.
    pub fn move_mod(&mut self, from: usize, to: usize) {
        if from >= self.mods.len() {
            return;
        }
        let entry = self.mods.remove(from);
        self.mods.insert(to.min(self.mods.len()), entry);
    }

    /// Enables or disables the mod called `name`; `false` if there is none.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.mods.iter_mut().find(|m| m.name == name) {
            Some(entry) => {
                entry.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// The enabled mods, in priority order.
    pub fn enabled_mods(&self) -> impl Iterator<Item = &ModEntry> {
        self.mods.iter().filter(|m| m.enabled)
    }

    /// Serializes the profile as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parses a profile from JSON.
    pub fn from_json(json: &str) -> Result<Self> {
        Ok(serde_json::from_str(json)?)
    }

    /// Writes the profile's JSON to `path` (a plain write, not atomic —
    /// profiles change rarely and are cheap to rewrite).
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        Ok(std::fs::write(path, self.to_json()?)?)
    }

    /// Reads a profile saved by [`Profile::save`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_json(&std::fs::read_to_string(path)?)
    }

    /// Assembles the deployed view:
    ///
    /// * **upper** = `PhysicalFs` over `overwrite_dir` (created if missing);
    /// * **lowers** = enabled mods in priority order over the `game_dir`,
    ///   which is always lowest.
    ///
    /// Mod and game layers are opened *read-only*: writes can only ever land
    /// in the overwrite. An **enabled mod whose directory is missing is
    /// skipped**, not fatal — it lands in [`BuiltProfile::skipped`] so the UI
    /// can warn; a missing game directory is a hard [`Error::MissingDirectory`].
    pub fn build(&self) -> Result<BuiltProfile> {
        if !self.game_dir.is_dir() {
            return Err(Error::MissingDirectory { role: "base game", path: self.game_dir.clone() });
        }
        if self.overwrite_dir.exists() && !self.overwrite_dir.is_dir() {
            return Err(Error::MissingDirectory { role: "overwrite", path: self.overwrite_dir.clone() });
        }
        std::fs::create_dir_all(&self.overwrite_dir)?;

        let upper: Arc<dyn FileSystem> = Arc::new(PhysicalFs::new(&self.overwrite_dir)?);
        let mut mods = Vec::new();
        let mut skipped = Vec::new();
        let mut lowers: Vec<Arc<dyn FileSystem>> = Vec::new();
        for entry in self.enabled_mods() {
            match PhysicalFs::read_only(&entry.source_dir) {
                Ok(fs) => {
                    let fs: Arc<dyn FileSystem> = Arc::new(fs);
                    lowers.push(Arc::clone(&fs));
                    mods.push(LoadedMod { name: entry.name.clone(), source_dir: entry.source_dir.clone(), fs });
                }
                Err(e) => skipped.push(SkippedMod {
                    name: entry.name.clone(),
                    source_dir: entry.source_dir.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        let base: Arc<dyn FileSystem> = Arc::new(PhysicalFs::read_only(&self.game_dir)?);
        lowers.push(Arc::clone(&base));

        Ok(BuiltProfile { overlay: Arc::new(OverlayFs::with_layers(upper, lowers)), mods, base, skipped })
    }

    /// [`Profile::build`], reduced to just the merged view. Prefer `build`
    /// when the skipped-mod diagnostics matter.
    pub fn vfs(&self) -> Result<Vfs> {
        Ok(self.build()?.vfs())
    }
}

/// An enabled mod that [`Profile::build`] found on disk and layered in.
#[derive(Clone)]
pub struct LoadedMod {
    name: String,
    source_dir: PathBuf,
    fs: Arc<dyn FileSystem>,
}

impl LoadedMod {
    /// The mod's name, as in the profile.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The host directory backing the mod's layer.
    #[must_use]
    pub fn source_dir(&self) -> &Path {
        &self.source_dir
    }

    /// The read-only layer serving the mod's files.
    #[must_use]
    pub fn fs(&self) -> &Arc<dyn FileSystem> {
        &self.fs
    }
}

impl fmt::Debug for LoadedMod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedMod")
            .field("name", &self.name)
            .field("source_dir", &self.source_dir)
            .finish_non_exhaustive()
    }
}

/// What [`Profile::build`] assembled: the overlay plus a handle to every
/// layer, so callers can reach [`OverlayFs::changes`], [`OverlayFs::commit`]
/// or build a [`MergedIndex`] over the same set.
pub struct BuiltProfile {
    overlay: Arc<OverlayFs>,
    mods: Vec<LoadedMod>,
    base: Arc<dyn FileSystem>,
    skipped: Vec<SkippedMod>,
}

impl BuiltProfile {
    /// The assembled overlay: overwrite upper over mods over the game.
    #[must_use]
    pub fn overlay(&self) -> &Arc<OverlayFs> {
        &self.overlay
    }

    /// The merged view as an ergonomic [`Vfs`].
    #[must_use]
    pub fn vfs(&self) -> Vfs {
        Vfs::from(Arc::clone(&self.overlay))
    }

    /// The writable upper layer (`PhysicalFs` over the overwrite directory).
    #[must_use]
    pub fn upper(&self) -> &Arc<dyn FileSystem> {
        self.overlay.upper()
    }

    /// The enabled, present mods as layers, highest priority first.
    #[must_use]
    pub fn mods(&self) -> &[LoadedMod] {
        &self.mods
    }

    /// The base game layer (lowest priority).
    #[must_use]
    pub fn base(&self) -> &Arc<dyn FileSystem> {
        &self.base
    }

    /// Enabled mods skipped because their directory could not be opened.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedMod] {
        &self.skipped
    }

    /// The layers a [`MergedIndex`] should cover: the loaded mods in
    /// priority order, then the base game under the name `"base game"`.
    #[must_use]
    pub fn layers(&self) -> Vec<Layer> {
        self.mods
            .iter()
            .map(|m| Layer::new(m.name.clone(), Arc::clone(&m.fs)))
            .chain(std::iter::once(Layer::new("base game", Arc::clone(&self.base))))
            .collect()
    }

    /// Builds a [`MergedIndex`] over [`BuiltProfile::layers`].
    pub fn index(&self) -> Result<MergedIndex> {
        MergedIndex::build(self.layers())
    }
}

impl fmt::Debug for BuiltProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltProfile")
            .field("mods", &self.mods)
            .field("skipped", &self.skipped.len())
            .finish_non_exhaustive()
    }
}
