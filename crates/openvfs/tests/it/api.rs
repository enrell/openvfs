//! The public API contract: auto traits, object safety, conversions, error
//! reporting, and properties of `VPath`.

use crate::common;

use std::io;
use std::sync::Arc;

use common::Rng;
use openvfs::prelude::*;
use openvfs::{PhysicalFsBuilder, Walk};

// --- compile-time guarantees ---------------------------------------------------

#[allow(dead_code)] // only used in const contexts; older rustc flags it
const fn assert_send_sync<T: Send + Sync>() {}
#[allow(dead_code)]
const fn assert_send<T: Send>() {}

const _: () = {
    assert_send_sync::<Vfs>();
    assert_send_sync::<VPath>();
    assert_send_sync::<VfsError>();
    assert_send_sync::<MemoryFs>();
    assert_send_sync::<PhysicalFs>();
    assert_send_sync::<PhysicalFsBuilder>();
    assert_send_sync::<OverlayFs>();
    assert_send_sync::<MountFs>();
    assert_send_sync::<ReadOnlyFs<MemoryFs>>();
    assert_send_sync::<Arc<dyn FileSystem>>();
    assert_send::<Box<dyn VfsFile>>();
    assert_send::<Walk>();
};

/// Errors must be usable with `?` in any `Box<dyn Error + Send + Sync>` context.
fn _error_is_static(e: VfsError) -> Box<dyn std::error::Error + Send + Sync + 'static> {
    Box::new(e)
}

#[test]
fn file_system_is_object_safe_and_composable() {
    let layers: Vec<Arc<dyn FileSystem>> = vec![
        Arc::new(MemoryFs::new()),
        Arc::new(ReadOnlyFs::new(MemoryFs::new())),
        Arc::new(MountFs::new()),
        Arc::new(OverlayFs::new(MemoryFs::new(), MemoryFs::new())),
    ];
    for layer in layers {
        assert!(Vfs::from_arc(layer).is_dir("/"));
    }
    // An Arc<dyn FileSystem> is itself a FileSystem, so layers nest freely.
    let shared: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    let nested = Vfs::new(ReadOnlyFs::new(Arc::clone(&shared)));
    Vfs::from_arc(shared).write("/x", "1").unwrap();
    assert_eq!(nested.read_to_string("/x").unwrap(), "1");
}

#[test]
fn vfs_clones_share_one_backend() {
    let a = Vfs::memory();
    let b = a.clone();
    a.write("/shared", "yes").unwrap();
    assert_eq!(b.read_to_string("/shared").unwrap(), "yes");
    assert!(Arc::ptr_eq(a.backend(), b.backend()));
    assert_eq!(format!("{a:?}"), "Vfs { .. }");
}

#[test]
fn debug_output_is_available_for_handles_and_walks() {
    let vfs = Vfs::memory();
    vfs.write("/f", "x").unwrap();
    assert_eq!(format!("{:?}", vfs.open_read("/f").unwrap()), "VfsFile { .. }");
    assert!(format!("{:?}", vfs.walk("/").unwrap().max_depth(2)).contains("max_depth: 2"));
    let err = vfs.open_read("/missing").unwrap_err(); // compiles only because handles are Debug
    assert!(err.is_not_found());
}

// --- errors --------------------------------------------------------------------------

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

#[test]
fn every_error_maps_to_the_right_io_kind() {
    let cases: Vec<(VfsError, io::ErrorKind)> = vec![
        (VfsError::NotFound(p("/a")), io::ErrorKind::NotFound),
        (VfsError::AlreadyExists(p("/a")), io::ErrorKind::AlreadyExists),
        (VfsError::NotADirectory(p("/a")), io::ErrorKind::NotADirectory),
        (VfsError::IsADirectory(p("/a")), io::ErrorKind::IsADirectory),
        (VfsError::DirectoryNotEmpty(p("/a")), io::ErrorKind::DirectoryNotEmpty),
        (VfsError::ReadOnly(p("/a")), io::ErrorKind::ReadOnlyFilesystem),
        (VfsError::PermissionDenied(p("/a")), io::ErrorKind::PermissionDenied),
        (VfsError::StorageFull(p("/a")), io::ErrorKind::StorageFull),
        (VfsError::PathEscapesRoot(p("/a")), io::ErrorKind::PermissionDenied),
        (VfsError::CrossDevice { from: p("/a"), to: p("/b") }, io::ErrorKind::CrossesDevices),
        (VfsError::InvalidPath("x".into()), io::ErrorKind::InvalidInput),
        (VfsError::InvalidInput("x"), io::ErrorKind::InvalidInput),
        (VfsError::Io { path: None, source: io::Error::other("boom") }, io::ErrorKind::Other),
    ];
    for (err, kind) in cases {
        assert_eq!(err.kind(), kind, "{err:?}");
        let display = err.to_string();
        assert!(!display.is_empty());
        if let Some(path) = err.path() {
            assert!(display.contains(path.as_str()), "display must name the path: {display}");
        }
        let io_err: io::Error = err.into();
        assert_eq!(io_err.kind(), kind);
        assert!(io_err.get_ref().unwrap().downcast_ref::<VfsError>().is_some(), "the VfsError is preserved inside");
    }
}

#[test]
fn io_errors_keep_their_source() {
    let err = VfsError::from(io::Error::other("root cause"));
    let source = std::error::Error::source(&err).expect("Io errors expose their source");
    assert_eq!(source.to_string(), "root cause");
    assert!(std::error::Error::source(&VfsError::NotFound(p("/x"))).is_none());
    assert!(VfsError::NotFound(p("/x")).is_not_found());
    assert!(!VfsError::ReadOnly(p("/x")).is_not_found());
}

#[test]
fn invalid_utf8_is_reported_as_invalid_data() {
    let vfs = Vfs::memory();
    vfs.write("/bin", [0xff, 0xfe]).unwrap();
    let err = vfs.read_to_string("/bin").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert_eq!(err.path().unwrap(), "/bin");
}

#[test]
fn nul_bytes_are_rejected_everywhere() {
    let vfs = Vfs::memory();
    assert!(matches!(vfs.write("/a\0b", "x"), Err(VfsError::InvalidPath(_))));
    assert!(matches!(vfs.read("/a\0b"), Err(VfsError::InvalidPath(_))));
    assert!(!vfs.exists("/a\0b"));
    assert!(vfs.try_exists("/a\0b").is_err());
}

#[test]
fn open_options_presets_and_validation() {
    assert!(OpenOptions::reading().read && !OpenOptions::reading().is_write());
    let w = OpenOptions::writing();
    assert!(w.write && w.create && w.truncate && w.is_write());
    let a = OpenOptions::appending();
    assert!(a.append && a.create && !a.truncate && a.is_write());
    for valid in [OpenOptions::reading(), w, a, OpenOptions::new().write(true).create_new(true)] {
        valid.validate().unwrap();
    }
    for invalid in [
        OpenOptions::new(),
        OpenOptions::new().read(true).create(true),
        OpenOptions::new().read(true).create_new(true),
        OpenOptions::new().append(true).truncate(true),
    ] {
        assert!(invalid.validate().is_err(), "{invalid:?}");
    }
}

#[test]
fn metadata_and_entry_helpers() {
    let file = Metadata::file(3).with_read_only(true);
    assert!(file.is_file() && !file.is_dir() && file.read_only && file.len == 3);
    assert!(Metadata::dir().is_dir());
    let vfs = Vfs::memory();
    vfs.create_dir("/d").unwrap();
    let entry = &vfs.read_dir("/").unwrap()[0];
    assert!(entry.is_dir() && !entry.is_file());
}

#[test]
fn change_display_and_accessors() {
    let added = Change::Added { path: p("/d"), kind: FileType::Dir };
    assert_eq!(added.to_string(), "A /d/");
    assert_eq!(Change::Modified { path: p("/f") }.to_string(), "M /f");
    assert_eq!(Change::Removed { path: p("/f"), kind: FileType::File }.to_string(), "D /f");
    assert_eq!(added.path(), "/d");
}

// --- VPath properties over random inputs ----------------------------------------------

fn random_raw_path(rng: &mut Rng) -> String {
    const PIECES: [&str; 10] = ["a", "b", "..", ".", "", "ç", "x y", "\\", "/", "..."];
    (0..rng.below(8)).map(|_| *rng.pick(&PIECES)).collect::<Vec<_>>().join(if rng.chance(50) { "/" } else { "\\" })
}

#[test]
fn vpath_normalization_properties() {
    let mut rng = Rng::new(2024);
    for _ in 0..5_000 {
        let raw = random_raw_path(&mut rng);
        let path = VPath::new(&raw).unwrap();
        let s = path.as_str();
        let context = format!("{raw:?} -> {s:?}");
        // Invariants.
        assert!(s.starts_with('/'), "{context}");
        assert!(s == "/" || !s.ends_with('/'), "{context}");
        assert!(path.components().all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\')), "{context}");
        // Idempotent: normalizing a normalized path is the identity.
        assert_eq!(VPath::new(s).unwrap(), path, "{context}");
        // Parent/child round trip.
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            assert_eq!(parent.join(name).unwrap(), path, "{context}");
            assert!(path.starts_with(&parent), "{context}");
            assert_eq!(parent.depth() + 1, path.depth(), "{context}");
        }
        // Everything is under the root, and stripping the root is the identity.
        assert!(path.starts_with(&VPath::root()));
        assert_eq!(path.strip_prefix(&VPath::root()).unwrap(), path);
        // Ancestors are ordered, end at the root, and each is a prefix.
        let ancestors: Vec<VPath> = path.ancestors().collect();
        assert_eq!(ancestors.len(), path.depth() + 1, "{context}");
        assert!(ancestors.iter().all(|a| path.starts_with(a)), "{context}");
        assert_eq!(ancestors.last().unwrap(), &VPath::root());
    }
}

#[test]
fn vpath_ordering_is_consistent_with_strings() {
    let mut rng = Rng::new(9);
    let mut paths: Vec<VPath> = (0..500).map(|_| VPath::new(random_raw_path(&mut rng)).unwrap()).collect();
    paths.sort();
    assert!(paths.windows(2).all(|w| w[0].as_str() <= w[1].as_str()));
    let hashed: std::collections::HashSet<VPath> = paths.iter().cloned().collect();
    let distinct: std::collections::BTreeSet<&str> = paths.iter().map(VPath::as_str).collect();
    assert_eq!(hashed.len(), distinct.len());
}
