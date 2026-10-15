//! The crate's error type.

use std::fmt;
use std::io;
use std::path::PathBuf;

use openvfs::VfsError;

/// `Result` specialized to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while managing mods.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A failure inside the virtual file systems themselves.
    Vfs(VfsError),
    /// A host I/O failure outside the VFS (e.g. reading a profile file).
    Io(io::Error),
    /// A profile's JSON could not be parsed or written.
    Json(serde_json::Error),
    /// A directory a role depends on is missing or is not a directory.
    MissingDirectory {
        /// Which role needs it: `"base game"`, `"overwrite"`, ….
        role: &'static str,
        /// The host path that was expected to be a directory.
        path: PathBuf,
    },
    /// A deployment precondition failed, or the deployment state (journal,
    /// manifest) is inconsistent — e.g. an interrupted deploy that must be
    /// resumed or rolled back first.
    Deploy {
        /// What went wrong.
        reason: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vfs(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "invalid profile JSON: {e}"),
            Self::MissingDirectory { role, path } => {
                write!(f, "{role} directory does not exist: {}", path.display())
            }
            Self::Deploy { reason } => write!(f, "deployment: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vfs(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::MissingDirectory { .. } | Self::Deploy { .. } => None,
        }
    }
}

impl From<VfsError> for Error {
    fn from(e: VfsError) -> Self {
        Self::Vfs(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
