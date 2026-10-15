//! End-to-end injection test.
//!
//! Runs `synth-game` twice — once plainly (baseline) and once under
//! `openvfs-inject` with `openvfs_hook.dll` + an `OPENVFS_HOOK_CONFIG`
//! pointing at a synthetic layer stack — and asserts that the injected run
//! observes the merged view while the baseline sees the raw base directory.
//!
//! The test is **opt-in and gracefully skipping**: it only runs the injected
//! path when `OPENVFS_HOOK_E2E=1`. If the DLL isn't built, injection is
//! blocked (AV/policy), or the platform isn't x64 Windows, it prints a skip
//! reason and passes — so `cargo test --workspace` stays green everywhere.
//!
//!     OPENVFS_HOOK_E2E=1 cargo test -p openvfs-hook --test e2e

#![cfg(all(windows, target_arch = "x86_64"))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Fixture tree laid out under a tempdir. `base` is the "game data" dir; the
/// mods overlay it.
struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    overwrite: PathBuf,
    mod_hi: PathBuf,
    mod_lo: PathBuf,
}

fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn make_fixture(tag: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("game");
    let overwrite = dir.path().join("overwrite");
    let mod_hi = dir.path().join("mod_hi");
    let mod_lo = dir.path().join("mod_lo");
    std::fs::create_dir_all(&overwrite).unwrap();

    write(&base.join("base.txt"), &format!("base-file-{tag}"));
    write(&base.join("shared.txt"), "from-base");
    write(&base.join("base_write.txt"), "orig");
    write(&base.join("to_delete.txt"), "del-me");
    write(&base.join("rename_src.txt"), "rename-me");
    write(&base.join("sub").join("inside.txt"), "in-sub");
    std::fs::create_dir_all(base.join("emptydir")).unwrap();

    // mod_hi wins over mod_lo wins over base.
    write(&mod_hi.join("shared.txt"), "from-mod-hi");
    write(&mod_hi.join("mod_only.txt"), "mod-hi-file");
    write(&mod_hi.join("sub").join("hi_extra.txt"), "hi-sub");
    write(&mod_lo.join("shared.txt"), "from-mod-lo");
    write(&mod_lo.join("mod2_only.txt"), "mod-lo-file");
    write(&mod_lo.join("to_delete.txt"), "del-me-lo");

    Fixture { dir, root: base, overwrite, mod_hi, mod_lo }
}

fn write_config(fx: &Fixture) -> PathBuf {
    let cfg = serde_json::json!({
        "virtual_root": fx.root,
        "overwrite": fx.overwrite,
        "layers": [
            { "name": "mod-hi", "dir": fx.mod_hi, "enabled": true },
            { "name": "mod-lo", "dir": fx.mod_lo, "enabled": true },
            { "name": "disabled-mod", "dir": fx.dir.path().join("nope"), "enabled": false },
        ],
    });
    let path = fx.dir.path().join("openvfs.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
    path
}

fn run_synth(root: &Path, out: &Path) -> std::io::Result<std::process::Output> {
    Command::new(env!("CARGO_BIN_EXE_synth-game")).arg(root).arg("--out").arg(out).output()
}

fn parse(out_path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(out_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Locate (or build) the cdylib next to the test binaries.
fn hook_dll() -> Option<PathBuf> {
    // Cargo puts cdylib artifacts next to the binaries.
    let inject = PathBuf::from(env!("CARGO_BIN_EXE_openvfs-inject"));
    let dir = inject.parent()?.parent()?.to_path_buf(); // target/<profile>/
    for name in ["openvfs_hook.dll", "openvfs-hook.dll"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    // Not built yet — try to build it (cdylib isn't an implicit test dep).
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new(cargo).args(["build", "-p", "openvfs-hook", "--lib"]).current_dir(&ws).status().ok()?;
    if !status.success() {
        return None;
    }
    for name in ["openvfs_hook.dll", "openvfs-hook.dll"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    // Look in the workspace target dir (cargo may build to a shared target).
    let target = ws.parent()?.parent()?.join("target").join("debug");
    for name in ["openvfs_hook.dll", "openvfs-hook.dll"] {
        let p = target.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn assert_shared_view(lines: &HashMap<String, String>) {
    assert_eq!(lines["shared"], "from-mod-hi", "higher-priority layer must win");
    assert_eq!(lines["modonly"], "mod-hi-file", "mod-only file must resolve");
    let list = &lines["list_std"];
    for name in ["base.txt", "shared.txt", "mod_only.txt", "mod2_only.txt", "sub"] {
        assert!(list.split(',').any(|n| n == name), "merged listing must contain {name}: {list}");
    }
    let list_ff = &lines["list_ff"];
    assert!(list_ff.split(',').any(|n| n == "mod_only.txt"), "FindFirstFileW listing: {list_ff}");
    assert!(list_ff.split(',').any(|n| n == "mod2_only.txt"), "FindFirstFileW listing: {list_ff}");
    assert!(lines["attr_shared"].starts_with("0x"), "attr_shared: {}", lines["attr_shared"]);
    assert_eq!(lines["attr_missing"], "err:2", "missing file → ERROR_FILE_NOT_FOUND");
    assert_eq!(lines["missing"], "err:2");
    assert_eq!(lines["write_new"], "ok");
    assert_eq!(lines["readback_new"], "created-by-game");
    assert_eq!(lines["write_over"], "ok");
    assert_eq!(lines["readback_over"], "modified-by-game");
    assert_eq!(lines["mkdir"], "ok");
    assert_eq!(lines["mkdir_nested"], "ok");
    assert_eq!(lines["mkdir_exist"], "err:183", "existing dir → ERROR_ALREADY_EXISTS");
    assert_eq!(lines["delete"], "ok");
    assert_eq!(lines["deleted_still"], "false", "whiteout must hide deleted file");
    assert_eq!(lines["delete_missing"], "err:2");
    assert_eq!(lines["rename"], "ok");
    assert_eq!(lines["renamed_read"], "rename-me");
    assert_eq!(lines["copy"], "ok");
    assert!(lines["copied_read"].starts_with("base-file"));
    assert_eq!(lines["relative"], lines["base"], "relative paths must resolve");
}

#[test]
fn injected_process_sees_merged_view() {
    // Baseline run always happens (it exercises no hooks at all).
    let fx_plain = make_fixture("p");
    let out_plain = fx_plain.dir.path().join("out_plain.txt");
    let output = run_synth(&fx_plain.root, &out_plain).expect("run synth-game");
    assert!(output.status.success(), "synth-game failed: {output:?}");
    let plain = parse(&out_plain);
    assert_eq!(plain["shared"], "from-base", "baseline must see only the base layer");
    assert_eq!(plain["modonly"], "err:2");
    assert_eq!(plain["deleted_still"], "false", "baseline delete is real");

    if std::env::var("OPENVFS_HOOK_E2E").as_deref() != Ok("1") {
        eprintln!("skipping injected run: set OPENVFS_HOOK_E2E=1 to enable");
        return;
    }
    let Some(dll) = hook_dll() else {
        eprintln!("skipping injected run: openvfs_hook.dll not built/found");
        return;
    };

    let fx = make_fixture("v");
    let cfg = write_config(&fx);
    let out_path = fx.dir.path().join("out.txt");
    let log_path = fx.dir.path().join("hook.log");

    let inject = env!("CARGO_BIN_EXE_openvfs-inject");
    let synth = env!("CARGO_BIN_EXE_synth-game");
    let output = Command::new(inject)
        .arg("--dll")
        .arg(&dll)
        .arg("--")
        .arg(synth)
        .arg(&fx.root)
        .arg("--out")
        .arg(&out_path)
        .env("OPENVFS_HOOK_CONFIG", &cfg)
        .env("OPENVFS_HOOK_LOG", &log_path)
        .output()
        .expect("run openvfs-inject");

    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("injector stderr:\n{stderr}");
    if !output.status.success() {
        // Infrastructure failures (injection blocked, AV, policy) → skip,
        // not fail. Child failures propagate the child's own exit code
        // (< 64 or an NTSTATUS) and still indicate a real problem.
        if let Some(code) = output.status.code()
            && (64..128).contains(&code)
        {
            eprintln!("skipping: injector failed with code {code}");
            return;
        }
        panic!("injected child failed: status={:?}\nstderr:\n{stderr}", output.status);
    }

    if let Ok(log) = std::fs::read_to_string(&log_path) {
        eprintln!("hook log:\n{log}");
    }
    let lines = parse(&out_path);
    assert_shared_view(&lines);

    // Filesystem-level assertions: writes land in overwrite only; layers and
    // base are untouched.
    assert_eq!(std::fs::read_to_string(fx.overwrite.join("new_file.txt")).unwrap(), "created-by-game");
    assert_eq!(std::fs::read_to_string(fx.overwrite.join("base_write.txt")).unwrap(), "modified-by-game");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("base_write.txt")).unwrap(),
        "orig",
        "base layer must stay pristine"
    );
    assert!(fx.overwrite.join("to_delete.txt.ovh-deleted").exists(), "expected whiteout marker");
    assert!(fx.root.join("to_delete.txt").exists(), "base file must not be physically deleted");
    assert_eq!(
        std::fs::read_to_string(fx.mod_hi.join("shared.txt")).unwrap(),
        "from-mod-hi",
        "mod layer content untouched"
    );
    assert_eq!(
        std::fs::read_to_string(fx.root.join("shared.txt")).unwrap(),
        "from-base",
        "layer/base content untouched"
    );
    assert!(!fx.root.join("new_file.txt").exists(), "write must not land in base");
    assert!(fx.overwrite.join("newdir").join("inner.txt").exists());
    assert_eq!(std::fs::read_to_string(fx.overwrite.join("renamed_dst.txt")).unwrap(), "rename-me");
    assert!(fx.overwrite.join("rename_src.txt.ovh-deleted").exists(), "rename should whiteout src");
}
