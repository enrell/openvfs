//! `openvfs-inject` — launches a program suspended, injects `openvfs_hook.dll`
//! into it via `CreateRemoteThread(LoadLibraryW)`, then resumes the main
//! thread and waits for the child to exit, propagating its exit code.
//!
//! Usage: `openvfs-inject --dll <path\to\openvfs_hook.dll> -- <exe> [args...]`
//!
//! The child inherits our environment (so `OPENVFS_HOOK_CONFIG` /
//! `OPENVFS_HOOK_LOG` flow down) and our standard handles (its stdout/stderr
//! are ours, which is what the e2e test captures).

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(windows, target_arch = "x86_64"))]
mod imp {
    use std::env;
    use std::ffi::OsStr;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::process::ExitCode;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, CreateRemoteThread, GetExitCodeProcess,
        GetExitCodeThread, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
        WaitForSingleObject,
    };

    /// Injector-side failures use exit codes ≥ 64 so the test can distinguish
    /// infrastructure problems from child exit codes.
    const E_USAGE: u8 = 64;
    const E_SPAWN: u8 = 65;
    const E_ALLOC: u8 = 66;
    const E_WRITE: u8 = 67;
    const E_THREAD: u8 = 68;
    const E_LOAD: u8 = 69;
    const E_WAIT: u8 = 70;

    fn die(code: u8, msg: &str) -> ! {
        eprintln!("openvfs-inject: {msg} (last_error={})", unsafe { GetLastError() });
        std::process::exit(code as i32);
    }

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    struct Handles(Vec<HANDLE>);
    impl Drop for Handles {
        fn drop(&mut self) {
            for h in self.0.drain(..) {
                // SAFETY: handle was opened successfully and not yet closed.
                unsafe { CloseHandle(h) };
            }
        }
    }

    pub fn run() -> ExitCode {
        let mut args = env::args_os().skip(1);
        let mut dll: Option<std::path::PathBuf> = None;
        let mut child: Vec<std::ffi::OsString> = Vec::new();
        let mut seen_sep = false;
        while let Some(a) = args.next() {
            if seen_sep {
                child.push(a);
            } else if a == "--dll" {
                dll = args.next().map(std::path::PathBuf::from);
            } else if a == "--" {
                seen_sep = true;
            } else {
                child.push(a); // tolerate missing `--`
            }
        }
        let Some(dll) = dll else { die(E_USAGE, "usage: openvfs-inject --dll <dll> -- <exe> [args]") };
        if child.is_empty() {
            die(E_USAGE, "usage: openvfs-inject --dll <dll> -- <exe> [args]");
        }
        let dll = std::fs::canonicalize(&dll).unwrap_or_else(|e| die(E_USAGE, &format!("dll {}: {e}", dll.display())));
        let dll_w = wide(dll.as_os_str());

        // Build the child command line: "exe" arg arg …
        let mut cmdline = String::from("\"");
        cmdline.push_str(&child[0].to_string_lossy());
        cmdline.push('"');
        for a in &child[1..] {
            cmdline.push(' ');
            cmdline.push('"');
            cmdline.push_str(&a.to_string_lossy());
            cmdline.push('"');
        }
        let mut cmd_w = wide(OsStr::new(&cmdline));

        let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        let mut handles = Handles(Vec::new());

        // SAFETY: buffers live for the duration of the call; `pi`/`si` are
        // valid out/in structs of the documented sizes.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmd_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1, // inherit handles: child shares our stdout/stderr
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            die(E_SPAWN, &format!("CreateProcessW({cmdline}) failed"));
        }
        handles.0.push(pi.hProcess);
        handles.0.push(pi.hThread);

        // --- Write the DLL path into the child ---------------------------
        // SAFETY: `pi.hProcess` is a valid handle to our suspended child.
        let remote_buf = unsafe {
            VirtualAllocEx(pi.hProcess, std::ptr::null(), dll_w.len() * 2, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        };
        if remote_buf.is_null() {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_ALLOC, "VirtualAllocEx failed");
        }
        let mut written = 0usize;
        // SAFETY: remote_buf is a valid writable region of dll_w.len()*2 bytes.
        let ok = unsafe {
            WriteProcessMemory(pi.hProcess, remote_buf, dll_w.as_ptr() as *const c_void, dll_w.len() * 2, &mut written)
        };
        if ok == 0 {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_WRITE, "WriteProcessMemory failed");
        }

        // --- LoadLibraryW remote thread ----------------------------------
        // kernel32 is mapped at the same address in every process, so a local
        // GetProcAddress is a valid remote entry point.
        // SAFETY: kernel32.dll is always loaded in this process.
        let k32 = unsafe { GetModuleHandleW(sys_wide("kernel32.dll").as_ptr()) };
        if k32.is_null() {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_LOAD, "GetModuleHandleW(kernel32) failed");
        }
        // SAFETY: k32 is valid; "LoadLibraryW\0" is a valid ANSI name.
        let loadlib = unsafe { GetProcAddress(k32, c"LoadLibraryW".as_ptr() as *const u8) };
        let Some(loadlib) = loadlib else {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_LOAD, "GetProcAddress(LoadLibraryW) failed");
        };
        // SAFETY: `loadlib` is a valid LPTHREAD_START_ROUTINE-equivalent
        // (LoadLibraryW signature is ABI-compatible), remote_buf is valid in
        // the child.
        let start: LPTHREAD_START_ROUTINE = unsafe { std::mem::transmute(loadlib) };
        // SAFETY: all handles/args valid as described above.
        let thread =
            unsafe { CreateRemoteThread(pi.hProcess, std::ptr::null(), 0, start, remote_buf, 0, std::ptr::null_mut()) };
        if thread.is_null() {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_THREAD, "CreateRemoteThread(LoadLibraryW) failed");
        }
        handles.0.push(thread);
        // SAFETY: `thread` is a valid thread handle.
        unsafe { WaitForSingleObject(thread, 30_000) };
        let mut code = 0u32;
        // SAFETY: `thread` valid; `code` out-ptr.
        unsafe { GetExitCodeThread(thread, &mut code) };
        if code == 0 {
            unsafe { TerminateProcess(pi.hProcess, 1) };
            die(E_LOAD, "LoadLibraryW in child failed (returned NULL)");
        }
        eprintln!("openvfs-inject: injected {} (module {code:#x})", dll.display());
        // SAFETY: remote_buf is ours to free.
        unsafe { VirtualFreeEx(pi.hProcess, remote_buf, 0, MEM_RELEASE) };

        // --- Resume and join the child ------------------------------------
        // SAFETY: hThread valid.
        unsafe { ResumeThread(pi.hThread) };
        // SAFETY: hProcess valid.
        let wait = unsafe { WaitForSingleObject(pi.hProcess, 0xFFFF_FFFF) }; // INFINITE
        if wait != 0 {
            die(E_WAIT, &format!("WaitForSingleObject -> {wait:#x}"));
        }
        let mut exit = 0u32;
        // SAFETY: hProcess valid.
        unsafe { GetExitCodeProcess(pi.hProcess, &mut exit) };
        eprintln!("openvfs-inject: child exited with {exit:#x}");
        // Propagate the *full* exit code (an NTSTATUS on crash) so callers
        // can distinguish child failures from infrastructure errors.
        std::process::exit(exit as i32);
    }

    fn sys_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // Keep Path referenced for docs builds.
    #[allow(dead_code)]
    fn _p(_: &Path) {}
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn main() -> std::process::ExitCode {
    imp::run()
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn main() -> std::process::ExitCode {
    eprintln!("openvfs-inject is only supported on 64-bit Windows");
    std::process::ExitCode::from(2)
}
