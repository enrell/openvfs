//! `MemoryFs`-specific guarantees: quota accounting, Unix-like handle
//! semantics, case sensitivity and directory timestamps.

use std::io::{Read, Seek, SeekFrom, Write};

use openvfs::prelude::*;

#[test]
fn quota_is_enforced_for_every_way_of_growing_data() {
    let fs = MemoryFs::with_quota(100);
    assert_eq!(fs.quota(), Some(100));
    let vfs = Vfs::new(fs);
    vfs.write("/a", [0u8; 60]).unwrap();
    assert!(matches!(vfs.write("/b", [0u8; 41]), Err(VfsError::StorageFull(p)) if p == "/b"));
    assert!(matches!(vfs.append("/a", [0u8; 41]), Err(VfsError::StorageFull(_))));
    assert!(matches!(vfs.set_len("/a", 101), Err(VfsError::StorageFull(_))));
    assert!(matches!(vfs.copy("/a", "/c"), Err(VfsError::StorageFull(_))));
    let mut f = vfs.open("/a", OpenOptions::new().write(true)).unwrap();
    f.seek(SeekFrom::Start(200)).unwrap();
    assert_eq!(f.write(b"x").unwrap_err().kind(), std::io::ErrorKind::StorageFull, "sparse write past quota");
    drop(f);
    vfs.write("/b", [0u8; 40]).unwrap(); // exactly at the limit
    assert_eq!(vfs.metadata("/a").unwrap().len, 60, "failed growth leaves the file unchanged");
}

#[test]
fn quota_accounting_tracks_frees() {
    let fs = std::sync::Arc::new(MemoryFs::with_quota(1_000));
    let vfs = Vfs::from(fs.clone());
    vfs.write("/a", [1u8; 300]).unwrap();
    vfs.copy("/a", "/b").unwrap();
    assert_eq!(fs.used_bytes(), 600);
    vfs.write("/a", [1u8; 100]).unwrap(); // truncate + rewrite
    assert_eq!(fs.used_bytes(), 400);
    vfs.set_len("/b", 50).unwrap();
    assert_eq!(fs.used_bytes(), 150);
    vfs.copy("/a", "/b").unwrap(); // overwrite: net change only
    assert_eq!(fs.used_bytes(), 200);
    vfs.remove_file("/a").unwrap();
    vfs.create_dir_all("/d/e").unwrap();
    vfs.write("/d/e/f", [1u8; 10]).unwrap();
    vfs.remove_dir_all("/d").unwrap();
    assert_eq!(fs.used_bytes(), 100);
    vfs.rename("/b", "/renamed").unwrap();
    assert_eq!(fs.used_bytes(), 100, "renames are free");
}

#[test]
fn bytes_of_removed_files_are_freed_when_the_last_handle_closes() {
    let fs = std::sync::Arc::new(MemoryFs::with_quota(1_000));
    let vfs = Vfs::from(fs.clone());
    vfs.write("/held", [1u8; 500]).unwrap();
    let handle = vfs.open_read("/held").unwrap();
    vfs.remove_file("/held").unwrap();
    assert_eq!(fs.used_bytes(), 500, "still referenced by an open handle");
    drop(handle);
    assert_eq!(fs.used_bytes(), 0);
}

#[test]
fn unbounded_by_default() {
    let fs = MemoryFs::new();
    assert_eq!(fs.quota(), None);
    let vfs = Vfs::new(fs);
    vfs.write("/big", vec![0u8; 8 << 20]).unwrap();
}

#[test]
fn handles_survive_removal_like_unix_unlink() {
    let vfs = Vfs::memory();
    vfs.write("/f", "before").unwrap();
    let mut handle = vfs.open("/f", OpenOptions::new().read(true).write(true)).unwrap();
    vfs.remove_file("/f").unwrap();
    assert!(!vfs.exists("/f"));
    handle.seek(SeekFrom::End(0)).unwrap();
    handle.write_all(b" and after").unwrap();
    handle.seek(SeekFrom::Start(0)).unwrap();
    let mut s = String::new();
    handle.read_to_string(&mut s).unwrap();
    assert_eq!(s, "before and after");
    vfs.write("/f", "new file").unwrap();
    assert_eq!(vfs.read_to_string("/f").unwrap(), "new file", "the new file is independent");
}

#[test]
fn handles_follow_renames() {
    let vfs = Vfs::memory();
    let mut handle = vfs.create("/old").unwrap();
    vfs.rename("/old", "/new").unwrap();
    handle.write_all(b"written after rename").unwrap();
    assert_eq!(vfs.read_to_string("/new").unwrap(), "written after rename");
}

#[test]
fn copy_rewrites_in_place_for_existing_handles() {
    let vfs = Vfs::memory();
    vfs.write("/src", "fresh").unwrap();
    vfs.write("/dst", "stale").unwrap();
    let mut reader = vfs.open_read("/dst").unwrap();
    vfs.copy("/src", "/dst").unwrap();
    let mut s = String::new();
    reader.read_to_string(&mut s).unwrap();
    assert_eq!(s, "fresh");
}

#[test]
fn paths_are_case_sensitive() {
    let vfs = Vfs::memory();
    vfs.write("/Readme", "upper").unwrap();
    vfs.write("/readme", "lower").unwrap();
    assert_eq!(vfs.read_to_string("/Readme").unwrap(), "upper");
    assert_eq!(vfs.read_dir("/").unwrap().len(), 2);
}

#[test]
fn directory_mtime_changes_when_entries_change() {
    let vfs = Vfs::memory();
    vfs.create_dir("/d").unwrap();
    let mut last = vfs.metadata("/d").unwrap().modified.unwrap();
    let steps: [&dyn Fn(&Vfs); 4] = [
        &|v| v.write("/d/f", "x").unwrap(),
        &|v| v.rename("/d/f", "/d/g").unwrap(),
        &|v| v.copy("/d/g", "/d/h").map(drop).unwrap(),
        &|v| v.remove_file("/d/g").unwrap(),
    ];
    for step in steps {
        std::thread::sleep(std::time::Duration::from_millis(2));
        step(&vfs);
        let now = vfs.metadata("/d").unwrap().modified.unwrap();
        assert!(now > last, "directory mtime must advance");
        last = now;
    }
}

#[test]
fn node_count_and_debug() {
    let fs = MemoryFs::with_quota(10);
    assert_eq!(fs.node_count(), 1);
    let debug = format!("{fs:?}");
    assert!(debug.contains("nodes: 1") && debug.contains("quota: Some(10)"), "{debug}");
}

#[test]
fn deep_and_wide_trees() {
    let vfs = Vfs::memory();
    let deep = (0..200).map(|i| format!("/d{i}")).collect::<Vec<_>>().concat();
    vfs.create_dir_all(&deep).unwrap();
    vfs.write(format!("{deep}/leaf"), "bottom").unwrap();
    assert_eq!(vfs.read_to_string(format!("{deep}/leaf")).unwrap(), "bottom");
    vfs.create_dir("/wide").unwrap();
    for i in 0..5_000 {
        vfs.write(format!("/wide/f{i:05}"), "").unwrap();
    }
    let listing = vfs.read_dir("/wide").unwrap();
    assert_eq!(listing.len(), 5_000);
    assert_eq!(listing[0].name, "f00000");
    vfs.rename("/wide", "/moved").unwrap();
    assert_eq!(vfs.read_dir("/moved").unwrap().len(), 5_000);
    vfs.remove_dir_all("/moved").unwrap();
    vfs.remove_dir_all("/d0").unwrap();
    assert!(vfs.read_dir("/").unwrap().is_empty());
}
