//! Diagnostics logging, enabled by the `OPENVFS_HOOK_LOG` environment
//! variable: a file path appends to that file; `1`/`true`/`stderr` logs to
//! the process's stderr; unset or `0` disables logging.

use std::fmt::Arguments;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

enum Sink {
    Stderr,
    File(std::fs::File),
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Initializes logging from `OPENVFS_HOOK_LOG`. Called once by `install()`.
pub fn init() {
    let Ok(spec) = std::env::var("OPENVFS_HOOK_LOG") else { return };
    let spec = spec.trim().to_owned();
    let sink = match spec.as_str() {
        "" | "0" | "off" | "no" => None,
        "1" | "true" | "stderr" | "-" => Some(Sink::Stderr),
        path => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(Sink::File)
            .map_err(|e| eprintln!("openvfs-hook: cannot open log {path}: {e}"))
            .ok(),
    };
    if let Ok(mut slot) = SINK.lock() {
        *slot = sink;
    }
}

fn with_sink(f: impl FnOnce(&mut Sink)) {
    let Ok(mut slot) = SINK.lock() else { return };
    if let Some(sink) = slot.as_mut() {
        f(sink);
    }
}

/// Writes one log line (newline added). Cheap no-op when logging is off.
pub fn line(args: Arguments<'_>) {
    with_sink(|sink| match sink {
        Sink::Stderr => {
            let _ = writeln!(std::io::stderr(), "openvfs-hook: {args}");
        }
        Sink::File(f) => {
            let _ = writeln!(f, "openvfs-hook: {args}");
        }
    });
}

/// `true` when logging is enabled — lets callers skip expensive formatting.
pub fn enabled() -> bool {
    SINK.lock().map(|s: MutexGuard<'_, Option<Sink>>| s.is_some()).unwrap_or(false)
}

/// `log::msg!("CreateFileW {} -> {}", a, b)`
macro_rules! msg {
    ($($arg:tt)*) => {
        if $crate::log::enabled() {
            $crate::log::line(format_args!($($arg)*));
        }
    };
}

pub(crate) use msg;
