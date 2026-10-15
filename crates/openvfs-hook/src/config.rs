//! Configuration for the hook layer.
//!
//! The injector points [`openvfs_hook_install`](crate::openvfs_hook_install) at
//! a JSON file through the `OPENVFS_HOOK_CONFIG` environment variable:
//!
//! ```json
//! {
//!   "virtual_root": "C:/game/Data",
//!   "overwrite": "C:/mods/overwrite",
//!   "layers": [
//!     { "name": "mod-high", "dir": "C:/mods/high", "enabled": true },
//!     { "name": "mod-low",  "dir": "C:/mods/low",  "enabled": true }
//!   ]
//! }
//! ```
//!
//! `layers[0]` is the highest-priority mod layer. The overwrite directory
//! outranks every layer; the real contents of `virtual_root` act as the
//! lowest-priority base layer.

use std::path::PathBuf;

use serde::Deserialize;

/// Top-level configuration loaded from `OPENVFS_HOOK_CONFIG`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Absolute path of the directory the game believes it is reading.
    pub virtual_root: PathBuf,
    /// Directory that receives every captured write inside `virtual_root`.
    pub overwrite: PathBuf,
    /// Mod layers in priority order: `layers[0]` wins.
    #[serde(default)]
    pub layers: Vec<Layer>,
}

/// One mod layer.
#[derive(Debug, Clone, Deserialize)]
pub struct Layer {
    /// Human-readable name, used in log output only.
    pub name: String,
    /// Absolute path of the layer's root directory.
    pub dir: PathBuf,
    /// Disabled layers are ignored entirely.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

const fn default_enabled() -> bool {
    true
}

impl Config {
    /// Parses a config file. Returns a human-readable error string.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        let cfg: Self = serde_json::from_str(&text).map_err(|e| format!("invalid config {}: {e}", path.display()))?;
        Ok(cfg)
    }
}
