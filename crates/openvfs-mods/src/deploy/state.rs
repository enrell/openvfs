//! Deployment bookkeeping: the on-disk [`Manifest`], the write-ahead
//! [`Journal`], and the host-side primitives both rely on.
//!
//! Layout of the *state directory* a [`crate::Deployer`] works in (kept
//! **outside** the game directory — see the module docs of [`crate::deploy`]):
//!
//! ```text
//! <state_dir>/
//!   journal.jsonl   — write-ahead log, one JSON record per line
//!   manifest.json   — what a completed deployment put where
//!   backup/         — originals displaced by deployed files, rel paths kept
//!     stray/          unexpected files displaced from deployed paths
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use openvfs::{FileType, VPath};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::plan::{DeployOp, DeploymentPlan};
use crate::error::{Error, Result};

/// Journal file inside the state directory: one JSON record per line.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Manifest file inside the state directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Directory inside the state directory that receives displaced originals.
pub const BACKUP_DIR: &str = "backup";

/// Sub-directory of [`BACKUP_DIR`] for files displaced from deployed paths
/// whose provenance is unknown (the real original was already backed up).
pub const STRAY_DIR: &str = "stray";

/// How a deployed file was materialized in the game directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployMethod {
    /// A hardlink: the game directory entry shares the mod file's inode.
    /// Zero-copy, but a game writing *in place* mutates the mod file itself —
    /// see the [`crate::deploy`] module docs.
    Link,
    /// An independent copy of the mod file's bytes.
    Copy,
}

/// What a [`ManifestEntry`] is: a file the deployment placed, or a
/// pre-existing game file it recognized and left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Placed by the deployment (hardlink or copy of a mod file).
    Deployed,
    /// The game's own file, already at the right place — recorded so
    /// [`crate::Deployer::verify`] and [`crate::Deployer::capture`] can tell
    /// "known game file" from "foreign write".
    Base,
}

/// One file expected in the deployed game directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Path relative to the game directory, in the spelling the file was
    /// deployed with, rooted at `/` (e.g. `"/textures/a.dds"`).
    pub path: String,
    /// `path` folded (see [`VPath::folded`]) — the identity scans compare.
    pub folded: String,
    /// Deployed-by-us, or a pre-existing game file.
    pub kind: EntryKind,
    /// Link or copy — [`EntryKind::Deployed`] entries only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<DeployMethod>,
    /// Hex SHA-256 of the file's bytes at deploy time.
    pub sha256: String,
    /// Byte length at deploy time.
    pub size: u64,
    /// The mod file this entry links or copies (`Deployed` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// The displaced original, relative to the state dir's `backup/`
    /// (`Deployed` entries that replaced something at the path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// A foreign file the deployment was told to keep
/// ([`ForeignPolicy::Keep`](super::ForeignPolicy::Keep)): recorded so that
/// [`crate::Deployer::capture`] does not mistake it for a fresh game write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignFile {
    /// Path relative to the game directory, real spelling, rooted at `/`.
    pub path: String,
    /// `path` folded.
    pub folded: String,
    /// Hex SHA-256 of the file's bytes when the deployment ran.
    pub sha256: String,
    /// Byte length when the deployment ran.
    pub size: u64,
}

/// The deployment record: everything [`crate::Deployer::undeploy`],
/// [`crate::Deployer::verify`] and [`crate::Deployer::capture`] need to know
/// about a finished deployment, without consulting the profile again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version; currently `1`.
    pub version: u32,
    /// The game directory this manifest describes.
    pub game_dir: PathBuf,
    /// The overwrite directory the profile uses, when the plan knew it —
    /// lets capture default its target and every scan skip it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite_dir: Option<PathBuf>,
    /// Unix timestamp (seconds) of when the deployment completed.
    pub created: u64,
    /// Every expected file, sorted by folded path.
    pub entries: Vec<ManifestEntry>,
    /// Directories the deployment created (rel paths, parents first) —
    /// undeploy removes exactly these, never pre-existing ones.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Foreign files present at deploy time and kept
    /// ([`ForeignPolicy::Keep`](super::ForeignPolicy::Keep)).
    #[serde(default)]
    pub foreign: Vec<ForeignFile>,
}

impl Manifest {
    /// The manifest's own file inside `state_dir`.
    #[must_use]
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(MANIFEST_FILE)
    }

    /// Reads the manifest, or `None` when no deployment has completed yet.
    pub fn load(state_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(state_dir);
        let json = match fs::read_to_string(&path) {
            Ok(json) => json,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let manifest: Self = serde_json::from_str(&json)?;
        Ok(Some(manifest))
    }

    /// Writes the manifest atomically: a sibling temporary file is synced and
    /// renamed over `manifest.json`, so readers never see a torn record.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        fs::create_dir_all(state_dir)?;
        let path = Self::path(state_dir);
        let tmp = path.with_extension("json.tmp");
        log::debug!("manifest: writing {} entries to {}", self.entries.len(), path.display());
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path).or_else(|e| {
            // Some platforms refuse to rename over an existing file.
            if e.kind() == io::ErrorKind::AlreadyExists || path.exists() {
                fs::remove_file(&path).and_then(|()| fs::rename(&tmp, &path))
            } else {
                Err(e)
            }
        })?;
        Ok(())
    }

    /// Deletes the manifest; `false` when there was none.
    pub(crate) fn remove(state_dir: &Path) -> bool {
        fs::remove_file(Self::path(state_dir)).is_ok()
    }

    /// Every [`EntryKind::Deployed`] entry.
    pub fn deployed(&self) -> impl Iterator<Item = &ManifestEntry> {
        self.entries.iter().filter(|e| e.kind == EntryKind::Deployed)
    }

    /// The entry covering `folded` (a `/`-rooted, already folded path).
    #[must_use]
    pub fn entry(&self, folded: &str) -> Option<&ManifestEntry> {
        self.entries.iter().find(|e| e.folded == folded)
    }

    /// Where displaced originals live.
    #[must_use]
    pub fn backup_root(state_dir: &Path) -> PathBuf {
        state_dir.join(BACKUP_DIR)
    }
}

/// One line of `journal.jsonl`.
///
/// A deployment writes the `Plan` record first (so a resume knows every step,
/// not just the ones already journaled), then brackets each step with
/// `Pending` — written and synced *before* the op runs — and `Done` after:
///
/// ```json
/// {"status":"plan","plan":{ … }}
/// {"status":"pending","seq":0,"op":{ … }}
/// {"status":"done","seq":0}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum JournalRecord {
    /// The whole plan, written before any step — resume's source of truth.
    Plan {
        /// The plan being applied.
        plan: DeploymentPlan,
    },
    /// Step `seq` is about to run.
    Pending {
        /// Step index into the plan's op list.
        seq: u64,
        /// The op being executed (kept in the record so rollback can invert
        /// even a journal whose plan record was never written).
        op: DeployOp,
    },
    /// Step `seq` finished.
    Done {
        /// Step index into the plan's op list.
        seq: u64,
    },
}

/// The write-ahead log of an in-flight deployment.
///
/// Writers [`Journal::append`] a record and fsync before touching the disk,
/// so a crash can only ever lose the *tail*: readers tolerate a truncated
/// last line. See the [`crate::deploy`] docs for the recovery model.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    file: fs::File,
}

/// What a journal says about a deployment in flight.
#[derive(Debug, Default)]
pub(crate) struct JournalState {
    /// The plan record, if one was written.
    pub(crate) plan: Option<DeploymentPlan>,
    /// `seq` → `(op, done?)`, in journaled order.
    pub(crate) steps: BTreeMap<u64, (Option<DeployOp>, bool)>,
}

impl Journal {
    /// The journal's path inside `state_dir`.
    #[must_use]
    pub fn path(state_dir: &Path) -> PathBuf {
        state_dir.join(JOURNAL_FILE)
    }

    /// `true` while a journal file exists — an in-flight or finished apply.
    #[must_use]
    pub fn exists(state_dir: &Path) -> bool {
        Self::path(state_dir).is_file()
    }

    /// Truncates/creates the journal in `state_dir` (created as needed).
    pub fn open(state_dir: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        let path = Self::path(state_dir);
        let file = fs::File::create(&path)?;
        Ok(Self { path, file })
    }

    /// Opens the existing journal for appending, keeping every record —
    /// what [`crate::Deployer::resume`] uses to continue a WAL.
    pub(crate) fn open_append(state_dir: &Path) -> Result<Self> {
        let path = Self::path(state_dir);
        let file = fs::OpenOptions::new().append(true).open(&path)?;
        Ok(Self { path, file })
    }

    /// The journal's path on disk.
    #[must_use]
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Appends one record, then fsyncs — the "write-ahead" half of the WAL.
    pub fn append(&mut self, record: &JournalRecord) -> Result<()> {
        match record {
            JournalRecord::Plan { plan } => log::trace!("journal: plan ops={}", plan.ops.len()),
            JournalRecord::Pending { seq, .. } => log::trace!("journal: pending seq={seq}"),
            JournalRecord::Done { seq } => log::trace!("journal: done seq={seq}"),
        }
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Parses the journal. A torn final line (a crash mid-write) is dropped;
    /// a malformed record anywhere else is an [`Error::Deploy`].
    pub(crate) fn read(state_dir: &Path) -> Result<JournalState> {
        let path = Self::path(state_dir);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(JournalState::default()),
            Err(e) => return Err(e.into()),
        };
        let mut state = JournalState::default();
        let reader = io::BufReader::new(file);
        let mut lines = reader.lines();
        while let Some(line) = lines.next() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalRecord>(&line) {
                Ok(JournalRecord::Plan { plan }) => state.plan = Some(plan),
                Ok(JournalRecord::Pending { seq, op }) => {
                    state.steps.insert(seq, (Some(op), false));
                }
                Ok(JournalRecord::Done { seq }) => {
                    state.steps.entry(seq).or_insert((None, true)).1 = true;
                }
                Err(e) => {
                    // Only the very last record may be torn (crash mid-append).
                    if lines.next().is_none() {
                        log::warn!("journal: dropping torn last record in {}", path.display());
                        break;
                    }
                    return Err(Error::Deploy { reason: format!("corrupt journal record in {}: {e}", path.display()) });
                }
            }
        }
        Ok(state)
    }

    /// Deletes the journal; `false` when there was none.
    pub(crate) fn remove(state_dir: &Path) -> bool {
        fs::remove_file(Self::path(state_dir)).is_ok()
    }
}

/// Streams SHA-256 over a file (64 KiB at a time), returning the hex digest
/// and the byte length.
pub(crate) fn sha256_file(path: &Path) -> io::Result<(u64, String)> {
    use std::fmt::Write as _;

    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024].into_boxed_slice();
    let mut size = 0u64;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                size += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok((size, hex))
}

/// `true` when `a` and `b` are the same filesystem object — a hardlink pair —
/// decided by device+inode, never by content.
///
/// Windows has no *stable* std API for file identity (`file_index` is still
/// behind `windows_by_handle`), so callers must not rely on this alone: the
/// deploy code infers "already linked" from manifest hashes instead.
#[cfg(unix)]
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else { return false };
    ma.dev() == mb.dev() && ma.ino() == mb.ino()
}

/// Conservative fallback where no stable file-identity metadata exists:
/// two paths are never proven equal, so callers treat every file as a
/// separate object (a hash check then decides equality of *content*).
#[cfg(not(unix))]
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    let _ = (a, b);
    false
}

/// `true` for the "different volumes" error: `ERROR_NOT_SAME_DEVICE` (17) on
/// Windows, `EXDEV` (18) on Unix.
fn is_cross_device(e: &io::Error) -> bool {
    #[cfg(windows)]
    let code = 17; // ERROR_NOT_SAME_DEVICE
    #[cfg(not(windows))]
    let code = 18; // EXDEV
    e.raw_os_error() == Some(code)
}

/// Removes `path`, whatever it is (file, link, or a whole directory).
pub(crate) fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copies `from` to `to` recursively (directories merge; files replace).
fn copy_any(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_any(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// Moves `from` to `to`, replacing `to` if needed. Renames when both paths
/// share a volume — the common case for backups inside the state dir, which
/// keeps the original inode and costs nothing — and falls back to
/// copy-and-delete across volumes (`ERROR_NOT_SAME_DEVICE`/`EXDEV`).
///
/// Idempotent: a missing `from` with `to` already in place counts as done.
pub(crate) fn move_any(from: &Path, to: &Path) -> io::Result<()> {
    let from_meta = match fs::symlink_metadata(from) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return if to.exists() { Ok(()) } else { Err(e) };
        }
        Err(e) => return Err(e),
    };
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(to).is_ok() {
        remove_any(to)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) || e.kind() == io::ErrorKind::PermissionDenied => {
            // Cross-volume (or a filesystem refusing rename): copy the tree,
            // then delete the original — order keeps `to` complete before
            // `from` disappears.
            copy_any(from, to)?;
            if from_meta.is_dir() { fs::remove_dir_all(from) } else { fs::remove_file(from) }
        }
        Err(e) => Err(e),
    }
}

/// One host file or directory found by [`scan_dir`].
#[derive(Debug, Clone)]
pub(crate) struct ScannedNode {
    /// The real, original-cased path relative to the scanned root.
    pub(crate) path: VPath,
    /// File or directory.
    pub(crate) kind: FileType,
}

/// Recursively scans `root`, returning every file and directory keyed by its
/// *folded* root-relative path — the same key space [`crate::MergedIndex`]
/// uses, so "what the game dir has" diffs directly against "what the merged
/// tree wants".
///
/// * `exclude` lists directories (canonical host paths) to skip entirely —
///   the state dir and overwrite dir when a caller nested them inside the
///   game dir anyway. Matching subtrees produce no nodes at all.
/// * Symlinks and other non-file/non-dir entries are skipped: the deployment
///   never creates them and never follows them.
/// * Folded-case twins in one directory cannot both be keyed; the first
///   listed wins and the twin is invisible — a degenerate host layout no
///   virtual layer could represent anyway.
pub(crate) fn scan_dir(root: &Path, exclude: &[PathBuf]) -> Result<BTreeMap<String, ScannedNode>> {
    let root = root.canonicalize()?;
    let mut skip: Vec<VPath> = Vec::new();
    for dir in exclude {
        if let Ok(canon) = dir.canonicalize()
            && let Ok(rel) = canon.strip_prefix(&root)
        {
            skip.push(vpath_of(rel)?);
        }
    }
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.clone(), VPath::root())];
    while let Some((host, vpath)) = stack.pop() {
        for entry in fs::read_dir(&host)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
            // Same rule as `PhysicalFs`: a backslash in a name cannot be a
            // virtual-path component.
            if name.contains('\\') {
                continue;
            }
            let Ok(child) = vpath.join(&name) else { continue };
            let folded = child.as_str().to_lowercase();
            if skip.iter().any(|s| child == *s || child.starts_with(s)) {
                continue; // state dir or overwrite dir living inside the game dir
            }
            if ft.is_dir() {
                stack.push((entry.path(), child.clone()));
                out.entry(folded).or_insert(ScannedNode { path: child, kind: FileType::Dir });
            } else if ft.is_file() {
                out.entry(folded).or_insert(ScannedNode { path: child, kind: FileType::File });
            }
        }
    }
    Ok(out)
}

/// Maps a host-relative path (from `strip_prefix`) to a `/`-rooted `VPath`.
fn vpath_of(rel: &Path) -> Result<VPath> {
    let mut v = VPath::root();
    for comp in rel.components() {
        if let std::path::Component::Normal(name) = comp {
            let name = name
                .to_str()
                .ok_or_else(|| Error::Deploy { reason: format!("non-UTF-8 name under {}", rel.display()) })?;
            v = v.join(name)?;
        }
    }
    Ok(v)
}

/// The host path `vpath` (a `/`-rooted relative path) denotes under `root`.
pub(crate) fn host_path(root: &Path, vpath: &VPath) -> PathBuf {
    let mut host = root.to_path_buf();
    for component in vpath.components() {
        host.push(component);
    }
    host
}

/// [`host_path`] for a rel path stored as a string (manifest paths).
pub(crate) fn host_path_str(root: &Path, rel: &str) -> PathBuf {
    let mut host = root.to_path_buf();
    for component in rel.trim_start_matches('/').split('/') {
        if !component.is_empty() {
            host.push(component);
        }
    }
    host
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_any_is_idempotent_and_replaces() {
        let dir = std::env::temp_dir().join(format!("openvfs-moveany-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::write(&a, b"one").unwrap();
        move_any(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());
        move_any(&a, &b).unwrap(); // already moved: still fine
        // Destination replaced.
        fs::write(&a, b"two").unwrap();
        move_any(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"two");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_dir_folds_and_excludes() {
        let dir = std::env::temp_dir().join(format!("openvfs-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Data/Sub")).unwrap();
        fs::create_dir_all(dir.join("state/backup")).unwrap();
        fs::write(dir.join("Data/Sub/A.txt"), b"a").unwrap();
        fs::write(dir.join("state/journal.jsonl"), b"j").unwrap();
        let found = scan_dir(&dir, &[dir.join("state")]).unwrap();
        assert!(found.contains_key("/data/sub/a.txt"));
        assert!(found.contains_key("/data/sub"));
        assert_eq!(found["/data/sub"].path.as_str(), "/Data/Sub");
        assert!(!found.keys().any(|k| k.contains("state")), "state dir excluded");
        let _ = fs::remove_dir_all(&dir);
    }
}
