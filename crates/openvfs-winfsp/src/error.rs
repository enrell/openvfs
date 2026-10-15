//! The public error type of `openvfs-winfsp`.

use std::fmt;
use std::io;

use openvfs::VfsError;

/// `Result` specialized to [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while probing for or mounting through WinFsp.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The WinFsp runtime library (`winfsp-x64.dll` and friends) could not be
    /// loaded — WinFsp is probably not installed. See the crate docs.
    WinFspUnavailable,
    /// A WinFsp export expected by this crate is missing (bad/old WinFsp).
    WinFspMissingSymbol(&'static str),
    /// This platform has no WinFsp at all (non-Windows hosts).
    UnsupportedPlatform,
    /// The mount point is unusable (not an existing empty directory and not a
    /// `X:`-style drive letter).
    InvalidMountPoint(String),
    /// A WinFsp call returned a failing NTSTATUS; `op` says which one.
    NtStatus {
        /// The WinFsp entry point that failed, e.g. `"FspFileSystemCreate"`.
        op: &'static str,
        /// The raw `NTSTATUS` value.
        status: i32,
    },
    /// A backend operation failed while mounting (e.g. the root isn't a dir).
    Vfs(VfsError),
    /// Plain I/O failure (e.g. creating the mount-point directory).
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WinFspUnavailable => {
                write!(f, "WinFsp runtime not found — install WinFsp (https://winfsp.dev) to mount file systems")
            }
            Self::WinFspMissingSymbol(name) => write!(f, "WinFsp library is missing export {name}"),
            Self::UnsupportedPlatform => write!(f, "WinFsp mounts are only supported on Windows"),
            Self::InvalidMountPoint(p) => {
                write!(
                    f,
                    "invalid mount point {p:?}: expected an existing empty directory or a drive letter like \"X:\""
                )
            }
            Self::NtStatus { op, status } => write!(f, "{op} failed: NTSTATUS {status:#010x}"),
            Self::Vfs(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vfs(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
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
