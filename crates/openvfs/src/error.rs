//! The error type shared by every backend.

use std::fmt;
use std::io;

use crate::path::VPath;

/// `Result` specialized to [`VfsError`].
pub type Result<T, E = VfsError> = std::result::Result<T, E>;

/// Everything that can go wrong in a virtual file system.
///
/// Backends map their native failures onto these variants so callers can
/// match on *what* happened without caring *where* the bytes live.
///
/// # The path contract
///
/// Path-carrying variants report the path **argument of the failed
/// operation** — the one you passed, in your namespace — never an internal
/// path (an ancestor, a temporary file, or a path inside a mounted backend).
/// For two-path operations the path is the argument that caused the failure:
/// `rename("/a", "/x/y")` with no `/x` fails with `NotFound("/x/y")`.
#[derive(Debug)]
#[non_exhaustive]
pub enum VfsError {
    /// The path does not exist.
    NotFound(VPath),
    /// The path already exists.
    AlreadyExists(VPath),
    /// A directory was required, but the path — or one of its ancestors — is a file.
    NotADirectory(VPath),
    /// A file was required, but the path is a directory.
    IsADirectory(VPath),
    /// The directory still has entries.
    DirectoryNotEmpty(VPath),
    /// The backend (or mount) does not accept writes.
    ReadOnly(VPath),
    /// The host or a policy (e.g. [`crate::Symlinks::Deny`]) refuses access.
    PermissionDenied(VPath),
    /// A quota or the underlying storage is exhausted.
    StorageFull(VPath),
    /// A rename crossed two different mounts.
    CrossDevice {
        /// Source of the rename.
        from: VPath,
        /// Destination of the rename.
        to: VPath,
    },
    /// The path resolves (e.g. through a symlink) outside the backend's root.
    PathEscapesRoot(VPath),
    /// The path cannot be represented by the backend (e.g. `CON` on Windows).
    InvalidPath(String),
    /// The operation's arguments are inconsistent.
    InvalidInput(&'static str),
    /// Any other I/O failure reported by the host.
    Io {
        /// The virtual path involved, when known.
        path: Option<VPath>,
        /// The underlying error.
        source: io::Error,
    },
}

impl VfsError {
    /// The virtual path the error refers to, when there is one.
    #[must_use]
    pub fn path(&self) -> Option<&VPath> {
        match self {
            Self::NotFound(p)
            | Self::AlreadyExists(p)
            | Self::NotADirectory(p)
            | Self::IsADirectory(p)
            | Self::DirectoryNotEmpty(p)
            | Self::ReadOnly(p)
            | Self::PermissionDenied(p)
            | Self::StorageFull(p)
            | Self::PathEscapesRoot(p) => Some(p),
            Self::CrossDevice { from, .. } => Some(from),
            Self::Io { path, .. } => path.as_ref(),
            Self::InvalidPath(_) | Self::InvalidInput(_) => None,
        }
    }

    /// Shorthand for `matches!(err, VfsError::NotFound(_))`.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    /// The closest [`io::ErrorKind`], for code that speaks `std::io`.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::NotFound(_) => io::ErrorKind::NotFound,
            Self::AlreadyExists(_) => io::ErrorKind::AlreadyExists,
            Self::NotADirectory(_) => io::ErrorKind::NotADirectory,
            Self::IsADirectory(_) => io::ErrorKind::IsADirectory,
            Self::DirectoryNotEmpty(_) => io::ErrorKind::DirectoryNotEmpty,
            Self::ReadOnly(_) => io::ErrorKind::ReadOnlyFilesystem,
            Self::PermissionDenied(_) | Self::PathEscapesRoot(_) => io::ErrorKind::PermissionDenied,
            Self::StorageFull(_) => io::ErrorKind::StorageFull,
            Self::CrossDevice { .. } => io::ErrorKind::CrossesDevices,
            Self::InvalidPath(_) | Self::InvalidInput(_) => io::ErrorKind::InvalidInput,
            Self::Io { source, .. } => source.kind(),
        }
    }

    /// Translates a host I/O error about `path` into the closest variant.
    pub(crate) fn from_io(path: &VPath, err: io::Error) -> Self {
        let p = path.clone();
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotFound(p),
            io::ErrorKind::AlreadyExists => Self::AlreadyExists(p),
            io::ErrorKind::NotADirectory => Self::NotADirectory(p),
            io::ErrorKind::IsADirectory => Self::IsADirectory(p),
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty(p),
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnly(p),
            io::ErrorKind::PermissionDenied => Self::PermissionDenied(p),
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => Self::StorageFull(p),
            _ => Self::Io { path: Some(p), source: err },
        }
    }

    /// Re-expresses paths reported by a mounted backend in the outer namespace.
    pub(crate) fn rebase(self, mount: &VPath) -> Self {
        self.map_paths(|p| mount.concat(&p))
    }

    /// Replaces the reported path with `path` (see the path contract above).
    pub(crate) fn with_path(self, path: &VPath) -> Self {
        match self {
            cross @ Self::CrossDevice { .. } => cross,
            Self::Io { source, .. } => Self::Io { path: Some(path.clone()), source },
            other => other.map_paths(|_| path.clone()),
        }
    }

    fn map_paths(self, f: impl Fn(VPath) -> VPath) -> Self {
        match self {
            Self::NotFound(p) => Self::NotFound(f(p)),
            Self::AlreadyExists(p) => Self::AlreadyExists(f(p)),
            Self::NotADirectory(p) => Self::NotADirectory(f(p)),
            Self::IsADirectory(p) => Self::IsADirectory(f(p)),
            Self::DirectoryNotEmpty(p) => Self::DirectoryNotEmpty(f(p)),
            Self::ReadOnly(p) => Self::ReadOnly(f(p)),
            Self::PermissionDenied(p) => Self::PermissionDenied(f(p)),
            Self::StorageFull(p) => Self::StorageFull(f(p)),
            Self::PathEscapesRoot(p) => Self::PathEscapesRoot(f(p)),
            Self::CrossDevice { from, to } => Self::CrossDevice { from: f(from), to: f(to) },
            Self::Io { path, source } => Self::Io { path: path.map(f), source },
            other @ (Self::InvalidPath(_) | Self::InvalidInput(_)) => other,
        }
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "no such file or directory: {p}"),
            Self::AlreadyExists(p) => write!(f, "already exists: {p}"),
            Self::NotADirectory(p) => write!(f, "not a directory: {p}"),
            Self::IsADirectory(p) => write!(f, "is a directory: {p}"),
            Self::DirectoryNotEmpty(p) => write!(f, "directory not empty: {p}"),
            Self::ReadOnly(p) => write!(f, "read-only file system: {p}"),
            Self::PermissionDenied(p) => write!(f, "permission denied: {p}"),
            Self::StorageFull(p) => write!(f, "no space left (quota or storage full): {p}"),
            Self::CrossDevice { from, to } => write!(f, "cannot move across mounts: {from} -> {to}"),
            Self::PathEscapesRoot(p) => write!(f, "path escapes the backend root: {p}"),
            Self::InvalidPath(p) => write!(f, "invalid path: {p:?}"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::Io { path: Some(p), source } => write!(f, "{p}: {source}"),
            Self::Io { path: None, source } => write!(f, "{source}"),
        }
    }
}

impl std::error::Error for VfsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for VfsError {
    fn from(source: io::Error) -> Self {
        Self::Io { path: None, source }
    }
}

impl From<VfsError> for io::Error {
    fn from(err: VfsError) -> Self {
        io::Error::new(err.kind(), err)
    }
}
