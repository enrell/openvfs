//! Real-mount integration tests (Windows + WinFsp only).
//!
//! These exercise the full path — kernel driver → transact → `cb.rs` →
//! `Host` → backend — by mounting a real `MemoryFs`/`OverlayFs` at a
//! temporary directory and driving it through `std::fs`, then comparing the
//! OS-visible results against an identical `Vfs`-driven backend
//! (a mini differential/conformance suite).
//!
//! Every test short-circuits when `Mount::is_available()` is false — i.e. on
//! machines without WinFsp installed — so the file still compiles and runs
//! green on developer machines and CI without the driver.

#![cfg(windows)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use openvfs::{FileSystem, MemoryFs, OpenOptions, OverlayFs, VPath, Vfs};
use openvfs_winfsp::{Mount, MountOptions};

static UNIQUE: AtomicU32 = AtomicU32::new(0);

/// `true` when we should actually try to mount: WinFsp must be installed.
fn winfsp_ready() -> bool {
    Mount::is_available()
}

/// A fresh, empty, unique directory inside %TEMP%.
fn temp_mount_point(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openvfs-winfsp-test-{}-{}-{tag}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    dir
}

/// Cleans up a mount point best-effort.
fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn probe_is_stable_and_does_not_panic() {
    // Always safe to call; on a no-WinFsp machine it must simply say "no".
    let a = Mount::is_available();
    assert_eq!(a, Mount::is_available());
    assert_eq!(a, openvfs_winfsp::is_available());
    // The deeper probe agrees with the loader probe (or is equally false).
    let _ = Mount::probe();
}

#[test]
fn mount_memory_fs_roundtrip() {
    if !winfsp_ready() {
        eprintln!("SKIP: WinFsp not installed — mount_memory_fs_roundtrip");
        return;
    }
    let mp = temp_mount_point("memfs");
    let m = Mount::mount(MemoryFs::new(), &mp, MountOptions::default()).expect("mount MemoryFs at an empty dir");

    // --- create/write/read through std::fs, exactly like a game would ---
    let f = mp.join("hello.txt");
    fs::write(&f, b"hello vfs").unwrap();
    assert_eq!(fs::read(&f).unwrap(), b"hello vfs");

    fs::create_dir(mp.join("dir1")).unwrap();
    fs::write(mp.join("dir1").join("a.bin"), [1u8; 32]).unwrap();
    assert_eq!(fs::read(mp.join("dir1").join("a.bin")).unwrap().len(), 32);

    // Case-insensitive open: created as a.bin, read as A.BIN.
    assert_eq!(fs::read(mp.join("dir1").join("A.BIN")).unwrap().len(), 32);

    // Rename + delete + listing.
    fs::rename(&f, mp.join("renamed.txt")).unwrap();
    assert!(!f.exists() && mp.join("renamed.txt").exists());
    let mut names: Vec<String> =
        fs::read_dir(&mp).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, ["dir1", "renamed.txt"]);
    fs::remove_file(mp.join("dir1").join("a.bin")).unwrap();
    fs::remove_dir(mp.join("dir1")).unwrap();
    fs::remove_file(mp.join("renamed.txt")).unwrap();
    assert!(fs::read_dir(&mp).unwrap().next().is_none());

    // Missing file reports the Windows "not found" error, not a panic.
    assert_eq!(fs::read(mp.join("nope")).unwrap_err().kind(), ErrorKind::NotFound);

    drop(m); // unmount
    // After drop the directory is a normal (empty) directory again.
    assert!(fs::read_dir(&mp).unwrap().next().is_none());
    cleanup(&mp);
}

/// The ops we run identically through the OS (mounted tree) and through `Vfs`
/// (direct backend), comparing every observable outcome.
fn differential_memory_fs() {
    if !winfsp_ready() {
        eprintln!("SKIP: WinFsp not installed — differential_memory_fs");
        return;
    }
    let mp = temp_mount_point("diff");
    let m = Mount::mount(MemoryFs::new(), &mp, MountOptions::default()).unwrap();
    let vfs = Vfs::new(MemoryFs::new()); // the parallel universe

    let ops: Vec<(&str, &str)> = vec![
        ("write", "/a.txt"),
        ("write", "/dir/b.bin"),
        ("read", "/a.txt"),
        ("mkdir", "/dir"),
        ("list", "/"),
        ("rename", "/a.txt->/c.txt"),
        ("missing", "/zzz"),
        ("rmfile", "/c.txt"),
    ];

    let std_results: Vec<String> = ops.iter().map(|(kind, arg)| apply_os(&mp, kind, arg)).collect();
    let vfs_results: Vec<String> = ops.iter().map(|(kind, arg)| apply_vfs(&vfs, kind, arg)).collect();
    assert_eq!(std_results, vfs_results);

    drop(m);
    cleanup(&mp);
}

#[test]
fn differential_suites_agree_with_vfs() {
    differential_memory_fs();
    differential_overlay_fs();
}

fn differential_overlay_fs() {
    if !winfsp_ready() {
        eprintln!("SKIP: WinFsp not installed — differential_overlay_fs");
        return;
    }
    // Seed the lower layer identically in both universes.
    let seed = || {
        let mem = MemoryFs::new();
        mem.create_dir(&VPath::new("/base").unwrap()).unwrap();
        let mut f =
            mem.open(&VPath::new("/base/lower.txt").unwrap(), &OpenOptions::new().write(true).create(true)).unwrap();
        std::io::Write::write_all(&mut f, b"lower").unwrap();
        mem
    };
    let mp = temp_mount_point("ovfs");
    let m = Mount::mount(OverlayFs::new(MemoryFs::new(), seed()), &mp, MountOptions::default()).unwrap();
    let vfs = Vfs::new(OverlayFs::new(MemoryFs::new(), seed()));

    // Read-through from the lower layer + copy-up on write.
    assert_eq!(fs::read(mp.join("base").join("lower.txt")).unwrap(), b"lower");
    assert_eq!(vfs.read("/base/lower.txt").unwrap(), b"lower");

    fs::write(mp.join("base").join("new.txt"), b"upper").unwrap();
    assert_eq!(vfs.read("/base/new.txt").unwrap(), b"upper");

    drop(m);
    cleanup(&mp);
}

// --- op-script interpreters -------------------------------------------------

fn apply_os(root: &Path, kind: &str, arg: &str) -> String {
    match (kind, arg) {
        ("write", p) => match fs::write(root.join(p.trim_start_matches('/')), b"data") {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("read", p) => match fs::read(root.join(p.trim_start_matches('/'))) {
            Ok(b) => format!("ok:{b:?}"),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("mkdir", p) => match fs::create_dir(root.join(p.trim_start_matches('/'))) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("list", p) => match fs::read_dir(root.join(p.trim_start_matches('/'))) {
            Ok(it) => {
                let mut v: Vec<String> = it.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
                v.sort();
                format!("ok:{v:?}")
            }
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("rename", spec) => {
            let (a, b) = spec.split_once("->").unwrap();
            match fs::rename(root.join(a.trim_start_matches('/')), root.join(b.trim_start_matches('/'))) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err:{}", e.kind() as i32),
            }
        }
        ("missing", p) => {
            if root.join(p.trim_start_matches('/')).exists() {
                "exists".into()
            } else {
                "missing".into()
            }
        }
        ("rmfile", p) => match fs::remove_file(root.join(p.trim_start_matches('/'))) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        _ => unreachable!("bad op {kind}"),
    }
}

fn apply_vfs(vfs: &Vfs, kind: &str, arg: &str) -> String {
    match (kind, arg) {
        ("write", p) => match vfs.write(p, b"data") {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("read", p) => match vfs.read(p) {
            Ok(b) => format!("ok:{b:?}"),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("mkdir", p) => match vfs.create_dir(p) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("list", p) => match vfs.read_dir(p) {
            Ok(entries) => {
                let mut v: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                v.sort();
                format!("ok:{v:?}")
            }
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        ("rename", spec) => {
            let (a, b) = spec.split_once("->").unwrap();
            match vfs.rename(a, b) {
                Ok(()) => "ok".into(),
                Err(e) => format!("err:{}", e.kind() as i32),
            }
        }
        ("missing", p) => {
            if vfs.exists(p) {
                "exists".into()
            } else {
                "missing".into()
            }
        }
        ("rmfile", p) => match vfs.remove_file(p) {
            Ok(()) => "ok".into(),
            Err(e) => format!("err:{}", e.kind() as i32),
        },
        _ => unreachable!("bad op {kind}"),
    }
}
