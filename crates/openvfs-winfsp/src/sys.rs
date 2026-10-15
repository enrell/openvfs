//! Runtime binding of the WinFsp user-mode library (Windows only).
//!
//! The WinFsp runtime DLL (`winfsp-x64.dll` / `winfsp-a64.dll` /
//! `winfsp-x86.dll`) is `LoadLibraryW`'d at mount time and every `Fsp*` entry
//! point is resolved with `GetProcAddress` — no WinFsp headers, import library
//! or SDK involvement at build time. See `abi.rs` for the layout declarations
//! these functions operate on.
//!
//! # Safety audit notes
//!
//! * The loaded DLL is intentionally never `FreeLibrary`'d: dispatcher threads
//!   execute DLL code until process exit, so unloading it would be unsound.
//! * `GetProcAddress` results are transmuted to the documented signatures; the
//!   WinFsp ABI is `extern "C"` on every architecture.
//! * Every caller-visible pointer we hand to the DLL (volume params, vtable,
//!   `Host` context) outlives the file-system object that uses it.

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::abi::*;

/// The device name for a disk (local) file system — `FSP_FSCTL_DISK_DEVICE_NAME`.
/// (`WinFsp.Net` — UNC network file systems — is intentionally not supported.)
pub const DISK_DEVICE_NAME: &str = "WinFsp.Disk";

// Resolved entry points. `FspFileSystemSendResponse` is unnecessary: we never
// return STATUS_PENDING.
pub type FspFileSystemCreateFn = unsafe extern "C" fn(
    device_path: PWSTR,
    volume_params: *const FspFsctlVolumeParams,
    interface: *const FspFileSystemInterface,
    file_system_out: *mut *mut FspFileSystem,
) -> NTSTATUS;
pub type FspFileSystemDeleteFn = unsafe extern "C" fn(file_system: *mut FspFileSystem);
pub type FspFileSystemSetMountPointFn = unsafe extern "C" fn(*mut FspFileSystem, PWSTR) -> NTSTATUS;
pub type FspFileSystemRemoveMountPointFn = unsafe extern "C" fn(*mut FspFileSystem);
pub type FspFileSystemStartDispatcherFn = unsafe extern "C" fn(*mut FspFileSystem, ULONG) -> NTSTATUS;
pub type FspFileSystemStopDispatcherFn = unsafe extern "C" fn(*mut FspFileSystem);
pub type FspFileSystemAddDirInfoFn = unsafe extern "C" fn(*mut FspFsctlDirInfo, PVOID, ULONG, *mut ULONG) -> BOOLEAN;
/// `FspFsctlPreflight` — deep probe: asks whether the kernel driver is present.
pub type FspFsctlPreflightFn = unsafe extern "C" fn(device_path: PWSTR) -> NTSTATUS;

/// The resolved WinFsp API surface.
pub struct WinFspApi {
    pub create: FspFileSystemCreateFn,
    pub delete: FspFileSystemDeleteFn,
    pub set_mount_point: FspFileSystemSetMountPointFn,
    pub remove_mount_point: FspFileSystemRemoveMountPointFn,
    pub start_dispatcher: FspFileSystemStartDispatcherFn,
    pub stop_dispatcher: FspFileSystemStopDispatcherFn,
    pub add_dir_info: FspFileSystemAddDirInfoFn,
    /// Optional (present since early WinFsp); used by `probe` for a deeper check.
    pub preflight: Option<FspFsctlPreflightFn>,
    /// Keeps the module alive for as long as this value lives; the DLL is
    /// never unloaded (see module docs). `HMODULE` is an opaque handle.
    _module: HMODULE,
}

// The WinFsp runtime DLL name for this target architecture.
pub const DLL_NAME: &str = if cfg!(target_arch = "x86_64") {
    "winfsp-x64.dll"
} else if cfg!(target_arch = "aarch64") {
    "winfsp-a64.dll"
} else if cfg!(target_arch = "x86") {
    "winfsp-x86.dll"
} else {
    "winfsp.dll"
};

/// `Err(name)` carries either `"winfsp-<arch>.dll"` (the library itself could
/// not be loaded → WinFsp is not installed) or the name of a missing export
/// (a corrupt or ancient WinFsp install).
fn load_api() -> Result<WinFspApi, &'static str> {
    let wide: Vec<u16> = DLL_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 buffer that outlives the
    // call; LoadLibraryW returns a module handle or 0.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err(DLL_NAME);
    }

    // If a required export is missing we propagate its name and leak the
    // module — the DLL stays mapped but unused, which is harmless.
    // SAFETY: `module` is a live HMODULE for every `sym` call below.
    unsafe {
        Ok(WinFspApi {
            create: sym(module, "FspFileSystemCreate")?,
            delete: sym(module, "FspFileSystemDelete")?,
            set_mount_point: sym(module, "FspFileSystemSetMountPoint")?,
            remove_mount_point: sym(module, "FspFileSystemRemoveMountPoint")?,
            start_dispatcher: sym(module, "FspFileSystemStartDispatcher")?,
            stop_dispatcher: sym(module, "FspFileSystemStopDispatcher")?,
            add_dir_info: sym(module, "FspFileSystemAddDirInfo")?,
            preflight: sym_opt(module, "FspFsctlPreflight"),
            _module: module,
        })
    }
}

/// `GetProcAddress` → typed fn pointer; `Err(name)` if the export is missing.
///
/// # Safety
/// `module` must be a live HMODULE and `name` must be the documented export
/// name for signature `F` — guaranteed at every call site in `load_api`.
unsafe fn sym<F>(module: HMODULE, name: &'static str) -> Result<F, &'static str> {
    let cname = std::ffi::CString::new(name).map_err(|_| name)?;
    // SAFETY: `module` is a live HMODULE and `cname` is NUL-terminated.
    let fp = unsafe { GetProcAddress(module, cname.as_ptr() as *const u8) };
    match fp {
        // SAFETY: `F` is one of the `Fsp*Fn` aliases above — always a function
        // pointer the same size as `f` — and the WinFsp ABI is `extern "C"`,
        // which matches the real convention of these exports on every
        // supported architecture. `transmute_copy` is required because the
        // compiler cannot verify `F`'s size at this generic point.
        Some(f) => Ok(unsafe { std::mem::transmute_copy(&f) }),
        None => Err(name),
    }
}

/// Optional symbol: missing → `None` instead of an error.
///
/// # Safety
/// Same contract as [`sym`].
unsafe fn sym_opt<F>(module: HMODULE, name: &'static str) -> Option<F> {
    // SAFETY: forwarded to `sym`.
    unsafe { sym::<F>(module, name).ok() }
}

static WINFSP: OnceLock<Result<WinFspApi, &'static str>> = OnceLock::new();

/// The resolved WinFsp API, or the name of what failed to resolve.
///
/// The result is cached for the process lifetime; the DLL stays loaded.
pub fn api() -> Result<&'static WinFspApi, &'static str> {
    WINFSP.get_or_init(load_api).as_ref().map_err(|e| *e)
}

// SAFETY: `WinFspApi` is a bag of function pointers + a module handle — plain
// data safe to share across threads. The functions themselves are
// thread-safe by WinFsp's contract (its dispatcher is multi-threaded by design).
unsafe impl Send for WinFspApi {}
unsafe impl Sync for WinFspApi {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_loads_or_reports() {
        // Without WinFsp installed: Err("winfsp-x64.dll"). With it: Ok.
        // Either way it must not crash and must be consistent across calls.
        let first = api().is_ok();
        assert_eq!(first, api().is_ok());
    }
}
