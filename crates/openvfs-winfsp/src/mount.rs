//! The public mount API: [`Mount`] and its lifetime rules.
//!
//! `Mount::mount` brings a [`FileSystem`] online under a directory or drive
//! letter. Dropping the `Mount` stops the dispatcher, removes the mount point,
//! and tears the file-system object down — in that order, because the
//! dispatcher must be silent before the `Host` (the callbacks' `user_context`)
//! is dropped.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;

use openvfs::FileSystem;

use crate::abi::{FspFileSystem, FspFsctlVolumeParams};
use crate::cb::INTERFACE;
use crate::error::Error;
use crate::host::Host;
use crate::options::MountOptions;
use crate::status::{NtStatus, STATUS_SUCCESS};

/// Default timeouts (ms) from the WinFsp headers (`FspFsctlIrpTimeoutDefault`,
/// `FspFsctlIrpCapacityDefault`); `FileInfoTimeout`/friends get 1 s so the
/// FSD can cache metadata briefly — the side-map keeps them coherent.
const IRP_TIMEOUT_MS: u32 = 300_000;
const IRP_CAPACITY: u32 = 1000;
const INFO_TIMEOUT_MS: u32 = 1000;

// `flags2` validity bits (FSP_FSCTL_VOLUME_PARAMS_V1_FIELD_DEFN order).
const VOLUME_INFO_TIMEOUT_VALID: u32 = 1 << 0;
const DIR_INFO_TIMEOUT_VALID: u32 = 1 << 1;
const SECURITY_TIMEOUT_VALID: u32 = 1 << 2;

/// A live WinFsp mount.
///
/// While a `Mount` value exists, Windows programs see the [`FileSystem`] as an
/// ordinary filesystem rooted at the mount point. `Drop` unmounts cleanly:
/// stop the dispatcher → remove mount point → delete the file-system object.
///
/// The mounted files remain readable/writable for the whole `Mount` lifetime;
/// already-open file handles flush through `Cleanup`/`Close` during drop.
pub struct Mount {
    /// The WinFsp file-system object, torn down via `FspFileSystemDelete`.
    fs: *mut FspFileSystem,
    /// The bridge keeping the user's `FileSystem` alive for callbacks.
    /// Held as `Box<Host>` so the `user_context` pointer stays stable.
    host: Box<Host>,
    /// Where we mounted (informational only).
    mountpoint: String,
    /// An empty directory we removed so WinFsp could create its junction
    /// there; recreated on unmount (see `mount` for the semantics).
    recreate_dir: Option<PathBuf>,
}

// SAFETY: `Mount` owns the WinFsp `*mut FspFileSystem`. The only raw pointer
// (`fs`) is used exclusively through the WinFsp API, which is safe to call
// from any thread; dispatcher callbacks read `host` via `user_context` but
// never touch the `Mount` itself.
unsafe impl Send for Mount {}
unsafe impl Sync for Mount {}

impl Mount {
    /// Mounts `fs` at `mountpoint`.
    // The type is named `Mount` because a live mount *is* the object — the
    // `mount()` constructor mirrors `std::thread::spawn`-style naming.
    #[allow(clippy::self_named_constructors)]
    ///
    /// `mountpoint` is either a drive letter (`X:`) or a directory path.
    /// Directory mounts work like `mklink /J`: WinFsp creates the junction —
    /// so the path **must not exist**. An *existing empty directory* is
    /// handled gracefully: it is removed before mounting and recreated on
    /// unmount. A non-empty directory or a file at the path is
    /// [`Error::InvalidMountPoint`]. The mount stays live until the returned
    /// `Mount` is dropped.
    ///
    /// # Errors
    ///
    /// [`Error::WinFspUnavailable`]/`WinFspMissingSymbol` when the runtime is
    /// absent or too old, [`Error::NtStatus`] when WinFsp rejects the volume
    /// parameters or mount point, [`Error::InvalidMountPoint`] for a malformed
    /// mount point, and [`Error::Vfs`] when the backend rejects the mount
    /// (e.g. a root that is not a directory).
    #[allow(clippy::self_named_constructors)] // `Mount::mount(fs, point, opts)` is the public API
    pub fn mount<F>(fs: F, mountpoint: &Path, opts: MountOptions) -> Result<Self, Error>
    where
        F: FileSystem + 'static,
    {
        let api = crate::sys::api().map_err(|missing| {
            if missing == crate::sys::DLL_NAME { Error::WinFspUnavailable } else { Error::WinFspMissingSymbol(missing) }
        })?;

        let fs: Arc<dyn FileSystem> = Arc::new(fs);
        let backend_read_only = fs.is_read_only();

        // The Host is boxed before `FspFileSystemCreate` so its address is
        // stable for `user_context`; it's reclaimed into the `Mount` on
        // success — on failure nothing is left dangling.
        let mut host = Box::new(Host::new(fs, &opts)?);

        let mut params = FspFsctlVolumeParams {
            version: u16::try_from(size_of::<FspFsctlVolumeParams>()).unwrap_or(0),
            sector_size: 512,
            sectors_per_allocation_unit: 8, // 4 KiB allocation units
            // Header says bytes: 255 UTF-16 units = 510 bytes.
            max_component_length: (crate::host::MAX_COMPONENT_LEN * 2) as u16,
            volume_creation_time: crate::attrs::filetime_now(),
            volume_serial_number: opts.volume_serial,
            transact_timeout: 0, // deprecated field; WinFsp applies its default
            irp_timeout: IRP_TIMEOUT_MS,
            irp_capacity: IRP_CAPACITY,
            file_info_timeout: INFO_TIMEOUT_MS,
            flags: crate::abi::VP_CASE_PRESERVED_NAMES
                | crate::abi::VP_UNICODE_ON_DISK
                | crate::abi::VP_POST_CLEANUP_WHEN_MODIFIED_ONLY
                | crate::abi::VP_PASS_QUERY_DIRECTORY_FILE_NAME
                | crate::abi::VP_UM_FILE_CONTEXT_IS_USER_CONTEXT2
                | if opts.read_only || backend_read_only { crate::abi::VP_READ_ONLY_VOLUME } else { 0 },
            ..Default::default()
        };
        // `Prefix` stays empty: that is a `WinFsp.Net` (UNC) feature.
        let fs_name: Vec<u16> = opts.file_system_name.encode_utf16().take(15).collect();
        params.file_system_name[..fs_name.len()].copy_from_slice(&fs_name);
        // V1 fields: individual cache timeouts (all = FileInfoTimeout).
        params.flags2 = VOLUME_INFO_TIMEOUT_VALID | DIR_INFO_TIMEOUT_VALID | SECURITY_TIMEOUT_VALID;
        params.volume_info_timeout = INFO_TIMEOUT_MS;
        params.dir_info_timeout = INFO_TIMEOUT_MS;
        params.security_timeout = INFO_TIMEOUT_MS;

        let mut device_name: Vec<u16> = crate::sys::DISK_DEVICE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        let mut fs_obj: *mut FspFileSystem = ptr::null_mut();

        // SAFETY: `device_name` is a valid NUL-terminated UTF-16 buffer;
        // `params` is a fully initialized `FSP_FSCTL_VOLUME_PARAMS` with
        // `version == sizeof` selecting the V1 tail we filled; `INTERFACE` is
        // a static vtable; `fs_obj` receives the out-pointer on success.
        let st =
            unsafe { (api.create)(device_name.as_mut_ptr(), &raw const params, &raw const INTERFACE, &raw mut fs_obj) };
        check(st, "FspFileSystemCreate")?;

        // Install the `Host` pointer *before* any callback can fire.
        // SAFETY: `fs_obj` is a live file-system object (create just returned
        // it); `host` is a stable Box the `Mount` keeps alive to the end.
        unsafe { (*fs_obj).user_context = &mut *host as *mut Host as *mut c_void };

        // `FspFileSystemSetMountPoint` creates the mount point itself — it is
        // junction semantics (`mklink /J`), so the name must be free. A drive
        // letter skips the whole check; a path that already exists must be an
        // empty directory, which we remove now and recreate on unmount.
        let mut recreate_dir = None;
        if !looks_like_drive_letter(mountpoint) && mountpoint.exists() {
            let fail = |e: Error| -> Error {
                // SAFETY: dispatcher never started; the object is valid to delete.
                unsafe { (api.delete)(fs_obj) };
                e
            };
            if !mountpoint.is_dir() {
                return Err(fail(Error::InvalidMountPoint(format!("{}", mountpoint.display()))));
            }
            match std::fs::remove_dir(mountpoint) {
                Ok(()) => recreate_dir = Some(mountpoint.to_path_buf()),
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    return Err(fail(Error::InvalidMountPoint(format!("{}", mountpoint.display()))));
                }
                Err(e) => return Err(fail(Error::Io(e))),
            }
        }

        let mut mp_wide = os_wide(mountpoint)?;
        // SAFETY: `mp_wide` is NUL-terminated UTF-16.
        let st = unsafe { (api.set_mount_point)(fs_obj, mp_wide.as_mut_ptr()) };
        if st != STATUS_SUCCESS {
            // SAFETY: dispatcher never started; the object is valid to delete.
            unsafe { (api.delete)(fs_obj) };
            restore_removed_dir(&recreate_dir);
            return Err(Error::NtStatus { op: "FspFileSystemSetMountPoint", status: st });
        }

        // SAFETY: valid object; `dispatcher_threads == 0` asks for the WinFsp
        // default thread count.
        let st = unsafe { (api.start_dispatcher)(fs_obj, opts.dispatcher_threads) };
        if st != STATUS_SUCCESS {
            // SAFETY: object valid; mount point set — remove it, then delete.
            unsafe {
                (api.remove_mount_point)(fs_obj);
                (api.delete)(fs_obj);
            }
            restore_removed_dir(&recreate_dir);
            return Err(Error::NtStatus { op: "FspFileSystemStartDispatcher", status: st });
        }

        Ok(Self { fs: fs_obj, host, mountpoint: mountpoint.display().to_string(), recreate_dir })
    }

    /// Returns `true` if the WinFsp runtime (`winfsp-x64.dll` et al.) can be
    /// loaded on this machine — i.e. whether `Mount::mount` could work at all.
    /// Cheap and idempotent; the probe result is cached process-wide.
    #[must_use]
    pub fn is_available() -> bool {
        crate::sys::api().is_ok()
    }

    /// Deeper probe: also asks the user-mode library whether the kernel
    /// driver answers (`FspFsctlPreflight` when exported). Falls back to
    /// [`Mount::is_available`] when the export is missing.
    #[must_use]
    pub fn probe() -> bool {
        let Ok(api) = crate::sys::api() else { return false };
        let Some(preflight) = api.preflight else { return true };
        let mut name: Vec<u16> = crate::sys::DISK_DEVICE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is a valid NUL-terminated UTF-16 buffer.
        let st = unsafe { preflight(name.as_mut_ptr()) };
        st == STATUS_SUCCESS
    }

    /// The mount point this `Mount` was created for, as passed to `mount`.
    #[must_use]
    pub fn mountpoint(&self) -> &str {
        &self.mountpoint
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Ok(api) = crate::sys::api() {
            // SAFETY: `self.fs` is a live file-system object we own. Order
            // matters: the dispatcher is stopped before the object is deleted
            // so no callback can dereference `user_context` after `host`
            // dies. WinFsp guarantees no callbacks run after
            // `FspFileSystemDelete` returns, so freeing `host` afterwards is
            // sound. Leaked handle contexts (never `Close`d) are reclaimed
            // here too.
            unsafe {
                (api.stop_dispatcher)(self.fs);
                (api.remove_mount_point)(self.fs);
                (api.delete)(self.fs);
                for ctx in self.host.drain_handles() {
                    drop(Box::from_raw(ctx as *mut crate::host::Handle));
                }
            }
            restore_removed_dir(&self.recreate_dir);
        }
    }
}

/// Maps a failing `NTSTATUS` to [`Error::NtStatus`].
fn check(status: NtStatus, op: &'static str) -> Result<(), Error> {
    if status == STATUS_SUCCESS { Ok(()) } else { Err(Error::NtStatus { op, status }) }
}

/// Mount point → NUL-terminated UTF-16 (drive letter or directory path).
fn os_wide(p: &Path) -> Result<Vec<u16>, Error> {
    let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
    if v.is_empty() {
        return Err(Error::InvalidMountPoint(String::new()));
    }
    v.push(0);
    Ok(v)
}

/// `true` for `X:`-style drive letters (with or without a trailing `\`).
fn looks_like_drive_letter(p: &Path) -> bool {
    let s = p.as_os_str();
    let bytes = s.as_encoded_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2..].iter().all(|&b| b == b'\\' || b == b'/'))
}

/// Recreates an empty directory removed before mounting (junction semantics).
fn restore_removed_dir(dir: &Option<PathBuf>) {
    if let Some(dir) = dir {
        let _ = std::fs::create_dir(dir);
    }
}
