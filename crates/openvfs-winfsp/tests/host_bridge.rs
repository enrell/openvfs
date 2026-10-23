//! Bridge-level tests: exercise the `Host` callback logic against real
//! backends (`MemoryFs`, `OverlayFs`, `ReadOnlyFs`) without needing the WinFsp
//! driver installed.
//!
//! `Host` is the platform-independent heart of the crate — every WinFsp
//! callback funnels into it after `cb.rs` decodes raw pointers. Testing it
//! directly covers the semantics the kernel sees (case-insensitive
//! resolution, per-open handles, the attribute side-map, directory markers,
//! read-only enforcement) on any build host.

use std::io::Write;
use std::sync::Arc;

use openvfs::{FileSystem, MemoryFs, OpenOptions, OverlayFs, ReadOnlyFs, VPath};
use openvfs_winfsp::MountOptions;
use openvfs_winfsp::abi::DirInfoBuf;
use openvfs_winfsp::host::{CLEANUP_DELETE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, Handle, Host};
use openvfs_winfsp::status::{
    STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY, STATUS_MEDIA_WRITE_PROTECTED, STATUS_NOT_A_DIRECTORY,
    STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
};

const RW: u32 = 0xC000_0000; // GENERIC_READ | GENERIC_WRITE
const R: u32 = 0x8000_0000; // GENERIC_READ
const W: u32 = 0x4000_0000; // GENERIC_WRITE
const NORMAL_ATTRS: u32 = 0x80; // FILE_ATTRIBUTE_NORMAL

fn host_with(fs: impl FileSystem + 'static, opts: &MountOptions) -> Arc<Host> {
    Arc::new(Host::new(Arc::new(fs), opts).unwrap())
}

fn default_host() -> Arc<Host> {
    host_with(MemoryFs::new(), &MountOptions::default())
}

/// The UTF-16 name inside a filled `DirInfoBuf`.
fn buf_name(d: &DirInfoBuf) -> String {
    let n = ((d.info.size as usize) - 104) / 2;
    String::from_utf16_lossy(&d.name[..n])
}

/// Collects every name `read_directory` emits (buffer treated as unbounded).
fn dir_names(h: &Host, dir: &Handle) -> Vec<String> {
    let mut names = Vec::new();
    let mut emit = |d: &mut DirInfoBuf| -> bool {
        names.push(buf_name(d));
        true // always room for more
    };
    assert!(h.read_directory(dir, None, &mut emit).unwrap());
    names
}

#[test]
fn create_open_write_read_roundtrip() {
    let h = default_host();

    // Creating under a missing parent fails like a real FS.
    assert_eq!(
        h.create("\\docs\\note.txt", 0, RW, NORMAL_ATTRS).map(|_| ()).unwrap_err(),
        STATUS_OBJECT_NAME_NOT_FOUND
    );

    h.create("\\docs", FILE_DIRECTORY_FILE, RW, NORMAL_ATTRS).unwrap();
    let (fh, info) = h.create("\\docs\\note.txt", 0, RW, NORMAL_ATTRS).unwrap();
    assert_eq!(info.file_attributes & 0x20, 0x20); // ARCHIVE default
    assert_ne!(info.creation_time, 0);
    assert_ne!(info.index_number, 0);

    // Offset I/O through the handle.
    let (n, fi2) = h.write(&fh, b"hello world", 0, false, false).unwrap();
    assert_eq!(n, 11);
    assert_eq!(fi2.file_size, 11);

    let mut buf = [0u8; 64];
    let n = h.read(&fh, &mut buf, 6).unwrap();
    assert_eq!(&buf[..n], b"world");
    assert_eq!(h.get_file_info(&fh).unwrap().file_size, 11);
}

#[test]
fn windows_case_insensitive_resolution() {
    let h = default_host();
    h.create("\\Textures", FILE_DIRECTORY_FILE, RW, NORMAL_ATTRS).unwrap();
    h.create("\\Textures\\A.DDS", 0, RW, NORMAL_ATTRS).unwrap();

    // Different casing resolves to the real file.
    let (fh, _) = h.open("\\textures\\a.dds", 0, R).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(h.read(&fh, &mut buf, 0).unwrap(), 0);

    // Folded parent components resolve for creation too.
    h.create("\\textures\\B.dds", 0, RW, NORMAL_ATTRS).unwrap();

    // Directory listing preserves the stored casing.
    let (dir, _) = h.open("\\textures", FILE_DIRECTORY_FILE, R).unwrap();
    let names = dir_names(&h, &dir);
    assert!(names.iter().any(|n| n == "."), "{names:?}");
    assert!(names.iter().any(|n| n == ".."), "{names:?}");
    assert!(names.iter().any(|n| n == "A.DDS"), "{names:?}");
    assert!(names.iter().any(|n| n == "B.dds"), "{names:?}");
    assert!(!names.iter().any(|n| n == "a.dds"), "{names:?}");
}

#[test]
fn create_collisions_and_kind_mismatches() {
    let h = default_host();
    h.create("\\x", 0, RW, NORMAL_ATTRS).unwrap();
    // Case-folded collision is still a collision.
    assert_eq!(h.create("\\X", 0, RW, NORMAL_ATTRS).map(|_| ()).unwrap_err(), STATUS_OBJECT_NAME_COLLISION);

    h.create("\\d", FILE_DIRECTORY_FILE, RW, NORMAL_ATTRS).unwrap();
    // With neither flag the FSD is looking the name up: a directory is served as one.
    assert!(h.open("\\d", 0, R).unwrap().0.is_dir);
    assert_eq!(h.open("\\d", FILE_NON_DIRECTORY_FILE, R).map(|_| ()).unwrap_err(), STATUS_FILE_IS_A_DIRECTORY);
    assert_eq!(h.open("\\x", FILE_DIRECTORY_FILE, R).map(|_| ()).unwrap_err(), STATUS_NOT_A_DIRECTORY);
}

#[test]
fn rename_moves_handle_and_folds_target_parent() {
    let h = default_host();
    h.create("\\SRC", FILE_DIRECTORY_FILE, RW, NORMAL_ATTRS).unwrap();
    let (fh, _) = h.create("\\SRC\\f.bin", 0, RW, NORMAL_ATTRS).unwrap();

    // Rename into a differently-cased existing parent.
    h.rename(&fh, "\\src\\renamed.bin", false).unwrap();
    assert!(h.open("\\SRC\\renamed.BIN", 0, R).is_ok());

    // Collision without `replace` fails; with `replace` it succeeds.
    let (fh2, _) = h.create("\\src\\other.bin", 0, RW, NORMAL_ATTRS).unwrap();
    assert_eq!(h.rename(&fh2, "\\src\\renamed.bin", false).map(|_| ()).unwrap_err(), STATUS_OBJECT_NAME_COLLISION);
    h.rename(&fh2, "\\SRC\\Renamed.Bin", true).unwrap();
    assert!(h.open("\\src\\RENAMED.bin", 0, R).is_ok());
}

#[test]
fn delete_via_cleanup_flags() {
    let h = default_host();
    h.create("\\d", FILE_DIRECTORY_FILE, RW, NORMAL_ATTRS).unwrap();
    h.create("\\d\\f", 0, RW, NORMAL_ATTRS).unwrap();

    // A non-empty directory cannot be deleted.
    let (dirh, _) = h.open("\\d", FILE_DIRECTORY_FILE, R).unwrap();
    assert_eq!(h.can_delete(&dirh).map(|_| ()).unwrap_err(), STATUS_DIRECTORY_NOT_EMPTY);

    let (fh, _) = h.open("\\d\\f", 0, R).unwrap();
    h.can_delete(&fh).unwrap();
    h.cleanup(&fh, CLEANUP_DELETE); // the deferred delete
    assert_eq!(h.open("\\d\\f", 0, R).map(|_| ()).unwrap_err(), STATUS_OBJECT_NAME_NOT_FOUND);

    h.can_delete(&dirh).unwrap(); // now empty — allowed
}

#[test]
fn read_only_volume_rejects_writes() {
    let h = host_with(MemoryFs::new(), &MountOptions::default().read_only(true));
    assert_eq!(h.create("\\f", 0, RW, NORMAL_ATTRS).map(|_| ()).unwrap_err(), STATUS_MEDIA_WRITE_PROTECTED);
}

#[test]
fn read_only_backend_rejects_writes() {
    let h = host_with(ReadOnlyFs::new(MemoryFs::new()), &MountOptions::default());
    assert_eq!(h.create("\\f", 0, W, NORMAL_ATTRS).map(|_| ()).unwrap_err(), STATUS_MEDIA_WRITE_PROTECTED);
}

#[test]
fn set_basic_info_attributes_and_times_survive() {
    let h = default_host();
    let (fh, _) = h.create("\\f", 0, RW, NORMAL_ATTRS).unwrap();

    const HIDDEN: u32 = 0x2;
    let info = h.set_basic_info(&fh, HIDDEN, 0, 0, 0, 0).unwrap();
    assert_eq!(info.file_attributes & HIDDEN, HIDDEN);

    // The 0xFFFFFFFF sentinel leaves attributes (and 0 leaves times) alone.
    let info2 = h.set_basic_info(&fh, 0xFFFF_FFFF, 1_330_000_000_000_000_000, 0, 0, 0).unwrap();
    assert_eq!(info2.file_attributes & HIDDEN, HIDDEN);
    assert_eq!(info2.creation_time, 1_330_000_000_000_000_000);
}

#[test]
fn overwrite_and_set_file_size() {
    let h = default_host();
    let (fh, _) = h.create("\\f", 0, RW, NORMAL_ATTRS).unwrap();
    h.write(&fh, b"12345678", 0, false, false).unwrap();
    assert_eq!(h.set_file_size(&fh, 4, false).unwrap().file_size, 4);
    assert_eq!(h.overwrite(&fh, 0, false, 0).unwrap().file_size, 0);
}

#[test]
fn marker_resumes_directory_enumeration() {
    let h = default_host();
    for n in ["a", "b", "c", "d"] {
        h.create(&format!("\\{n}"), 0, RW, NORMAL_ATTRS).unwrap();
    }
    let (dir, _) = h.open("\\", FILE_DIRECTORY_FILE, R).unwrap();

    // First "page": pretend the caller's buffer only fits two records.
    let mut emitted = Vec::new();
    let mut emit = |d: &mut DirInfoBuf| -> bool {
        if emitted.len() == 2 {
            return false; // caller buffer full — stop here
        }
        emitted.push(buf_name(d));
        true
    };
    assert!(!h.read_directory(&dir, None, &mut emit).unwrap()); // not finished
    assert_eq!(emitted, [".", ".."]);

    // Resume after "a": folded order is ".", "..", a, b, c, d.
    let mut names = Vec::new();
    let mut emit = |d: &mut DirInfoBuf| -> bool {
        names.push(buf_name(d));
        true
    };
    assert!(h.read_directory(&dir, Some("a"), &mut emit).unwrap());
    assert_eq!(names, ["b", "c", "d"]);
}

#[test]
fn get_dir_info_by_name_folds() {
    let h = default_host();
    h.create("\\MiXeD.TxT", 0, RW, NORMAL_ATTRS).unwrap();
    let (dir, _) = h.open("\\", FILE_DIRECTORY_FILE, R).unwrap();
    let buf = h.get_dir_info_by_name(&dir, "mixed.txt").unwrap();
    assert_eq!(buf_name(&buf), "MiXeD.TxT");
}

#[test]
fn works_over_overlayfs() {
    let lower = MemoryFs::new();
    // Prep the lower layer through the `FileSystem` trait so ownership stays.
    lower.create_dir(&VPath::new("/lowerdir").unwrap()).unwrap();
    {
        let mut f = lower
            .open(&VPath::new("/lowerdir/base.txt").unwrap(), &OpenOptions::new().write(true).create(true))
            .unwrap();
        f.write_all(b"base data").unwrap();
    }
    let h = host_with(OverlayFs::new(MemoryFs::new(), lower), &MountOptions::default());

    let (fh, _) = h.open("\\LOWERDIR\\BASE.TXT", 0, R).unwrap();
    let mut buf = [0u8; 16];
    let n = h.read(&fh, &mut buf, 0).unwrap();
    assert_eq!(&buf[..n], b"base data");

    // Writes go to the upper layer transparently.
    h.create("\\lowerdir\\new.txt", 0, RW, NORMAL_ATTRS).unwrap();
}

#[test]
fn invalid_names_are_rejected() {
    let h = default_host();
    // `:` marks a named stream — unsupported, must not reach the backend.
    assert!(h.open("\\f:hidden", 0, R).is_err());
    assert!(h.create("\\f:hidden", 0, RW, NORMAL_ATTRS).is_err());
}

/// Reads a whole directory the way the kernel does: `page` records at a time, each request
/// resuming after the last name of the one before.
fn paged_names(h: &Host, dir: &Handle, page: usize) -> Vec<String> {
    let mut names = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut taken = 0;
        let mut last = None;
        let mut emit = |d: &mut DirInfoBuf| -> bool {
            let name = buf_name(d);
            names.push(name.clone());
            last = Some(name);
            taken += 1;
            taken < page
        };
        if h.read_directory(dir, marker.as_deref(), &mut emit).unwrap() {
            return names;
        }
        marker = last;
    }
}

#[test]
fn a_paged_scan_lists_every_entry_once_in_order() {
    let h = default_host();
    for i in 0..100 {
        h.create(&format!("\\f{i:03}.dds"), 0, RW, NORMAL_ATTRS).unwrap();
    }
    let (dir, _) = h.open("\\", FILE_DIRECTORY_FILE, R).unwrap();
    let whole = dir_names(&h, &dir);
    assert_eq!(whole.len(), 102);
    for page in [1, 2, 7, 30, 101, 500] {
        assert_eq!(paged_names(&h, &dir, page), whole, "{page} records per request");
    }
}

#[test]
fn resuming_after_a_name_that_sorts_before_the_dots_does_not_repeat_them() {
    let h = default_host();
    for n in ["$first", "a", "b"] {
        h.create(&format!("\\{n}"), 0, RW, NORMAL_ATTRS).unwrap();
    }
    let (dir, _) = h.open("\\", FILE_DIRECTORY_FILE, R).unwrap();
    let mut names = Vec::new();
    let mut emit = |d: &mut DirInfoBuf| -> bool {
        names.push(buf_name(d));
        true
    };
    assert!(h.read_directory(&dir, Some("$first"), &mut emit).unwrap());
    assert_eq!(names, ["a", "b"]);
}

#[test]
fn a_scan_that_starts_over_sees_what_changed_since() {
    let h = default_host();
    h.create("\\a", 0, RW, NORMAL_ATTRS).unwrap();
    let (dir, _) = h.open("\\", FILE_DIRECTORY_FILE, R).unwrap();
    assert_eq!(dir_names(&h, &dir), [".", "..", "a"]);
    h.create("\\b", 0, RW, NORMAL_ATTRS).unwrap();
    assert_eq!(dir_names(&h, &dir), [".", "..", "a", "b"], "no marker means a new scan");
}
