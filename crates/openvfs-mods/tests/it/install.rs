//! `install_mod`: archive extraction, layout detection, safety and the
//! `install` CLI command wiring it into a profile.

use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use openvfs_mods::{InstallOptions, Profile, install_mod};

use crate::common::{TempDir, host_tree, populate_host, tree};

/// Writes a zip at `path` from `(name, contents)`; a name ending in `/` is
/// a directory entry.
fn make_zip(path: &Path, entries: &[(&str, &str)]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, contents) in entries {
        if name.ends_with('/') {
            zip.add_directory(*name, options).unwrap();
        } else {
            zip.start_file(*name, options).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
    }
    zip.finish().unwrap();
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn zip_with_wrapper_folder_is_unwrapped() {
    let root = TempDir::new("install-wrap");
    let archive = root.path().join("Cool Mod-1.2.zip");
    make_zip(
        &archive,
        &[("Cool Mod v1.2/", ""), ("Cool Mod v1.2/textures/a.dds", "A"), ("Cool Mod v1.2/readme.txt", "hi")],
    );
    let mods = root.path().join("mods");

    let installed = install_mod(&archive, &mods, None, &InstallOptions::default()).unwrap();
    assert_eq!(installed.name, "Cool Mod-1.2", "name comes from the archive file name");
    assert_eq!(installed.unwrapped, ["Cool Mod v1.2"]);
    assert_eq!(installed.files, 2);
    assert_eq!(read(mods.join("Cool Mod-1.2/textures/a.dds")), "A");
    // No staging leftovers next to the installed mod.
    let names: Vec<String> =
        std::fs::read_dir(&mods).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["Cool Mod-1.2"]);
}

#[test]
fn folder_matching_the_game_root_is_kept() {
    let root = TempDir::new("install-gameroot");
    let game = root.path().join("game");
    populate_host(&game, &tree(&[("/Data/base.esm", "base")]));
    let archive = root.path().join("m.zip");
    make_zip(&archive, &[("data/textures/x.dds", "X")]);
    let mods = root.path().join("mods");

    let installed = install_mod(&archive, &mods, Some(&game), &InstallOptions::default()).unwrap();
    assert!(installed.unwrapped.is_empty(), "`data` matches the game's `Data` (case-insensitively)");
    assert_eq!(read(mods.join("m/data/textures/x.dds")), "X");

    // A wrapper around a game-matching level is dropped down to that level.
    let archive = root.path().join("Wrapped.zip");
    make_zip(&archive, &[("Some Folder/Data/a.esp", "A")]);
    let installed = install_mod(&archive, &mods, Some(&game), &InstallOptions::default()).unwrap();
    assert_eq!(installed.unwrapped, ["Some Folder"]);
    assert_eq!(read(mods.join("Wrapped/Data/a.esp")), "A");

    // keep_root stops unwrapping even when the wrapper is the mod's name.
    let archive = root.path().join("wrapper.zip");
    make_zip(&archive, &[("wrapper/f.txt", "F")]);
    let opts = InstallOptions { keep_root: true, ..InstallOptions::default() };
    install_mod(&archive, &mods, None, &opts).unwrap();
    assert_eq!(read(mods.join("wrapper/wrapper/f.txt")), "F");
}

#[test]
fn a_lone_content_folder_is_not_mistaken_for_a_wrapper() {
    // `textures/` alone *is* the mod: with no game match and a name that
    // doesn't resemble the mod's, nothing is unwrapped.
    let root = TempDir::new("install-content");
    let archive = root.path().join("HD Rocks.zip");
    make_zip(&archive, &[("textures/rocks/r.dds", "R")]);
    let mods = root.path().join("mods");
    let installed = install_mod(&archive, &mods, None, &InstallOptions::default()).unwrap();
    assert!(installed.unwrapped.is_empty(), "{:?}", installed.unwrapped);
    assert_eq!(read(mods.join("HD Rocks/textures/rocks/r.dds")), "R");
}

#[test]
fn escaping_zip_entries_are_rejected_without_leftovers() {
    let root = TempDir::new("install-slip");
    let archive = root.path().join("evil.zip");
    make_zip(&archive, &[("ok.txt", "fine"), ("../escaped.txt", "pwned")]);
    let mods = root.path().join("mods");

    let err = install_mod(&archive, &mods, None, &InstallOptions::default()).unwrap_err();
    assert!(err.to_string().contains("escapes"), "{err}");
    assert!(!root.path().join("escaped.txt").exists());
    assert_eq!(std::fs::read_dir(&mods).unwrap().count(), 0, "failed install leaves nothing behind");
}

#[test]
fn reinstall_requires_replace_and_swaps_content() {
    let root = TempDir::new("install-replace");
    let mods = root.path().join("mods");
    let v1 = root.path().join("v1");
    populate_host(&v1, &tree(&[("/old.txt", "1")]));
    let opts = InstallOptions { name: Some("thing".into()), ..InstallOptions::default() };
    install_mod(&v1, &mods, None, &opts).unwrap();

    let v2 = root.path().join("v2");
    populate_host(&v2, &tree(&[("/new.txt", "2")]));
    let err = install_mod(&v2, &mods, None, &opts).unwrap_err();
    assert!(err.to_string().contains("already installed"), "{err}");

    let replaced = install_mod(&v2, &mods, None, &InstallOptions { replace: true, ..opts }).unwrap();
    assert!(replaced.replaced);
    assert_eq!(host_tree(&mods.join("thing")), tree(&[("/new.txt", "2")]), "old files are gone");
}

#[test]
fn bad_names_and_empty_archives_are_refused() {
    let root = TempDir::new("install-bad");
    let mods = root.path().join("mods");
    let archive = root.path().join("empty.zip");
    make_zip(&archive, &[("dir/", "")]);
    let err = install_mod(&archive, &mods, None, &InstallOptions::default()).unwrap_err();
    assert!(err.to_string().contains("no files"), "{err}");

    for bad in ["..", "a/b", ".hidden", "con:"] {
        let opts = InstallOptions { name: Some(bad.into()), ..InstallOptions::default() };
        assert!(install_mod(&archive, &mods, None, &opts).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn default_names_strip_archive_extensions() {
    use openvfs_mods::install::default_mod_name;
    assert_eq!(default_mod_name(Path::new("x/Foo-1.2.tar.gz")).as_deref(), Some("Foo-1.2"));
    assert_eq!(default_mod_name(Path::new("Bar.7z")).as_deref(), Some("Bar"));
    assert_eq!(default_mod_name(Path::new("Baz.v2.zip")).as_deref(), Some("Baz.v2"));
}

/// Non-zip formats go through an external extractor; Windows always has
/// bsdtar as `System32\tar.exe`, so build a .tar with it and install that.
#[cfg(windows)]
#[test]
fn tar_archives_go_through_the_external_extractor() {
    let root = TempDir::new("install-tar");
    let src = root.path().join("src");
    populate_host(&src, &tree(&[("/pack/meshes/m.nif", "M")]));
    let tar_exe = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32/tar.exe");
    let archive = root.path().join("Pack.tar");
    let status = Command::new(&tar_exe).arg("-c").arg("-f").arg(&archive).arg("-C").arg(&src).arg("pack").status();
    if !status.is_ok_and(|s| s.success()) {
        eprintln!("skipping: could not build a tar with {}", tar_exe.display());
        return;
    }
    let mods = root.path().join("mods");
    let installed = install_mod(&archive, &mods, None, &InstallOptions::default()).unwrap();
    assert_eq!(installed.name, "Pack");
    assert_eq!(installed.unwrapped, ["pack"]);
    assert_eq!(read(mods.join("Pack/meshes/m.nif")), "M");
}

#[test]
fn cli_install_registers_the_mod_at_the_top() {
    let root = TempDir::new("install-cli");
    let game = root.path().join("game");
    populate_host(&game, &tree(&[("/base.txt", "base")]));
    let profile_path = root.path().join("profile.json");
    let mut profile = Profile::new("p", &game, root.path().join("overwrite"));
    let existing = root.path().join("existing");
    populate_host(&existing, &tree(&[("/e.txt", "e")]));
    profile.add_mod(openvfs_mods::ModEntry { name: "existing".into(), source_dir: existing, enabled: true });
    profile.save(&profile_path).unwrap();
    let archive = root.path().join("New Mod.zip");
    make_zip(&archive, &[("New Mod/plugin.dll", "P")]);

    let bin = env!("CARGO_BIN_EXE_openvfs-mods");
    let run = |args: &[&str]| Command::new(bin).args(args).current_dir(root.path()).output().unwrap();
    let out = run(&["install", "profile.json", archive.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("installed mod \"New Mod\""));

    let profile = Profile::load(&profile_path).unwrap();
    assert_eq!(profile.mods[0].name, "New Mod", "a fresh install wins the load order");
    assert_eq!(profile.mods[1].name, "existing");
    assert_eq!(read(profile.mods[0].source_dir.join("plugin.dll")), "P");
    assert!(
        profile.mods[0].source_dir.starts_with(root.path().join("mods")),
        "default mods dir is next to the profile"
    );

    // Same name again: refused without --replace, accepted with it (and
    // the mod keeps its slot).
    let out = run(&["install", "profile.json", archive.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    run(&["profile", "move", "profile.json", "New Mod", "--to", "1"]);
    let out = run(&["install", "profile.json", archive.to_str().unwrap(), "--replace"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let profile = Profile::load(&profile_path).unwrap();
    assert_eq!(profile.mods.len(), 2);
    assert_eq!(profile.mods[1].name, "New Mod", "reinstall keeps the load-order slot");

    let out = run(&["install", "profile.json", archive.to_str().unwrap(), "--bogus"]);
    assert_eq!(out.status.code(), Some(2), "unknown flags are usage errors");
}
