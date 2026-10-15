//! `openvfs-sandbox` — edit a real directory through a copy-on-write VFS.
//!
//! Every command runs against an overlay: the directory on disk is the read-only
//! lower layer and all edits land in memory. Review them with `status`/`diff`,
//! then `commit` them to disk or simply walk away.

mod diff;
mod script;
mod session;

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use session::{Flow, Session};

const USAGE: &str = "\
usage: openvfs-sandbox <DIR> [-c COMMANDS | -f SCRIPT] [--apply]

  <DIR>            directory to open (it is never modified unless you commit)
  -c COMMANDS      run `;`-separated commands, then report pending changes
  -f SCRIPT        run commands from a file (one per line, # comments)
  --apply          in batch mode, commit the changes to disk at the end
  -h, --help       show this help

Without -c/-f an interactive shell starts; type `help` inside it.

examples:
  openvfs-sandbox ./my-crate -c \"replace foo_bar foo_baz --ext rs; diff\"
  openvfs-sandbox ./my-crate -f rename.sandbox --apply";

struct Cli {
    dir: PathBuf,
    commands: Option<String>,
    script: Option<PathBuf>,
    apply: bool,
}

fn parse_cli() -> Result<Option<Cli>, String> {
    let mut dir = None;
    let mut commands = None;
    let mut script = None;
    let mut apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-c" | "--command" => commands = Some(args.next().ok_or("-c expects commands")?),
            "-f" | "--file" => script = Some(PathBuf::from(args.next().ok_or("-f expects a file")?)),
            "--apply" => apply = true,
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            _ if dir.is_some() => return Err(format!("unexpected argument `{arg}`")),
            _ => dir = Some(PathBuf::from(arg)),
        }
    }
    if commands.is_some() && script.is_some() {
        return Err("use either -c or -f, not both".into());
    }
    let dir = dir.ok_or("missing <DIR>")?;
    Ok(Some(Cli { dir, commands, script, apply }))
}

fn main() -> ExitCode {
    let cli = match parse_cli() {
        Ok(Some(cli)) => cli,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut session = match Session::open(&cli.dir, io::stdout(), color) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot open {}: {e}", cli.dir.display());
            return ExitCode::FAILURE;
        }
    };

    let batch = match (cli.commands, &cli.script) {
        (Some(commands), _) => Some(commands),
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(src) => Some(src),
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        (None, None) => None,
    };

    match batch {
        Some(src) => run_batch(&mut session, &src, cli.apply),
        None if cli.apply => {
            eprintln!("error: --apply only makes sense with -c or -f");
            ExitCode::from(2)
        }
        None => run_interactive(&mut session),
    }
}

fn run_batch<W: Write>(session: &mut Session<W>, src: &str, apply: bool) -> ExitCode {
    let statements = match script::parse(src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("parse error: {e}");
            return ExitCode::from(2);
        }
    };
    for statement in statements {
        match session.exec(&statement) {
            Ok(Flow::Continue) => {}
            Ok(Flow::Exit) => break,
            Err(e) => {
                eprintln!("error in `{}`: {e}", statement.join(" "));
                eprintln!("aborted: uncommitted changes were discarded, the directory is untouched.");
                return ExitCode::FAILURE;
            }
        }
    }
    let finished = if apply { session.commit().map(drop) } else { session.report_pending() };
    match finished {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_interactive<W: Write>(session: &mut Session<W>) -> ExitCode {
    println!("openvfs sandbox on {} (type `help`; edits stay in memory until `commit`)", session.host_dir().display());
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        let marker = if session.is_dirty() { "*" } else { "" };
        print!("sandbox:{}{marker}> ", session.cwd());
        let _ = io::stdout().flush();

        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        }
        let statements = match script::parse(&line) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("parse error: {e}");
                continue;
            }
        };
        for statement in statements {
            match session.exec(&statement) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Exit) => return farewell(session),
                Err(e) => eprintln!("error: {e}"),
            }
        }
    }
    farewell(session)
}

fn farewell<W: Write>(session: &Session<W>) -> ExitCode {
    match session.changes() {
        Ok(changes) if !changes.is_empty() => {
            eprintln!(
                "note: {} uncommitted change(s) discarded; {} is untouched.",
                changes.len(),
                session.host_dir().display()
            );
        }
        _ => {}
    }
    ExitCode::SUCCESS
}
