//! End-to-end tests of the `openvfs-sandbox` binary: exit codes, dry-run
//! safety, `--apply`, scripts, the interactive shell and output hygiene.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Project(PathBuf);

impl Project {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("openvfs-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn fetch_user() {}\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() { fetch_user(); }\n").unwrap();
        std::fs::write(dir.join("README.md"), "call fetch_user\n").unwrap();
        std::fs::write(dir.join(".git/HEAD"), "fetch_user\n").unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap()
    }

    /// Every file under the project, with contents — to prove "untouched".
    fn fingerprint(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    visit(&path, out);
                } else {
                    out.push((path.clone(), std::fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        visit(&self.0, &mut out);
        out
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sandbox(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_openvfs-sandbox"))
        .args(args)
        .env_remove("NO_COLOR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    pipe.write_all(stdin.unwrap_or("").as_bytes()).unwrap();
    drop(pipe);
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_and_usage_errors() {
    let help = sandbox(&["--help"], None);
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("usage: openvfs-sandbox"));
    for bad in [&[][..], &["--bogus"], &["a", "b"], &["dir", "-c"], &["dir", "-c", "x", "-f", "y"]] {
        let out = sandbox(bad, None);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", text(&out.stderr));
    }
}

#[test]
fn missing_directory_is_a_runtime_error() {
    let out = sandbox(&["/definitely/not/here", "-c", "ls"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("cannot open"));
}

#[test]
fn dry_run_reports_changes_and_touches_nothing() {
    let project = Project::new("dry");
    let before = project.fingerprint();
    let dir = project.path().to_str().unwrap();
    let out =
        sandbox(&[dir, "-c", "replace fetch_user load_user --ext rs; rm README.md; write NEW.txt hi; diff"], None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("-pub fn fetch_user() {}") && stdout.contains("+pub fn load_user() {}"), "{stdout}");
    assert!(stdout.contains("dry run: 4 change(s) NOT written"), "{stdout}");
    assert!(!stdout.contains("\x1b["), "no ANSI escapes when piped");
    assert_eq!(project.fingerprint(), before, "a dry run modified the directory");
}

#[test]
fn apply_writes_exactly_the_reported_changes() {
    let project = Project::new("apply");
    let dir = project.path().to_str().unwrap();
    let out =
        sandbox(&[dir, "--apply", "-c", "replace fetch_user load_user --ext rs; rm README.md; write NEW.txt hi"], None);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("committed 4 change(s)"));
    assert_eq!(project.read("src/lib.rs"), "pub fn load_user() {}\n");
    assert_eq!(project.read("src/main.rs"), "fn main() { load_user(); }\n");
    assert_eq!(project.read("NEW.txt"), "hi");
    assert!(!project.path().join("README.md").exists());
    assert_eq!(project.read(".git/HEAD"), "fetch_user\n", ".git must be skipped by recursive commands");
}

#[test]
fn a_failing_command_aborts_without_writing() {
    let project = Project::new("abort");
    let before = project.fingerprint();
    let dir = project.path().to_str().unwrap();
    let out = sandbox(&[dir, "--apply", "-c", "rm -r src; cat does-not-exist.txt; write never.txt x"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("error in `cat does-not-exist.txt`"));
    assert_eq!(project.fingerprint(), before, "an aborted run must not write anything, even with --apply");
}

#[test]
fn script_files_with_comments_and_explicit_commit() {
    let project = Project::new("script");
    let script = project.path().join("..").join(format!("openvfs-cli-script-{}.txt", std::process::id()));
    std::fs::write(&script, "# rename the function\nreplace fetch_user get_user src --ext rs\ncommit\n").unwrap();
    let out = sandbox(&[project.path().to_str().unwrap(), "-f", script.to_str().unwrap()], None);
    let _ = std::fs::remove_file(&script);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(project.read("src/lib.rs"), "pub fn get_user() {}\n", "`commit` inside a script writes");
    assert_eq!(project.read("README.md"), "call fetch_user\n", "the path argument limited the replace");
}

#[test]
fn parse_errors_exit_with_usage_code() {
    let project = Project::new("parse");
    let out = sandbox(&[project.path().to_str().unwrap(), "-c", "write f 'unterminated"], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("unterminated"));
}

#[test]
fn interactive_session_discards_on_exit() {
    let project = Project::new("repl");
    let before = project.fingerprint();
    let session = "mkdir docs\ncd docs\nwrite guide.md hello\npwd\nstatus\nbogus-command\nexit\n";
    let out = sandbox(&[project.path().to_str().unwrap()], Some(session));
    assert!(out.status.success());
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(stdout.contains("sandbox:/docs*>"), "the prompt shows cwd and the dirty marker: {stdout}");
    assert!(stdout.contains("A /docs/guide.md"), "{stdout}");
    assert!(stderr.contains("unknown command `bogus-command`"), "errors don't end the session: {stderr}");
    assert!(stderr.contains("uncommitted change(s) discarded"), "{stderr}");
    assert_eq!(project.fingerprint(), before);
}

#[test]
fn interactive_commit_writes() {
    let project = Project::new("repl-commit");
    let out = sandbox(&[project.path().to_str().unwrap()], Some("write a.txt one\ncommit\n"));
    assert!(out.status.success());
    assert_eq!(project.read("a.txt"), "one");
}

#[test]
fn apply_without_batch_is_rejected() {
    let project = Project::new("apply-only");
    let out = sandbox(&[project.path().to_str().unwrap(), "--apply"], None);
    assert_eq!(out.status.code(), Some(2));
}
