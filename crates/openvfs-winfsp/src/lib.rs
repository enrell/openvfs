//! Mount any [`openvfs::FileSystem`] as a real Windows filesystem via WinFsp.
//!
//! `openvfs-winfsp` hosts a user-mode WinFsp file system: Windows programs —
//! including unmodified binaries such as games — see the virtual tree as
//! ordinary files under a directory mount point or drive letter.
//!
//! ```no_run
//! # fn main() -> Result<(), openvfs_winfsp::Error> {
//! use openvfs::MemoryFs;
//! use openvfs_winfsp::{Mount, MountOptions};
//!
//! let fs = MemoryFs::new();
//! let mount = Mount::mount(fs, "X:".as_ref(), MountOptions::default())?;
//! // ... drive X: is live until `mount` drops
//! # Ok(())
//! # }
//! ```
//!
//! # Requirements
//!
//! * Windows with [WinFsp](https://winfsp.dev) 2.x installed
//!   (`winget install WinFsp.WinFsp`), plus a reboot when the installer asks.
//!   The runtime `winfsp-x64.dll` is `LoadLibrary`'d at mount time — **no SDK,
//!   import library or build-time headers are needed**, and this crate compiles
//!   and links on machines without WinFsp ([`Mount::is_available`] reports
//!   `false` there).
//! * The WinFsp service normally runs mounts without administrator rights for
//!   directory mount points. Drive-letter mounts and the *first* mount after
//!   install may need elevation while the driver/service warms up; membership
//!   in the `WinFsp` local group is the supported way to run unprivileged.
//!
//! # License caveat
//!
//! WinFsp itself is **GPLv3 with a FLOSS license exception** — dynamically
//! linking it (as this crate does at runtime) is what the exception is for,
//! but **redistributing `winfsp-*.dll` or the installer alongside a product
//! requires a commercial license or legal review**. This crate ships no WinFsp
//! code: it re-declares the documented public ABI in `abi.rs` and binds it
//! dynamically.
//!
//! # Windows semantics provided
//!
//! * Case-insensitive lookup with case preservation (`openvfs` paths are
//!   case-sensitive, so the host layer folds names with Unicode lowercase and
//!   resolves them to the real-cased [`openvfs::VPath`] via directory scans + a hint
//!   cache — see `pathx.rs`/`host.rs`).
//! * Per-open file descriptors (`UmFileContextIsUserContext2`), so I/O,
//!   `SetFileSize`, `Overwrite` and friends act on independent cursors.
//! * `FILE_ATTRIBUTE_*` (hidden/system/archive/readonly…), four file times and
//!   a stable `IndexNumber` — kept in a session-local side map where
//!   [`openvfs::Metadata`] can't express them (`attrs.rs`). Nothing is written
//!   back: attributes survive for the mount's lifetime only.
//! * A permissive self-relative security descriptor
//!   (`Everyone: Full Control`, see `security.rs`) — openvfs has no ACL model.
//!   The `AccessCheck` path is short-circuited (empty `GetSecurityByName`
//!   descriptor), which is the documented escape hatch for ACL-free volumes.
//! * Deferred delete (`Cleanup(FspCleanupDelete)`), `CanDelete`/`SetDelete`
//!   validation, POSIX-style `Rename` conflict handling.
//!
//! ## Deliberately not supported
//!
//! Named streams (`file:stream`), reparse points/symlinks, hard links,
//! extended attributes, ACL persistence, `Control` (custom ioctls), and
//! `CreateEx`/`OverwriteEx` (extra-create-parameter plumbing). Their vtable
//! slots are NULL; WinFsp then answers the kernel request itself or returns
//! `STATUS_NOT_SUPPORTED` — a graceful degradation. Sharing-mode conflicts are
//! enforced by the kernel-side FSD, which is exactly what Windows programs
//! expect.
//!
//! # Callback coverage
//!
//! | Implemented | Stubbed (NULL slot) |
//! |---|---|
//! | `GetVolumeInfo`, `SetVolumeLabel` | `ResolveReparsePoints`, `Get/Set/DeleteReparsePoint` |
//! | `GetSecurityByName` (attrs only) | `GetStreamInfo` (no named streams) |
//! | `Create`, `Open`, `Overwrite` | `Control` (no custom ioctls) |
//! | `Cleanup`, `Close` | `CreateEx`, `OverwriteEx` (no extra params) |
//! | `Read`, `Write`, `Flush` | `GetEa`/`SetEa` (no EAs) |
//! | `GetFileInfo`, `SetBasicInfo`, `SetFileSize` | `Obsolete0` |
//! | `CanDelete`, `SetDelete`, `Rename` | all 31 `Reserved` slots |
//! | `GetSecurity` (permissive SD), `SetSecurity` (no-op) | |
//! | `ReadDirectory` (markers + `.`/`..`), `GetDirInfoByName` | |
//! | `DispatcherStopped` | |
//!
//! # Safety notes
//!
//! Unsafe code is confined to `sys.rs` (runtime `LoadLibrary`/`GetProcAddress`
//! binding) and `cb.rs` (the `extern "C"` trampolines that decode raw pointers
//! into Rust values); both carry `// SAFETY:` justifications per block.
//! Everything else — path conversion, folding, attributes, statuses, the
//! whole `Host` bridge — is safe Rust and unit-testable without the driver.
//! This crate therefore does **not** inherit the workspace's
//! `unsafe_code = "forbid"` (FFI is the point); `#![deny(unsafe_op_in_unsafe_fn)]`
//! keeps the unsafe surface explicit instead.

#![deny(unsafe_op_in_unsafe_fn)]

// ---------------------------------------------------------------------------
// Platform-independent modules — these compile and test everywhere so the
// bulk of the logic is verifiable on machines without WinFsp (and on CI).
// ---------------------------------------------------------------------------

pub mod abi;
pub mod attrs;
pub mod dlog;
mod error;
pub mod host;
mod options;
pub mod pathx;
pub mod security;
pub mod status;

pub use error::{Error, Result};
pub use options::MountOptions;

// ---------------------------------------------------------------------------
// Windows: the real binding (DLL loader, FFI trampolines, Mount).
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod cb;
#[cfg(windows)]
mod mount;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use mount::Mount;

/// `true` when the WinFsp runtime library can be loaded on this machine —
/// i.e. [`Mount::mount`] could possibly succeed. On non-Windows hosts this is
/// always `false`. Cheap and idempotent.
#[must_use]
#[cfg(windows)]
pub fn is_available() -> bool {
    Mount::is_available()
}

// ---------------------------------------------------------------------------
// Non-Windows stub: same API surface, every mount attempt fails with
// `Error::UnsupportedPlatform`, probes return `false`.
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod stub {
    use std::path::Path;

    use openvfs::FileSystem;

    use crate::error::Error;
    use crate::options::MountOptions;

    /// Stub [`Mount`](crate::Mount) for non-Windows platforms.
    ///
    /// Exists so code written against this crate compiles cross-platform;
    /// every operation reports [`Error::UnsupportedPlatform`].
    pub struct Mount {
        _private: (),
    }

    impl Mount {
        /// Always fails with [`Error::UnsupportedPlatform`] — WinFsp exists
        /// only on Windows.
        #[allow(clippy::self_named_constructors)] // `Mount::mount(fs, point, opts)` is the public API
        pub fn mount<F>(fs: F, mountpoint: &Path, opts: MountOptions) -> Result<Self, Error>
        where
            F: FileSystem + 'static,
        {
            let _ = (fs, mountpoint, opts);
            Err(Error::UnsupportedPlatform)
        }

        /// Always `false` on non-Windows.
        #[must_use]
        pub fn is_available() -> bool {
            false
        }

        /// Always `false` on non-Windows.
        #[must_use]
        pub fn probe() -> bool {
            false
        }

        /// Always `""` — there is no mount point on non-Windows.
        #[must_use]
        pub fn mountpoint(&self) -> &str {
            ""
        }
    }
}

#[cfg(not(windows))]
pub use stub::Mount;

/// `false` — WinFsp exists only on Windows.
#[must_use]
#[cfg(not(windows))]
pub fn is_available() -> bool {
    false
}
