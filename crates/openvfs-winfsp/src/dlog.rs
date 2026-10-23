//! Optional debug log for the WinFsp callbacks.
//!
//! Writes one line per interesting callback to the file named by the
//! `OPENVFS_WINFSP_LOG` environment variable. When the variable is unset
//! (the normal case) nothing is written and the log costs one atomic load
//! per call.

use std::io::Write;

/// Appends one line to the debug log, if one is configured.
pub fn dlog(msg: &str) {
    let Some(path) = std::env::var_os("OPENVFS_WINFSP_LOG") else {
        return;
    };
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(f, "{msg}");
}

/// Logs the start of a callback: `ENTER op  path`. A call that never
/// completes (a hang) is the last `ENTER` with no matching result line.
pub fn dlog_enter(op: &str, path: &str) {
    if std::env::var_os("OPENVFS_WINFSP_LOG").is_none() {
        return;
    }
    dlog(&format!("ENTER {op:20} {path}"));
}

/// Logs a callback with its result: `op  path  ->  status`.
pub fn dlog_call(op: &str, path: &str, status: &str) {
    if std::env::var_os("OPENVFS_WINFSP_LOG").is_none() {
        return;
    }
    dlog(&format!("{op:24} {path:60} -> {status}"));
}
