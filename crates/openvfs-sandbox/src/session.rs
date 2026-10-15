//! A sandbox session: a real directory seen through a copy-on-write overlay.
//!
//! ```text
//!            Vfs  (what every command talks to)
//!             │
//!         OverlayFs ──── changes() / commit() / reset()
//!          │      │
//!   MemoryFs      PhysicalFs(<DIR>)
//!   (upper:       (lower: never written
//!    all edits)    until `commit`)
//! ```

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openvfs::{Change, FileType, MemoryFs, OverlayFs, PhysicalFs, VPath, Vfs, VfsError, ops};

use crate::diff::{self, Style};

/// Directories that recursive commands skip unless you point at them directly.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

const HELP: &str = "\
Every command works on a copy-on-write view: the real directory is untouched
until you `commit`. Paths are relative to the current directory or absolute (/...).

  navigation   pwd | cd [DIR] | ls [PATH] | tree [PATH]
  reading      cat FILE | find [PATH] [--name TEXT] [--ext EXT]
               grep TEXT [PATH] [--ext EXT] [-i]
  editing      write FILE TEXT... | append FILE TEXT... | mkdir DIR...
               rm [-r] PATH... | mv FROM TO | cp [-r] FROM TO
               replace FROM TO [PATH] [--ext EXT]     (bulk find & replace)
  reviewing    status | diff [PATH]
  finishing    commit (write to disk) | discard (drop all edits) | exit

Quote arguments with spaces: write notes.txt \"line one\\nline two\"";

/// A command failure, printed to the user as-is.
#[derive(Debug)]
pub struct CmdError(pub String);

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<VfsError> for CmdError {
    fn from(e: VfsError) -> Self {
        Self(e.to_string())
    }
}

impl From<io::Error> for CmdError {
    fn from(e: io::Error) -> Self {
        Self(e.to_string())
    }
}

/// Result of a command.
pub type CmdResult<T = ()> = Result<T, CmdError>;

macro_rules! bail {
    ($($arg:tt)*) => { return Err(CmdError(format!($($arg)*))) };
}

/// Whether the caller should keep reading commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

pub struct Session<W: Write> {
    vfs: Vfs,
    overlay: Arc<OverlayFs>,
    disk: Arc<PhysicalFs>,
    host_dir: PathBuf,
    cwd: VPath,
    out: W,
    style: Style,
}

impl<W: Write> Session<W> {
    /// Opens `dir` through a fresh overlay. Nothing is written to `dir`.
    pub fn open(dir: &Path, out: W, color: bool) -> Result<Self, VfsError> {
        let disk = Arc::new(PhysicalFs::new(dir)?);
        let overlay = Arc::new(OverlayFs::with_layers(Arc::new(MemoryFs::new()), vec![disk.clone()]));
        Ok(Self {
            vfs: Vfs::from(Arc::clone(&overlay)),
            overlay,
            disk,
            host_dir: dir.to_path_buf(),
            cwd: VPath::root(),
            out,
            style: Style { color },
        })
    }

    pub fn cwd(&self) -> &VPath {
        &self.cwd
    }

    pub fn host_dir(&self) -> &Path {
        &self.host_dir
    }

    pub fn is_dirty(&self) -> bool {
        self.overlay.is_dirty().unwrap_or(true)
    }

    pub fn changes(&self) -> Result<Vec<Change>, VfsError> {
        self.overlay.changes()
    }

    #[cfg(test)]
    pub fn output(&self) -> &W {
        &self.out
    }

    /// Runs one parsed command line.
    pub fn exec(&mut self, argv: &[String]) -> CmdResult<Flow> {
        let Some((cmd, rest)) = argv.split_first() else { return Ok(Flow::Continue) };
        match cmd.as_str() {
            "help" | "?" => writeln!(self.out, "{HELP}")?,
            "pwd" => writeln!(self.out, "{}", self.cwd)?,
            "cd" => self.cd(rest)?,
            "ls" => self.ls(rest)?,
            "tree" => self.tree(rest)?,
            "cat" => self.cat(rest)?,
            "write" => self.write(rest, false)?,
            "append" => self.write(rest, true)?,
            "mkdir" => self.mkdir(rest)?,
            "rm" => self.rm(rest)?,
            "mv" => self.mv(rest)?,
            "cp" => self.cp(rest)?,
            "find" => self.find(rest)?,
            "grep" => self.grep(rest)?,
            "replace" => self.replace(rest)?,
            "status" | "st" => self.status()?,
            "diff" => self.diff(rest)?,
            "commit" => {
                self.commit()?;
            }
            "discard" | "reset" => self.discard()?,
            "exit" | "quit" => return Ok(Flow::Exit),
            other => bail!("unknown command `{other}` (try `help`)"),
        }
        Ok(Flow::Continue)
    }

    /// Writes every pending change to the real directory. Returns how many.
    pub fn commit(&mut self) -> CmdResult<usize> {
        let applied = self.overlay.commit(&*self.disk)?;
        self.overlay.reset()?;
        if applied.is_empty() {
            writeln!(self.out, "nothing to commit")?;
        } else {
            writeln!(self.out, "committed {} change(s) to {}", applied.len(), self.host_dir.display())?;
        }
        Ok(applied.len())
    }

    /// Prints pending changes followed by a dry-run notice (used at the end of batch runs).
    pub fn report_pending(&mut self) -> CmdResult {
        let changes = self.changes()?;
        if changes.is_empty() {
            return Ok(());
        }
        self.print_changes(&changes)?;
        writeln!(
            self.out,
            "\ndry run: {} change(s) NOT written. Re-run with --apply (or end with `commit`) to write them.",
            changes.len()
        )?;
        Ok(())
    }

    // --- helpers ------------------------------------------------------------

    fn resolve(&self, arg: &str) -> CmdResult<VPath> {
        Ok(if arg.starts_with(['/', '\\']) { VPath::new(arg)? } else { self.cwd.join(arg)? })
    }

    fn resolve_or_cwd(&self, arg: Option<&str>) -> CmdResult<VPath> {
        arg.map_or_else(|| Ok(self.cwd.clone()), |a| self.resolve(a))
    }

    /// If `to` is an existing directory, targets `to/<name of from>` (shell semantics).
    fn target_inside(&self, from: &VPath, to: VPath) -> CmdResult<VPath> {
        if self.vfs.is_dir(&to) {
            let name = from.file_name().ok_or_else(|| CmdError("cannot move the root".into()))?;
            return Ok(to.join(name)?);
        }
        Ok(to)
    }

    fn print_changes(&mut self, changes: &[Change]) -> CmdResult {
        for change in changes {
            let (code, text) = match change {
                Change::Added { .. } => ("32", change.to_string()),
                Change::Modified { .. } => ("33", change.to_string()),
                Change::Removed { .. } => ("31", change.to_string()),
            };
            writeln!(self.out, "  {}", self.style.paint(code, &text))?;
        }
        Ok(())
    }

    // --- commands -----------------------------------------------------------

    fn cd(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(0, 1, "cd [DIR]")?;
        let target = args.pos.first().map_or_else(|| Ok(VPath::root()), |a| self.resolve(a))?;
        if !self.vfs.is_dir(&target) {
            bail!("not a directory: {target}");
        }
        self.cwd = target;
        Ok(())
    }

    fn ls(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(0, 1, "ls [PATH]")?;
        let target = self.resolve_or_cwd(args.pos.first().copied())?;
        let meta = self.vfs.metadata(&target)?;
        if meta.is_file() {
            writeln!(self.out, "{}  {}", target, human_size(meta.len))?;
            return Ok(());
        }
        let entries = self.vfs.read_dir(&target)?;
        let width = entries.iter().map(|e| e.name.chars().count() + 1).max().unwrap_or(0);
        for entry in entries {
            if entry.is_dir() {
                let name = format!("{}/", entry.name);
                writeln!(self.out, "  {}", self.style.paint("34", &name))?;
            } else {
                let size = self.vfs.metadata(&entry.path).map(|m| human_size(m.len)).unwrap_or_default();
                writeln!(self.out, "  {:<width$}  {size}", entry.name)?;
            }
        }
        Ok(())
    }

    fn tree(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(0, 1, "tree [PATH]")?;
        let root = self.resolve_or_cwd(args.pos.first().copied())?;
        let mut lines = vec![root.to_string()];
        tree_lines(&self.vfs, &root, 1, &mut lines)?;
        for line in lines {
            writeln!(self.out, "{line}")?;
        }
        Ok(())
    }

    fn cat(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(1, usize::MAX, "cat FILE...")?;
        for arg in &args.pos {
            let bytes = self.vfs.read(self.resolve(arg)?)?;
            let text = String::from_utf8_lossy(&bytes);
            write!(self.out, "{text}")?;
            if !text.is_empty() && !text.ends_with('\n') {
                writeln!(self.out)?;
            }
        }
        Ok(())
    }

    fn write(&mut self, rest: &[String], append: bool) -> CmdResult {
        // No flag parsing: the text is taken verbatim.
        let Some((file, text)) = rest.split_first() else {
            bail!("usage: {} FILE TEXT...", if append { "append" } else { "write" });
        };
        let path = self.resolve(file)?;
        let text = text.join(" ");
        if let Some(parent) = path.parent() {
            self.vfs.create_dir_all(parent)?;
        }
        if append {
            self.vfs.append(&path, &text)?;
        } else {
            self.vfs.write(&path, &text)?;
        }
        writeln!(self.out, "{} {} bytes -> {path}", if append { "appended" } else { "wrote" }, text.len())?;
        Ok(())
    }

    fn mkdir(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(1, usize::MAX, "mkdir DIR...")?;
        for arg in &args.pos {
            self.vfs.create_dir_all(self.resolve(arg)?)?;
        }
        Ok(())
    }

    fn rm(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &["-r"], &[])?.arity(1, usize::MAX, "rm [-r] PATH...")?;
        for arg in &args.pos {
            let path = self.resolve(arg)?;
            if path.is_root() {
                bail!("refusing to remove /");
            }
            if self.vfs.is_dir(&path) {
                if !args.has("-r") {
                    bail!("{path} is a directory (use rm -r)");
                }
                self.vfs.remove_dir_all(&path)?;
            } else {
                self.vfs.remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn mv(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(2, 2, "mv FROM TO")?;
        let from = self.resolve(args.pos[0])?;
        let to = self.target_inside(&from, self.resolve(args.pos[1])?)?;
        self.vfs.rename(&from, &to)?;
        Ok(())
    }

    fn cp(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &["-r"], &[])?.arity(2, 2, "cp [-r] FROM TO")?;
        let from = self.resolve(args.pos[0])?;
        let to = self.target_inside(&from, self.resolve(args.pos[1])?)?;
        if self.vfs.is_dir(&from) {
            if !args.has("-r") {
                bail!("{from} is a directory (use cp -r)");
            }
            self.vfs.copy_dir_all(&from, &to)?;
        } else {
            self.vfs.copy(&from, &to)?;
        }
        Ok(())
    }

    fn find(&mut self, rest: &[String]) -> CmdResult {
        let args =
            Args::parse(rest, &[], &["--name", "--ext"])?.arity(0, 1, "find [PATH] [--name TEXT] [--ext EXT]")?;
        let root = self.resolve_or_cwd(args.pos.first().copied())?;
        let name = args.value("--name");
        for file in files_under(&self.vfs, &root, args.value("--ext"))? {
            if name.is_none_or(|n| file.file_name().is_some_and(|f| f.contains(n))) {
                writeln!(self.out, "{file}")?;
            }
        }
        Ok(())
    }

    fn grep(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &["-i"], &["--ext"])?.arity(1, 2, "grep TEXT [PATH] [--ext EXT] [-i]")?;
        let ignore_case = args.has("-i");
        let needle = if ignore_case { args.pos[0].to_lowercase() } else { args.pos[0].to_owned() };
        let root = self.resolve_or_cwd(args.pos.get(1).copied())?;
        let mut hits = 0usize;
        for file in files_under(&self.vfs, &root, args.value("--ext"))? {
            let Ok(text) = self.vfs.read_to_string(&file) else { continue }; // skip binary files
            for (n, line) in text.lines().enumerate() {
                let haystack = if ignore_case { line.to_lowercase() } else { line.to_owned() };
                if haystack.contains(&needle) {
                    hits += 1;
                    let location = self.style.paint("35", &format!("{file}:{}", n + 1));
                    writeln!(self.out, "{location}: {line}")?;
                }
            }
        }
        if hits == 0 {
            writeln!(self.out, "no matches")?;
        }
        Ok(())
    }

    fn replace(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &["--ext"])?.arity(2, 3, "replace FROM TO [PATH] [--ext EXT]")?;
        let (from, to) = (args.pos[0], args.pos[1]);
        if from.is_empty() {
            bail!("replace: FROM must not be empty");
        }
        let root = self.resolve_or_cwd(args.pos.get(2).copied())?;
        let (mut files, mut total) = (0usize, 0usize);
        for file in files_under(&self.vfs, &root, args.value("--ext"))? {
            let Ok(text) = self.vfs.read_to_string(&file) else { continue };
            let count = text.matches(from).count();
            if count > 0 {
                self.vfs.write(&file, text.replace(from, to))?;
                writeln!(self.out, "  {file}  ({count}x)")?;
                files += 1;
                total += count;
            }
        }
        writeln!(self.out, "replaced {total} occurrence(s) in {files} file(s)")?;
        Ok(())
    }

    fn status(&mut self) -> CmdResult {
        let changes = self.changes()?;
        if changes.is_empty() {
            writeln!(self.out, "clean: no pending changes")?;
            return Ok(());
        }
        self.print_changes(&changes)?;
        writeln!(self.out, "{} change(s) pending, not yet written to {}", changes.len(), self.host_dir.display())?;
        Ok(())
    }

    fn diff(&mut self, rest: &[String]) -> CmdResult {
        let args = Args::parse(rest, &[], &[])?.arity(0, 1, "diff [PATH]")?;
        let filter = args.pos.first().map(|a| self.resolve(a)).transpose()?;
        let changes = self.changes()?;
        for change in changes.iter().filter(|c| filter.as_ref().is_none_or(|f| c.path().starts_with(f))) {
            let path = change.path();
            let (old, new) = match change {
                Change::Added { kind: FileType::Dir, .. } | Change::Removed { kind: FileType::Dir, .. } => {
                    writeln!(self.out, "{}", self.style.paint("1", &change.to_string()))?;
                    continue;
                }
                Change::Added { .. } => (Vec::new(), self.vfs.read(path)?),
                Change::Removed { .. } => (ops::read_all(&*self.disk, path)?, Vec::new()),
                Change::Modified { .. } => (ops::read_all(&*self.disk, path)?, self.vfs.read(path)?),
            };
            writeln!(self.out, "{}", self.style.paint("1", &format!("--- a{path}\n+++ b{path}")))?;
            match (std::str::from_utf8(&old), std::str::from_utf8(&new)) {
                (Ok(old), Ok(new)) => match diff::unified(old, new, 3, self.style) {
                    Some(hunks) => write!(self.out, "{hunks}")?,
                    None => writeln!(self.out, "(too large for a line diff: {} -> {} bytes)", old.len(), new.len())?,
                },
                _ => writeln!(self.out, "binary file differs ({} -> {} bytes)", old.len(), new.len())?,
            }
        }
        Ok(())
    }

    fn discard(&mut self) -> CmdResult {
        let n = self.changes()?.len();
        self.overlay.reset()?;
        if !self.vfs.is_dir(&self.cwd) {
            self.cwd = VPath::root();
        }
        writeln!(self.out, "discarded {n} change(s)")?;
        Ok(())
    }
}

/// Every file below `root` (or `root` itself if it is a file), sorted,
/// optionally filtered by extension, skipping [`SKIPPED_DIRS`].
fn files_under(vfs: &Vfs, root: &VPath, ext: Option<&str>) -> CmdResult<Vec<VPath>> {
    if vfs.is_file(root) {
        return Ok(vec![root.clone()]);
    }
    let ext = ext.map(|e| e.trim_start_matches('.'));
    let walk = vfs.walk(root)?.filter_entry(|e| !(e.is_dir() && SKIPPED_DIRS.contains(&e.name.as_str())));
    let mut out = Vec::new();
    for entry in walk {
        let entry = entry?;
        if entry.is_file() && ext.is_none_or(|e| entry.path.extension() == Some(e)) {
            out.push(entry.path);
        }
    }
    Ok(out)
}

fn tree_lines(vfs: &Vfs, dir: &VPath, depth: usize, out: &mut Vec<String>) -> CmdResult {
    for entry in vfs.read_dir(dir)? {
        let indent = "  ".repeat(depth);
        if !entry.is_dir() {
            out.push(format!("{indent}{}", entry.name));
        } else if SKIPPED_DIRS.contains(&entry.name.as_str()) {
            out.push(format!("{indent}{}/ (skipped)", entry.name));
        } else {
            out.push(format!("{indent}{}/", entry.name));
            tree_lines(vfs, &entry.path, depth + 1, out)?;
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)] // display only
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Minimal flag parsing: boolean switches, `--option VALUE`, and `--` to end flags.
#[derive(Default)]
struct Args<'a> {
    pos: Vec<&'a str>,
    switches: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    fn parse(raw: &'a [String], switches: &[&str], options: &[&str]) -> CmdResult<Self> {
        let mut args = Self::default();
        let mut it = raw.iter().map(String::as_str);
        while let Some(arg) = it.next() {
            if arg == "--" {
                args.pos.extend(it.by_ref());
            } else if switches.contains(&arg) {
                args.switches.push(arg);
            } else if options.contains(&arg) {
                let value = it.next().ok_or_else(|| CmdError(format!("{arg} expects a value")))?;
                args.values.push((arg, value));
            } else if arg.len() > 1 && arg.starts_with('-') {
                bail!("unknown flag `{arg}` (use `--` before arguments that start with '-')");
            } else {
                args.pos.push(arg);
            }
        }
        Ok(args)
    }

    fn arity(self, min: usize, max: usize, usage: &str) -> CmdResult<Self> {
        if (min..=max).contains(&self.pos.len()) { Ok(self) } else { bail!("usage: {usage}") }
    }

    fn has(&self, switch: &str) -> bool {
        self.switches.contains(&switch)
    }

    fn value(&self, option: &str) -> Option<&'a str> {
        self.values.iter().rev().find(|(k, _)| *k == option).map(|(_, v)| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("openvfs-sandbox-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(session: &mut Session<Vec<u8>>, src: &str) {
        for stmt in script::parse(src).unwrap() {
            session.exec(&stmt).unwrap_or_else(|e| panic!("`{}` failed: {e}", stmt.join(" ")));
        }
    }

    fn output(session: &Session<Vec<u8>>) -> String {
        String::from_utf8_lossy(session.output()).into_owned()
    }

    fn project() -> TempDir {
        let dir = TempDir::new("proj");
        std::fs::create_dir_all(dir.0.join("src")).unwrap();
        std::fs::write(dir.0.join("src/lib.rs"), "pub fn old_name() {}\n").unwrap();
        std::fs::write(dir.0.join("src/main.rs"), "fn main() { old_name(); }\n").unwrap();
        std::fs::write(dir.0.join("README.md"), "uses old_name\n").unwrap();
        dir
    }

    #[test]
    fn edits_stay_in_memory_until_commit() {
        let dir = project();
        let mut s = Session::open(&dir.0, Vec::new(), false).unwrap();
        run(&mut s, "replace old_name new_name --ext rs; rm README.md; write NOTES.txt hi");

        // Nothing touched the disk yet.
        assert_eq!(std::fs::read_to_string(dir.0.join("src/lib.rs")).unwrap(), "pub fn old_name() {}\n");
        assert!(dir.0.join("README.md").exists());
        assert!(!dir.0.join("NOTES.txt").exists());

        let changes: Vec<String> = s.changes().unwrap().iter().map(ToString::to_string).collect();
        assert_eq!(changes, ["A /NOTES.txt", "D /README.md", "M /src/lib.rs", "M /src/main.rs"]);

        assert_eq!(s.commit().unwrap(), 4);
        assert_eq!(std::fs::read_to_string(dir.0.join("src/lib.rs")).unwrap(), "pub fn new_name() {}\n");
        assert!(!dir.0.join("README.md").exists());
        assert_eq!(std::fs::read_to_string(dir.0.join("NOTES.txt")).unwrap(), "hi");
        assert!(s.changes().unwrap().is_empty());
    }

    #[test]
    fn discard_drops_everything() {
        let dir = project();
        let mut s = Session::open(&dir.0, Vec::new(), false).unwrap();
        run(&mut s, "rm -r src; mkdir build/out; discard");
        assert!(s.changes().unwrap().is_empty());
        assert!(dir.0.join("src/lib.rs").exists());
        assert!(!dir.0.join("build").exists());
    }

    #[test]
    fn diff_shows_line_changes() {
        let dir = project();
        let mut s = Session::open(&dir.0, Vec::new(), false).unwrap();
        run(&mut s, "replace old_name new_name src/lib.rs; diff");
        let out = output(&s);
        assert!(out.contains("--- a/src/lib.rs"), "{out}");
        assert!(out.contains("-pub fn old_name() {}"), "{out}");
        assert!(out.contains("+pub fn new_name() {}"), "{out}");
    }

    #[test]
    fn cd_mv_and_relative_paths() {
        let dir = project();
        let mut s = Session::open(&dir.0, Vec::new(), false).unwrap();
        run(&mut s, "cd src; mv main.rs bin.rs; mkdir ../docs; cp lib.rs ../docs");
        assert_eq!(s.cwd(), "/src");
        let changes: Vec<String> = s.changes().unwrap().iter().map(ToString::to_string).collect();
        assert_eq!(changes, ["A /docs/", "A /docs/lib.rs", "A /src/bin.rs", "D /src/main.rs"]);
    }

    #[test]
    fn bad_commands_report_errors() {
        let dir = project();
        let mut s = Session::open(&dir.0, Vec::new(), false).unwrap();
        assert!(s.exec(&["frobnicate".into()]).is_err());
        assert!(s.exec(&["rm".into(), "src".into()]).is_err()); // needs -r
        assert!(s.exec(&["cat".into(), "missing.txt".into()]).is_err());
        assert!(s.exec(&["cd".into(), "../../..".into()]).is_ok()); // clamps at the sandbox root
        assert_eq!(s.cwd(), "/");
    }
}
