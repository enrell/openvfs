//! `openvfs-mods` — the mod manager's command-line front end.
//!
//! Profiles are JSON files; deployments live in a *state directory* chosen
//! with `--state`. The binary is also where the library's `log` output gets
//! wired up: a tiny stderr logger honoring `-v`/`-q` or `RUST_LOG`.

#[cfg(windows)]
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use openvfs_mods::deploy::{Deployer, Journal, Manifest, PlanOptions};
use openvfs_mods::{
    ConflictReport, DeployMode, DeployReport, ForeignPolicy, Mismatch, MismatchKind, ModEntry, Profile,
};

const USAGE: &str = "\
usage: openvfs-mods [-v|-q] <command> [args]

  profile new <file> --name N --game DIR --overwrite DIR
  profile add <file> <mod> <mod-dir> [--at INDEX]
  profile remove <file> <mod>
  profile enable|disable <file> <mod>
  profile move <file> <mod> --to INDEX
  profile show <file>
  conflicts <file> [--mod NAME]
  deploy <file> --state DIR [--mode link|copy] [--copy-under PATH]...
         [--foreign keep|error] [--dry-run]
  undeploy <file> --state DIR
  verify <file> --state DIR
  capture <file> --state DIR
  status <file> --state DIR
  resume --state DIR
  rollback --state DIR
  game <file> [--state DIR] [-- command args...]
  mount <file> <dir-or-drive>       (Windows + WinFsp: serve the merged view
                                   as a real filesystem until Enter)

global flags: -v/-vv/-vvv raise log verbosity (info/debug/trace),
              -q warns only, RUST_LOG overrides; logs go to stderr.

`openvfs-mods <command> --help` shows the command's own help.";

const DEPLOY_USAGE: &str = "\
usage: openvfs-mods deploy <file> --state DIR [options]

  --mode link|copy    hardlink files (default) or copy them
  --copy-under PATH   always copy under the folded path prefix (repeatable)
  --foreign keep|error  keep unexpected game-dir files (default) or refuse
  --dry-run           print the plan without touching the game dir";

const GAME_USAGE: &str = "\
usage: openvfs-mods game <file> [--state DIR] [-- command args...]

Deploys the profile, then either runs `command` and captures its writes on
exit, or — without a command — prints how to capture after playing.
--state defaults to <file>.state next to the profile.";

const PROFILE_USAGE: &str = "\
usage: openvfs-mods profile <sub> ...

  new <file> --name N --game DIR --overwrite DIR   create an empty profile
  add <file> <mod> <mod-dir> [--at INDEX]          add a mod (INDEX 0 = top)
  remove <file> <mod>                              drop a mod by name
  enable|disable <file> <mod>                      toggle a mod
  move <file> <mod> --to INDEX                     re-prioritize a mod
  show <file>                                      print the load order";

const CONFLICTS_USAGE: &str = "\
usage: openvfs-mods conflicts <file> [--mod NAME]

Prints every contested path (winner over losers). --mod filters to the
conflicts NAME takes part in, as winner or loser.";

#[cfg(windows)]
const MOUNT_USAGE: &str = "\
usage: openvfs-mods mount <file> <dir-or-drive>

Mounts the profile's merged view (mods over the base game, with the
overwrite dir on top) as a REAL filesystem — every process sees it, no
deployment into the game dir needed. Requires WinFsp installed.

The directory mount point is created by the mount itself (junction
semantics): it must not exist, or must be an EMPTY directory (which is
preserved across the mount). Press Enter (or close the console) to
unmount; writes through the mount land in the overwrite directory, and
deletes are recorded in <file>.whiteouts next to the profile so they
survive re-mounts.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let mut verbose = 0i32;
    let mut command = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" if command.is_none() => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-v" | "--verbose" => verbose += 1,
            "-vv" => verbose += 2,
            "-vvv" => verbose += 3,
            "-q" | "--quiet" => verbose = i32::MIN,
            _ if command.is_none() => command = Some(arg),
            _ => {
                // The command owns the rest of argv, unparsed.
                let rest: Vec<String> = std::iter::once(arg).chain(args).collect();
                return dispatch(&command.unwrap(), &rest, verbose);
            }
        }
    }
    match command {
        None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(cmd) => dispatch(&cmd, &[], verbose),
    }
}

fn dispatch(command: &str, args: &[String], verbose: i32) -> ExitCode {
    init_logger(verbose);
    match command {
        "profile" => cmd_profile(args),
        "conflicts" => cmd_conflicts(args),
        "deploy" => cmd_deploy(args),
        "undeploy" => cmd_undeploy(args),
        "verify" => cmd_verify(args),
        "capture" => cmd_capture(args),
        "status" => cmd_status(args),
        "resume" => cmd_resume_or_rollback(args, true),
        "rollback" => cmd_resume_or_rollback(args, false),
        "game" => cmd_game(args),
        "mount" => cmd_mount(args),
        other => {
            eprintln!("error: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(f: impl FnOnce() -> Result<ExitCode, openvfs_mods::Error>) -> ExitCode {
    match f() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage_err(msg: &str, usage: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{usage}");
    ExitCode::from(2)
}

// ---------------------------------------------------------------- logger

fn init_logger(verbose: i32) {
    let level = rust_log_level().unwrap_or(match verbose {
        i32::MIN..=-1 => log::LevelFilter::Error,
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    });
    let logger: &'static StderrLogger = Box::leak(Box::new(StderrLogger { max: level }));
    log::set_logger(logger).map(|()| log::set_max_level(level)).ok();
}

/// A minimal `RUST_LOG` reader: a bare level (`debug`) or comma-separated
/// `module=level` filters; the deepest matching filter wins. Anything else
/// is ignored — this is a stderr convenience, not `env_logger`.
fn rust_log_level() -> Option<log::LevelFilter> {
    let spec = std::env::var("RUST_LOG").ok()?;
    let mut level = None;
    for filter in spec.split(',') {
        let filter = filter.trim();
        if filter.is_empty() {
            continue;
        }
        let (applies, lvl) = match filter.split_once('=') {
            Some((m, l)) => ("openvfs_mods".starts_with(m), l.parse::<log::LevelFilter>().ok()),
            None => (true, filter.parse::<log::LevelFilter>().ok()),
        };
        if applies && let Some(lvl) = lvl {
            level = Some(lvl);
        }
    }
    level
}

struct StderrLogger {
    max: log::LevelFilter,
}

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.max
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{:>5} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

// ---------------------------------------------------------------- profile

fn cmd_profile(args: &[String]) -> ExitCode {
    let Some(sub) = args.first() else { return usage_err("missing profile subcommand", PROFILE_USAGE) };
    match sub.as_str() {
        "new" => profile_new(&args[1..]),
        "add" => profile_add(&args[1..]),
        "remove" => profile_remove(&args[1..]),
        "enable" | "disable" => profile_enable(&args[1..], sub == "enable"),
        "move" => profile_move(&args[1..]),
        "show" => profile_show(&args[1..]),
        "-h" | "--help" => {
            println!("{PROFILE_USAGE}");
            ExitCode::SUCCESS
        }
        other => usage_err(&format!("unknown profile subcommand `{other}`"), PROFILE_USAGE),
    }
}

/// Loads a profile and absolutizes every host path in it: the manifest
/// stores the game dir verbatim, so a profile written (or invoked) with
/// relative paths must not become CWD-dependent across subcommands.
fn load_profile(path: &str) -> Result<Profile, openvfs_mods::Error> {
    let mut profile = Profile::load(path)?;
    let absolutize = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    profile.game_dir = absolutize(&profile.game_dir);
    profile.overwrite_dir = absolutize(&profile.overwrite_dir);
    for m in &mut profile.mods {
        m.source_dir = absolutize(&m.source_dir);
    }
    Ok(profile)
}

fn profile_new(args: &[String]) -> ExitCode {
    let mut file = None;
    let (mut name, mut game, mut overwrite) = (None, None, None);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => name = iter.next().cloned(),
            "--game" => game = iter.next().map(PathBuf::from),
            "--overwrite" => overwrite = iter.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("{PROFILE_USAGE}");
                return ExitCode::SUCCESS;
            }
            flag if flag.starts_with('-') => return usage_err(&format!("unknown flag `{flag}`"), PROFILE_USAGE),
            _ if file.is_none() => file = Some(arg.clone()),
            _ => return usage_err(&format!("unexpected argument `{arg}`"), PROFILE_USAGE),
        }
    }
    let (Some(file), Some(name), Some(game), Some(overwrite)) = (file, name, game, overwrite) else {
        return usage_err("expected <file> --name N --game DIR --overwrite DIR", PROFILE_USAGE);
    };
    run(move || {
        Profile::new(name, game, overwrite).save(&file)?;
        println!("created profile {file}");
        Ok(ExitCode::SUCCESS)
    })
}

fn profile_add(args: &[String]) -> ExitCode {
    let mut positional = positionals(args, &["--at"]).into_iter();
    let at = flag_value(args, "--at").and_then(|v| v.parse::<usize>().ok());
    let (Some(file), Some(name), Some(dir)) = (positional.next(), positional.next(), positional.next()) else {
        return usage_err("expected <file> <mod> <mod-dir> [--at INDEX]", PROFILE_USAGE);
    };
    run(move || {
        let mut profile = load_profile(&file)?;
        let entry = ModEntry { name: name.clone(), source_dir: PathBuf::from(&dir), enabled: true };
        match at {
            Some(index) => profile.insert_mod(index, entry),
            None => profile.add_mod(entry),
        }
        profile.save(&file)?;
        println!("added mod {name:?} ({dir}) at priority {}", at.unwrap_or(profile.mods.len() - 1));
        Ok(ExitCode::SUCCESS)
    })
}

fn profile_remove(args: &[String]) -> ExitCode {
    let (Some(file), Some(name)) = (args.first(), args.get(1)) else {
        return usage_err("expected <file> <mod>", PROFILE_USAGE);
    };
    run(move || {
        let mut profile = load_profile(file)?;
        if profile.remove_mod(name).is_none() {
            eprintln!("error: no mod named {name:?}");
            return Ok(ExitCode::FAILURE);
        }
        profile.save(file)?;
        println!("removed mod {name:?}");
        Ok(ExitCode::SUCCESS)
    })
}

fn profile_enable(args: &[String], enabled: bool) -> ExitCode {
    let (Some(file), Some(name)) = (args.first(), args.get(1)) else {
        return usage_err("expected <file> <mod>", PROFILE_USAGE);
    };
    run(move || {
        let mut profile = load_profile(file)?;
        if !profile.set_enabled(name, enabled) {
            eprintln!("error: no mod named {name:?}");
            return Ok(ExitCode::FAILURE);
        }
        profile.save(file)?;
        println!("{} mod {name:?}", if enabled { "enabled" } else { "disabled" });
        Ok(ExitCode::SUCCESS)
    })
}

fn profile_move(args: &[String]) -> ExitCode {
    let to = flag_value(args, "--to").and_then(|v| v.parse::<usize>().ok());
    let mut positional = positionals(args, &["--to"]).into_iter();
    let (Some(file), Some(name), Some(to)) = (positional.next(), positional.next(), to) else {
        return usage_err("expected <file> <mod> --to INDEX", PROFILE_USAGE);
    };
    run(move || {
        let mut profile = load_profile(&file)?;
        let Some(from) = profile.mods.iter().position(|m| m.name == name) else {
            eprintln!("error: no mod named {name:?}");
            return Ok(ExitCode::FAILURE);
        };
        profile.move_mod(from, to);
        profile.save(&file)?;
        println!("moved mod {name:?} to priority {to}");
        Ok(ExitCode::SUCCESS)
    })
}

fn profile_show(args: &[String]) -> ExitCode {
    let Some(file) = args.first() else { return usage_err("expected <file>", PROFILE_USAGE) };
    run(move || {
        let profile = load_profile(file)?;
        println!("profile {:?}:", profile.name);
        println!("  game:      {}", profile.game_dir.display());
        println!("  overwrite: {}", profile.overwrite_dir.display());
        if profile.mods.is_empty() {
            println!("  (no mods)");
        }
        for (i, m) in profile.mods.iter().enumerate() {
            let mark = if m.enabled { "x" } else { " " };
            println!("  {i:>3}. [{mark}] {} ({})", m.name, m.source_dir.display());
        }
        Ok(ExitCode::SUCCESS)
    })
}

// ---------------------------------------------------------------- conflicts

fn cmd_conflicts(args: &[String]) -> ExitCode {
    let only_mod = flag_value(args, "--mod");
    let mut positional = positionals(args, &["--mod"]).into_iter();
    let Some(file) = positional.next() else { return usage_err("expected <file> [--mod NAME]", CONFLICTS_USAGE) };
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{CONFLICTS_USAGE}");
        return ExitCode::SUCCESS;
    }
    run(move || {
        let profile = load_profile(&file)?;
        let built = profile.build()?;
        for skipped in built.skipped() {
            eprintln!("warning: skipped mod {skipped}");
        }
        let report = built.index()?.conflicts();
        match only_mod {
            Some(name) => print_conflicts_for(&report, &name),
            None => print!("{report}"),
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn print_conflicts_for(report: &ConflictReport, name: &str) {
    let mut shown = 0usize;
    for entry in report.entries() {
        if entry.providers().iter().any(|p| p.name() == name) {
            let losers: Vec<String> = entry.losers().iter().map(|p| format!("{:?}", p.name())).collect();
            println!("  {}: {:?} over {}", entry.path(), entry.winner().name(), losers.join(", "));
            shown += 1;
        }
    }
    println!("{shown} conflict(s) involving {name:?}");
}

// ---------------------------------------------------------------- deploy

struct DeployArgs {
    file: String,
    state: PathBuf,
    options: PlanOptions,
    dry_run: bool,
}

fn parse_deploy_args(args: &[String]) -> Result<DeployArgs, String> {
    let mut file = None;
    let mut state = None;
    let mut options = PlanOptions::new();
    let mut dry_run = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--state" => state = iter.next().map(PathBuf::from),
            "--mode" => match iter.next().map(String::as_str) {
                Some("link") => options = options.mode(DeployMode::Link),
                Some("copy") => options = options.mode(DeployMode::Copy),
                other => return Err(format!("--mode expects link|copy, got {other:?}")),
            },
            "--copy-under" => {
                let path = iter.next().ok_or("--copy-under expects a path")?;
                options = options.copy_under(path);
            }
            "--foreign" => match iter.next().map(String::as_str) {
                Some("keep") => options = options.foreign(ForeignPolicy::Keep),
                Some("error") => options = options.foreign(ForeignPolicy::Error),
                other => return Err(format!("--foreign expects keep|error, got {other:?}")),
            },
            "--dry-run" => dry_run = true,
            "-h" | "--help" => return Err(String::new()),
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            _ if file.is_none() => file = Some(arg.clone()),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }
    let file = file.ok_or("missing <file>")?;
    let state = state.ok_or("missing --state DIR")?;
    Ok(DeployArgs { file, state, options, dry_run })
}

fn cmd_deploy(args: &[String]) -> ExitCode {
    let parsed = match parse_deploy_args(args) {
        Ok(p) => p,
        Err(msg) if msg.is_empty() => {
            println!("{DEPLOY_USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => return usage_err(&msg, DEPLOY_USAGE),
    };
    run(move || {
        let profile = load_profile(&parsed.file)?;
        let deployer = Deployer::new(&parsed.state);
        let plan = deployer.plan(&profile, parsed.options)?;
        if parsed.dry_run {
            println!(
                "plan: {} ops, {} files, {} dirs to create",
                plan.len(),
                plan.files.len(),
                plan.created_dirs.len()
            );
            for foreign in &plan.foreign {
                println!("  foreign kept: {foreign}");
            }
            for lost in &plan.unreachable {
                println!("  unreachable: {lost}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        let report = deployer.deploy(&plan)?;
        print_report(&report);
        Ok(ExitCode::SUCCESS)
    })
}

fn print_report(report: &DeployReport) {
    println!(
        "deployed: {} linked, {} copied, {} removed, {} restored, {} backed up, {} dirs created",
        report.linked, report.copied, report.removed, report.restored, report.backed_up, report.dirs_created
    );
    if !report.foreign.is_empty() {
        println!("kept foreign files: {}", report.foreign.len());
    }
    if !report.captured.is_empty() {
        println!("captured {} write(s) to the overwrite dir", report.captured.len());
    }
    for lost in &report.unrecoverable {
        println!("unrecoverable: {lost}");
    }
}

// ---------------------------------------------------------------- lifecycle

fn cmd_undeploy(args: &[String]) -> ExitCode {
    let Some((file, state)) = positional_and_state(args) else {
        return usage_err("expected <file> --state DIR", USAGE);
    };
    run(move || {
        let profile = load_profile(&file)?;
        let report = Deployer::new(&state).undeploy(Some(&profile.overwrite_dir))?;
        println!(
            "undeployed: {} removed, {} restored, {} captured, {} dirs removed",
            report.removed,
            report.restored,
            report.captured.len(),
            report.dirs_removed
        );
        for lost in &report.unrecoverable {
            println!("unrecoverable: {lost}");
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn cmd_verify(args: &[String]) -> ExitCode {
    let Some((file, state)) = positional_and_state(args) else {
        return usage_err("expected <file> --state DIR", USAGE);
    };
    run(move || {
        // The profile is loaded for context only — verification runs off the
        // manifest, which knows the game dir it was written for.
        let profile = load_profile(&file)?;
        if let Some(manifest) = Manifest::load(&state)?
            && manifest.game_dir != profile.game_dir
        {
            eprintln!(
                "warning: manifest describes {}, not the profile's {}",
                manifest.game_dir.display(),
                profile.game_dir.display()
            );
        }
        let mismatches = Deployer::new(&state).verify()?;
        if mismatches.is_empty() {
            println!("verify: clean — the game dir matches the deployment");
            return Ok(ExitCode::SUCCESS);
        }
        for m in &mismatches {
            println!("{} {}", mismatch_tag(m), m.path);
        }
        println!("verify: {} drifted path(s)", mismatches.len());
        Ok(ExitCode::FAILURE)
    })
}

fn mismatch_tag(m: &Mismatch) -> &'static str {
    match m.kind {
        MismatchKind::Missing => "missing  ",
        MismatchKind::Modified => "modified ",
        MismatchKind::Foreign => "foreign  ",
        MismatchKind::WrongType => "wrongtype",
    }
}

fn cmd_capture(args: &[String]) -> ExitCode {
    let Some((file, state)) = positional_and_state(args) else {
        return usage_err("expected <file> --state DIR", USAGE);
    };
    run(move || {
        let profile = load_profile(&file)?;
        let report = Deployer::new(&state).capture(Some(&profile.overwrite_dir))?;
        for captured in &report.moved {
            println!("captured {} -> {}", captured.path, captured.target.display());
        }
        for repaired in &report.repaired {
            println!("repaired {repaired}");
        }
        for corrupted in &report.corrupted {
            println!("CORRUPTED {} (mod file {} is mutated)", corrupted.path, corrupted.source.display());
        }
        for lost in &report.lost {
            println!("lost {lost} (base file changed; original bytes gone)");
        }
        println!(
            "capture: {} moved, {} repaired, {} corrupted, {} lost, {} foreign kept",
            report.moved.len(),
            report.repaired.len(),
            report.corrupted.len(),
            report.lost.len(),
            report.kept_foreign
        );
        Ok(if report.corrupted.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
    })
}

fn cmd_status(args: &[String]) -> ExitCode {
    let Some((file, state)) = positional_and_state(args) else {
        return usage_err("expected <file> --state DIR", USAGE);
    };
    run(move || {
        let profile = load_profile(&file)?;
        let deployer = Deployer::new(&state);
        if Journal::exists(deployer.state_dir()) {
            println!("status: deployment IN FLIGHT — run `resume` or `rollback` before anything else");
            return Ok(ExitCode::SUCCESS);
        }
        match Manifest::load(deployer.state_dir())? {
            None => {
                println!("status: nothing deployed");
            }
            Some(manifest) => {
                let placed = manifest.deployed().count();
                let base = manifest.entries.len() - placed;
                println!("status: deployed");
                println!("  game dir: {}", manifest.game_dir.display());
                println!("  files: {placed} deployed, {base} base");
                println!("  dirs created: {}", manifest.dirs.len());
                println!("  foreign kept: {}", manifest.foreign.len());
                println!("  game dir matches profile: {}", manifest.game_dir == profile.game_dir);
            }
        }
        Ok(ExitCode::SUCCESS)
    })
}

fn cmd_resume_or_rollback(args: &[String], resume: bool) -> ExitCode {
    let Some(state) = flag_value(args, "--state").map(PathBuf::from) else {
        return usage_err("expected --state DIR", USAGE);
    };
    run(move || {
        let deployer = Deployer::new(&state);
        let report = if resume { deployer.resume()? } else { deployer.rollback()? };
        let verb = if resume { "resumed" } else { "rolled back" };
        println!(
            "{verb}: {} linked/copied, {} removed, {} restored, {} skipped",
            report.linked + report.copied,
            report.removed,
            report.restored,
            report.skipped
        );
        Ok(ExitCode::SUCCESS)
    })
}

// ---------------------------------------------------------------- game

fn cmd_game(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{GAME_USAGE}");
        return ExitCode::SUCCESS;
    }
    // Everything after `--` is the command to run.
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let trailing: Vec<String> = args.get(split + 1..).map_or_else(Vec::new, <[String]>::to_vec);
    let state = flag_value(&args[..split], "--state").map(PathBuf::from);
    let mut positional = positionals(&args[..split], &["--state"]).into_iter();
    let Some(file) = positional.next() else { return usage_err("expected <file>", GAME_USAGE) };
    let state = state.unwrap_or_else(|| Path::new(&file).with_extension("state"));
    run(move || {
        let profile = load_profile(&file)?;
        let deployer = Deployer::new(&state);
        let plan = deployer.plan(&profile, PlanOptions::new())?;
        let report = deployer.deploy(&plan)?;
        print_report(&report);
        if trailing.is_empty() {
            println!(
                "\nMods are deployed into {}. Run your game, then:\n  openvfs-mods capture {file:?} --state {}",
                profile.game_dir.display(),
                state.display()
            );
            return Ok(ExitCode::SUCCESS);
        }
        let exit_status = std::process::Command::new(&trailing[0]).args(&trailing[1..]).status()?;
        println!("game exited: {exit_status}");
        let captured = deployer.capture(Some(&profile.overwrite_dir))?;
        println!(
            "capture: {} moved, {} repaired, {} corrupted, {} lost",
            captured.moved.len(),
            captured.repaired.len(),
            captured.corrupted.len(),
            captured.lost.len()
        );
        Ok(ExitCode::SUCCESS)
    })
}

// ---------------------------------------------------------------- mount

/// Windows + `WinFsp`: serve the profile's merged view as a real filesystem.
/// Every process sees the mounted tree; writes land in the overwrite dir
/// (the overlay's upper), so nothing touches the base game at all.
#[cfg(windows)]
fn cmd_mount(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{MOUNT_USAGE}");
        return ExitCode::SUCCESS;
    }
    let mut positional = positionals(args, &[]).into_iter();
    let (Some(file), Some(mountpoint)) = (positional.next(), positional.next()) else {
        return usage_err("expected <file> <dir-or-drive>", MOUNT_USAGE);
    };
    if !openvfs_winfsp::Mount::is_available() {
        eprintln!(
            "error: WinFsp is not installed — `mount` needs its driver.\n\
             Install it once (as admin) from https://winfsp.dev or `winget install winfsp`."
        );
        return ExitCode::FAILURE;
    }
    run(move || {
        let profile = load_profile(&file)?;
        let built = profile.build()?;
        for skipped in built.skipped() {
            eprintln!("warning: skipped mod {skipped}");
        }
        // Deletes through the mount are in-memory whiteouts; persist them
        // next to the profile so a re-mount keeps the same merged view.
        let overlay = built.overlay();
        let whiteout_file = Path::new(&file).with_extension("whiteouts");
        if let Ok(saved) = std::fs::read_to_string(&whiteout_file) {
            for path in saved.lines().filter_map(|l| openvfs::VPath::new(l).ok()) {
                overlay.hide(&path);
            }
        }
        let mount = openvfs_winfsp::Mount::mount(
            std::sync::Arc::clone(overlay),
            Path::new(&mountpoint),
            openvfs_winfsp::MountOptions::default(),
        )
        .map_err(|e| openvfs_mods::Error::Mount { reason: e.to_string() })?;
        println!("mounted profile {:?} at {}", profile.name, mount.mountpoint());
        println!("writes land in {}", profile.overwrite_dir.display());
        // Flush whiteouts whenever they change (once a second at most), so
        // even a killed process keeps its deletes for the next mount.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flusher = {
            let overlay = std::sync::Arc::clone(overlay);
            let file = whiteout_file.clone();
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut last = String::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let saved = overlay.whiteouts().iter().fold(String::new(), |mut s, p| {
                        use std::fmt::Write as _;
                        let _ = writeln!(s, "{p}");
                        s
                    });
                    if saved != last && std::fs::write(&file, &saved).is_ok() {
                        last = saved;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            })
        };
        if std::io::stdin().is_terminal() {
            println!("press Enter to unmount");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
        } else {
            println!("running non-interactively — the mount lives until this process exits");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flusher.join().ok();
        drop(mount);
        println!("unmounted");
        Ok(ExitCode::SUCCESS)
    })
}

#[cfg(not(windows))]
fn cmd_mount(_args: &[String]) -> ExitCode {
    eprintln!("error: `mount` is only supported on Windows with WinFsp installed");
    ExitCode::FAILURE
}

// ---------------------------------------------------------------- misc

/// `--flag VALUE`; the flag's value is invisible to positional scanning.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// Positionals in `args`, skipping `value_flags` *and their values* — so
/// `--state DIR` never leaks `DIR` into the positional list.
fn positionals(args: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if value_flags.contains(&arg.as_str()) {
            iter.next(); // consume the flag's value
        } else if !arg.starts_with('-') {
            out.push(arg.clone());
        }
    }
    out
}

/// `<file> --state DIR` — the shape most deployment subcommands share.
fn positional_and_state(args: &[String]) -> Option<(String, PathBuf)> {
    let file = positionals(args, &["--state"]).into_iter().next()?;
    let state = flag_value(args, "--state").map(PathBuf::from)?;
    Some((file, state))
}
