//! Raw Win32 plumbing shared by the detours: string conversion, error codes,
//! and helpers that call the *real* APIs (they re-enter the hooks and are
//! passed through by the recursion guard — see [`crate::guard`]).
//!
//! Everything here is `unsafe` because it dereferences caller-supplied FFI
//! pointers; each block carries a `// SAFETY:` note.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, SetLastError};
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar, WideCharToMultiByte};
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesExW, WIN32_FILE_ATTRIBUTE_DATA};
use windows_sys::core::{PCSTR, PCWSTR};

/// Reads a NUL-terminated UTF-16 string. `None` for null pointers.
///
/// # Safety
/// `p` must be a valid NUL-terminated UTF-16 pointer (or null) — which is the
/// contract of every hooked `PCWSTR` parameter.
pub unsafe fn wide_to_string(p: PCWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — valid NUL-terminated string.
    let len = unsafe { (0usize..).find(|&i| *p.add(i) == 0) }?;
    // SAFETY: `len` units at `p` were just proven readable/NUL-free.
    let slice = unsafe { std::slice::from_raw_parts(p, len) };
    Some(String::from_utf16_lossy(slice))
}

/// Reads a NUL-terminated ANSI (system codepage) string into UTF-16/UTF-8.
///
/// # Safety
/// `p` must be a valid NUL-terminated ANSI string pointer (or null).
pub unsafe fn ansi_to_string(p: PCSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller contract — valid NUL-terminated string.
    let len = unsafe { (0usize..).find(|&i| *p.add(i) == 0) }?;
    // SAFETY: `len` bytes at `p` are readable.
    let bytes = unsafe { std::slice::from_raw_parts(p, len) };
    if bytes.is_empty() {
        return Some(String::new());
    }
    // SAFETY: MultiByteToWideChar with a null output returns the needed size.
    let need = unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return None;
    }
    let mut buf = vec![0u16; need as usize];
    // SAFETY: `buf` has `need` units of capacity; input slice is valid.
    let got = unsafe { MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), bytes.len() as i32, buf.as_mut_ptr(), need) };
    if got <= 0 {
        return None;
    }
    buf.truncate(got as usize);
    Some(String::from_utf16_lossy(&buf))
}

/// NUL-terminated UTF-16 buffer for a host path, `\\?\`-prefixed when the
/// path exceeds the classic 260-char `MAX_PATH` (in components it stays
/// legal; without the prefix long paths would fail).
pub fn wide_path(path: &Path) -> Vec<u16> {
    let verbatim = path.as_os_str().to_string_lossy().starts_with("\\\\?\\");
    let mut w: Vec<u16> = OsStr::new(path).encode_wide().collect();
    // Rust's `\\?\` extended-length prefix for long absolute paths; skip it
    // when the path is already verbatim (canonicalized paths always are).
    if !verbatim && w.len() >= 260 && w.len() < 30000 {
        let mut prefixed: Vec<u16> = OsStr::new("\\\\?\\").encode_wide().collect();
        prefixed.extend_from_slice(&w);
        w = prefixed;
    }
    w.push(0);
    w
}

/// NUL-terminated UTF-16 buffer for a `&str`.
pub fn wide_str(s: &str) -> Vec<u16> {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    w.push(0);
    w
}

/// Sets the thread's `LastError` like a failing Win32 API would.
pub fn fail(code: u32) {
    // SAFETY: SetLastError is always safe to call.
    unsafe { SetLastError(code) };
}

/// `GetFileAttributesExW` on a real host path. Called with the recursion
/// guard held, so this goes through the (detoured) public entry point and is
/// passed on to the real implementation.
pub fn file_attr_data(path: &Path) -> Option<WIN32_FILE_ATTRIBUTE_DATA> {
    let w = wide_path(path);
    let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
    // SAFETY: `w` is a NUL-terminated UTF-16 buffer; `data` is a valid
    // WIN32_FILE_ATTRIBUTE_DATA out-pointer (GetFileExInfoStandard).
    let ok = unsafe { GetFileAttributesExW(w.as_ptr(), 0, &mut data as *mut _ as *mut _) };
    (ok != 0).then_some(data)
}

/// `WideCharToMultiByte` (CP_ACP) convenience for FindNextFileA-style output.
pub fn wide_to_ansi(name: &[u16]) -> Vec<u8> {
    let name = {
        // Trim at the first NUL.
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        &name[..end]
    };
    if name.is_empty() {
        return Vec::new();
    }
    // SAFETY: null output returns the required size.
    let need = unsafe {
        WideCharToMultiByte(
            CP_ACP,
            0,
            name.as_ptr(),
            name.len() as i32,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if need <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; need as usize];
    // SAFETY: `buf` capacity is `need`; input slice is valid UTF-16.
    let got = unsafe {
        WideCharToMultiByte(
            CP_ACP,
            0,
            name.as_ptr(),
            name.len() as i32,
            buf.as_mut_ptr(),
            need,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    buf.truncate(got.max(0) as usize);
    buf
}

/// The Win32 "file not found" error used when a virtual path resolves nowhere.
pub fn fail_not_found() {
    fail(ERROR_FILE_NOT_FOUND);
}
