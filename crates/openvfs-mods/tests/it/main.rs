//! The crate's integration tests, built as **one** test binary — same
//! pattern as `openvfs/tests/it`: one compile, one link, parallel suites.
//!
//! Run a single suite by name: `cargo test -p openvfs-mods --test it index::`.

mod common;

mod ci;
mod cli;
mod conflicts;
mod deploy;
mod deploy_faults;
mod differential_deploy;
mod index;
mod priority_prop;
mod profile;
mod scaling;

/// Sorts first, so the suite-wide deadline is armed before any test can hang.
#[test]
fn a_suite_deadline_is_armed() {
    common::arm_suite_deadline();
}
