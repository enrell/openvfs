//! Fault injection: failures at *every* possible point — under both a
//! dead-disk and a transient fault model — must leave state consistent and
//! recoverable, exactly as documented.

use crate::common;

use std::sync::Arc;

use common::faulty::FaultyFs;
use common::{Rng, Tree, assert_trees_eq, model, names, populate, snapshot, tree};
use openvfs::ops;
use openvfs::prelude::*;

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    DeadDisk,
    Transient,
}

const MODELS: [Model; 2] = [Model::DeadDisk, Model::Transient];

fn arm<F: FileSystem>(fs: &FaultyFs<F>, model: Model, n: usize) {
    match model {
        Model::DeadDisk => fs.fail_from(n),
        Model::Transient => fs.fail_once_at(n),
    }
}

/// Runs `attempt(n)` for n = 0, 1, 2, … until an attempt completes without
/// hitting the fault. Returns the number of distinct failure points.
fn every_failure_point(mut attempt: impl FnMut(usize) -> bool) -> usize {
    (0..10_000).find(|&n| attempt(n)).expect("operation never completed")
}

fn lower_and_overlay(initial: &Tree) -> (Arc<dyn FileSystem>, Arc<OverlayFs>, Vfs) {
    let lower: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*lower, initial);
    let overlay = Arc::new(OverlayFs::new(MemoryFs::new(), lower.clone()));
    let vfs = Vfs::from(overlay.clone());
    (lower, overlay, vfs)
}

#[test]
fn interrupted_commit_leaves_overlay_intact_and_can_be_resumed() {
    let mut rng = Rng::new(42);
    let initial = model::random_tree(&mut rng, 40);
    let (_, overlay, vfs) = lower_and_overlay(&initial);
    for _ in 0..120 {
        let _ = model::apply(&vfs, &model::random_op(&mut rng));
    }
    let merged = snapshot(&*overlay);
    let changes = overlay.changes().unwrap();
    assert!(changes.len() > 5, "need a non-trivial change set");

    for fault in MODELS {
        let points = every_failure_point(|n| {
            let target = FaultyFs::new(MemoryFs::new());
            populate(&target, &initial);
            arm(&target, fault, n);
            let completed = overlay.commit(&target).is_ok();
            if !completed {
                assert_eq!(overlay.changes().unwrap(), changes, "{fault:?}@{n}: failure altered the overlay");
                target.heal();
                overlay.commit(&target).unwrap(); // resuming finishes the job
            }
            assert_trees_eq(&merged, &snapshot(&target), &format!("{fault:?}@{n}: committed result"));
            completed
        });
        assert!(points > 5, "{fault:?}: faults hit only {points} points");
    }
}

#[test]
fn interrupted_atomic_write_never_exposes_partial_data() {
    for fault in MODELS {
        let points = every_failure_point(|n| {
            let fs = FaultyFs::new(MemoryFs::new());
            populate(&fs, &tree(&[("/cfg/", ""), ("/cfg/app.toml", "old = true")]));
            arm(&fs, fault, n);
            let result = ops::write_atomic(&fs, &p("/cfg/app.toml"), b"new = true\nmore = 1");
            let content = String::from_utf8(ops::read_all(&fs, &p("/cfg/app.toml")).unwrap()).unwrap();
            match &result {
                Ok(()) => assert_eq!(content, "new = true\nmore = 1"),
                Err(e) => {
                    assert_eq!(content, "old = true", "{fault:?}@{n}: exposed new bytes");
                    assert_eq!(e.path().unwrap(), "/cfg/app.toml", "errors name the target, not the temp file");
                }
            }
            // With a transient fault, cleanup succeeds: no debris.
            if fault == Model::Transient {
                assert_eq!(names(&fs, "/cfg"), ["app.toml"], "{fault:?}@{n}: temp file left behind");
            }
            result.is_ok()
        });
        assert!(points >= 3, "{fault:?}: open, write, sync and rename are failure points ({points})");
    }
}

#[test]
fn failed_copy_up_never_corrupts_the_merged_view() {
    for fault in MODELS {
        every_failure_point(|n| {
            let lower = Vfs::memory();
            lower.write("/doc.txt", "original").unwrap();
            let upper = Arc::new(FaultyFs::new(MemoryFs::new()));
            arm(&upper, fault, n);
            let overlay = Vfs::new(OverlayFs::with_layers(upper.clone(), vec![lower.backend().clone()]));
            let completed = overlay.append("/doc.txt", " + edit").is_ok();
            let seen = overlay.read_to_string("/doc.txt").unwrap();
            if completed {
                assert_eq!(seen, "original + edit");
            } else {
                // Either untouched, or the copy-up landed and only the append failed.
                assert!(seen == "original" || seen == "original + edit", "{fault:?}@{n}: corrupted view {seen:?}");
            }
            if fault == Model::Transient {
                assert_eq!(names(&**overlay.backend(), "/"), ["doc.txt"], "{fault:?}@{n}: debris visible");
            }
            assert_eq!(lower.read_to_string("/doc.txt").unwrap(), "original", "{fault:?}@{n}: lower touched");
            completed
        });
    }
}

#[test]
fn failed_rename_of_lower_file_keeps_both_sides() {
    for fault in MODELS {
        every_failure_point(|n| {
            let lower = Vfs::memory();
            populate(&**lower.backend(), &tree(&[("/a", "source bytes"), ("/b", "destination bytes")]));
            let upper = Arc::new(FaultyFs::new(MemoryFs::new()));
            arm(&upper, fault, n);
            let overlay = Vfs::new(OverlayFs::with_layers(upper, vec![lower.backend().clone()]));
            let completed = overlay.rename("/a", "/b").is_ok();
            if completed {
                assert!(!overlay.exists("/a"));
                assert_eq!(overlay.read_to_string("/b").unwrap(), "source bytes");
            } else {
                assert_eq!(overlay.read_to_string("/a").unwrap(), "source bytes", "{fault:?}@{n}: source lost");
                assert_eq!(
                    overlay.read_to_string("/b").unwrap(),
                    "destination bytes",
                    "{fault:?}@{n}: destination lost"
                );
            }
            completed
        });
    }
}

#[test]
fn failed_rename_of_lower_directory_rolls_back() {
    let initial = tree(&[("/src/", ""), ("/src/a", "1"), ("/src/sub/", ""), ("/src/sub/b", "2")]);
    every_failure_point(|n| {
        let lower: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
        populate(&*lower, &initial);
        let upper = Arc::new(FaultyFs::new(MemoryFs::new()));
        upper.fail_once_at(n);
        let overlay = Vfs::new(OverlayFs::with_layers(upper, vec![lower]));
        let completed = overlay.rename("/src", "/dst").is_ok();
        let view = snapshot(&**overlay.backend());
        if completed {
            assert_eq!(view.keys().filter(|k| k.starts_with("/dst")).count(), 4);
            assert!(!view.keys().any(|k| k.starts_with("/src")));
        } else {
            assert_trees_eq(&initial, &view, &format!("transient@{n}: rename must roll back"));
        }
        completed
    });
}

#[test]
fn partial_create_dir_all_can_be_retried() {
    let fs = FaultyFs::new(MemoryFs::new());
    fs.fail_from(2);
    assert!(ops::create_dir_all(&fs, &p("/a/b/c/d")).is_err());
    assert!(fs.metadata(&p("/a/b")).unwrap().is_dir(), "the successful prefix stays");
    fs.heal();
    ops::create_dir_all(&fs, &p("/a/b/c/d")).unwrap();
    assert!(fs.metadata(&p("/a/b/c/d")).unwrap().is_dir());
}

#[test]
fn partial_recursive_delete_can_be_retried() {
    let fs = FaultyFs::new(MemoryFs::new());
    populate(&fs, &tree(&[("/t/", ""), ("/t/a/", ""), ("/t/a/1", "1"), ("/t/b", "2"), ("/t/c", "3")]));
    fs.fail_from(2);
    assert!(ops::remove_tree(&fs, &p("/t")).is_err());
    assert!(fs.exists(&p("/t")), "deletion stopped midway");
    fs.heal();
    ops::remove_tree(&fs, &p("/t")).unwrap();
    assert!(!fs.exists(&p("/t")));
}

#[test]
fn failed_cross_backend_copy_reports_the_destination_and_keeps_the_source() {
    let src = MemoryFs::new();
    ops::write_all(&src, &p("/data"), &vec![1u8; 100_000]).unwrap();
    let dst = FaultyFs::new(MemoryFs::new());
    dst.fail_once_at(1); // open succeeds, the first write fails
    let err = ops::copy_file(&src, &p("/data"), &dst, &p("/copy")).unwrap_err();
    assert_eq!(err.path().unwrap(), "/copy");
    assert_eq!(ops::read_all(&src, &p("/data")).unwrap(), vec![1u8; 100_000]);
}

#[test]
fn overlay_with_full_upper_reports_storage_full_and_stays_consistent() {
    let lower = Vfs::memory();
    lower.write("/big", vec![b'x'; 1000]).unwrap();
    let overlay = Vfs::new(OverlayFs::with_layers(Arc::new(MemoryFs::with_quota(500)), vec![lower.backend().clone()]));
    assert!(matches!(overlay.append("/big", "more"), Err(VfsError::StorageFull(_))));
    assert_eq!(overlay.read("/big").unwrap().len(), 1000, "the lower version is still served");
    assert_eq!(names(&**overlay.backend(), "/"), ["big"], "no partial copy-up left behind");
    overlay.write("/small", vec![b'y'; 400]).unwrap();
    assert!(matches!(overlay.write("/small2", vec![b'z'; 200]), Err(VfsError::StorageFull(_))));
}
