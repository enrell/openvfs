//! Installing a mod from an archive (or a loose folder) into a mods dir.
//!
//! [`install_mod`] is the step every real mod-manager flow starts with:
//! take what the user downloaded, put its files under `<mods>/<name>/`
//! with the right layout, and hand back the directory to register in a
//! [`Profile`](crate::Profile).
//!
//! * **Formats.** `.zip` is read natively. Everything else (`.7z`, `.rar`,
//!   `.tar.*`, …) is handed to an external extractor found on the system:
//!   `7z`/`7za`/`7zz` first, then `bsdtar` — which ships with Windows 10+ as
//!   `System32\tar.exe` and reads 7z and RAR through libarchive. A plain
//!   directory is copied.
//! * **Safety.** Extraction always happens into a fresh staging directory
//!   inside `mods/`; zip entries that would escape it (absolute paths,
//!   `..`) are rejected, and anything an external tool left behind that is
//!   not a plain file or directory (symlinks, junctions) is removed before
//!   the tree is published. The staging dir is renamed into place only once
//!   the extraction fully succeeded, so a failed install never leaves a
//!   half-written mod behind.
//! * **Layout detection.** Archives very often wrap their content in one
//!   top-level folder (`Cool Mod v1.2/textures/…`), but a lone folder may
//!   just as well *be* the content (`textures/…`). The chain of lone
//!   folders is walked down and the first level whose entries match a
//!   top-level entry of the game dir (`Data/`, `textures/`, `bin/`) becomes
//!   the mod root. With no such match, one wrapper is dropped only if it
//!   looks like the mod's name; otherwise the layout is kept as is.
//!   [`InstallOptions::keep_root`] disables unwrapping altogether.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Knobs for [`install_mod`].
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// The mod's name (its directory under the mods dir). Defaults to the
    /// archive's file name without its extension(s).
    pub name: Option<String>,
    /// Never unwrap a lone top-level directory.
    pub keep_root: bool,
    /// Replace an already installed mod of the same name instead of failing.
    pub replace: bool,
}

/// What [`install_mod`] did.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The mod's name.
    pub name: String,
    /// Where its files now live (`<mods>/<name>`).
    pub dir: PathBuf,
    /// Number of regular files installed.
    pub files: usize,
    /// Wrapper directories that were unwrapped, outermost first.
    pub unwrapped: Vec<String>,
    /// `true` if an existing install of the same name was replaced.
    pub replaced: bool,
}

/// Installs `source` (an archive or a directory) as `<mods_dir>/<name>`.
///
/// `game_dir`, when given, steers layout detection (see the module docs).
pub fn install_mod(
    source: &Path,
    mods_dir: &Path,
    game_dir: Option<&Path>,
    opts: &InstallOptions,
) -> Result<Installed> {
    let name = match &opts.name {
        Some(n) => n.clone(),
        None => default_mod_name(source)
            .ok_or_else(|| install_err(format!("cannot derive a mod name from {}", source.display())))?,
    };
    validate_name(&name)?;
    if !source.exists() {
        return Err(install_err(format!("{} does not exist", source.display())));
    }
    fs::create_dir_all(mods_dir)?;
    let target = mods_dir.join(&name);
    if target.exists() && !opts.replace {
        return Err(install_err(format!("mod {name:?} is already installed at {} (use --replace)", target.display())));
    }

    let staging = mods_dir.join(format!(".installing-{name}"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?; // leftover of an interrupted install
    }
    fs::create_dir_all(&staging)?;
    let result = (|| {
        extract_into(source, &staging)?;
        scrub_links(&staging)?;
        let (root, unwrapped) =
            if opts.keep_root { (staging.clone(), Vec::new()) } else { unwrap_root(&staging, &name, game_dir)? };
        let files = count_files(&root)?;
        if files == 0 {
            return Err(install_err(format!("{} contains no files", source.display())));
        }
        Ok((root, unwrapped, files))
    })();
    let (root, unwrapped, files) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Publish: move the old install aside first so a failed rename can
    // put it back, then rename the (possibly unwrapped) root into place.
    let replaced = target.exists();
    let previous = mods_dir.join(format!(".replaced-{name}"));
    if replaced {
        if previous.exists() {
            fs::remove_dir_all(&previous)?;
        }
        fs::rename(&target, &previous)?;
    }
    if let Err(e) = fs::rename(&root, &target) {
        if replaced {
            let _ = fs::rename(&previous, &target);
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(e.into());
    }
    if replaced {
        let _ = fs::remove_dir_all(&previous);
    }
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging); // the emptied wrapper shells
    }
    log::info!("installed mod {name:?} ({files} files) into {}", target.display());
    Ok(Installed { name, dir: target, files, unwrapped, replaced })
}

/// The mod name an archive implies: its file name minus archive
/// extensions (`Foo-1.2.tar.gz` → `Foo-1.2`, `Bar.7z` → `Bar`).
#[must_use]
pub fn default_mod_name(source: &Path) -> Option<String> {
    let file = source.file_name()?.to_str()?;
    if source.is_dir() {
        return Some(file.to_owned());
    }
    let mut stem = file;
    for _ in 0..2 {
        match stem.rsplit_once('.') {
            Some((s, ext)) if is_archive_ext(ext) && !s.is_empty() => stem = s,
            _ => break,
        }
    }
    Some(stem.to_owned())
}

fn is_archive_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "lzma" | "cab"
    )
}

fn validate_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control);
    if bad { Err(install_err(format!("{name:?} is not a valid mod name"))) } else { Ok(()) }
}

fn install_err(reason: String) -> Error {
    Error::Install { reason }
}

// ---------------------------------------------------------------- extraction

fn extract_into(source: &Path, dest: &Path) -> Result<()> {
    if source.is_dir() {
        return copy_tree(source, dest).map_err(Into::into);
    }
    let ext = source.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    if ext.as_deref() == Some("zip") {
        return extract_zip(source, dest);
    }
    extract_external(source, dest)
}

fn extract_zip(source: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(source)?;
    let mut archive = zip::ZipArchive::new(io::BufReader::new(file))
        .map_err(|e| install_err(format!("{}: not a readable zip: {e}", source.display())))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| install_err(format!("zip entry #{i}: {e}")))?;
        let Some(rel) = entry.enclosed_name().filter(|p| is_contained(p)) else {
            return Err(install_err(format!("zip entry {:?} escapes the mod directory", entry.name())));
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
        } else if entry.is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut w = fs::File::create(&out)?;
            io::copy(&mut entry, &mut w).map_err(|e| install_err(format!("zip entry {:?}: {e}", entry.name())))?;
        }
        // Symlink entries are skipped: a mod is plain files.
    }
    Ok(())
}

/// Only normal components: no root, prefix, `.` or `..`.
fn is_contained(rel: &Path) -> bool {
    rel.components().all(|c| matches!(c, Component::Normal(_)))
}

/// External extractors, in preference order, with the argv that extracts
/// `archive` into `dest`.
fn extractor_candidates(archive: &Path, dest: &Path) -> Vec<(PathBuf, Vec<std::ffi::OsString>)> {
    let seven = |exe: &str| {
        let mut out = std::ffi::OsString::from("-o");
        out.push(dest);
        (PathBuf::from(exe), vec!["x".into(), "-y".into(), "-bd".into(), out, archive.into()])
    };
    let tar = |exe: PathBuf| (exe, vec!["-x".into(), "-f".into(), archive.into(), "-C".into(), dest.into()]);
    let mut list = vec![seven("7z"), seven("7za"), seven("7zz")];
    if cfg!(windows) {
        list.push(seven(r"C:\Program Files\7-Zip\7z.exe"));
        // Windows' bundled tar is bsdtar (libarchive): reads 7z and RAR too.
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        list.push(tar(Path::new(&root).join("System32").join("tar.exe")));
    }
    list.push(tar(PathBuf::from("bsdtar")));
    list
}

fn extract_external(source: &Path, dest: &Path) -> Result<()> {
    let source = std::path::absolute(source)?;
    let mut tried = Vec::new();
    for (exe, args) in extractor_candidates(&source, dest) {
        match Command::new(&exe).args(&args).output() {
            Err(_) => tried.push(format!("{} (not found)", exe.display())),
            Ok(out) if out.status.success() => {
                log::debug!("extracted {} with {}", source.display(), exe.display());
                return Ok(());
            }
            Ok(out) => {
                // The tool exists but refused the archive: that's the answer,
                // don't keep shopping for one that might half-extract it.
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(install_err(format!(
                    "{} failed on {}: {}",
                    exe.display(),
                    source.display(),
                    stderr.trim()
                )));
            }
        }
    }
    Err(install_err(format!(
        "no extractor for {} — install 7-Zip (or bsdtar); tried: {}",
        source.display(),
        tried.join(", ")
    )))
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Removes everything that is not a plain file or directory (symlinks,
/// junctions) — an archive must not plant links pointing out of the mod.
fn scrub_links(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) {
            log::warn!("removing link {} from extracted mod", path.display());
            if meta.is_dir() { fs::remove_dir(&path)? } else { fs::remove_file(&path)? }
        } else if meta.is_dir() {
            scrub_links(&path)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

// ---------------------------------------------------------------- layout

/// Picks the directory to install from (see the module docs): the chain of
/// lone wrapper directories is walked, and the first level whose entries
/// match the game's top level wins; failing that, a single wrapper that
/// looks like the mod's own name is dropped; otherwise nothing is.
fn unwrap_root(staging: &Path, mod_name: &str, game_dir: Option<&Path>) -> Result<(PathBuf, Vec<String>)> {
    let game_top: Vec<String> = game_dir
        .and_then(|g| fs::read_dir(g).ok())
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
        .unwrap_or_default();
    // chain[i] = (dir at depth i, its entries' lowercased names); every
    // level but the last holds exactly one directory.
    let mut chain: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut dir = staging.to_path_buf();
    loop {
        let entries: Vec<fs::DirEntry> = fs::read_dir(&dir)?.collect::<io::Result<_>>()?;
        let names = entries.iter().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect();
        let next = match entries.as_slice() {
            [only] if only.file_type()?.is_dir() => Some(only.path()),
            _ => None,
        };
        chain.push((dir, names));
        match next {
            Some(n) => dir = n,
            None => break,
        }
    }
    let depth = chain
        .iter()
        .position(|(_, names)| names.iter().any(|n| game_top.contains(n)))
        .unwrap_or_else(|| usize::from(chain.len() > 1 && resembles(&chain[0].1[0], mod_name)));
    let unwrapped = chain[1..=depth]
        .iter()
        .map(|(p, _)| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .collect();
    Ok((chain.swap_remove(depth).0, unwrapped))
}

/// `true` if a wrapper folder name looks like the mod's name: their
/// lowercase alphanumerics share a prefix of at least five characters (or
/// the shorter one entirely) — `Cool Mod v1.2` ~ `Cool Mod-1.2`.
fn resembles(folder: &str, mod_name: &str) -> bool {
    let norm =
        |s: &str| s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
    let (a, b) = (norm(folder), norm(mod_name));
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    common > 0 && common >= a.len().min(b.len()).min(5)
}

fn count_files(dir: &Path) -> io::Result<usize> {
    let mut n = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            n += count_files(&entry.path())?;
        } else if ft.is_file() {
            n += 1;
        }
    }
    Ok(n)
}
