//! End-to-end tests of the `openvfs-mods` binary: the profile lifecycle,
//! deploy → simulated game write → capture → undeploy, and the failure
//! surfaces (usage errors, drift, in-flight journals) coming out as the
//! right exit codes.
//!
//! The binary under test is `env!("CARGO_BIN_EXE_openvfs-mods")` — Cargo
//! builds it for the test target automatically.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::common::{TempDir, host_tree, populate_host, tree};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_openvfs-mods")
}

/// Runs the binary in `cwd`, returning the captured output. Asserting on
/// `status.code()` is the caller's job — several tests *want* a nonzero exit.
fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).current_dir(cwd).output().expect("failed to spawn openvfs-mods")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A small world: game dir with a base file, two mods contesting one path.
struct World {
    profile: PathBuf,
    game: PathBuf,
    overwrite: PathBuf,
    state: PathBuf,
    root: TempDir,
}

impl World {
    fn new() -> Self {
        let root = TempDir::new("cli");
        let game = root.path().join("game");
        let m1 = root.path().join("mods/m1");
        let m2 = root.path().join("mods/m2");
        populate_host(&game, &tree(&[("/data/base.txt", "base")]));
        populate_host(&m1, &tree(&[("/data/shared.txt", "m1 wins")]));
        populate_host(&m2, &tree(&[("/data/shared.txt", "m2 loses"), ("/only2.txt", "m2 only")]));
        let world = World {
            profile: root.path().join("profile.json"),
            overwrite: root.path().join("overwrite"),
            state: root.path().join("state"),
            game,
            root,
        };
        let out = run(
            world.root.path(),
            &[
                "profile",
                "new",
                "profile.json",
                "--name",
                "cli-test",
                "--game",
                world.game.to_str().unwrap(),
                "--overwrite",
                world.overwrite.to_str().unwrap(),
            ],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        for (name, dir) in [("m1", &m1), ("m2", &m2)] {
            let out = run(world.root.path(), &["profile", "add", "profile.json", name, dir.to_str().unwrap()]);
            assert!(out.status.success(), "{}", stderr(&out));
        }
        world
    }

    fn arg(path: &Path) -> String {
        path.to_str().unwrap().to_owned()
    }
}

#[test]
fn profile_lifecycle() {
    let world = World::new();

    let out = run(world.root.path(), &["profile", "show", "profile.json"]);
    let text = stdout(&out);
    assert!(out.status.success());
    assert!(text.contains("\"cli-test\""), "{text}");
    assert!(text.contains("0. [x] m1"), "{text}");
    assert!(text.contains("1. [x] m2"), "{text}");

    // disable flips the mark; move re-prioritizes; remove drops the entry.
    let out = run(world.root.path(), &["profile", "disable", "profile.json", "m2"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = run(world.root.path(), &["profile", "move", "profile.json", "m2", "--to", "0"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&run(world.root.path(), &["profile", "show", "profile.json"]));
    assert!(text.contains("0. [ ] m2"), "{text}");

    let out = run(world.root.path(), &["profile", "remove", "profile.json", "m2"]);
    assert!(out.status.success());
    let out = run(world.root.path(), &["profile", "remove", "profile.json", "m2"]);
    assert_eq!(out.status.code(), Some(1), "removing a missing mod is a runtime error");
    let out = run(world.root.path(), &["profile", "enable", "profile.json", "ghost"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn deploy_capture_undeploy_cycle() {
    let world = World::new();
    let profile = World::arg(&world.profile);
    let state = World::arg(&world.state);

    // Dry run changes nothing on disk.
    let before = host_tree(&world.game);
    let out = run(world.root.path(), &["deploy", &profile, "--state", &state, "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("ops"), "plan summary: {}", stdout(&out));
    assert_eq!(host_tree(&world.game), before, "a dry run must not touch the game dir");

    // Deploy: the winner's bytes stand where the loser was contested.
    let out = run(world.root.path(), &["deploy", &profile, "--state", &state]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(world.game.join("data/shared.txt")).unwrap(), "m1 wins");
    assert!(world.game.join("only2.txt").is_file());

    // Status and clean verify.
    let out = run(world.root.path(), &["status", &profile, "--state", &state]);
    assert!(stdout(&out).contains("status: deployed"), "{}", stdout(&out));
    let out = run(world.root.path(), &["verify", &profile, "--state", &state]);
    assert!(out.status.success(), "clean verify must exit 0: {}", stderr(&out));

    // A "game write" is foreign until captured; verify reports the drift
    // with exit 1, capture moves it to the overwrite.
    std::fs::write(world.game.join("save.dat"), "autosave").unwrap();
    let out = run(world.root.path(), &["verify", &profile, "--state", &state]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("/save.dat"), "{}", stdout(&out));
    let out = run(world.root.path(), &["capture", &profile, "--state", &state]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(std::fs::read_to_string(world.overwrite.join("save.dat")).unwrap(), "autosave");

    // Undeploy restores exactly the pre-deploy tree.
    let out = run(world.root.path(), &["undeploy", &profile, "--state", &state]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(host_tree(&world.game), before);
}

#[test]
fn conflicts_and_winner_bytes() {
    let world = World::new();
    let profile = World::arg(&world.profile);

    let out = run(world.root.path(), &["conflicts", &profile]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("shared.txt"), "{text}");
    assert!(text.contains("\"m1\" over"), "{text}");

    // --mod filters to the conflicts the named mod takes part in.
    let out = run(world.root.path(), &["conflicts", &profile, "--mod", "m2"]);
    let text = stdout(&out);
    assert!(text.contains("shared.txt"), "{text}");
}

#[test]
fn usage_errors_and_in_flight_journals() {
    let world = World::new();
    let profile = World::arg(&world.profile);
    let state = World::arg(&world.state);

    // Unknown flag → exit 2 with usage on stderr.
    let out = run(world.root.path(), &["deploy", &profile, "--state", &state, "--bogus"]);
    assert_eq!(out.status.code(), Some(2), "usage errors exit 2: {}", stderr(&out));

    // resume/rollback with nothing in flight → runtime error, exit 1.
    let out = run(world.root.path(), &["resume", "--state", &state]);
    assert_eq!(out.status.code(), Some(1));
    let out = run(world.root.path(), &["rollback", "--state", &state]);
    assert_eq!(out.status.code(), Some(0), "an empty journal rolls back to nothing cleanly");
}

#[test]
fn game_prints_instructions_without_command() {
    let world = World::new();
    let profile = World::arg(&world.profile);
    let state = World::arg(&world.state);

    let out = run(world.root.path(), &["game", &profile, "--state", &state]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("capture"), "{text}");
    assert_eq!(std::fs::read_to_string(world.game.join("data/shared.txt")).unwrap(), "m1 wins");
}

#[test]
fn logger_honors_quiet_and_verbose() {
    let world = World::new();
    let profile = World::arg(&world.profile);
    let state = World::arg(&world.state);

    // -q silences info-level chatter; -v shows it.
    let out = run(world.root.path(), &["-q", "deploy", &profile, "--state", &state]);
    assert!(out.status.success());
    assert!(!stderr(&out).contains("INFO"), "quiet still leaked info: {}", stderr(&out));

    let out = run(world.root.path(), &["-v", "undeploy", &profile, "--state", &state]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("INFO"), "verbose must show info logs: {}", stderr(&out));
}
