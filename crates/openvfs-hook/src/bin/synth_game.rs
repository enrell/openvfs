//! `synth-game` — a stand-in "game" that exercises the Win32 file-API
//! surface the hook intercepts. It knows *nothing* about openvfs; it just
//! reads/writes/lists/deletes under a given root and prints `key=value`
//! lines so a test can diff behavior with and without the hook.
//!
//! Usage: `synth-game <root-dir> [--out <file>]`

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::{Path, PathBuf};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(root) = args.next() else {
        eprintln!("usage: synth-game <root> [--out <file>]");
        std::process::exit(2);
    };
    let mut out_file: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        if a == "--out" {
            out_file = args.next().map(PathBuf::from);
        }
    }
    let root = PathBuf::from(root);

    let mut lines: Vec<String> = Vec::new();
    run(&root, &mut lines);

    let text = lines.join("\n") + "\n";
    print!("{text}");
    if let Some(f) = out_file {
        let _ = std::fs::write(f, &text);
    }
}

fn rec(lines: &mut Vec<String>, key: &str, v: impl std::fmt::Display) {
    lines.push(format!("{key}={v}"));
}

fn read_to_s(p: &Path) -> String {
    match std::fs::read_to_string(p) {
        Ok(s) => s.trim().to_owned(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

fn list_dir(p: &Path) -> String {
    match std::fs::read_dir(p) {
        Ok(rd) => {
            let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names.join(",")
        }
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

fn try_write(p: &Path, data: &str) -> String {
    match std::fs::write(p, data) {
        Ok(()) => "ok".into(),
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(-1)),
    }
}

fn run(root: &Path, lines: &mut Vec<String>) {
    // -- reads through std::fs (CreateFileW + GetFileInformationByHandle) --
    rec(lines, "base", read_to_s(&root.join("base.txt")));
    rec(lines, "shared", read_to_s(&root.join("shared.txt")));
    rec(lines, "modonly", read_to_s(&root.join("mod_only.txt")));
    rec(lines, "missing", read_to_s(&root.join("does_not_exist.txt")));

    // -- enumeration: std::fs::read_dir (FindFirstFileExW) -----------------
    rec(lines, "list_std", list_dir(root));
    rec(lines, "list_sub", list_dir(&root.join("sub")));

    // -- enumeration: raw FindFirstFileW ----------------------------------
    rec(lines, "list_ff", find_files(root));

    // -- attributes --------------------------------------------------------
    rec(lines, "attr_shared", attrs(root.join("shared.txt")));
    rec(lines, "attr_missing", attrs(root.join("does_not_exist.txt")));

    // -- writes (overwrite capture) ---------------------------------------
    rec(lines, "write_new", try_write(&root.join("new_file.txt"), "created-by-game"));
    rec(lines, "write_over", try_write(&root.join("base_write.txt"), "modified-by-game"));
    rec(lines, "readback_new", read_to_s(&root.join("new_file.txt")));
    rec(lines, "readback_over", read_to_s(&root.join("base_write.txt")));

    // -- mkdir ------------------------------------------------------------
    rec(lines, "mkdir", create_dir(&root.join("newdir")));
    rec(lines, "mkdir_nested", try_write(&root.join("newdir").join("inner.txt"), "nested"));
    rec(lines, "mkdir_exist", create_dir(&root.join("sub")));

    // -- delete ------------------------------------------------------------
    rec(lines, "delete", delete_file(&root.join("to_delete.txt")));
    rec(lines, "deleted_still", root.join("to_delete.txt").exists());
    rec(lines, "delete_missing", delete_file(&root.join("does_not_exist.txt")));

    // -- rename / copy -----------------------------------------------------
    rec(lines, "rename", move_file(&root.join("rename_src.txt"), &root.join("renamed_dst.txt")));
    rec(lines, "renamed_read", read_to_s(&root.join("renamed_dst.txt")));
    rec(lines, "copy", copy_file(&root.join("base.txt"), &root.join("copied.txt")));
    rec(lines, "copied_read", read_to_s(&root.join("copied.txt")));

    // -- remove dir ---------------------------------------------------------
    let _ = create_dir(&root.join("emptydir"));
    rec(lines, "rmdir", remove_dir(&root.join("emptydir")));

    // -- set attributes ------------------------------------------------------
    rec(lines, "setattr", set_attrs(&root.join("base_write.txt"), 0x01)); // READONLY
    rec(lines, "setattr_back", set_attrs(&root.join("base_write.txt"), 0x80)); // NORMAL

    // -- relative path -------------------------------------------------------
    #[cfg(windows)]
    {
        let w = wide(root);
        // SAFETY: `w` is a NUL-terminated UTF-16 path buffer.
        if unsafe { windows_sys::Win32::System::Environment::SetCurrentDirectoryW(w.as_ptr()) } != 0 {
            rec(lines, "relative", read_to_s(Path::new("base.txt")));
        } else {
            rec(lines, "relative", "err:setcwd");
        }
    }
}

// --- raw Win32 helpers (Windows-only; the whole program targets Windows) ----

#[cfg(windows)]
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn find_files(dir: &Path) -> String {
    use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW};

    let pat = dir.join("*");
    let pat_w = wide(&pat);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pat_w` is a NUL-terminated UTF-16 buffer; `data` out-buffer.
    let h = unsafe { FindFirstFileW(pat_w.as_ptr(), &mut data) };
    if h == INVALID_HANDLE_VALUE {
        return format!("err:{}", unsafe { GetLastError() });
    }
    let mut names = Vec::new();
    loop {
        let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(data.cFileName.len());
        names.push(String::from_utf16_lossy(&data.cFileName[..len]));
        // SAFETY: `h` is a valid find handle; `data` out-buffer.
        if unsafe { FindNextFileW(h, &mut data) } == 0 {
            break;
        }
    }
    // SAFETY: `h` valid.
    unsafe { FindClose(h) };
    names.sort();
    names.join(",")
}

#[cfg(windows)]
fn attrs(p: PathBuf) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::GetFileAttributesW;
    let w = wide(&p);
    // SAFETY: `w` NUL-terminated.
    let v = unsafe { GetFileAttributesW(w.as_ptr()) };
    if v == u32::MAX { format!("err:{}", unsafe { GetLastError() }) } else { format!("{v:#x}") }
}

#[cfg(windows)]
fn create_dir(p: &Path) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let w = wide(p);
    // SAFETY: `w` NUL-terminated; NULL security attrs.
    let ok = unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

#[cfg(windows)]
fn delete_file(p: &Path) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::DeleteFileW;
    let w = wide(p);
    // SAFETY: `w` NUL-terminated.
    let ok = unsafe { DeleteFileW(w.as_ptr()) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

#[cfg(windows)]
fn move_file(src: &Path, dst: &Path) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;
    let (s, d) = (wide(src), wide(dst));
    // SAFETY: both buffers NUL-terminated.
    let ok = unsafe { MoveFileW(s.as_ptr(), d.as_ptr()) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

#[cfg(windows)]
fn copy_file(src: &Path, dst: &Path) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::CopyFileW;
    let (s, d) = (wide(src), wide(dst));
    // SAFETY: both buffers NUL-terminated.
    let ok = unsafe { CopyFileW(s.as_ptr(), d.as_ptr(), 0) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

#[cfg(windows)]
fn remove_dir(p: &Path) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::RemoveDirectoryW;
    let w = wide(p);
    // SAFETY: `w` NUL-terminated.
    let ok = unsafe { RemoveDirectoryW(w.as_ptr()) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

#[cfg(windows)]
fn set_attrs(p: &Path, attrs: u32) -> String {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let w = wide(p);
    // SAFETY: `w` NUL-terminated.
    let ok = unsafe { SetFileAttributesW(w.as_ptr(), attrs) };
    if ok != 0 { "ok".into() } else { format!("err:{}", unsafe { GetLastError() }) }
}

// Non-Windows stubs keep `cargo build` happy elsewhere.
#[cfg(not(windows))]
fn find_files(_: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn attrs(_: PathBuf) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn create_dir(_: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn delete_file(_: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn move_file(_: &Path, _: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn copy_file(_: &Path, _: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn remove_dir(_: &Path) -> String {
    "unsupported".into()
}
#[cfg(not(windows))]
fn set_attrs(_: &Path, _: u32) -> String {
    "unsupported".into()
}
