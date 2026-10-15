//! `extern "C"` trampolines — the FFI boundary of the file system.
//!
//! Every function here is a slot in the `FSP_FILE_SYSTEM_INTERFACE` vtable
//! WinFsp calls on its dispatcher threads. Each one is intentionally thin:
//! decode raw arguments (`PWSTR` → `String`, `PVOID` → `&Handle`, buffers →
//! slices), run the safe [`Host`] logic inside [`catch_unwind`] so a panic can
//! never unwind into C code, and encode the result as an `NTSTATUS` plus
//! output parameters.
//!
//! # Safety invariants (audit guide)
//!
//! * `FileSystem->user_context` is the `*mut Host` installed by `mount`
//!   before the dispatcher starts; the `Host` outlives the file-system object
//!   (the `Mount` drops it only after `FspFileSystemDelete`), so it is valid
//!   for every call.
//! * `FileContext` is a `Box<Handle>` we created in `Create`/`Open` and freed
//!   in `Close`. WinFsp guarantees `Close` is the last operation for a handle
//!   and pairs every successful open with exactly one `Close`, so `ctx` is a
//!   valid, uniquely-owned `*mut Handle` until `Close`.
//! * `PWSTR` file names are NUL-terminated UTF-16 inside the request buffer;
//!   we scan with a hard bound and never trust a length we didn't check.
//! * I/O buffers (`Buffer`, `Length`) are valid for `Length` bytes for the
//!   duration of the call — guaranteed by the transact protocol.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;

use crate::abi::*;
use crate::host::{Handle, Host};
use crate::status::{
    NtStatus, STATUS_BUFFER_OVERFLOW, STATUS_INTERNAL_ERROR, STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER,
    STATUS_OBJECT_NAME_INVALID,
};

/// Hard bound for scanning a `PWSTR` (WinFsp paths are < 1024 UTF-16 units;
/// anything beyond 4096 is treated as corrupt input, not trusted).
const MAX_CSTR16: usize = 4096;

/// Runs `f` inside a panic boundary: panics become `STATUS_INTERNAL_ERROR`
/// instead of unwinding into the WinFsp dispatcher (which would be UB).
fn ffi(f: impl FnOnce() -> Result<(), NtStatus>) -> NtStatus {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => crate::status::STATUS_SUCCESS,
        Ok(Err(st)) => st,
        Err(_) => STATUS_INTERNAL_ERROR,
    }
}

/// [`ffi`] for `VOID` callbacks — swallows both errors and panics.
fn ffi_void(f: impl FnOnce()) {
    drop(catch_unwind(AssertUnwindSafe(f)));
}

/// Recovers the `&Host` for this call.
///
/// # Safety
/// `fs` is the live `FSP_FILE_SYSTEM` pointer the dispatcher passes to every
/// callback; `user_context` was set to a `*mut Host` before dispatcher start
/// and the `Host` outlives the file-system object.
unsafe fn host(fs: *mut FspFileSystem) -> Result<&'static Host, NtStatus> {
    if fs.is_null() {
        return Err(STATUS_INTERNAL_ERROR);
    }
    // SAFETY: `fs` is valid for the duration of the call (dispatcher contract).
    let ctx = unsafe { (*fs).user_context };
    if ctx.is_null() {
        return Err(STATUS_INTERNAL_ERROR);
    }
    // SAFETY: `ctx` is the stable `Box<Host>` pointer installed by `mount`,
    // valid until `FspFileSystemDelete` returns.
    Ok(unsafe { &*(ctx as *const Host) })
}

/// Recovers the `&Handle` from a `FileContext`.
///
/// # Safety
/// `ctx` is a `Box<Handle>` pointer produced by `Create`/`Open`, valid until
/// `Close`; WinFsp never calls operations on a closed handle.
unsafe fn handle(ctx: PVOID) -> Result<&'static Handle, NtStatus> {
    if ctx.is_null() {
        return Err(STATUS_INVALID_HANDLE);
    }
    // SAFETY: see fn docs — `ctx` points at a live `Handle`.
    Ok(unsafe { &*(ctx as *const Handle) })
}

/// Decodes a NUL-terminated UTF-16 `PWSTR` to a Rust `String` (bounded).
/// Null pointer → `INVALID_PARAMETER`; invalid UTF-16 → `OBJECT_NAME_INVALID`.
///
/// # Safety
/// `p` must point into a live WinFsp request buffer (dispatcher contract).
unsafe fn str16(p: PCWSTR) -> Result<String, NtStatus> {
    String::from_utf16(&unsafe { units16(p)? }).map_err(|_| STATUS_OBJECT_NAME_INVALID)
}

/// Optional [`str16`]: NULL → `None`.
///
/// # Safety
/// Same contract as [`str16`].
unsafe fn str16_opt(p: PCWSTR) -> Result<Option<String>, NtStatus> {
    if p.is_null() { Ok(None) } else { unsafe { str16(p) }.map(Some) }
}

/// Copies a NUL-terminated `PWSTR` into an owned `Vec<u16>` (bounded scan —
/// the buffer is only trusted for `MAX_CSTR16` units).
///
/// # Safety
/// `p` must be a valid pointer to a NUL-terminated UTF-16 string; the copied
/// units carry no lifetime dependency on the request buffer.
unsafe fn units16(p: PCWSTR) -> Result<Vec<u16>, NtStatus> {
    if p.is_null() {
        return Err(STATUS_INVALID_PARAMETER);
    }
    // SAFETY: scan for the NUL terminator within the documented bound, then
    // copy exactly those units into an owned Vec.
    unsafe {
        let mut n = 0usize;
        while n < MAX_CSTR16 {
            if *p.add(n) == 0 {
                return Ok(slice::from_raw_parts(p, n).to_vec());
            }
            n += 1;
        }
    }
    Err(STATUS_OBJECT_NAME_INVALID)
}

/// The vtable handed to `FspFileSystemCreate`. `None` slots are NULL function
/// pointers — WinFsp probes them before use, so unsupported operations degrade
/// gracefully (the FSD answers them itself or returns `STATUS_NOT_SUPPORTED`).
pub static INTERFACE: FspFileSystemInterface = FspFileSystemInterface {
    get_volume_info: Some(get_volume_info),
    set_volume_label: Some(set_volume_label),
    get_security_by_name: Some(get_security_by_name),
    create: Some(create),
    open: Some(open),
    overwrite: Some(overwrite),
    cleanup: Some(cleanup),
    close: Some(close),
    read: Some(read),
    write: Some(write),
    flush: Some(flush),
    get_file_info: Some(get_file_info),
    set_basic_info: Some(set_basic_info),
    set_file_size: Some(set_file_size),
    can_delete: Some(can_delete),
    rename: Some(rename),
    get_security: Some(get_security),
    set_security: Some(set_security),
    read_directory: Some(read_directory),
    resolve_reparse_points: None,
    get_reparse_point: None,
    set_reparse_point: None,
    delete_reparse_point: None,
    get_stream_info: None,
    get_dir_info_by_name: Some(get_dir_info_by_name),
    control: None,
    set_delete: Some(set_delete),
    create_ex: None,
    overwrite_ex: None,
    get_ea: None,
    set_ea: None,
    obsolete0: None,
    dispatcher_stopped: Some(dispatcher_stopped),
    reserved: [None; 31],
};

unsafe extern "C" fn get_volume_info(fs: *mut FspFileSystem, info: *mut FspFsctlVolumeInfo) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        if info.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: `info` is a valid out-param for the call duration.
        unsafe { ptr::write(info, h.get_volume_info()) };
        Ok(())
    })
}

unsafe extern "C" fn set_volume_label(fs: *mut FspFileSystem, label: PWSTR, info: *mut FspFsctlVolumeInfo) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // SAFETY: `label` is a NUL-terminated request string.
        let units = unsafe { units16(label)? };
        let vi = h.set_volume_label(&units)?;
        if !info.is_null() {
            // SAFETY: `info` is a valid out-param.
            unsafe { ptr::write(info, vi) };
        }
        Ok(())
    })
}

unsafe extern "C" fn get_security_by_name(
    fs: *mut FspFileSystem,
    file_name: PWSTR,
    p_file_attributes: *mut UINT32,
    _security_descriptor: PVOID,
    p_sd_size: *mut SIZE_T,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // SAFETY: `file_name` is a NUL-terminated request string.
        let name = unsafe { str16(file_name)? };
        let attrs = h.get_security_by_name(&name)?;
        if !p_file_attributes.is_null() {
            // SAFETY: `p_file_attributes` is a valid out-param.
            unsafe { *p_file_attributes = attrs };
        }
        if !p_sd_size.is_null() {
            // SAFETY: `p_sd_size` is a valid in/out param. Reporting size 0
            // means "no descriptor": WinFsp then skips the AccessCheck, which
            // is the right model for an ACL-free VFS (see crate docs).
            unsafe { *p_sd_size = 0 };
        }
        Ok(())
    })
}

/// Shared tail of `create`/`open`: box the handle into a `FileContext` and
/// fill the caller's `FileInfo`.
fn open_out(
    h: &Host,
    result: Result<(Handle, FspFsctlFileInfo), NtStatus>,
    p_file_context: *mut PVOID,
    file_info: *mut FspFsctlFileInfo,
) -> Result<(), NtStatus> {
    let (handle, info) = result?;
    if p_file_context.is_null() || file_info.is_null() {
        return Err(STATUS_INVALID_PARAMETER);
    }
    // SAFETY: both are valid out-params for the call. `Box::into_raw` hands
    // the handle's ownership to the kernel side; `close` takes it back.
    unsafe {
        ptr::write(file_info, info);
        let raw = Box::into_raw(Box::new(handle)) as usize;
        h.register_handle(raw);
        *p_file_context = raw as PVOID;
    }
    Ok(())
}

unsafe extern "C" fn create(
    fs: *mut FspFileSystem,
    file_name: PWSTR,
    create_options: UINT32,
    granted_access: UINT32,
    file_attributes: UINT32,
    _security_descriptor: PVOID,
    _allocation_size: UINT64,
    p_file_context: *mut PVOID,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // SAFETY: `file_name` is a NUL-terminated request string.
        let name = unsafe { str16(file_name)? };
        open_out(h, h.create(&name, create_options, granted_access, file_attributes), p_file_context, file_info)
    })
}

unsafe extern "C" fn open(
    fs: *mut FspFileSystem,
    file_name: PWSTR,
    create_options: UINT32,
    granted_access: UINT32,
    p_file_context: *mut PVOID,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // SAFETY: `file_name` is a NUL-terminated request string.
        let name = unsafe { str16(file_name)? };
        open_out(h, h.open(&name, create_options, granted_access), p_file_context, file_info)
    })
}

unsafe extern "C" fn overwrite(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    file_attributes: UINT32,
    replace_file_attributes: BOOLEAN,
    allocation_size: UINT64,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // SAFETY: `file_context` is a live Handle.
        let handle = unsafe { handle(file_context)? };
        let info = h.overwrite(handle, file_attributes, replace_file_attributes != 0, allocation_size)?;
        if !file_info.is_null() {
            // SAFETY: `file_info` is a valid out-param.
            unsafe { ptr::write(file_info, info) };
        }
        Ok(())
    })
}

unsafe extern "C" fn cleanup(fs: *mut FspFileSystem, file_context: PVOID, _file_name: PWSTR, flags: ULONG) {
    ffi_void(|| {
        // `FileName` is informational — the handle's resolved path is better.
        if let (Ok(h), Ok(handle)) = (unsafe { host(fs) }, unsafe { handle(file_context) }) {
            h.cleanup(handle, flags);
        }
    });
}

unsafe extern "C" fn close(fs: *mut FspFileSystem, file_context: PVOID) {
    ffi_void(|| {
        if let Ok(h) = unsafe { host(fs) } {
            h.unregister_handle(file_context as usize);
        }
        if !file_context.is_null() {
            // SAFETY: `file_context` was produced by `Box::into_raw` in
            // `create`/`open`, was never freed, and `Close` is the last op —
            // unique ownership returns here exactly once.
            drop(unsafe { Box::from_raw(file_context as *mut Handle) });
        }
    });
}

unsafe extern "C" fn read(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    buffer: PVOID,
    offset: UINT64,
    length: ULONG,
    p_bytes_transferred: *mut ULONG,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        if (buffer.is_null() && length != 0) || p_bytes_transferred.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: `buffer` is valid for `length` bytes for the call duration;
        // a zero-length read may pass NULL, so fall back to a dangling
        // (aligned, non-null) pointer — `from_raw_parts_*` forbids NULL even
        // at length 0.
        let data = if buffer.is_null() { ptr::NonNull::dangling().as_ptr() } else { buffer as *mut u8 };
        let buf = unsafe { slice::from_raw_parts_mut(data, length as usize) };
        let n = h.read(handle, buf, offset)?;
        // SAFETY: `p_bytes_transferred` is a valid out-param.
        unsafe { *p_bytes_transferred = n as ULONG };
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn write(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    buffer: PVOID,
    offset: UINT64,
    length: ULONG,
    write_to_end_of_file: BOOLEAN,
    constrained_io: BOOLEAN,
    p_bytes_transferred: *mut ULONG,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        if (buffer.is_null() && length != 0) || p_bytes_transferred.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: `buffer` is valid for `length` bytes for the call duration;
        // NULL at length 0 falls back to a dangling pointer (see `read`).
        let data = if buffer.is_null() { ptr::NonNull::dangling().as_ptr() } else { buffer as *const u8 };
        let buf = unsafe { slice::from_raw_parts(data, length as usize) };
        let (n, info) = h.write(handle, buf, offset, write_to_end_of_file != 0, constrained_io != 0)?;
        // SAFETY: `p_bytes_transferred` and `file_info` are valid out-params.
        unsafe {
            *p_bytes_transferred = n as ULONG;
            if !file_info.is_null() {
                ptr::write(file_info, info);
            }
        }
        Ok(())
    })
}

unsafe extern "C" fn flush(fs: *mut FspFileSystem, file_context: PVOID, file_info: *mut FspFsctlFileInfo) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        // A NULL context flushes the whole volume (no per-file info to report).
        let handle = if file_context.is_null() { None } else { Some(unsafe { handle(file_context)? }) };
        if let Some(info) = h.flush(handle)?
            && !file_info.is_null()
        {
            // SAFETY: `file_info` is a valid out-param.
            unsafe { ptr::write(file_info, info) };
        }
        Ok(())
    })
}

unsafe extern "C" fn get_file_info(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        if file_info.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: `file_info` is a valid out-param.
        unsafe { ptr::write(file_info, h.get_file_info(handle)?) };
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn set_basic_info(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    file_attributes: UINT32,
    creation_time: UINT64,
    last_access_time: UINT64,
    last_write_time: UINT64,
    change_time: UINT64,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        let info =
            h.set_basic_info(handle, file_attributes, creation_time, last_access_time, last_write_time, change_time)?;
        if !file_info.is_null() {
            // SAFETY: `file_info` is a valid out-param.
            unsafe { ptr::write(file_info, info) };
        }
        Ok(())
    })
}

unsafe extern "C" fn set_file_size(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    new_size: UINT64,
    set_allocation_size: BOOLEAN,
    file_info: *mut FspFsctlFileInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        let info = h.set_file_size(handle, new_size, set_allocation_size != 0)?;
        if !file_info.is_null() {
            // SAFETY: `file_info` is a valid out-param.
            unsafe { ptr::write(file_info, info) };
        }
        Ok(())
    })
}

unsafe extern "C" fn can_delete(fs: *mut FspFileSystem, file_context: PVOID, _file_name: PWSTR) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        // `FileName` is informational — the handle's resolved path is better.
        h.can_delete(handle)
    })
}

unsafe extern "C" fn set_delete(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    _file_name: PWSTR,
    delete_file: BOOLEAN,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        h.set_delete(handle, delete_file != 0)
    })
}

unsafe extern "C" fn rename(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    _file_name: PWSTR,
    new_file_name: PWSTR,
    replace_if_exists: BOOLEAN,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        // SAFETY: `new_file_name` is a NUL-terminated request string.
        let new_name = unsafe { str16(new_file_name)? };
        h.rename(handle, &new_name, replace_if_exists != 0)
    })
}

unsafe extern "C" fn get_security(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    security_descriptor: PVOID,
    p_sd_size: *mut SIZE_T,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let _ = unsafe { handle(file_context)? }; // must be a live handle
        if p_sd_size.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let sd = h.get_security();
        // SAFETY: `p_sd_size` is a valid in/out param carrying the buffer size.
        let avail = unsafe { *p_sd_size };
        if security_descriptor.is_null() || avail < sd.len() {
            // SAFETY: `p_sd_size` is a valid out-param.
            unsafe { *p_sd_size = sd.len() };
            return Err(STATUS_BUFFER_OVERFLOW);
        }
        // SAFETY: the caller's buffer holds `avail >= sd.len()` bytes.
        unsafe {
            ptr::copy_nonoverlapping(sd.as_ptr(), security_descriptor as *mut u8, sd.len());
            *p_sd_size = sd.len();
        }
        Ok(())
    })
}

unsafe extern "C" fn set_security(
    _fs: *mut FspFileSystem,
    _file_context: PVOID,
    _security_information: UINT32,
    _modification_descriptor: PVOID,
) -> NTSTATUS {
    // The VFS doesn't persist ACLs; accept-and-ignore is the friendliest
    // behavior (tools that "set" permissions succeed without effect, matching
    // what other ACL-free WinFsp file systems do).
    ffi(|| Ok(()))
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn read_directory(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    _pattern: PWSTR,
    marker: PWSTR,
    buffer: PVOID,
    length: ULONG,
    p_bytes_transferred: *mut ULONG,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        if p_bytes_transferred.is_null() || buffer.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: `marker` is NUL-terminated or NULL. `_pattern` is ignored:
        // the FSD performs its own pattern matching (documented behavior of
        // WinFsp file systems — see `PassQueryDirectoryPattern`).
        let marker = unsafe { str16_opt(marker)? };
        // `api()` is safe; the DLL is certainly loaded — it is what called us.
        let add = crate::sys::api().map_err(|_| STATUS_INTERNAL_ERROR)?.add_dir_info;
        let pbytes = p_bytes_transferred;
        let mut emit = |d: &mut DirInfoBuf| -> bool {
            // SAFETY: `d.info` is a valid DirInfo of `size` bytes; `buffer`,
            // `length` and `pbytes` are this call's ReadDirectory params.
            unsafe { add(&raw mut d.info, buffer, length, pbytes) != 0 }
        };
        let done = h.read_directory(handle, marker.as_deref(), &mut emit)?;
        if done {
            // A NULL DirInfo is the EOF marker telling the FSD the listing
            // is complete.
            unsafe { add(ptr::null_mut(), buffer, length, pbytes) };
        }
        Ok(())
    })
}

unsafe extern "C" fn get_dir_info_by_name(
    fs: *mut FspFileSystem,
    file_context: PVOID,
    file_name: PWSTR,
    dir_info: *mut FspFsctlDirInfo,
) -> NTSTATUS {
    ffi(|| {
        let h = unsafe { host(fs)? };
        let handle = unsafe { handle(file_context)? };
        // SAFETY: `file_name` is a NUL-terminated single-component name.
        let name = unsafe { str16(file_name)? };
        let buf = h.get_dir_info_by_name(handle, &name)?;
        if dir_info.is_null() {
            return Err(STATUS_INVALID_PARAMETER);
        }
        // SAFETY: the caller's DirInfo buffer is `sizeof(FSP_FSCTL_DIR_INFO) +
        // 255 WCHARs` per the WinFsp calling convention — at least `size` bytes.
        unsafe {
            ptr::copy_nonoverlapping(&raw const buf as *const u8, dir_info as *mut u8, buf.info.size as usize);
        }
        Ok(())
    })
}

unsafe extern "C" fn dispatcher_stopped(fs: *mut FspFileSystem, normally: BOOLEAN) {
    ffi_void(|| {
        if let Ok(h) = unsafe { host(fs) } {
            h.dispatcher_stopped(normally != 0);
        }
    });
}
