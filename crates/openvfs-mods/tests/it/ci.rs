//! `CaseInsensitiveFs`: case-insensitive resolution over any backend.

use std::sync::Arc;

use openvfs::ops;
use openvfs::prelude::*;

use crate::common::{assert_trees_eq, names, populate, tree};
use openvfs_mods::CaseInsensitiveFs;

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

/// A memory backend wrapped in the case-insensitive adapter.
fn ci_fs(contents: &[(&str, &str)]) -> (Vfs, Arc<CaseInsensitiveFs>) {
    let inner: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*inner, &tree(contents));
    let ci = Arc::new(CaseInsensitiveFs::new(inner));
    (Vfs::from(ci.clone()), ci)
}

#[test]
fn lookups_resolve_any_spelling() {
    let (vfs, _) = ci_fs(&[("/Textures/", ""), ("/Textures/A.DDS", "hi-res"), ("/readme.txt", "r")]);
    assert_eq!(vfs.read_to_string("/textures/a.dds").unwrap(), "hi-res");
    assert_eq!(vfs.read_to_string("/TEXTURES/A.DDS").unwrap(), "hi-res");
    assert!(vfs.is_file("/README.TXT"));
    assert!(vfs.metadata("/textures").unwrap().is_dir());
    assert!(matches!(vfs.metadata("/Textures/missing.dds"), Err(VfsError::NotFound(_))));
}

#[test]
fn listings_show_the_real_spelling() {
    let (vfs, _) = ci_fs(&[("/MiXeD/", ""), ("/MiXeD/F.TXT", "x"), ("/plain", "p")]);
    assert_eq!(names(&**vfs.backend(), "/mixed"), ["F.TXT"]);
    let mut root: Vec<String> = vfs.read_dir("/").unwrap().into_iter().map(|e| e.name).collect();
    root.sort();
    assert_eq!(root, ["MiXeD", "plain"], "never the caller's case");
}

#[test]
fn writes_keep_the_caller_spelling_for_the_new_tail() {
    let (vfs, ci) = ci_fs(&[("/Textures/", ""), ("/Textures/old.dds", "old")]);
    vfs.write("/textures/NEW.dds", "new").unwrap();
    // Created under the *real* `Textures` dir, with the caller's file name.
    assert_eq!(names(&**ci.inner(), "/Textures"), ["NEW.dds", "old.dds"]);
    vfs.create_dir_all("/DATA/Sub").unwrap();
    assert!(ci.inner().exists(&p("/DATA/Sub")));
    assert!(vfs.is_dir("/data/sub"));
}

#[test]
fn writes_through_the_adapter_invalidate_the_cache() {
    let (vfs, _) = ci_fs(&[("/d/", ""), ("/d/a", "a")]);
    assert!(matches!(vfs.metadata("/D/b"), Err(VfsError::NotFound(_)))); // caches the miss
    vfs.write("/d/b", "b").unwrap(); // through the adapter
    assert_eq!(vfs.read_to_string("/D/B").unwrap(), "b", "its own writes stay visible");
    vfs.remove_file("/D/a").unwrap();
    assert!(matches!(vfs.metadata("/d/a"), Err(VfsError::NotFound(_))));
}

#[test]
fn renames_resolve_both_ends() {
    let (vfs, ci) = ci_fs(&[("/Dir/", ""), ("/Dir/old", "x")]);
    vfs.rename("/dir/OLD", "/DIR/new").unwrap();
    assert_eq!(vfs.read_to_string("/dir/NEW").unwrap(), "x");
    assert!(matches!(vfs.metadata("/dir/old"), Err(VfsError::NotFound(_))));
    assert!(ci.inner().exists(&p("/Dir/new")));
}

#[test]
fn errors_report_the_callers_path() {
    let (vfs, _) = ci_fs(&[("/f", "x")]);
    match vfs.metadata("/F/child") {
        Err(VfsError::NotADirectory(path)) => assert_eq!(path, "/F/child", "not the resolved /f/child"),
        other => panic!("expected NotADirectory, got {other:?}"),
    }
}

#[test]
fn clear_cache_picks_up_external_changes() {
    let (vfs, ci) = ci_fs(&[("/d/", "")]);
    let inner = ci.inner().clone();
    vfs.metadata("/D/x").unwrap_err(); // caches /d's (empty) listing

    // A change through another handle: invisible until the cache is cleared.
    inner.create_dir(&p("/d/sub")).unwrap();
    assert!(!vfs.is_dir("/D/SUB"), "a stale listing keeps serving the old answer");
    ci.clear_cache();
    assert!(vfs.is_dir("/d/sub"), "after clear_cache the external change resolves");
}

#[test]
fn adapter_is_a_real_filesystem() {
    // Snapshot through the adapter sees exactly the inner tree.
    let (vfs, _) = ci_fs(&[("/A/", ""), ("/A/b/", ""), ("/A/b/f", "x"), ("/top", "t")]);
    let expected = tree(&[("/A/", ""), ("/A/b/", ""), ("/A/b/f", "x"), ("/top", "t")]);
    assert_trees_eq(&expected, &crate::common::snapshot(&**vfs.backend()), "CI view must equal the inner tree");
    // And reads through weird case work end to end.
    assert_eq!(ops::read_all(&**vfs.backend(), &p("/a/B/F")).unwrap(), b"x");
}
