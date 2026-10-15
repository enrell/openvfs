//! The Win32 file-API detours and the virtualization policy they implement.
//!
//! Layout:
//!
//! * [`install_all`] registers every detour with the [`crate::engine`].
//! * Each `det_*` function is the patched entry point: it fetches the
//!   trampoline (filled per-call by ilhook's pre-jump routine), takes the
//!   recursion [`guard`], then runs the `vfs_*` implementation inside
//!   `catch_unwind` — a panic can never unwind across the FFI boundary.
//! * `vfs_*` functions implement the merged-view policy. Whenever they need a
//!   *real* file operation they just call the `windows-sys` import — under the
//!   guard those calls re-enter the patched entry point and pass straight
//!   through to the original code.
//!
//! Behavioral contract (see `docs/hook-spec.md`):
//! * Reads resolve to the highest-priority provider (overwrite, layers in
//!   order, then the physical base dir).
//! * Any open/create with write intent — or a create/truncate disposition —
//!   is captured into the overwrite directory (parents are created).
//! * Deletes/renames of lower-layer files leave `.ovh-deleted` markers in the
//!   overwrite dir; the files stay hidden until the marker is removed.
//! * Everything outside the virtual root is passed through untouched.

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIR_NOT_EMPTY, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    self, CREATE_NEW, CREATEFILE2_EXTENDED_PARAMETERS, FILE_FLAG_DELETE_ON_CLOSE, FIND_FIRST_EX_CASE_SENSITIVE,
    FindExInfoBasic, FindExInfoStandard, FindExSearchLimitToDirectories, FindExSearchNameMatch, MOVEFILE_COPY_ALLOWED,
    MOVEFILE_REPLACE_EXISTING, WIN32_FIND_DATAA, WIN32_FIND_DATAW,
};
use windows_sys::core::{BOOL, PCSTR, PCWSTR};

use crate::routes::Routes;
use crate::util;
use crate::{engine, guard, log, sys};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

static ROUTES: OnceLock<Routes> = OnceLock::new();
static HOOKS: OnceLock<Mutex<Vec<engine::InstalledHook>>> = OnceLock::new();

/// The route map, if the layer was configured and initialized.
fn routes() -> Option<&'static Routes> {
    ROUTES.get()
}

/// Stores the route map (called once by `openvfs_hook_install`).
pub fn set_routes(r: Routes) {
    let _ = ROUTES.set(r);
}

/// Snapshot-style state of one synthesized directory search.
struct Search {
    entries: Vec<WIN32_FIND_DATAW>,
    next: usize,
}

static SEARCHES: OnceLock<Mutex<HashMap<usize, Search>>> = OnceLock::new();

fn searches() -> &'static Mutex<HashMap<usize, Search>> {
    SEARCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn search_map() -> std::sync::MutexGuard<'static, HashMap<usize, Search>> {
    searches().lock().unwrap_or_else(|p| p.into_inner())
}

/// Stashes a synthesized listing and returns the fake `HANDLE` for it.
fn register_search(entries: Vec<WIN32_FIND_DATAW>) -> HANDLE {
    let mut map = search_map();
    let mut search = Box::new(Search { entries, next: 1 });
    let key = std::ptr::from_mut(&mut *search) as usize;
    map.insert(key, *search);
    key as HANDLE
}

// ---------------------------------------------------------------------------
// Detour wrapper macro
// ---------------------------------------------------------------------------

/// Generates a `det_*` entry point and its trampoline slot.
///
/// The wrapper guarantees: trampoline populated (ilhook runs its routine
/// first), recursion-guard passthrough, and panic containment.
macro_rules! detour {
    (fn $det:ident ($($a:ident : $t:ty),*) -> $ret:ty
         => slot $slot:ident, fail $fail:expr, impl $imp:ident) => {
        /// Trampoline (original function) address, filled by ilhook's
        /// pre-jump routine on every call.
        static $slot: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "system" fn $det($($a : $t),*) -> $ret {
            type Sig = unsafe extern "system" fn($($t),*) -> $ret;
            let Some(orig) = engine::trampoline::<Sig>(&$slot) else {
                // Unreachable once hooked (the pre-jump routine stores the
                // trampoline before this function runs); fail closed anyway.
                return $fail;
            };
            let Some(_guard) = guard::enter() else {
                // Re-entrant call from inside our own layer: pass through to
                // the original function.
                // SAFETY: `orig` is the trampoline for this signature.
                return unsafe { orig($($a),*) };
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $imp($($a),*))) {
                Ok(v) => v,
                Err(_) => {
                    // Never unwind over an FFI boundary; fall back to the
                    // real function so a logic bug can't crash the app.
                    // SAFETY: `orig` is the trampoline for this signature.
                    unsafe { orig($($a),*) }
                }
            }
        }
    };
}

detour! { fn det_create_file_w(name: PCWSTR, access: u32, share: u32, sa: *const SECURITY_ATTRIBUTES, disp: u32, flags: u32, template: HANDLE) -> HANDLE
=> slot T_CREATE_FILE_W, fail INVALID_HANDLE_VALUE, impl imp_create_file_w }
detour! { fn det_create_file_a(name: PCSTR, access: u32, share: u32, sa: *const SECURITY_ATTRIBUTES, disp: u32, flags: u32, template: HANDLE) -> HANDLE
=> slot T_CREATE_FILE_A, fail INVALID_HANDLE_VALUE, impl imp_create_file_a }
detour! { fn det_create_file_2(name: PCWSTR, access: u32, share: u32, disp: u32, params: *const CREATEFILE2_EXTENDED_PARAMETERS) -> HANDLE
=> slot T_CREATE_FILE_2, fail INVALID_HANDLE_VALUE, impl imp_create_file_2 }
detour! { fn det_find_first_file_w(name: PCWSTR, data: *mut WIN32_FIND_DATAW) -> HANDLE
=> slot T_FIND_FIRST_FILE_W, fail INVALID_HANDLE_VALUE, impl imp_find_first_file_w }
detour! { fn det_find_first_file_a(name: PCSTR, data: *mut WIN32_FIND_DATAA) -> HANDLE
=> slot T_FIND_FIRST_FILE_A, fail INVALID_HANDLE_VALUE, impl imp_find_first_file_a }
detour! { fn det_find_first_file_ex_w(name: PCWSTR, level: i32, data: *mut c_void, op: i32, filter: *const c_void, flags: u32) -> HANDLE
=> slot T_FIND_FIRST_FILE_EX_W, fail INVALID_HANDLE_VALUE, impl imp_find_first_file_ex_w }
detour! { fn det_find_next_file_w(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL
=> slot T_FIND_NEXT_FILE_W, fail 0, impl imp_find_next_file_w }
detour! { fn det_find_next_file_a(h: HANDLE, data: *mut WIN32_FIND_DATAA) -> BOOL
=> slot T_FIND_NEXT_FILE_A, fail 0, impl imp_find_next_file_a }
detour! { fn det_find_close(h: HANDLE) -> BOOL
=> slot T_FIND_CLOSE, fail 0, impl imp_find_close }
detour! { fn det_get_file_attributes_w(name: PCWSTR) -> u32
=> slot T_GET_FILE_ATTRIBUTES_W, fail u32::MAX, impl imp_get_file_attributes_w }
detour! { fn det_get_file_attributes_a(name: PCSTR) -> u32
=> slot T_GET_FILE_ATTRIBUTES_A, fail u32::MAX, impl imp_get_file_attributes_a }
detour! { fn det_get_file_attributes_ex_w(name: PCWSTR, level: i32, out: *mut c_void) -> BOOL
=> slot T_GET_FILE_ATTRIBUTES_EX_W, fail 0, impl imp_get_file_attributes_ex_w }
detour! { fn det_get_file_attributes_ex_a(name: PCSTR, level: i32, out: *mut c_void) -> BOOL
=> slot T_GET_FILE_ATTRIBUTES_EX_A, fail 0, impl imp_get_file_attributes_ex_a }
detour! { fn det_delete_file_w(name: PCWSTR) -> BOOL
=> slot T_DELETE_FILE_W, fail 0, impl imp_delete_file_w }
detour! { fn det_delete_file_a(name: PCSTR) -> BOOL
=> slot T_DELETE_FILE_A, fail 0, impl imp_delete_file_a }
detour! { fn det_create_directory_w(name: PCWSTR, sa: *const SECURITY_ATTRIBUTES) -> BOOL
=> slot T_CREATE_DIRECTORY_W, fail 0, impl imp_create_directory_w }
detour! { fn det_create_directory_a(name: PCSTR, sa: *const SECURITY_ATTRIBUTES) -> BOOL
=> slot T_CREATE_DIRECTORY_A, fail 0, impl imp_create_directory_a }
detour! { fn det_set_file_attributes_w(name: PCWSTR, attrs: u32) -> BOOL
=> slot T_SET_FILE_ATTRIBUTES_W, fail 0, impl imp_set_file_attributes_w }
detour! { fn det_set_file_attributes_a(name: PCSTR, attrs: u32) -> BOOL
=> slot T_SET_FILE_ATTRIBUTES_A, fail 0, impl imp_set_file_attributes_a }
detour! { fn det_copy_file_w(src: PCWSTR, dst: PCWSTR, fail_if_exists: BOOL) -> BOOL
=> slot T_COPY_FILE_W, fail 0, impl imp_copy_file_w }
detour! { fn det_copy_file_a(src: PCSTR, dst: PCSTR, fail_if_exists: BOOL) -> BOOL
=> slot T_COPY_FILE_A, fail 0, impl imp_copy_file_a }
detour! { fn det_move_file_w(src: PCWSTR, dst: PCWSTR) -> BOOL
=> slot T_MOVE_FILE_W, fail 0, impl imp_move_file_w }
detour! { fn det_move_file_a(src: PCSTR, dst: PCSTR) -> BOOL
=> slot T_MOVE_FILE_A, fail 0, impl imp_move_file_a }
detour! { fn det_move_file_ex_w(src: PCWSTR, dst: PCWSTR, flags: u32) -> BOOL
=> slot T_MOVE_FILE_EX_W, fail 0, impl imp_move_file_ex_w }
detour! { fn det_move_file_ex_a(src: PCSTR, dst: PCSTR, flags: u32) -> BOOL
=> slot T_MOVE_FILE_EX_A, fail 0, impl imp_move_file_ex_a }
detour! { fn det_remove_directory_w(name: PCWSTR) -> BOOL
=> slot T_REMOVE_DIRECTORY_W, fail 0, impl imp_remove_directory_w }
detour! { fn det_remove_directory_a(name: PCSTR) -> BOOL
=> slot T_REMOVE_DIRECTORY_A, fail 0, impl imp_remove_directory_a }

// ---------------------------------------------------------------------------
// Shared helpers used by the vfs_* implementations. All run under the
// recursion guard; calls into windows-sys therefore hit the *real* code.
// ---------------------------------------------------------------------------

/// Maps an internal io error to a Win32 error code.
fn io_code(e: &std::io::Error) -> u32 {
    e.raw_os_error().map(|c| c as u32).unwrap_or(ERROR_ACCESS_DENIED)
}

/// Calls the real `CreateFileW` on a concrete host path.
fn real_create(
    path: &Path,
    access: u32,
    share: u32,
    sa: *const SECURITY_ATTRIBUTES,
    disp: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let w = sys::wide_path(path);
    // SAFETY: `w` is a NUL-terminated UTF-16 buffer for a real path; all other
    // args are forwarded verbatim. Under the guard this hits the real API.
    unsafe { FileSystem::CreateFileW(w.as_ptr(), access, share, sa, disp, flags, template) }
}

/// Calls the real `GetFileAttributesW` on a concrete host path.
fn real_get_attrs(path: &Path) -> u32 {
    let w = sys::wide_path(path);
    // SAFETY: `w` is NUL-terminated.
    unsafe { FileSystem::GetFileAttributesW(w.as_ptr()) }
}

/// Clears the read-only attribute on `path` — the captured overwrite copy
/// must be writable even when the source layer carried a read-only bit.
#[allow(clippy::permissions_set_readonly_false)] // intentional: clearing, not setting
fn clear_readonly(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perm = meta.permissions();
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(path, perm);
    }
}

/// Creates an empty overwrite file (and parents) or copies the winning file
/// up so that a non-truncating write open sees the merged content.
fn capture_for_write(routes: &Routes, rel: &str, winner: &Path, disp: u32) -> std::io::Result<()> {
    routes.ensure_overwrite_parents(rel)?;
    let target = routes.overwrite_path(rel);
    routes.clear_deleted(rel);
    if util::truncates(disp) {
        // The file will be truncated anyway; a fresh empty file is enough.
        if !target.exists() {
            std::fs::File::create(&target)?;
        }
    } else {
        std::fs::copy(winner, &target)?;
        clear_readonly(&target);
    }
    Ok(())
}

/// The decision computed for a `CreateFile*`-family call.
enum OpenAction {
    /// Call the real API with this resolved host path.
    Real(PathBuf),
    /// Fail with a Win32 error code.
    Fail(u32),
}

/// Decides where a `CreateFileW`-family open under the virtual root should
/// land. Pure resolution + capture side effects; the caller then invokes the
/// real API on the returned path.
fn decide_open(routes: &Routes, rel: &str, access: u32, disp: u32, flags: u32) -> OpenAction {
    let wants = util::wants_write(access, disp);
    let del_close = flags & FILE_FLAG_DELETE_ON_CLOSE != 0;
    match routes.resolve_read(rel) {
        Some(winner) if winner.is_dir() => {
            if disp == CREATE_NEW {
                OpenAction::Fail(ERROR_FILE_EXISTS)
            } else if wants || util::truncates(disp) {
                OpenAction::Fail(ERROR_ACCESS_DENIED)
            } else {
                OpenAction::Real(winner)
            }
        }
        Some(winner) => {
            if !(wants || del_close) {
                return OpenAction::Real(winner); // pure read of the winner
            }
            let target = routes.overwrite_path(rel);
            if routes.is_in_overwrite(&winner) {
                routes.clear_deleted(rel); // stale marker cleanup
            } else if let Err(e) = capture_for_write(routes, rel, &winner, disp) {
                return OpenAction::Fail(io_code(&e));
            }
            if del_close {
                // Hide lower providers now; the overwrite copy dies on close.
                let _ = routes.mark_deleted(rel);
            }
            OpenAction::Real(target)
        }
        None => {
            if !util::may_create(disp) {
                return OpenAction::Fail(ERROR_FILE_NOT_FOUND);
            }
            if let Some(parent) = util::parent_rel(rel)
                && !routes.exists(&parent)
            {
                return OpenAction::Fail(ERROR_PATH_NOT_FOUND);
            }
            if routes.ensure_overwrite_parents(rel).is_err() {
                return OpenAction::Fail(ERROR_PATH_NOT_FOUND);
            }
            routes.clear_deleted(rel);
            OpenAction::Real(routes.overwrite_path(rel))
        }
    }
}

/// Copies a directory tree for virtual move operations.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else if ft.is_file() {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

/// Deletes `rel` from the merged view (whiteout or physical removal of the
/// overwrite entry). Used by delete + replace-move semantics.
fn delete_virtual(routes: &Routes, rel: &str) -> std::io::Result<()> {
    let Some(winner) = routes.resolve_read(rel) else {
        return Ok(()); // already absent
    };
    if routes.is_in_overwrite(&winner) {
        if winner.is_dir() {
            let _ = std::fs::remove_dir_all(&winner);
        } else {
            let _ = std::fs::remove_file(&winner);
        }
    }
    if routes.lower_provides(rel) {
        routes.mark_deleted(rel)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Find* machinery
// ---------------------------------------------------------------------------

/// Copies a name into `WIN32_FIND_DATAW.cFileName` (truncating at 259).
fn fill_name_wide(data: &mut WIN32_FIND_DATAW, name: &str) {
    let wide: Vec<u16> = name.encode_utf16().collect();
    let n = wide.len().min(259);
    data.cFileName[..n].copy_from_slice(&wide[..n]);
    data.cFileName[n] = 0;
}

/// Builds a `WIN32_FIND_DATAW` for a merged entry (real attrs, virtual name).
fn find_data_of(name: &str, real: &Path) -> Option<WIN32_FIND_DATAW> {
    let ad = sys::file_attr_data(real)?;
    let mut d = WIN32_FIND_DATAW {
        dwFileAttributes: ad.dwFileAttributes,
        ftCreationTime: ad.ftCreationTime,
        ftLastAccessTime: ad.ftLastAccessTime,
        ftLastWriteTime: ad.ftLastWriteTime,
        nFileSizeHigh: ad.nFileSizeHigh,
        nFileSizeLow: ad.nFileSizeLow,
        ..Default::default()
    };
    fill_name_wide(&mut d, name);
    Some(d)
}

/// Synthesizes `.`/`..` directory entries (attribute source: the dir itself).
fn pseudo_entry(routes: &Routes, dir_rel: &str, name: &str) -> Option<WIN32_FIND_DATAW> {
    let real = if name == "." {
        routes.resolve_read(dir_rel).or_else(|| Some(routes.root().to_path_buf()))
    } else {
        let parent = util::parent_rel(dir_rel).unwrap_or_default();
        routes.resolve_read(&parent).or_else(|| Some(routes.root().to_path_buf()))
    };
    let ad = real.and_then(|p| sys::file_attr_data(&p));
    let mut d = ad.map_or_else(WIN32_FIND_DATAW::default, |ad| WIN32_FIND_DATAW {
        dwFileAttributes: ad.dwFileAttributes,
        ftCreationTime: ad.ftCreationTime,
        ftLastAccessTime: ad.ftLastAccessTime,
        ftLastWriteTime: ad.ftLastWriteTime,
        nFileSizeHigh: ad.nFileSizeHigh,
        nFileSizeLow: ad.nFileSizeLow,
        ..Default::default()
    });
    d.dwFileAttributes |= windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    fill_name_wide(&mut d, name);
    Some(d)
}

/// Builds the merged, pattern-filtered entry list for a `FindFirstFile*` call
/// on a virtual-root-relative pattern.
fn start_search(
    routes: &Routes,
    rel_pattern: &str,
    dirs_only: bool,
    case_sensitive: bool,
) -> Result<Vec<WIN32_FIND_DATAW>, u32> {
    if rel_pattern.is_empty() {
        // Bare-root search ("C:\vroot"): return the root's own entry.
        return pseudo_entry(routes, "", ".").map(|d| vec![d]).ok_or(ERROR_FILE_NOT_FOUND);
    }
    let (dir_rel, pattern) = util::split_dir_pattern(rel_pattern);
    let (entries, dir_exists) = routes.enumerate(&dir_rel);
    if !dir_exists {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    let wild = util::has_wildcards(&pattern);
    let matched = |name: &str| {
        let (p, n) = if case_sensitive {
            (pattern.clone(), name.to_owned())
        } else {
            (crate::routes::fold(&pattern), crate::routes::fold(name))
        };
        if wild { util::wildcard_match(&p, &n) } else { p == n }
    };
    let mut out = Vec::new();
    for pseudo in [".", ".."] {
        if matched(pseudo)
            && let Some(d) = pseudo_entry(routes, &dir_rel, pseudo)
        {
            out.push(d);
        }
    }
    for e in entries {
        if dirs_only && !e.is_dir {
            continue;
        }
        if !matched(&e.name) {
            continue;
        }
        if let Some(d) = find_data_of(&e.name, &e.real) {
            out.push(d);
        }
    }
    if out.is_empty() { Err(ERROR_FILE_NOT_FOUND) } else { Ok(out) }
}

/// Shared tail of FindFirstFile*: register the search, write the first entry.
fn finish_search(
    routes: &Routes,
    raw_path: &str,
    dirs_only: bool,
    case_sensitive: bool,
) -> Result<(Vec<WIN32_FIND_DATAW>, HANDLE), u32> {
    let Some(rel_pat) = routes.rel_under_root(raw_path) else {
        return Err(u32::MAX); // sentinel: not ours, caller passes through
    };
    let entries = start_search(routes, &rel_pat, dirs_only, case_sensitive)?;
    let handle = register_search(entries.clone());
    Ok((entries, handle))
}

// ---------------------------------------------------------------------------
// vfs_* implementations
// ---------------------------------------------------------------------------

fn imp_create_file_w(
    name: PCWSTR,
    access: u32,
    share: u32,
    sa: *const SECURITY_ATTRIBUTES,
    disp: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let fallback = || unsafe { FileSystem::CreateFileW(name, access, share, sa, disp, flags, template) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    log::msg!("CreateFileW {path} (rel={rel} access={access:#x} disp={disp})");
    match decide_open(routes, &rel, access, disp, flags) {
        OpenAction::Real(p) => real_create(&p, access, share, sa, disp, flags, template),
        OpenAction::Fail(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
    }
}

fn imp_create_file_a(
    name: PCSTR,
    access: u32,
    share: u32,
    sa: *const SECURITY_ATTRIBUTES,
    disp: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    let fallback = || unsafe { FileSystem::CreateFileA(name, access, share, sa, disp, flags, template) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    match decide_open(routes, &rel, access, disp, flags) {
        OpenAction::Real(p) => real_create(&p, access, share, sa, disp, flags, template),
        OpenAction::Fail(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
    }
}

fn imp_create_file_2(
    name: PCWSTR,
    access: u32,
    share: u32,
    disp: u32,
    params: *const CREATEFILE2_EXTENDED_PARAMETERS,
) -> HANDLE {
    let fallback = || unsafe { FileSystem::CreateFile2(name, access, share, disp, params) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    // Extract flags/attrs from the params blob when present.
    let flags = if params.is_null() {
        0
    } else {
        // SAFETY: caller passes a valid CREATEFILE2_EXTENDED_PARAMETERS.
        unsafe { (*params).dwFileFlags }
    };
    match decide_open(routes, &rel, access, disp, flags) {
        OpenAction::Real(p) => {
            let w = sys::wide_path(&p);
            // SAFETY: `w` NUL-terminated; `params` forwarded verbatim.
            unsafe { FileSystem::CreateFile2(w.as_ptr(), access, share, disp, params) }
        }
        OpenAction::Fail(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
    }
}

fn imp_find_first_file_w(name: PCWSTR, data: *mut WIN32_FIND_DATAW) -> HANDLE {
    let fallback = || unsafe { FileSystem::FindFirstFileW(name, data) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    match finish_search(routes, &path, false, false) {
        Err(u32::MAX) => fallback(),
        Err(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
        Ok((entries, handle)) => {
            if !data.is_null() {
                // SAFETY: caller supplies the WIN32_FIND_DATAW out-buffer.
                unsafe { *data = entries[0] };
            }
            handle
        }
    }
}

fn imp_find_first_file_a(name: PCSTR, data: *mut WIN32_FIND_DATAA) -> HANDLE {
    let fallback = || unsafe { FileSystem::FindFirstFileA(name, data) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    match finish_search(routes, &path, false, false) {
        Err(u32::MAX) => fallback(),
        Err(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
        Ok((entries, handle)) => {
            if !data.is_null() {
                // SAFETY: caller supplies the WIN32_FIND_DATAA out-buffer.
                unsafe { *data = find_data_a(&entries[0]) };
            }
            handle
        }
    }
}

fn imp_find_first_file_ex_w(
    name: PCWSTR,
    level: i32,
    data: *mut c_void,
    op: i32,
    filter: *const c_void,
    flags: u32,
) -> HANDLE {
    let fallback = || unsafe { FileSystem::FindFirstFileExW(name, level, data, op, filter, flags) };
    // Only the info levels/ops we can emulate are intercepted.
    if !(level == FindExInfoStandard || level == FindExInfoBasic)
        || !(op == FindExSearchNameMatch || op == FindExSearchLimitToDirectories)
        || !filter.is_null()
    {
        return fallback();
    }
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let dirs_only = op == FindExSearchLimitToDirectories;
    let case_sensitive = flags & FIND_FIRST_EX_CASE_SENSITIVE != 0;
    match finish_search(routes, &path, dirs_only, case_sensitive) {
        Err(u32::MAX) => fallback(),
        Err(code) => {
            sys::fail(code);
            INVALID_HANDLE_VALUE
        }
        Ok((entries, handle)) => {
            if !data.is_null() {
                // SAFETY: documented out-buffer for both supported levels is
                // WIN32_FIND_DATAW.
                unsafe { *(data as *mut WIN32_FIND_DATAW) = entries[0] };
            }
            handle
        }
    }
}

fn imp_find_next_file_w(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL {
    let key = h as usize;
    let mut map = search_map();
    if let Some(search) = map.get_mut(&key) {
        if let Some(entry) = search.entries.get(search.next) {
            search.next += 1;
            if !data.is_null() {
                // SAFETY: caller-supplied out-buffer.
                unsafe { *data = *entry };
            }
            return 1;
        }
        sys::fail(ERROR_NO_MORE_FILES);
        return 0;
    }
    drop(map);
    // SAFETY: not our handle — forward to the real FindNextFileW.
    unsafe { FileSystem::FindNextFileW(h, data) }
}

fn imp_find_next_file_a(h: HANDLE, data: *mut WIN32_FIND_DATAA) -> BOOL {
    let key = h as usize;
    let mut map = search_map();
    if let Some(search) = map.get_mut(&key) {
        if let Some(entry) = search.entries.get(search.next) {
            search.next += 1;
            if !data.is_null() {
                // SAFETY: caller-supplied out-buffer.
                unsafe { *data = find_data_a(entry) };
            }
            return 1;
        }
        sys::fail(ERROR_NO_MORE_FILES);
        return 0;
    }
    drop(map);
    // SAFETY: not our handle.
    unsafe { FileSystem::FindNextFileA(h, data) }
}

fn imp_find_close(h: HANDLE) -> BOOL {
    let key = h as usize;
    let mut map = search_map();
    if map.remove(&key).is_some() {
        return 1;
    }
    drop(map);
    // SAFETY: not our handle.
    unsafe { FileSystem::FindClose(h) }
}

/// Converts a `WIN32_FIND_DATAW` to the ANSI variant (name via CP_ACP).
fn find_data_a(w: &WIN32_FIND_DATAW) -> WIN32_FIND_DATAA {
    let mut d = WIN32_FIND_DATAA {
        dwFileAttributes: w.dwFileAttributes,
        ftCreationTime: w.ftCreationTime,
        ftLastAccessTime: w.ftLastAccessTime,
        ftLastWriteTime: w.ftLastWriteTime,
        nFileSizeHigh: w.nFileSizeHigh,
        nFileSizeLow: w.nFileSizeLow,
        dwReserved0: w.dwReserved0,
        dwReserved1: w.dwReserved1,
        ..Default::default()
    };
    let bytes = sys::wide_to_ansi(&w.cFileName);
    let n = bytes.len().min(259);
    for (i, b) in bytes[..n].iter().enumerate() {
        d.cFileName[i] = *b as i8;
    }
    d.cFileName[n] = 0;
    d
}

fn imp_get_file_attributes_w(name: PCWSTR) -> u32 {
    let fallback = || unsafe { FileSystem::GetFileAttributesW(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    match routes.resolve_read(&rel) {
        Some(real) => real_get_attrs(&real),
        None => {
            sys::fail_not_found();
            u32::MAX // INVALID_FILE_ATTRIBUTES
        }
    }
}

fn imp_get_file_attributes_a(name: PCSTR) -> u32 {
    let fallback = || unsafe { FileSystem::GetFileAttributesA(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    match routes.resolve_read(&rel) {
        Some(real) => real_get_attrs(&real),
        None => {
            sys::fail_not_found();
            u32::MAX
        }
    }
}

fn imp_get_file_attributes_ex_w(name: PCWSTR, level: i32, out: *mut c_void) -> BOOL {
    let fallback = || unsafe { FileSystem::GetFileAttributesExW(name, level, out) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    match routes.resolve_read(&rel) {
        Some(real) => {
            let w = sys::wide_path(&real);
            // SAFETY: `out` forwarded verbatim; level forwarded verbatim.
            unsafe { FileSystem::GetFileAttributesExW(w.as_ptr(), level, out) }
        }
        None => {
            sys::fail_not_found();
            0
        }
    }
}

fn imp_get_file_attributes_ex_a(name: PCSTR, level: i32, out: *mut c_void) -> BOOL {
    let fallback = || unsafe { FileSystem::GetFileAttributesExA(name, level, out) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    match routes.resolve_read(&rel) {
        Some(real) => {
            let w = sys::wide_path(&real);
            // SAFETY: `out` forwarded verbatim.
            unsafe { FileSystem::GetFileAttributesExW(w.as_ptr(), level, out) }
        }
        None => {
            sys::fail_not_found();
            0
        }
    }
}

fn imp_delete_file_w(name: PCWSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::DeleteFileW(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_delete_file(routes, &rel)
}

fn imp_delete_file_a(name: PCSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::DeleteFileA(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_delete_file(routes, &rel)
}

/// DeleteFile semantics: erase the overwrite copy if present; hide lower
/// providers behind a whiteout marker. Never touches a layer directory.
fn vfs_delete_file(routes: &Routes, rel: &str) -> BOOL {
    let Some(winner) = routes.resolve_read(rel) else {
        sys::fail_not_found();
        return 0;
    };
    if winner.is_dir() {
        sys::fail(ERROR_ACCESS_DENIED);
        return 0;
    }
    if routes.is_in_overwrite(&winner) {
        if let Err(e) = std::fs::remove_file(&winner) {
            sys::fail(io_code(&e));
            return 0;
        }
        if routes.lower_provides(rel) {
            let _ = routes.mark_deleted(rel);
        }
        return 1;
    }
    match routes.mark_deleted(rel) {
        Ok(()) => 1,
        Err(e) => {
            sys::fail(io_code(&e));
            0
        }
    }
}

fn imp_create_directory_w(name: PCWSTR, sa: *const SECURITY_ATTRIBUTES) -> BOOL {
    let fallback = || unsafe { FileSystem::CreateDirectoryW(name, sa) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    if rel.is_empty() || routes.exists(&rel) {
        sys::fail(ERROR_ALREADY_EXISTS);
        return 0;
    }
    if let Some(parent) = util::parent_rel(&rel)
        && !routes.exists(&parent)
    {
        sys::fail(ERROR_PATH_NOT_FOUND);
        return 0;
    }
    let target = routes.overwrite_path(&rel);
    match std::fs::create_dir(&target) {
        Ok(()) => {
            routes.clear_deleted(&rel); // recreation unhides
            1
        }
        Err(e) => {
            // Parent chain may need mirroring from the merged view.
            if routes.ensure_overwrite_parents(&rel).is_ok() && std::fs::create_dir(&target).is_ok() {
                routes.clear_deleted(&rel);
                return 1;
            }
            sys::fail(io_code(&e));
            0
        }
    }
}

fn imp_create_directory_a(name: PCSTR, sa: *const SECURITY_ATTRIBUTES) -> BOOL {
    let fallback = || unsafe { FileSystem::CreateDirectoryA(name, sa) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    if rel.is_empty() || routes.exists(&rel) {
        sys::fail(ERROR_ALREADY_EXISTS);
        return 0;
    }
    let target = routes.overwrite_path(&rel);
    match std::fs::create_dir(&target) {
        Ok(()) => {
            routes.clear_deleted(&rel);
            1
        }
        Err(e) => {
            if routes.ensure_overwrite_parents(&rel).is_ok() && std::fs::create_dir(&target).is_ok() {
                routes.clear_deleted(&rel);
                return 1;
            }
            sys::fail(io_code(&e));
            0
        }
    }
}

fn imp_set_file_attributes_w(name: PCWSTR, attrs: u32) -> BOOL {
    let fallback = || unsafe { FileSystem::SetFileAttributesW(name, attrs) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_set_attrs(routes, &rel, attrs)
}

fn imp_set_file_attributes_a(name: PCSTR, attrs: u32) -> BOOL {
    let fallback = || unsafe { FileSystem::SetFileAttributesA(name, attrs) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_set_attrs(routes, &rel, attrs)
}

/// SetFileAttributes: applied to the overwrite copy — a lower-layer file is
/// copied up first so mod dirs stay read-only.
fn vfs_set_attrs(routes: &Routes, rel: &str, attrs: u32) -> BOOL {
    let Some(winner) = routes.resolve_read(rel) else {
        sys::fail_not_found();
        return 0;
    };
    let target = if routes.is_in_overwrite(&winner) {
        winner
    } else if winner.is_dir() {
        let ov = routes.overwrite_path(rel);
        if std::fs::create_dir_all(&ov).is_err() {
            sys::fail(ERROR_ACCESS_DENIED);
            return 0;
        }
        ov
    } else {
        match copy_up(routes, rel, &winner) {
            Ok(p) => p,
            Err(e) => {
                sys::fail(io_code(&e));
                return 0;
            }
        }
    };
    let w = sys::wide_path(&target);
    // SAFETY: `w` is NUL-terminated.
    unsafe { FileSystem::SetFileAttributesW(w.as_ptr(), attrs) }
}

/// Copies `winner` to the overwrite path for `rel` (content preserved,
/// read-only bit cleared).
fn copy_up(routes: &Routes, rel: &str, winner: &Path) -> std::io::Result<PathBuf> {
    let target = routes.overwrite_path(rel);
    if target.exists() {
        return Ok(target);
    }
    routes.ensure_overwrite_parents(rel)?;
    std::fs::copy(winner, &target)?;
    clear_readonly(&target);
    routes.clear_deleted(rel);
    Ok(target)
}

fn imp_copy_file_w(src: PCWSTR, dst: PCWSTR, fail_if_exists: BOOL) -> BOOL {
    let fallback = || unsafe { FileSystem::CopyFileW(src, dst, fail_if_exists) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::wide_to_string(src) }, unsafe { sys::wide_to_string(dst) }) else {
        return fallback();
    };
    vfs_copy_file(routes, &src_s, &dst_s, fail_if_exists != 0).unwrap_or_else(|code| {
        sys::fail(code);
        0
    })
}

fn imp_copy_file_a(src: PCSTR, dst: PCSTR, fail_if_exists: BOOL) -> BOOL {
    let fallback = || unsafe { FileSystem::CopyFileA(src, dst, fail_if_exists) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::ansi_to_string(src) }, unsafe { sys::ansi_to_string(dst) }) else {
        return fallback();
    };
    vfs_copy_file(routes, &src_s, &dst_s, fail_if_exists != 0).unwrap_or_else(|code| {
        sys::fail(code);
        0
    })
}

/// CopyFile semantics for virtual paths. Returns `Err(code)` on failure,
/// `Ok(BOOL)` on handled calls.
fn vfs_copy_file(routes: &Routes, src: &str, dst: &str, fail_if_exists: bool) -> Result<BOOL, u32> {
    let src_rel = routes.rel_under_root(src);
    let dst_rel = routes.rel_under_root(dst);
    if src_rel.is_none() && dst_rel.is_none() {
        return Ok(unsafe {
            FileSystem::CopyFileW(sys::wide_str(src).as_ptr(), sys::wide_str(dst).as_ptr(), fail_if_exists as BOOL)
        });
    }
    // Resolve the source to real bytes.
    let src_real: PathBuf = match &src_rel {
        Some(rel) => match routes.resolve_read(rel) {
            Some(p) => p,
            None => return Err(ERROR_FILE_NOT_FOUND),
        },
        None => PathBuf::from(src),
    };
    match dst_rel {
        None => {
            // Destination is outside the VFS: copy the real winner out.
            let (s, d) = (sys::wide_path(&src_real), sys::wide_str(dst));
            Ok(unsafe { FileSystem::CopyFileW(s.as_ptr(), d.as_ptr(), fail_if_exists as BOOL) })
        }
        Some(dr) => {
            if fail_if_exists && routes.exists(&dr) {
                return Err(ERROR_FILE_EXISTS);
            }
            if let Some(parent) = util::parent_rel(&dr)
                && !routes.exists(&parent)
            {
                return Err(ERROR_PATH_NOT_FOUND);
            }
            routes.ensure_overwrite_parents(&dr).map_err(|e| io_code(&e))?;
            routes.clear_deleted(&dr);
            let target = routes.overwrite_path(&dr);
            let (s, d) = (sys::wide_path(&src_real), sys::wide_path(&target));
            // fail_if_exists handled above at the virtual level.
            Ok(unsafe { FileSystem::CopyFileW(s.as_ptr(), d.as_ptr(), 0) })
        }
    }
}

fn imp_move_file_w(src: PCWSTR, dst: PCWSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::MoveFileW(src, dst) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::wide_to_string(src) }, unsafe { sys::wide_to_string(dst) }) else {
        return fallback();
    };
    vfs_move(routes, &src_s, &dst_s, MOVEFILE_COPY_ALLOWED)
}

fn imp_move_file_a(src: PCSTR, dst: PCSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::MoveFileA(src, dst) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::ansi_to_string(src) }, unsafe { sys::ansi_to_string(dst) }) else {
        return fallback();
    };
    vfs_move(routes, &src_s, &dst_s, MOVEFILE_COPY_ALLOWED)
}

fn imp_move_file_ex_w(src: PCWSTR, dst: PCWSTR, flags: u32) -> BOOL {
    let fallback = || unsafe { FileSystem::MoveFileExW(src, dst, flags) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::wide_to_string(src) }, unsafe { sys::wide_to_string(dst) }) else {
        return fallback();
    };
    vfs_move(routes, &src_s, &dst_s, flags)
}

fn imp_move_file_ex_a(src: PCSTR, dst: PCSTR, flags: u32) -> BOOL {
    let fallback = || unsafe { FileSystem::MoveFileExA(src, dst, flags) };
    let Some(routes) = routes() else { return fallback() };
    let (Some(src_s), Some(dst_s)) = (unsafe { sys::ansi_to_string(src) }, unsafe { sys::ansi_to_string(dst) }) else {
        return fallback();
    };
    vfs_move(routes, &src_s, &dst_s, flags)
}

/// MoveFile semantics across the virtual boundary.
fn vfs_move(routes: &Routes, src: &str, dst: &str, flags: u32) -> BOOL {
    let src_rel = routes.rel_under_root(src);
    let dst_rel = routes.rel_under_root(dst);
    let replace = flags & MOVEFILE_REPLACE_EXISTING != 0;
    match (src_rel, dst_rel) {
        (None, None) => {
            let (s, d) = (sys::wide_str(src), sys::wide_str(dst));
            unsafe { FileSystem::MoveFileExW(s.as_ptr(), d.as_ptr(), flags) }
        }
        (None, Some(dr)) => {
            // A real file moved into the virtual root lands in the overwrite
            // layer — the merged view then shows it.
            if routes.exists(&dr) && !replace {
                sys::fail(ERROR_ALREADY_EXISTS);
                return 0;
            }
            if routes.exists(&dr) && delete_virtual(routes, &dr).is_err() {
                sys::fail(ERROR_ACCESS_DENIED);
                return 0;
            }
            if routes.ensure_overwrite_parents(&dr).is_err() {
                sys::fail(ERROR_PATH_NOT_FOUND);
                return 0;
            }
            routes.clear_deleted(&dr);
            let (s, d) = (sys::wide_str(src), sys::wide_path(&routes.overwrite_path(&dr)));
            unsafe { FileSystem::MoveFileExW(s.as_ptr(), d.as_ptr(), flags) }
        }
        (Some(sr), None) => {
            // The file leaves the VFS: copy real content out, hide the source.
            let Some(winner) = routes.resolve_read(&sr) else {
                sys::fail_not_found();
                return 0;
            };
            let dst_path = PathBuf::from(dst);
            if dst_path.exists() && !replace {
                sys::fail(ERROR_ALREADY_EXISTS);
                return 0;
            }
            let res = if winner.is_dir() {
                copy_dir_recursive(&winner, &dst_path)
            } else {
                std::fs::copy(&winner, &dst_path).map(|_| ())
            };
            if res.is_err() || routes.mark_deleted(&sr).is_err() {
                sys::fail(ERROR_ACCESS_DENIED);
                return 0;
            }
            1
        }
        (Some(sr), Some(dr)) => {
            if sr == dr {
                return 1;
            }
            let Some(winner) = routes.resolve_read(&sr) else {
                sys::fail_not_found();
                return 0;
            };
            if winner.is_dir() && (dr == format!("{sr}\\") || dr.starts_with(&format!("{sr}\\"))) {
                sys::fail(ERROR_ACCESS_DENIED);
                return 0;
            }
            if routes.exists(&dr) {
                if !replace {
                    sys::fail(ERROR_ALREADY_EXISTS);
                    return 0;
                }
                if delete_virtual(routes, &dr).is_err() {
                    sys::fail(ERROR_ACCESS_DENIED);
                    return 0;
                }
            }
            if let Some(parent) = util::parent_rel(&dr)
                && !routes.exists(&parent)
            {
                sys::fail(ERROR_PATH_NOT_FOUND);
                return 0;
            }
            if routes.ensure_overwrite_parents(&dr).is_err() {
                sys::fail(ERROR_PATH_NOT_FOUND);
                return 0;
            }
            routes.clear_deleted(&dr);
            let target = routes.overwrite_path(&dr);
            if routes.is_in_overwrite(&winner) {
                if std::fs::rename(&winner, &target).is_err() {
                    sys::fail(ERROR_ACCESS_DENIED);
                    return 0;
                }
            } else {
                let res = if winner.is_dir() {
                    copy_dir_recursive(&winner, &target)
                } else {
                    std::fs::copy(&winner, &target).map(|_| ())
                };
                if res.is_err() {
                    sys::fail(ERROR_ACCESS_DENIED);
                    return 0;
                }
                // Invalidate any read-only bit on the captured copy.
                clear_readonly(&target);
                if routes.mark_deleted(&sr).is_err() {
                    sys::fail(ERROR_ACCESS_DENIED);
                    return 0;
                }
            }
            1
        }
    }
}

fn imp_remove_directory_w(name: PCWSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::RemoveDirectoryW(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::wide_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_remove_dir(routes, &rel)
}

fn imp_remove_directory_a(name: PCSTR) -> BOOL {
    let fallback = || unsafe { FileSystem::RemoveDirectoryA(name) };
    let Some(routes) = routes() else { return fallback() };
    let Some(path) = (unsafe { sys::ansi_to_string(name) }) else { return fallback() };
    let Some(rel) = routes.rel_under_root(&path) else { return fallback() };
    vfs_remove_dir(routes, &rel)
}

/// RemoveDirectory: requires virtual emptiness, clears overwrite remnants
/// (whiteout markers), then hides lower providers.
fn vfs_remove_dir(routes: &Routes, rel: &str) -> BOOL {
    if rel.is_empty() || !routes.is_dir(rel) {
        sys::fail(ERROR_PATH_NOT_FOUND);
        return 0;
    }
    let (entries, _) = routes.enumerate(rel);
    if !entries.is_empty() {
        sys::fail(ERROR_DIR_NOT_EMPTY);
        return 0;
    }
    let ov = routes.overwrite_path(rel);
    if ov.exists() {
        // Merged view is empty: anything physically left inside is our own
        // bookkeeping (markers) — safe to drop recursively.
        let _ = std::fs::remove_dir_all(&ov);
    }
    if routes.lower_provides(rel) && routes.mark_deleted(rel).is_err() {
        sys::fail(ERROR_ACCESS_DENIED);
        return 0;
    }
    1
}

// ---------------------------------------------------------------------------
// Install / uninstall
// ---------------------------------------------------------------------------

type HookSpec = (&'static str, usize, &'static AtomicUsize);

/// `(export name, detour address, trampoline slot)` for every hooked API.
fn hook_table() -> Vec<HookSpec> {
    vec![
        ("CreateFileW", det_create_file_w as *const () as usize, &T_CREATE_FILE_W),
        ("CreateFileA", det_create_file_a as *const () as usize, &T_CREATE_FILE_A),
        ("CreateFile2", det_create_file_2 as *const () as usize, &T_CREATE_FILE_2),
        ("FindFirstFileW", det_find_first_file_w as *const () as usize, &T_FIND_FIRST_FILE_W),
        ("FindFirstFileA", det_find_first_file_a as *const () as usize, &T_FIND_FIRST_FILE_A),
        ("FindFirstFileExW", det_find_first_file_ex_w as *const () as usize, &T_FIND_FIRST_FILE_EX_W),
        ("FindNextFileW", det_find_next_file_w as *const () as usize, &T_FIND_NEXT_FILE_W),
        ("FindNextFileA", det_find_next_file_a as *const () as usize, &T_FIND_NEXT_FILE_A),
        ("FindClose", det_find_close as *const () as usize, &T_FIND_CLOSE),
        ("GetFileAttributesW", det_get_file_attributes_w as *const () as usize, &T_GET_FILE_ATTRIBUTES_W),
        ("GetFileAttributesA", det_get_file_attributes_a as *const () as usize, &T_GET_FILE_ATTRIBUTES_A),
        ("GetFileAttributesExW", det_get_file_attributes_ex_w as *const () as usize, &T_GET_FILE_ATTRIBUTES_EX_W),
        ("GetFileAttributesExA", det_get_file_attributes_ex_a as *const () as usize, &T_GET_FILE_ATTRIBUTES_EX_A),
        ("DeleteFileW", det_delete_file_w as *const () as usize, &T_DELETE_FILE_W),
        ("DeleteFileA", det_delete_file_a as *const () as usize, &T_DELETE_FILE_A),
        ("CreateDirectoryW", det_create_directory_w as *const () as usize, &T_CREATE_DIRECTORY_W),
        ("CreateDirectoryA", det_create_directory_a as *const () as usize, &T_CREATE_DIRECTORY_A),
        ("SetFileAttributesW", det_set_file_attributes_w as *const () as usize, &T_SET_FILE_ATTRIBUTES_W),
        ("SetFileAttributesA", det_set_file_attributes_a as *const () as usize, &T_SET_FILE_ATTRIBUTES_A),
        ("CopyFileW", det_copy_file_w as *const () as usize, &T_COPY_FILE_W),
        ("CopyFileA", det_copy_file_a as *const () as usize, &T_COPY_FILE_A),
        ("MoveFileW", det_move_file_w as *const () as usize, &T_MOVE_FILE_W),
        ("MoveFileA", det_move_file_a as *const () as usize, &T_MOVE_FILE_A),
        ("MoveFileExW", det_move_file_ex_w as *const () as usize, &T_MOVE_FILE_EX_W),
        ("MoveFileExA", det_move_file_ex_a as *const () as usize, &T_MOVE_FILE_EX_A),
        ("RemoveDirectoryW", det_remove_directory_w as *const () as usize, &T_REMOVE_DIRECTORY_W),
        ("RemoveDirectoryA", det_remove_directory_a as *const () as usize, &T_REMOVE_DIRECTORY_A),
    ]
}

/// Installs every hook on `kernel32.dll`/`kernelbase.dll` (deduped by
/// resolved address — kernel32 forwards most of these to KernelBase).
///
/// # Safety
/// Must be called once, before application threads run (our injector calls
/// `openvfs_hook_install` on a remote thread while the app is suspended).
pub unsafe fn install_all() -> Result<(), String> {
    let modules = ["kernel32.dll", "kernelbase.dll"];
    let mut hooked_addrs = std::collections::HashSet::new();
    let mut installed = Vec::new();
    let mut failures = Vec::new();
    for module in modules {
        let mw = sys::wide_str(module);
        for (name, detour, slot) in hook_table() {
            let Some(addr) = engine::proc_address(&mw, name) else {
                continue;
            };
            if !hooked_addrs.insert(addr) {
                continue; // forwarder: already patched
            }
            match unsafe { engine::install(addr, detour, slot, name) } {
                Ok(h) => installed.push(h),
                Err(e) => {
                    log::msg!("hook failed: {e}");
                    failures.push(e);
                }
            }
        }
    }
    if installed.is_empty() {
        return Err(format!("no hooks could be installed: {failures:?}"));
    }
    log::msg!("installed {} hooks ({} failures)", installed.len(), failures.len());
    let _ = HOOKS.set(Mutex::new(installed));
    Ok(())
}

/// Removes all installed hooks (restores original prologues).
///
/// # Safety
/// Dangerous if any thread is executing a hooked function; intended for
/// controlled teardown only.
pub unsafe fn uninstall_all() -> Result<(), String> {
    let Some(mutex) = HOOKS.get() else { return Ok(()) };
    let mut hooks = mutex.lock().map_err(|e| format!("{e}"))?;
    let mut first_err = None;
    while let Some(h) = hooks.pop() {
        if let Err(e) = unsafe { h.unhook() } {
            log::msg!("unhook failed: {e}");
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
