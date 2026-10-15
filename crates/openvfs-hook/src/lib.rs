//! # openvfs-hook — per-process file virtualization for Windows (x64)
//!
//! An injectable DLL that intercepts Win32 file APIs (`CreateFileW`,
//! `FindFirstFileW`, `GetFileAttributesW`, `DeleteFileW`, …) and presents a
//! merged view of one *virtual root* backed by several read-only mod layers,
//! an overwrite layer that captures writes, and whiteout markers for deletes.
//! The mechanism is the same category of technique Mod Organizer 2's USVFS
//! uses, implemented clean-room from the observable contract (see
//! `docs/hook-spec.md`).
//!
//! ## Lifecycle
//!
//! 1. The host process loads this DLL (e.g. `openvfs-inject` does
//!    `CreateRemoteThread(LoadLibraryW)`). `DllMain` only disables thread
//!    notifications — no work happens under the loader lock.
//! 2. `DllMain` runs the install synchronously (see the module docs): it
//!    reads `OPENVFS_HOOK_CONFIG`, builds the route map, and installs the
//!    hooks — all before the game's main thread resumes. The
//!    `openvfs_hook_install` export is an idempotent alternative for
//!    injectors that prefer an explicit init call.
//! 3. Without a config, `install` refuses cleanly (no hooks are installed,
//!    the process is unaffected).
//!
//! ## Platform support
//!
//! Everything is `#[cfg(all(windows, target_arch = "x86_64"))]`: on any other
//! target this crate compiles to an empty stub so the workspace stays green.
//! 32-bit Windows is out of scope (see the spec's coverage matrix).
//!
//! ## Unsafe policy
//!
//! Unlike the rest of the workspace, unsafe code is required here — we are
//! rewriting foreign code in a foreign process's address space. It is
//! concentrated in `engine` (detour installation) and `sys` (FFI pointer
//! plumbing), with `// SAFETY:` comments per block, and the crate uses
//! `#![deny(unsafe_op_in_unsafe_fn)]`. Detour entry points additionally run
//! their logic inside `catch_unwind` so a Rust panic can never unwind into
//! the host application.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(windows, target_arch = "x86_64"))]
mod config;
#[cfg(all(windows, target_arch = "x86_64"))]
mod engine;
#[cfg(all(windows, target_arch = "x86_64"))]
mod guard;
#[cfg(all(windows, target_arch = "x86_64"))]
mod hooks;
#[cfg(all(windows, target_arch = "x86_64"))]
mod log;
#[cfg(all(windows, target_arch = "x86_64"))]
mod routes;
#[cfg(all(windows, target_arch = "x86_64"))]
mod sys;
#[cfg(all(windows, target_arch = "x86_64"))]
mod util;

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{
        ERROR_DIRECTORY, ERROR_ENVVAR_NOT_FOUND, ERROR_INVALID_DATA, ERROR_INVALID_FUNCTION, HMODULE,
    };
    use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;

    use crate::config::Config;
    use crate::hooks;
    use crate::log;
    use crate::routes::Routes;

    static INSTALLED: AtomicBool = AtomicBool::new(false);

    /// `DllMain` logic: runs [`do_install`] synchronously on
    /// `DLL_PROCESS_ATTACH`. This is deliberate: our injector spawns the
    /// child suspended and needs the hooks live *before* the game's main
    /// thread resumes, so a deferred "spawn an init thread" style attach
    /// would race it. Every operation in `do_install` (env-var reads, config
    /// file read, JSON parse, route-map build, `GetModuleHandle`/
    /// `GetProcAddress` on already-loaded modules, `VirtualProtect` patching)
    /// is safe under the loader lock — no new modules are loaded and no other
    /// threads are synchronized with. `openvfs_hook_install` remains exported
    /// for injectors that prefer an explicit init call; both paths funnel
    /// through `INSTALLED`.
    ///
    /// # Safety
    /// Called by the OS loader with a valid module handle.
    pub(super) unsafe fn dll_main(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
        const DLL_PROCESS_ATTACH: u32 = 1;
        if reason == DLL_PROCESS_ATTACH {
            // SAFETY: `module` is our own valid module handle.
            unsafe { DisableThreadLibraryCalls(module) };
            // Swallow failures: an unconfigured (or misconfigured) process
            // must still load the DLL — it just stays inert.
            let _ = std::panic::catch_unwind(do_install);
        }
        1
    }

    /// The real installation, shared by `openvfs_hook_install`.
    fn do_install() -> u32 {
        log::init();
        if INSTALLED.swap(true, Ordering::SeqCst) {
            return 0; // idempotent
        }
        let result = (|| -> Result<(), u32> {
            let Ok(cfg_path) = std::env::var("OPENVFS_HOOK_CONFIG") else {
                log::msg!("OPENVFS_HOOK_CONFIG not set; refusing to install hooks");
                return Err(ERROR_ENVVAR_NOT_FOUND);
            };
            let cfg = Config::load(std::path::Path::new(&cfg_path)).map_err(|e| {
                log::msg!("config error: {e}");
                ERROR_INVALID_DATA
            })?;
            let routes = Routes::new(&cfg).map_err(|e| {
                log::msg!("routes error: {e}");
                ERROR_DIRECTORY
            })?;
            hooks::set_routes(routes);
            unsafe { hooks::install_all() }.map_err(|e| {
                log::msg!("hook install error: {e}");
                ERROR_INVALID_FUNCTION
            })?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                log::msg!("installed successfully");
                0
            }
            Err(code) => {
                // Leave the process untouched on failure.
                let _ = unsafe { hooks::uninstall_all() };
                INSTALLED.store(false, Ordering::SeqCst);
                code
            }
        }
    }

    /// Full installation entry point, invoked by the injector through
    /// `CreateRemoteThread`. Returns a Win32-style exit code: `0` = ok,
    /// `ERROR_ENVVAR_NOT_FOUND` (203) = no config, `ERROR_INVALID_DATA` (13) =
    /// bad config, `ERROR_DIRECTORY` (267) = bad root, `ERROR_INVALID_FUNCTION`
    /// (1) = hook installation failed.
    ///
    /// # Safety
    /// `param` is ignored; safe to call once at process bring-up.
    pub unsafe extern "system" fn install(_param: *mut c_void) -> u32 {
        match std::panic::catch_unwind(do_install) {
            Ok(code) => code,
            Err(_) => ERROR_INVALID_FUNCTION,
        }
    }

    /// Removes all installed hooks. Exit code 0 on success.
    ///
    /// # Safety
    /// Only safe when no application thread is executing a hooked function.
    pub unsafe extern "system" fn uninstall(_param: *mut c_void) -> u32 {
        match std::panic::catch_unwind(|| unsafe { hooks::uninstall_all() }) {
            Ok(Ok(())) => {
                INSTALLED.store(false, Ordering::SeqCst);
                0
            }
            Ok(Err(_)) | Err(_) => ERROR_INVALID_FUNCTION,
        }
    }
}

/// `DllMain` entry point (invoked by the CRT/loader on Windows).
///
/// # Safety
/// Called only by the OS loader, which passes a valid module handle; the
/// function must not be invoked from Rust code.
#[cfg(all(windows, target_arch = "x86_64"))]
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    module: windows_sys::Win32::Foundation::HMODULE,
    reason: u32,
    reserved: *mut core::ffi::c_void,
) -> i32 {
    // SAFETY: called by the OS loader with a valid module handle.
    unsafe { imp::dll_main(module, reason, reserved) }
}

/// Install entry point — see `imp::install`.
///
/// # Safety
/// Invoke via `CreateRemoteThread` (or directly) before app code runs.
#[cfg(all(windows, target_arch = "x86_64"))]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn openvfs_hook_install(param: *mut core::ffi::c_void) -> u32 {
    // SAFETY: forwarded to the installation routine.
    unsafe { imp::install(param) }
}

/// Uninstall entry point — see `imp::uninstall`.
///
/// # Safety
/// Only call when no application thread can be inside a hooked function.
#[cfg(all(windows, target_arch = "x86_64"))]
#[unsafe(no_mangle)]
pub unsafe extern "system" fn openvfs_hook_uninstall(param: *mut core::ffi::c_void) -> u32 {
    // SAFETY: forwarded to the teardown routine.
    unsafe { imp::uninstall(param) }
}
