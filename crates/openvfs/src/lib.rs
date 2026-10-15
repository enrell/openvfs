//! # openvfs — a layered virtual file system
//!
//! `openvfs` gives your program one file-system API with many possible
//! storage layers behind it:
//!
//! | Backend          | What it is                                                         |
//! |------------------|--------------------------------------------------------------------|
//! | [`MemoryFs`]     | Everything in RAM, atomic operations, optional byte quota.         |
//! | [`PhysicalFs`]   | A host directory, confined so paths and links can't escape it.     |
//! | [`ReadOnlyFs`]   | Wraps any backend and rejects writes.                              |
//! | [`MountFs`]      | A mount table composing backends into one namespace.               |
//! | [`OverlayFs`]    | Copy-on-write layers with change tracking and commit.              |
//!
//! All of them implement the small [`FileSystem`] trait and pass one shared
//! conformance and differential test suite, so code written against [`Vfs`]
//! behaves the same wherever the bytes live. Files are plain
//! [`std::io::Read`] + [`Write`](std::io::Write) + [`Seek`](std::io::Seek)
//! objects ([`VfsFile`]), so they work with any existing Rust I/O code.
//!
//! ```
//! use openvfs::prelude::*;
//!
//! // Compose: an in-memory root with a read-only area mounted inside it.
//! let mounts = MountFs::new();
//! mounts.mount("/", MemoryFs::new())?;
//! mounts.mount("/assets", ReadOnlyFs::new(MemoryFs::new()))?;
//!
//! let vfs = Vfs::new(mounts);
//! vfs.create_dir_all("/save/slot1")?;
//! vfs.write_atomic("/save/slot1/state.json", r#"{"level": 3}"#)?;
//! assert!(vfs.is_dir("/assets"));
//! assert!(vfs.write("/assets/hack.txt", "").is_err());
//! # Ok::<(), openvfs::VfsError>(())
//! ```
//!
//! ## Paths
//!
//! Every path goes through [`VPath`]: absolute, `/`-separated, normalized,
//! and unable to climb above the root. Backends never see `..`.
//!
//! ## Errors
//!
//! Every failure is a typed [`VfsError`] that names the path you passed in,
//! and converts losslessly into [`std::io::Error`].

#![warn(missing_docs)]

mod error;
mod fs;
mod memory;
mod mount;
pub mod ops;
mod overlay;
mod path;
mod physical;
mod readonly;
mod sync;
mod vfs;

pub use error::{Result, VfsError};
pub use fs::{DirEntry, FileSystem, FileType, Metadata, OpenOptions, VfsFile};
pub use memory::MemoryFs;
pub use mount::MountFs;
pub use ops::Walk;
pub use overlay::{Change, OverlayFs};
pub use path::VPath;
pub use physical::{PhysicalFs, PhysicalFsBuilder, Symlinks};
pub use readonly::ReadOnlyFs;
pub use vfs::Vfs;

/// Everything needed for typical use: `use openvfs::prelude::*;`.
pub mod prelude {
    pub use crate::{
        Change, DirEntry, FileSystem, FileType, MemoryFs, Metadata, MountFs, OpenOptions, OverlayFs, PhysicalFs,
        ReadOnlyFs, Symlinks, VPath, Vfs, VfsError, VfsFile,
    };
}
