//! `PhysicalFs`: the host directory is a security boundary for *paths*.
//! These tests attack it — traversal, device names, aliases, symlinks.

use crate::common;

use common::TempDir;
use openvfs::prelude::*;

fn host(name: &str) -> (TempDir, Vfs) {
    let dir = TempDir::new(name);
    let vfs = Vfs::new(PhysicalFs::new(dir.path()).unwrap());
    (dir, vfs)
}

#[test]
fn files_land_where_expected_on_the_host() {
    let (dir, vfs) = host("host-map");
    std::fs::write(dir.path().join("existing.txt"), "from disk").unwrap();
    assert_eq!(vfs.read_to_string("/existing.txt").unwrap(), "from disk");
    vfs.create_dir_all("/out/nested").unwrap();
    vfs.write("/out/nested/result.txt", "from vfs").unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("out/nested/result.txt")).unwrap(), "from vfs");
    let fs = PhysicalFs::new(dir.path()).unwrap();
    assert_eq!(fs.host_path(&VPath::new("/out/nested").unwrap()).unwrap(), fs.root().join("out").join("nested"));
}

#[test]
fn traversal_never_leaves_the_root() {
    let outer = TempDir::new("host-outer");
    std::fs::write(outer.path().join("secret.txt"), "secret").unwrap();
    let inner_path = outer.path().join("jail");
    std::fs::create_dir(&inner_path).unwrap();
    let vfs = Vfs::new(PhysicalFs::new(&inner_path).unwrap());
    for attack in ["/../secret.txt", "../secret.txt", "/a/../../secret.txt", r"\..\secret.txt", "/./../secret.txt"] {
        assert!(vfs.read(attack).is_err(), "{attack} escaped");
    }
    vfs.write("/../../../planted.txt", "x").unwrap();
    assert!(inner_path.join("planted.txt").exists());
    assert!(!outer.path().join("planted.txt").exists());
}

#[test]
fn read_only_host() {
    let (dir, _) = host("host-ro");
    std::fs::write(dir.path().join("keep.txt"), "keep").unwrap();
    let vfs = Vfs::new(PhysicalFs::read_only(dir.path()).unwrap());
    assert!(vfs.backend().is_read_only());
    assert!(vfs.metadata("/keep.txt").unwrap().read_only);
    for result in [
        vfs.write("/x", "nope"),
        vfs.create_dir("/d"),
        vfs.remove_file("/keep.txt"),
        vfs.rename("/keep.txt", "/moved"),
        vfs.copy("/keep.txt", "/copy").map(drop),
        vfs.remove_dir_all("/keep.txt"),
    ] {
        assert!(matches!(result, Err(VfsError::ReadOnly(_))), "{result:?}");
    }
    assert_eq!(vfs.read_to_string("/keep.txt").unwrap(), "keep");
}

#[test]
fn root_must_be_an_existing_directory() {
    let dir = TempDir::new("host-root");
    let file = dir.path().join("plain.txt");
    std::fs::write(&file, "").unwrap();
    assert!(PhysicalFs::new(&file).is_err());
    assert!(PhysicalFs::new(dir.path().join("does-not-exist")).is_err());
    assert!(PhysicalFs::builder(dir.path()).read_only(true).symlinks(Symlinks::FollowWithinRoot).build().is_ok());
}

#[test]
fn builder_settings_are_reported() {
    let dir = TempDir::new("host-builder");
    let fs = PhysicalFs::builder(dir.path()).symlinks(Symlinks::FollowWithinRoot).build().unwrap();
    assert_eq!(fs.symlink_policy(), Symlinks::FollowWithinRoot);
    assert!(!fs.is_read_only());
    assert_eq!(PhysicalFs::new(dir.path()).unwrap().symlink_policy(), Symlinks::Deny, "deny is the default");
}

#[cfg(windows)]
#[test]
fn windows_device_names_and_aliases_are_rejected() {
    let (dir, vfs) = host("host-win");
    for evil in [
        "/CON",
        "/con.txt",
        "/NUL",
        "/nul.tar.gz",
        "/AUX",
        "/COM1",
        "/LPT9.log",
        "/CONOUT$",
        "/dir/PRN",
        "/C:",
        "/C:/Windows/win.ini",
        "/file.txt:stream",
        "/trailing.",
        "/trailing ",
        "/what?",
        "/a*b",
        "/a<b",
    ] {
        assert!(matches!(vfs.write(evil, "x"), Err(VfsError::InvalidPath(_))), "{evil} accepted");
        assert!(matches!(vfs.read(evil), Err(VfsError::InvalidPath(_))), "{evil} accepted");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing was created");
}

#[cfg(windows)]
#[test]
fn case_only_rename_on_case_insensitive_hosts() {
    let (_dir, vfs) = host("host-case");
    vfs.write("/readme.md", "x").unwrap();
    vfs.rename("/readme.md", "/README.md").unwrap();
    assert_eq!(vfs.read_dir("/").unwrap()[0].name, "README.md");
    vfs.create_dir("/folder").unwrap();
    vfs.rename("/folder", "/Folder").unwrap();
    assert!(vfs.read_dir("/").unwrap().iter().any(|e| e.name == "Folder"));
    // Copying a file onto a case alias of itself must not truncate it.
    assert!(vfs.copy("/README.md", "/readme.md").is_err());
    assert_eq!(vfs.read_to_string("/README.md").unwrap(), "x");
}

/// Creates a directory link: a symlink, or on Windows without Developer Mode
/// a junction (which needs no privileges and which `std` also reports as a
/// link). Returns `None` only if neither is possible.
fn try_symlink(target: &std::path::Path, link: &std::path::Path) -> Option<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link).ok();
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Some(());
        }
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(())
    }
}

struct LinkLab {
    outside: TempDir,
    root: TempDir,
}

/// Layout: `root/inside/…`, `root/inside_link -> root/inside`, `root/escape -> outside`,
/// `root/dangling -> outside/nope`, `root/loop -> root`.
fn link_lab() -> Option<LinkLab> {
    let outside = TempDir::new("links-outside");
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let root = TempDir::new("links-root");
    std::fs::create_dir(root.path().join("inside")).unwrap();
    std::fs::write(root.path().join("inside/data.txt"), "data").unwrap();
    try_symlink(&root.path().join("inside"), &root.path().join("inside_link"))?;
    try_symlink(outside.path(), &root.path().join("escape"))?;
    try_symlink(&outside.path().join("nope"), &root.path().join("dangling"))?;
    try_symlink(root.path(), &root.path().join("loop"))?;
    Some(LinkLab { outside, root })
}

#[test]
fn deny_policy_makes_links_invisible_and_inaccessible() {
    let Some(lab) = link_lab() else {
        eprintln!("skipped: this host does not allow creating symlinks");
        return;
    };
    let vfs = Vfs::new(PhysicalFs::new(lab.root.path()).unwrap());
    assert_eq!(common::names(&**vfs.backend(), "/"), ["inside"], "links are not listed");
    for path in ["/inside_link/data.txt", "/escape/secret.txt", "/dangling", "/loop/inside/data.txt"] {
        assert!(matches!(vfs.read(path), Err(VfsError::PermissionDenied(_))), "{path}");
    }
    assert!(matches!(vfs.write("/escape/planted.txt", "x"), Err(VfsError::PermissionDenied(_))));
    assert!(matches!(vfs.write("/dangling", "x"), Err(VfsError::PermissionDenied(_))));
    assert!(!lab.outside.path().join("planted.txt").exists() && !lab.outside.path().join("nope").exists());
    assert!(vfs.walk("/").unwrap().count() == 2, "the walk is finite and link-free");
    recursive_delete_removes_links_not_targets(&lab, &vfs);
}

#[test]
fn follow_policy_stays_inside_the_root() {
    let Some(lab) = link_lab() else {
        eprintln!("skipped: this host does not allow creating symlinks");
        return;
    };
    let fs = PhysicalFs::builder(lab.root.path()).symlinks(Symlinks::FollowWithinRoot).build().unwrap();
    let vfs = Vfs::new(fs);
    assert_eq!(vfs.read_to_string("/inside_link/data.txt").unwrap(), "data");
    assert!(matches!(vfs.read("/escape/secret.txt"), Err(VfsError::PathEscapesRoot(_))));
    assert!(matches!(vfs.write("/dangling", "x"), Err(VfsError::PathEscapesRoot(_))));
    let listed = common::names(&**vfs.backend(), "/");
    assert!(listed.contains(&"inside_link".to_owned()) && !listed.contains(&"escape".to_owned()));
    assert!(!listed.contains(&"dangling".to_owned()));
    // The loop makes the tree infinite; max_depth keeps a walk bounded.
    assert!(vfs.walk("/").unwrap().max_depth(4).count() < 1_000);
    recursive_delete_removes_links_not_targets(&lab, &vfs);
}

/// `remove_dir_all` on a directory holding a link must delete the link only.
fn recursive_delete_removes_links_not_targets(lab: &LinkLab, vfs: &Vfs) {
    let container = lab.root.path().join("container");
    std::fs::create_dir(&container).unwrap();
    try_symlink(&lab.root.path().join("inside"), &container.join("link")).expect("link creation worked above");
    vfs.remove_dir_all("/container").unwrap();
    assert!(!container.exists());
    assert_eq!(
        std::fs::read_to_string(lab.root.path().join("inside/data.txt")).unwrap(),
        "data",
        "a link target was deleted"
    );
}

#[cfg(unix)]
#[test]
fn special_files_and_unaddressable_names_are_invisible() {
    use std::os::unix::ffi::OsStrExt;
    let (dir, vfs) = host("host-special");
    // macOS refuses non-UTF-8 names; where creating one fails there is simply nothing to hide.
    let _ = std::fs::write(dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xffutf8")), "x");
    std::fs::write(dir.path().join(r"back\slash"), "x").unwrap();
    std::fs::write(dir.path().join("ok.txt"), "x").unwrap();
    let fifo = dir.path().join("pipe");
    if std::process::Command::new("mkfifo").arg(&fifo).status().is_ok_and(|s| s.success()) {
        assert!(matches!(vfs.open_read("/pipe"), Err(VfsError::NotFound(_))), "opening a FIFO would block");
    }
    assert_eq!(common::names(&**vfs.backend(), "/"), ["ok.txt"]);
}
