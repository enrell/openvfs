//! Inline-detour engine.
//!
//! Thin layer over [`ilhook`]'s x64 hooker. For each hooked function ilhook
//! rewrites the target's prologue with a jump to our detour and builds a
//! *trampoline* (the relocated original instructions plus a jump back). The
//! trampoline address is delivered per call to the `JmpToAddrRoutine`
//! [`record_trampoline`], which stores it in a per-hook [`AtomicUsize`] slot
//! before the CPU jumps to our detour — so by the time any detour function
//! runs, [`trampoline`] can fetch the callable original.
//!
//! [`HookPoint`]s (which own the trampoline allocation and restore-on-drop)
//! are leaked for the process lifetime; hooks are installed once at
//! `openvfs_hook_install` time before the application's threads run.

use std::sync::atomic::{AtomicUsize, Ordering};

use ilhook::x64::{CallbackOption, HookFlags, HookPoint, HookType, Hooker, Registers};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

/// Per-call routine for `HookType::JmpToAddr`: ilhook runs it (with the
/// caller's registers untouched) right before jumping to our detour. We use
/// it to capture the trampoline address, which ilhook does not expose
/// through `HookPoint`.
///
/// # Safety
///
/// `user_data` must be a `&'static AtomicUsize` — we pass
/// `ptr::from_ref(slot)` in [`install`]. `regs`/`ori_func_ptr` are supplied by
/// ilhook's stub for the duration of the call.
unsafe extern "win64" fn record_trampoline(_regs: *mut Registers, ori_func_ptr: usize, user_data: usize) {
    // SAFETY: `user_data` is a leaked `&'static AtomicUsize` (see install()).
    let slot = unsafe { &*(user_data as *const AtomicUsize) };
    slot.store(ori_func_ptr, Ordering::Release);
}

/// A live hook. Dropping the inner [`HookPoint`] would unpatch the target,
/// so we keep them all in a leaked registry.
pub struct InstalledHook {
    /// Name of the hooked export, for logging.
    pub name: &'static str,
    /// The hook point; keeps the trampoline allocation alive.
    point: HookPoint,
}

// SAFETY: `HookPoint` is `!Send` only because of its `Box<dyn ThreadCallback>`
// slot — we always pass `CallbackOption::None`, so no callback object exists.
// The point is otherwise plain data: it is moved into a `Mutex` after
// installation and only touched again by `unhook`.
unsafe impl Send for InstalledHook {}

impl InstalledHook {
    /// Disables this hook, restoring the original prologue.
    ///
    /// # Safety
    /// In-flight calls through the trampoline stay valid, but patching while
    /// another thread executes the target's prologue can crash — only do this
    /// when no application code can be running the hooked function.
    pub unsafe fn unhook(self) -> Result<(), String> {
        // SAFETY: forwarded from ilhook; caller guarantees quiescence.
        unsafe { self.point.unhook() }.map_err(|e| format!("unhook {}: {e:?}", self.name))
    }
}

/// Resolves `name` in an already-loaded module (e.g. `kernel32.dll`).
/// Returns `None` when the module or export is missing.
pub fn proc_address(module_w16: &[u16], name: &str) -> Option<usize> {
    // SAFETY: `module_w16` is a valid NUL-terminated wide string; the module
    // is already loaded in this process so the handle stays valid.
    let module = unsafe { GetModuleHandleW(module_w16.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let mut cname = Vec::with_capacity(name.len() + 1);
    cname.extend_from_slice(name.as_bytes());
    cname.push(0);
    // SAFETY: `module` is a valid HMODULE, `cname` is NUL-terminated.
    let addr = unsafe { GetProcAddress(module, cname.as_ptr()) };
    addr.map(|f| f as usize).filter(|a| *a != 0)
}

/// Installs an inline hook at `target`, jumping to `detour`.
///
/// `slot` receives the trampoline (original function) address on the first
/// call. The returned [`InstalledHook`] must be kept alive.
///
/// # Safety
/// `target` must point at the start of a function with enough prologue space,
/// and `detour` must be ABI-compatible with it. Patching while other threads
/// execute `target` is unsafe; we install before application threads run.
pub unsafe fn install(
    target: usize,
    detour: usize,
    slot: &'static AtomicUsize,
    name: &'static str,
) -> Result<InstalledHook, String> {
    let hooker = Hooker::new(
        target,
        HookType::JmpToAddr(detour, record_trampoline),
        CallbackOption::None,
        std::ptr::from_ref(slot) as usize,
        HookFlags::empty(),
    );
    // SAFETY: caller guarantees `target`/`detour` are compatible entry points.
    let point = unsafe { hooker.hook() }.map_err(|e| format!("hook {name} at {target:#x}: {e:?}"))?;
    Ok(InstalledHook { name, point })
}

/// Fetches the trampoline (original function) recorded for `slot`,
/// transmuted to the requested function-pointer type.
///
/// The value is `0` only if the hooked function has never been invoked —
/// which cannot happen through the detour itself, because `record_trampoline`
/// runs before the detour on every call. Guard with [`trampoline_ready`].
///
/// # Safety
/// The caller must request exactly the target's true signature type.
pub fn trampoline<T: Copy>(slot: &AtomicUsize) -> Option<T> {
    let addr = slot.load(Ordering::Acquire);
    if addr == 0 {
        return None;
    }
    assert_eq!(size_of::<usize>(), size_of::<T>());
    // SAFETY: fn pointers and usize are pointer-sized on x64; `addr` is a
    // valid trampoline produced by ilhook for a function of type `T`.
    Some(unsafe { std::mem::transmute_copy::<usize, T>(&addr) })
}

/// `true` if the trampoline pointer has been captured for `slot`.
#[allow(dead_code)] // diagnostic helper, used when debugging hook install
pub fn trampoline_ready(slot: &AtomicUsize) -> bool {
    slot.load(Ordering::Acquire) != 0
}
