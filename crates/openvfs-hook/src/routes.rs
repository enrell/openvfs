//! The per-process redirect map.
//!
//! [`Routes`] decides where a path under the virtual root really lives. It is
//! a standalone folded-path map rather than a reuse of `open-vfs`'s
//! `OverlayFs`: hook detours need to translate *absolute host paths* into
//! other *host paths* with the fewest possible syscalls and allocations on the
//! hot path, while `OverlayFs`/`PhysicalFs` resolve virtual `VPath`s
//! component-by-component through the `FileSystem` trait. The semantics are
//! deliberately the same, though — merged view, layer priority, overwrite
//! capture, whiteouts — so the observable behavior matches the library model.
//!
//! All file-system probing here uses `std::fs`, which is safe to call from
//! inside a detour **only** while the recursion guard
//! ([`crate::guard`]) is held: the guard makes the detours pass everything
//! straight through to the real OS functions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Marker suffix for whiteouts inside the overwrite layer. A zero-byte file
/// named `<name>.ovh-deleted` inside `overwrite/<dir>/` hides the
/// corresponding `<name>` from all lower layers.
pub const WHITEOUT_SUFFIX: &str = ".ovh-deleted";

/// A single mod layer: a directory projected at the virtual root.
#[derive(Debug)]
pub struct Layer {
    /// Name used in log output.
    pub name: String,
    /// Layer root directory.
    pub dir: PathBuf,
}

/// The resolved redirect table for this process.
///
/// Priority order (highest first): **overwrite**, `layers[0]` … `layers[n]`,
/// then the physical contents of `virtual_root` (the base game files).
#[derive(Debug)]
pub struct Routes {
    /// The physical base directory behind the virtual root (lowest priority).
    root: PathBuf,
    /// Folded components of `root`, used for prefix matching
    /// (`["c:", "game", "data"]`).
    root_comps: Vec<String>,
    /// Directory that receives captured writes (highest priority).
    overwrite: PathBuf,
    /// Mod layers, `layers[0]` highest priority.
    layers: Vec<Layer>,
}

/// Lowercases a path string with Unicode-aware case folding.
///
/// Windows compares file names case-insensitively per its own folding table;
/// `char::to_lowercase` approximates it. For ASCII names the result is
/// identical. Documented limitation: exotic characters whose Windows fold
/// differs from Unicode lowercase can alias incorrectly.
pub fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// `true` for `C:\`-style absolute paths (or `C:/`, normalized later).
fn is_drive_absolute(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

/// `true` for drive-relative (`C:foo`) paths — never virtualized.
fn is_drive_relative(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && !is_drive_absolute(s)
}

/// Converts a Win32 path to a normalized absolute form with `\` separators.
///
/// Handles `\\?\` / `\\.\` prefixes, `\\?\UNC\`, `/` separators, `.`/`..`
/// components, duplicate separators and a trailing backslash. Relative paths
/// are resolved against the process current directory; drive-relative
/// (`C:foo`) paths return `None` (the caller passes them through).
///
/// This performs no I/O: it neither canonicalizes links nor hits the disk.
pub fn normalize(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_owned();
    // Strip NT-style extended prefixes. `\\?\UNC\server\share` -> `\\server\share`.
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_owned();
        if let Some(unc) = s.strip_prefix("UNC\\") {
            s = format!("\\\\{unc}");
        }
    } else if let Some(rest) = s.strip_prefix("\\\\.\\") {
        s = rest.to_owned();
    }
    s = s.replace('/', "\\");

    if !s.starts_with("\\\\") && !is_drive_absolute(&s) {
        if is_drive_relative(&s) {
            return None; // drive-relative: bail, let the real API decide
        }
        if s.is_empty() {
            return None;
        }
        // Relative path: join with the process CWD, then normalize.
        let cwd = std::env::current_dir().ok()?;
        s = format!("{}\\{s}", cwd.to_string_lossy().trim_end_matches('\\'));
        // Re-run prefix normalization on the joined string.
        return normalize(&s);
    }

    // Split into head ("C:" or "\\server\share") + components.
    let (head, rest) = if let Some(stripped) = s.strip_prefix("\\\\") {
        // UNC: \\server\share\rest — the share name is part of the root.
        let mut it = stripped.splitn(3, '\\');
        let server = it.next()?;
        let share = it.next().unwrap_or("");
        if server.is_empty() || share.is_empty() {
            return None;
        }
        (format!("\\\\{server}\\{share}"), it.next().unwrap_or("").to_owned())
    } else {
        // Drive-absolute: "C:\rest" -> head "C:".
        (s[..2].to_owned(), s[3..].to_string())
    };

    let mut comps: Vec<&str> = Vec::new();
    for comp in rest.split('\\') {
        match comp {
            "" | "." => {}
            ".." => {
                comps.pop(); // cannot climb above root: root has no components
            }
            c => comps.push(c),
        }
    }

    let mut out = head;
    if comps.is_empty() {
        out.push('\\'); // bare root: "C:\" / "\\srv\share\"
    } else {
        out.push('\\');
        out.push_str(&comps.join("\\"));
    }
    Some(out)
}

/// Normalizes and case-folds `raw`.
#[allow(dead_code)] // used by unit tests / future diagnostics
pub fn normalize_folded(raw: &str) -> Option<String> {
    normalize(raw).map(|s| fold(&s))
}

/// Splits a folded normalized path string into path components.
fn components_of(rel: &str) -> impl Iterator<Item = &str> {
    rel.split('\\').filter(|c| !c.is_empty())
}

/// Joins a folded relative path (`a\b\c`) below `base`.
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for c in components_of(rel) {
        p.push(c);
    }
    p
}

impl Routes {
    /// Builds the map from a loaded [`Config`].
    ///
    /// `virtual_root` and `overwrite` must be existing directories; missing or
    /// disabled layers are dropped silently (a layer dir that appears later
    /// simply resolves nothing).
    pub fn new(cfg: &Config) -> Result<Self, String> {
        let root_raw = cfg.virtual_root.to_str().ok_or_else(|| "virtual_root is not valid UTF-16".to_string())?;
        let root = std::fs::canonicalize(&cfg.virtual_root)
            .map_err(|e| format!("virtual_root {}: {e}", cfg.virtual_root.display()))?;
        if !root.is_dir() {
            return Err(format!("virtual_root {} is not a directory", cfg.virtual_root.display()));
        }
        let root_norm = normalize(&root.to_string_lossy()).ok_or("cannot normalize virtual_root")?;
        let overwrite =
            std::fs::canonicalize(&cfg.overwrite).map_err(|e| format!("overwrite {}: {e}", cfg.overwrite.display()))?;
        if !overwrite.is_dir() {
            return Err(format!("overwrite {} is not a directory", cfg.overwrite.display()));
        }
        let mut layers = Vec::new();
        for l in &cfg.layers {
            if !l.enabled {
                continue;
            }
            let dir = std::fs::canonicalize(&l.dir).unwrap_or_else(|_| l.dir.clone());
            layers.push(Layer { name: l.name.clone(), dir });
        }
        let routes = Self {
            root,
            root_comps: fold(&root_norm).split('\\').filter(|c| !c.is_empty()).map(str::to_owned).collect(),
            overwrite,
            layers,
        };
        let _ = root_raw;
        Ok(routes)
    }

    /// The normalized virtual root (with trailing `\`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// If `raw` names a path under the virtual root, returns the relative
    /// path below it (`"a\\B.txt"`; `""` for the root itself) in the
    /// *original* casing, so captured files keep the name the app used.
    /// Prefix matching is case-folded per component.
    pub fn rel_under_root(&self, raw: &str) -> Option<String> {
        let norm = normalize(raw)?;
        let comps: Vec<&str> = norm.split('\\').filter(|c| !c.is_empty()).collect();
        // The drive component "C:" / UNC head is component 0.
        if comps.len() < self.root_comps.len() {
            return None;
        }
        for (have, want) in comps.iter().zip(&self.root_comps) {
            if fold(have) != *want {
                return None;
            }
        }
        Some(comps[self.root_comps.len()..].join("\\"))
    }

    /// The physical overwrite path for a folded relative path.
    pub fn overwrite_path(&self, rel: &str) -> PathBuf {
        join_rel(&self.overwrite, rel)
    }

    /// The physical base (real `virtual_root`) path for a relative path.
    pub fn base_path(&self, rel: &str) -> PathBuf {
        join_rel(&self.root, rel)
    }

    /// The whiteout marker path for a relative path inside the overwrite dir.
    fn marker_path(&self, rel: &str) -> PathBuf {
        let mut p = self.overwrite.to_path_buf();
        let mut comps = components_of(rel).peekable();
        while let Some(c) = comps.next() {
            if comps.peek().is_none() {
                p.push(format!("{c}{WHITEOUT_SUFFIX}"));
            } else {
                p.push(c);
            }
        }
        p
    }

    /// `true` if `rel` (or an ancestor of it) is whited out.
    pub fn is_deleted(&self, rel: &str) -> bool {
        let mut probe = String::new();
        for c in components_of(rel) {
            if !probe.is_empty() {
                probe.push('\\');
            }
            probe.push_str(c);
            if self.marker_path(&probe).exists() {
                return true;
            }
        }
        false
    }

    /// Writes a whiteout marker for `rel` into the overwrite layer.
    pub fn mark_deleted(&self, rel: &str) -> std::io::Result<()> {
        let marker = self.marker_path(rel);
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Empty file; existence is the whole signal.
        std::fs::File::create(&marker).map(|_| ())
    }

    /// Removes the whiteout marker for `rel`, if present.
    pub fn clear_deleted(&self, rel: &str) {
        let _ = std::fs::remove_file(self.marker_path(rel));
    }

    /// `true` if a layer below the overwrite dir provides `rel`.
    pub fn lower_provides(&self, rel: &str) -> bool {
        self.layers.iter().any(|l| join_rel(&l.dir, rel).exists()) || self.base_path(rel).exists()
    }

    /// Resolves `rel` for a read: highest-priority provider, honoring
    /// whiteouts. Returns `None` if nothing (still) provides it.
    pub fn resolve_read(&self, rel: &str) -> Option<PathBuf> {
        if rel.is_empty() {
            return Some(self.root.clone());
        }
        let ov = self.overwrite_path(rel);
        if ov.exists() {
            return Some(ov);
        }
        if self.is_deleted(rel) {
            return None;
        }
        for layer in &self.layers {
            let p = join_rel(&layer.dir, rel);
            if p.exists() {
                return Some(p);
            }
        }
        let base = self.base_path(rel);
        base.exists().then_some(base)
    }

    /// `true` if `path` lives inside the overwrite directory.
    pub fn is_in_overwrite(&self, path: &Path) -> bool {
        path.starts_with(&self.overwrite)
    }

    /// `true` if `rel` is a directory in the merged view.
    pub fn is_dir(&self, rel: &str) -> bool {
        self.resolve_read(rel).is_some_and(|p| p.is_dir())
    }

    /// `true` if `rel` exists in the merged view.
    pub fn exists(&self, rel: &str) -> bool {
        self.resolve_read(rel).is_some()
    }

    /// Which source currently provides `rel`, if any.
    #[allow(dead_code)] // diagnostic helper for logging/debugging
    pub fn provider(&self, rel: &str) -> Option<Provider<'_>> {
        if rel.is_empty() {
            return Some(Provider::Base);
        }
        if self.overwrite_path(rel).exists() {
            return Some(Provider::Overwrite);
        }
        if self.is_deleted(rel) {
            return None;
        }
        for (i, layer) in self.layers.iter().enumerate() {
            if join_rel(&layer.dir, rel).exists() {
                return Some(Provider::Layer(i, &layer.name));
            }
        }
        self.base_path(rel).exists().then_some(Provider::Base)
    }

    /// Merged listing of the virtual directory `rel_dir`.
    ///
    /// Returns `(visible entries, dir_exists)` where each entry is the
    /// file name as the winning layer reports it plus the real host path to
    /// query attributes from. Whiteout markers are hidden; a real overwrite
    /// file always beats a stale marker with the same name.
    pub fn enumerate(&self, rel_dir: &str) -> (Vec<MergedEntry>, bool) {
        // folded name -> (display name, real path, is_dir, from_overwrite)
        let mut merged: BTreeMap<String, (String, PathBuf, bool, bool)> = BTreeMap::new();
        let mut dir_exists = false;

        // Sources in ascending priority: base, layers (lowest first), overwrite.
        let base_dir = join_rel(&self.root, rel_dir);
        scan_dir(&base_dir, false, &mut merged, &mut dir_exists, &mut |_| {});
        for layer in self.layers.iter().rev() {
            let dir = join_rel(&layer.dir, rel_dir);
            scan_dir(&dir, false, &mut merged, &mut dir_exists, &mut |_| {});
        }
        let ov_dir = join_rel(&self.overwrite, rel_dir);
        let mut hidden: BTreeSet<String> = BTreeSet::new();
        scan_dir(&ov_dir, true, &mut merged, &mut dir_exists, &mut |folded_name| {
            hidden.insert(folded_name);
        });

        // Apply whiteouts: a marker hides everything except a real overwrite
        // entry of the same name (which means the file was recreated).
        merged.retain(|folded, v| !(hidden.contains(folded) && !v.3));
        // An ancestor-dir whiteout also hides the entire directory.
        if self.is_deleted(rel_dir) && !merged.values().any(|v| v.3) && !ov_dir.exists() {
            dir_exists = false;
            merged.clear();
        }

        let entries = merged.into_values().map(|(name, real, is_dir, _)| MergedEntry { name, real, is_dir }).collect();
        (entries, dir_exists || self.is_dir(rel_dir) || rel_dir.is_empty())
    }

    /// Ensures the overwrite parent directory chain for `rel` exists.
    pub fn ensure_overwrite_parents(&self, rel: &str) -> std::io::Result<()> {
        if let Some(parent) = self.overwrite_path(rel).parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

/// Which source provides a path — for logging and mutation decisions.
#[derive(Debug)]
#[allow(dead_code)] // see `provider()`
pub enum Provider<'a> {
    /// The overwrite layer.
    Overwrite,
    /// Mod layer at index `usize`.
    Layer(usize, &'a str),
    /// The physical base directory.
    Base,
}

/// One entry in a merged directory listing.
#[derive(Debug, Clone)]
pub struct MergedEntry {
    /// Displayed file name (case as stored by the winning layer).
    pub name: String,
    /// Real host path that provides this entry (for attribute queries).
    pub real: PathBuf,
    /// Whether the entry is a directory.
    pub is_dir: bool,
}

/// Scans `dir` and inserts entries into `merged` under folded names.
///
/// `from_overwrite` marks entries (and `.ovh-deleted` markers are diverted
/// into `on_marker` instead of `merged`).
fn scan_dir(
    dir: &Path,
    from_overwrite: bool,
    merged: &mut BTreeMap<String, (String, PathBuf, bool, bool)>,
    dir_exists: &mut bool,
    on_marker: &mut dyn FnMut(String),
) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    *dir_exists = true;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(hidden_name) = name.strip_suffix(WHITEOUT_SUFFIX) {
            if from_overwrite {
                on_marker(fold(hidden_name));
            }
            continue; // marker namespace is invisible everywhere
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        merged.insert(fold(&name), (name, entry.path(), is_dir, from_overwrite));
    }
}
