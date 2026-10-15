//! Path plumbing between the Windows world and the VFS world.
//!
//! WinFsp hands us volume-relative UTF-16 names such as `\dir\file.ext`;
//! [`openvfs`] wants normalized `/`-separated [`VPath`]s. This module owns that
//! conversion plus the *case folding* used for Windows-style case-insensitive
//! lookup. It is platform-independent so it can be unit-tested without the
//! WinFsp driver installed.
//!
//! ## Case sensitivity model
//!
//! Windows file systems resolve names case-insensitively while preserving the
//! stored case in directory listings (think `OPEN` on `Textures\A.DDS` when the
//! file is really `textures\a.dds`). The core [`VPath`] is case-sensitive, so
//! this module folds names with Unicode lowercase mapping — a pragmatic
//! approximation of the NTFS upcase table — and the host uses it to locate the
//! real-cased path behind every incoming name.

use std::cmp::Ordering;

use openvfs::VPath;

/// The outcome of normalizing a Windows path for the VFS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinPath {
    /// A normalized, in-tree path.
    Path(VPath),
    /// The name was syntactically unacceptable (`STATUS_OBJECT_NAME_INVALID`).
    Invalid,
}

/// `FSP_FSCTL_TRANSACT_PATH_SIZEMAX` in UTF-16 units — the longest path the
/// WinFsp protocol can express. Anything longer is rejected before it can
/// reach the VFS.
pub const MAX_PATH_UNITS: usize = 1024;

/// Converts a WinFsp volume-relative name to a [`VPath`].
///
/// WinFsp names always start with `\` (just `\` for the root) and never carry
/// `\\?\`, drive-letter or mount-point prefixes — those are consumed by Windows
/// before the request reaches the file system. We are still defensive: any
/// stray prefix form is rejected rather than silently reinterpreted, and `..`
/// can never climb above the virtual root because [`VPath::new`] clamps it
/// lexically.
#[must_use]
pub fn win_to_vpath(name: &str) -> WinPath {
    if name.encode_utf16().count() > MAX_PATH_UNITS {
        return WinPath::Invalid;
    }
    // Defense in depth: names arriving with device/prefix decorations are
    // rejected instead of being reinterpreted as VFS paths.
    let name = name.trim_start_matches('\\');
    if name.starts_with('?') || name.contains(':') {
        // `:` marks a named stream (`file:stream`) or a drive/device prefix;
        // `?` starts a `\\?\` device path. Neither is representable in the VFS.
        return WinPath::Invalid;
    }
    match VPath::new(name) {
        Ok(p) => WinPath::Path(p),
        Err(_) => WinPath::Invalid,
    }
}

/// Folds a path string for case-insensitive comparison.
///
/// Unicode lowercase is used instead of the NTFS upcase table; it agrees on
/// ASCII and the Latin/Greek/Cyrillic ranges that dominate file names.
#[must_use]
pub fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Folds a whole [`VPath`].
#[must_use]
pub fn fold_path(path: &VPath) -> String {
    fold(path.as_str())
}

/// Ordering key for a directory entry name in our enumeration order.
///
/// We order by the folded name first and the exact name second, which keeps
/// the order stable and total even when a directory contains names that differ
/// only in case (the VFS allows that; Windows tolerates it: both entries are
/// listed, though opening them resolves to the first match).
#[must_use]
pub fn entry_key(name: &str) -> (String, String) {
    (fold(name), name.to_owned())
}

/// Compares `name` against `marker` in enumeration order; used to resume a
/// `ReadDirectory` after `marker`. An entry is returned when it sorts strictly
/// after the marker.
#[must_use]
pub fn is_after_marker(name: &str, marker: &str) -> bool {
    match fold(name).cmp(&fold(marker)) {
        Ordering::Greater => true,
        Ordering::Equal => name > marker,
        Ordering::Less => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VPath {
        match win_to_vpath(s) {
            WinPath::Path(p) => p,
            WinPath::Invalid => panic!("{s} should be valid"),
        }
    }

    #[test]
    fn windows_names_normalize() {
        assert_eq!(p("\\"), VPath::root());
        assert_eq!(p("\\dir\\file.txt"), "/dir/file.txt");
        assert_eq!(p("\\Dir With Space\\"), "/Dir With Space");
        assert_eq!(p("\\a\\b\\c"), "/a/b/c");
        // Forward slashes and stray `.`/`..` are normalized away safely.
        assert_eq!(p("\\a\\.\\b\\..\\c"), "/a/c");
        assert_eq!(p("\\..\\..\\x"), "/x"); // clamps at root — can't escape
    }

    #[test]
    fn rejects_streams_and_prefixes() {
        assert_eq!(win_to_vpath("\\file:stream"), WinPath::Invalid);
        assert_eq!(win_to_vpath("\\??\\x"), WinPath::Invalid);
    }

    #[test]
    fn rejects_overlong_names() {
        let long = format!("\\{}", "x".repeat(MAX_PATH_UNITS));
        assert_eq!(win_to_vpath(&long), WinPath::Invalid);
        let at_limit = format!("\\{}", "x".repeat(MAX_PATH_UNITS - 1));
        assert!(matches!(win_to_vpath(&at_limit), WinPath::Path(_)));
    }

    #[test]
    fn folding_matches_windows_intent() {
        assert_eq!(fold("Textures\\A.DDS"), "textures\\a.dds");
        assert_eq!(fold_path(&VPath::new("/A/B").unwrap()), "/a/b");
        // Case-only differences order by the exact bytes: "A" < "a".
        assert!(is_after_marker("a", "A"));
        assert!(!is_after_marker("A", "a"));
        assert!(!is_after_marker("a", "a"));
        assert!(is_after_marker("b", "a"));
        assert!(!is_after_marker("a", "b"));
        // Same folded name, different case: order by exact bytes.
        assert!(is_after_marker("b", "B"));
        assert!(!is_after_marker("B", "b"));
    }
}
