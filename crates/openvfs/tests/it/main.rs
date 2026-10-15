//! The crate's integration tests, built as **one** test binary.
//!
//! Given one file per suite, Cargo would build ten executables. Each recompiles `common/`, links on
//! its own and, on Windows, pays a first-run antivirus scan of 1-2 s, which together cost far more
//! than the suites themselves take to run. One binary pays all of that once, and lets the test
//! harness run every suite in parallel.
//!
//! Run a single suite by name: `cargo test -p openvfs --test it differential::`.

mod common;

mod api;
mod concurrency;
mod conformance;
mod differential;
mod faults;
mod memory;
mod mount;
mod overlay;
mod physical;
mod scaling;

/// Sorts first, so the suite-wide deadline is armed before any other test can hang.
#[test]
fn a_suite_deadline_is_armed() {
    common::arm_suite_deadline();
}
