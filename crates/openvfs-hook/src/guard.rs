//! Thread-local recursion guard.
//!
//! Our detours perform real file operations internally (probing layer
//! directories, copying up files for write capture, building directory
//! listings). Those operations would re-enter the very hooks we installed.
//! Every detour starts by calling [`enter`]; if the guard is already held on
//! this thread, the detour passes the call straight to the original function.

use std::cell::Cell;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard; while it is alive this thread is "inside" the hook layer.
pub struct Guard {
    _private: (),
}

impl Drop for Guard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Enters the guarded region. Returns `None` if this thread is already inside
/// the hook layer — callers must then call the original function directly.
#[must_use]
pub fn enter() -> Option<Guard> {
    DEPTH.with(|d| {
        if d.get() == 0 {
            d.set(1);
            Some(Guard { _private: () })
        } else {
            None
        }
    })
}
