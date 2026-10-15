//! Mount-table routing, shadowing, structural protection and error rebasing.

use crate::common;

use common::{Rng, env_knob, names};
use openvfs::prelude::*;

fn p(s: &str) -> VPath {
    VPath::new(s).unwrap()
}

#[test]
fn ancestors_of_mounts_are_virtual_dirs() {
    let mounts = MountFs::new();
    mounts.mount("/data/cache", MemoryFs::new()).unwrap();
    let vfs = Vfs::new(mounts);
    assert!(vfs.is_dir("/") && vfs.is_dir("/data") && vfs.is_dir("/data/cache"));
    assert_eq!(names(&**vfs.backend(), "/"), ["data"]);
    assert_eq!(names(&**vfs.backend(), "/data"), ["cache"]);
    assert!(matches!(vfs.read("/elsewhere"), Err(VfsError::NotFound(_))));
    assert!(matches!(vfs.write("/data/x", "no backend here"), Err(VfsError::NotFound(_))));
    assert!(matches!(vfs.create_dir("/data"), Err(VfsError::AlreadyExists(_))));
    assert!(matches!(vfs.open_read("/data"), Err(VfsError::IsADirectory(_))));
}

#[test]
fn an_empty_mount_table_is_an_empty_root() {
    let vfs = Vfs::new(MountFs::new());
    assert!(vfs.is_dir("/"));
    assert!(vfs.read_dir("/").unwrap().is_empty());
    assert!(matches!(vfs.write("/x", "x"), Err(VfsError::NotFound(_))));
}

#[test]
fn longest_prefix_wins() {
    let (root, data) = (Vfs::memory(), Vfs::memory());
    let mounts = MountFs::new();
    mounts.mount_arc("/", root.backend().clone()).unwrap();
    mounts.mount_arc("/data", data.backend().clone()).unwrap();
    let vfs = Vfs::new(mounts);
    vfs.write("/top.txt", "root").unwrap();
    vfs.write("/data/inner.txt", "data").unwrap();
    assert!(root.exists("/top.txt"));
    assert!(data.exists("/inner.txt"), "stored relative to the mount");
    assert!(!root.exists("/data/inner.txt"));
    assert_eq!(names(&**vfs.backend(), "/"), ["data", "top.txt"]);
}

#[test]
fn mount_points_shadow_the_parent_backend() {
    let root = Vfs::memory();
    root.create_dir("/data").unwrap();
    root.write("/data/hidden.txt", "under the mount").unwrap();
    root.write("/file-then-mount", "a file").unwrap();
    let mounts = MountFs::new();
    mounts.mount_arc("/", root.backend().clone()).unwrap();
    mounts.mount("/data", MemoryFs::new()).unwrap();
    mounts.mount("/file-then-mount/inner", MemoryFs::new()).unwrap();
    let vfs = Vfs::new(mounts);
    assert!(!vfs.exists("/data/hidden.txt"), "the mount shadows what was there");
    assert!(vfs.is_dir("/file-then-mount"), "a mount's ancestors are always directories");
    assert_eq!(names(&**vfs.backend(), "/file-then-mount"), ["inner"]);
}

#[test]
fn errors_are_reported_in_outer_paths() {
    let mounts = MountFs::new();
    let inner = MountFs::new();
    inner.mount("/deep", MemoryFs::new()).unwrap();
    mounts.mount("/m", inner).unwrap();
    let vfs = Vfs::new(mounts);
    let err = vfs.read("/m/deep/missing.txt").unwrap_err();
    assert_eq!(err.path().unwrap(), "/m/deep/missing.txt", "rebased through two mount levels");
    vfs.write("/m/deep/f", "x").unwrap();
    assert!(matches!(vfs.create_dir("/m/deep/f"), Err(VfsError::AlreadyExists(p)) if p == "/m/deep/f"));
}

#[test]
fn structural_paths_cannot_be_removed_or_moved() {
    let mounts = MountFs::new();
    mounts.mount("/", MemoryFs::new()).unwrap();
    mounts.mount("/a/b", MemoryFs::new()).unwrap();
    let vfs = Vfs::new(mounts);
    for path in ["/a", "/a/b"] {
        assert!(matches!(vfs.remove_dir(path), Err(VfsError::InvalidInput(_))), "{path}");
        assert!(matches!(vfs.remove_dir_all(path), Err(VfsError::InvalidInput(_))), "{path}");
        assert!(matches!(vfs.rename(path, "/z"), Err(VfsError::InvalidInput(_))), "{path}");
        assert!(matches!(vfs.remove_file(path), Err(VfsError::IsADirectory(_))), "{path}");
    }
    vfs.write("/f", "x").unwrap();
    assert!(matches!(vfs.rename("/f", "/a"), Err(VfsError::AlreadyExists(_))));
    assert!(matches!(vfs.copy("/f", "/a/b"), Err(VfsError::IsADirectory(_))));
}

#[test]
fn cross_mount_rename_falls_back_to_copy_with_rename_semantics() {
    let mounts = MountFs::new();
    mounts.mount("/a", MemoryFs::new()).unwrap();
    mounts.mount("/b", MemoryFs::new()).unwrap();
    let vfs = Vfs::new(mounts);
    vfs.create_dir_all("/a/src/sub").unwrap();
    vfs.write("/a/src/sub/x", "1").unwrap();
    vfs.write("/a/file", "moving").unwrap();

    let raw = vfs.backend().rename(&p("/a/file"), &p("/b/file"));
    assert!(matches!(raw, Err(VfsError::CrossDevice { .. })));

    vfs.rename("/a/file", "/b/file").unwrap();
    assert!(!vfs.exists("/a/file"));
    assert_eq!(vfs.read_to_string("/b/file").unwrap(), "moving");

    vfs.create_dir("/b/dst").unwrap();
    assert!(matches!(vfs.rename("/a/src", "/b/dst"), Err(VfsError::AlreadyExists(_))), "never merges into a dir");
    assert!(matches!(vfs.rename("/a/src", "/b/file"), Err(VfsError::NotADirectory(_))));
    vfs.rename("/a/src", "/b/moved").unwrap();
    assert_eq!(vfs.read_to_string("/b/moved/sub/x").unwrap(), "1");
    assert!(!vfs.exists("/a/src"));
}

#[test]
fn cross_mount_copy_streams() {
    let mounts = MountFs::new();
    mounts.mount("/a", MemoryFs::new()).unwrap();
    mounts.mount("/b", MemoryFs::new()).unwrap();
    let vfs = Vfs::new(mounts);
    let data = vec![42u8; 200_000];
    vfs.write("/a/big", &data).unwrap();
    assert_eq!(vfs.copy("/a/big", "/b/big").unwrap(), 200_000);
    assert_eq!(vfs.read("/b/big").unwrap(), data);
    vfs.copy_dir_all("/a", "/b/copy-of-a").unwrap();
    assert_eq!(vfs.read("/b/copy-of-a/big").unwrap(), data);
}

#[test]
fn read_only_mounts() {
    let assets = Vfs::memory();
    assets.write("/logo.svg", "<svg/>").unwrap();
    let mounts = MountFs::new();
    mounts.mount("/", MemoryFs::new()).unwrap();
    mounts.mount("/assets", ReadOnlyFs::new(assets.backend().clone())).unwrap();
    let vfs = Vfs::new(mounts);
    assert_eq!(vfs.read_to_string("/assets/logo.svg").unwrap(), "<svg/>");
    assert!(vfs.metadata("/assets/logo.svg").unwrap().read_only);
    for result in [
        vfs.write("/assets/logo.svg", "x"),
        vfs.remove_file("/assets/logo.svg"),
        vfs.create_dir("/assets/new"),
        vfs.rename("/assets/logo.svg", "/assets/l.svg"),
        vfs.copy("/assets/logo.svg", "/assets/c.svg").map(drop),
    ] {
        assert!(matches!(result, Err(VfsError::ReadOnly(_))), "{result:?}");
    }
    vfs.copy("/assets/logo.svg", "/logo-copy.svg").unwrap();
    assert_eq!(vfs.read_to_string("/logo-copy.svg").unwrap(), "<svg/>");
}

#[test]
fn mount_management() {
    let mounts = MountFs::new();
    mounts.mount("/x", MemoryFs::new()).unwrap();
    mounts.mount("/y/z", MemoryFs::new()).unwrap();
    assert!(matches!(mounts.mount("/x", MemoryFs::new()), Err(VfsError::AlreadyExists(_))));
    assert_eq!(mounts.mount_points(), [p("/x"), p("/y/z")]);
    let backend = mounts.unmount("/x").unwrap();
    assert!(backend.read_dir(&VPath::root()).unwrap().is_empty());
    assert!(matches!(mounts.unmount("/x"), Err(VfsError::NotFound(_))));
    assert!(format!("{mounts:?}").contains("/y/z"));
    let vfs = Vfs::new(mounts);
    assert!(!vfs.exists("/x"));
}

/// The virtual directory structure (which paths exist, what they list, what is protected) checked
/// against a naive model, on mount points whose names sort *right next to* each other and next to the
/// `/` separator in a byte-ordered table (`a`, `a b`, `a-b`, `a0`, `ab`): the places where a range
/// query over the table could easily include too much or too little.
#[test]
fn virtual_structure_matches_a_naive_model() {
    const NAMES: [&str; 7] = ["a", "a b", "a-b", "a0", "ab", "b", "x"];
    fn random_path(rng: &mut Rng, max_depth: usize) -> VPath {
        (0..rng.below(max_depth + 1)).fold(VPath::root(), |path, _| path.join(*rng.pick(&NAMES)).unwrap())
    }

    for seed in 0..env_knob("OPENVFS_PROP_CASES", 150) {
        let mut rng = Rng::new(seed);
        let mounts = MountFs::new();
        let mut points: Vec<VPath> = Vec::new();
        for _ in 0..=rng.below(8) {
            let at = random_path(&mut rng, 3);
            if mounts.mount(at.as_str(), MemoryFs::new()).is_ok() {
                points.push(at);
            }
        }

        for _ in 0..60 {
            let query = random_path(&mut rng, 4);
            let below: Vec<&VPath> = points.iter().filter(|m| **m != query && m.starts_with(&query)).collect();
            let structural = query.is_root() || points.iter().any(|m| m.starts_with(&query));
            let context = format!("seed {seed}, mounts {points:?}, query {query}");

            // Every mounted backend is empty, so a path exists only if it is a mount point or one of its ancestors.
            assert_eq!(
                mounts.metadata(&query).map(|m| m.is_dir()).ok(),
                structural.then_some(true),
                "metadata: {context}"
            );

            let listed = mounts.read_dir(&query);
            if structural {
                let mut expected: Vec<String> = below
                    .iter()
                    .filter_map(|m| m.strip_prefix(&query)?.components().next().map(str::to_owned))
                    .collect();
                expected.sort();
                expected.dedup();
                let mut got: Vec<String> =
                    listed.unwrap_or_else(|e| panic!("read_dir: {e}: {context}")).into_iter().map(|e| e.name).collect();
                got.sort();
                assert_eq!(got, expected, "read_dir: {context}");
                assert!(matches!(mounts.create_dir(&query), Err(VfsError::AlreadyExists(_))), "create_dir: {context}");
                assert!(matches!(mounts.remove_dir(&query), Err(VfsError::InvalidInput(_))), "remove_dir: {context}");
            } else {
                assert!(matches!(listed, Err(VfsError::NotFound(_))), "read_dir of a missing path: {context}");
                assert!(matches!(mounts.remove_dir(&query), Err(VfsError::NotFound(_))), "remove_dir: {context}");
            }
        }
    }
}
