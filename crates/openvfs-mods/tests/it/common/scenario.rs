//! Random *coherent* mod trees for the differential and priority suites.
//!
//! A mod set is **coherent** when no two layers disagree on whether a folded
//! path is a file or a directory — the precondition the index docs state for
//! a well-formed load order (the virtual overlay and the deployment resolve
//! file-vs-dir clashes differently, so the suites compare them only inside
//! the coherent space). Coherence is baked in by generating one *namespace*
//! of folded paths with fixed kinds, then letting every layer spell and
//! include those paths independently — so `Textures/A.dds` in one mod and
//! `textures/a.dds` in another merge exactly like real mod packages.
//!
//! File contents are `"{label}:{folded-path}"` — unique per layer and path,
//! so a served/deployed byte string always names its provider.

use std::collections::BTreeMap;

use openvfs::FileType;

use crate::common::{Entry, Rng, Tree};

/// The directory vocabulary — a deliberately small pool so folded ancestors
/// collide across layers (the interesting merges).
const DIR_NAMES: &[&str] = &["data", "textures", "meshes", "sound", "scripts", "saves", "docs", "plugins"];

/// The file vocabulary — same reason.
const FILE_NAMES: &[&str] =
    &["a.dds", "b.dds", "c.nif", "shared.txt", "readme.txt", "core.dll", "x.esp", "config.ini", "notes.md"];

/// What one folded path is in every layer — file or directory, decided once.
pub type Namespace = BTreeMap<String, FileType>;

/// A folded-path namespace: `n_dirs` directories (depth ≤ 2) and `n_files`
/// files nested under them (or at the root). Every generated dir is a key,
/// so trees built from a namespace always agree on kinds.
pub fn namespace(rng: &mut Rng, n_dirs: usize, n_files: usize) -> Namespace {
    let mut ns = Namespace::new();
    let mut dirs = vec![String::new()]; // "" = root, parent for depth-1 children
    while ns.len() < n_dirs && dirs.len() < 64 {
        let parent = dirs[rng.below(dirs.len())].clone();
        if parent.matches('/').count() >= 2 {
            continue;
        }
        let dir = format!("{parent}/{}", rng.pick(DIR_NAMES));
        if ns.contains_key(&dir) {
            continue;
        }
        ns.insert(dir.clone(), FileType::Dir);
        dirs.push(dir);
    }
    for _ in 0..n_files {
        let dir = &dirs[rng.below(dirs.len())];
        let file = format!("{dir}/{}", rng.pick(FILE_NAMES));
        ns.entry(file).or_insert(FileType::File);
    }
    ns
}

/// The bytes a layer labelled `label` stores at `folded` — the provider's
/// signature for winner assertions.
pub fn content(label: &str, folded: &str) -> Vec<u8> {
    format!("{label}:{folded}").into_bytes()
}

/// `content`, decoded back to a string for assertions.
pub fn content_string(label: &str, folded: &str) -> String {
    String::from_utf8(content(label, folded)).unwrap()
}

/// One layer's randomized view of `ns`: each file present with probability
/// `file_pct`%, every directory of an included subtree present (implied on
/// the host anyway), plus empty dirs with `empty_dir_pct`%. Every path is
/// spelled through per-component case mangling, consistent within the layer,
/// so `/Textures` and `/textures` really do merge.
pub fn layer_tree(rng: &mut Rng, ns: &Namespace, label: &str, file_pct: u64, empty_dir_pct: u64) -> Tree {
    let mut included: BTreeMap<String, FileType> = BTreeMap::new();
    for (folded, kind) in ns {
        match kind {
            FileType::File if rng.chance(file_pct) => {
                included.insert(folded.clone(), FileType::File);
                // Ancestors travel with the file: on any real tree they
                // exist the moment the file does.
                let mut up = folded.as_str();
                while let Some(parent) = up.rfind('/').map(|i| &up[..i]) {
                    if parent.is_empty() {
                        break;
                    }
                    included.entry(parent.to_owned()).or_insert(FileType::Dir);
                    up = parent;
                }
            }
            FileType::Dir if rng.chance(empty_dir_pct) => {
                included.entry(folded.clone()).or_insert(FileType::Dir);
            }
            FileType::File | FileType::Dir => {}
        }
    }
    // Folded-prefix → this layer's spelling of it, so a dir's own entry and
    // every descendant's path agree on casing within the layer.
    let mut spellings: BTreeMap<String, String> = BTreeMap::new();
    let mut spell = |rng: &mut Rng, folded: &str| -> String {
        let mut out = String::new();
        let mut prefix = String::new();
        for comp in folded.trim_start_matches('/').split('/') {
            prefix.push('/');
            prefix.push_str(comp);
            let spelled = spellings.entry(prefix.clone()).or_insert_with(|| mangle(rng, comp));
            out.push('/');
            out.push_str(spelled);
        }
        out
    };
    let mut tree = Tree::new();
    for (folded, kind) in included {
        let spelled = spell(rng, &folded);
        match kind {
            FileType::Dir => tree.insert(spelled, Entry::Dir),
            FileType::File => tree.insert(spelled, Entry::File(content(label, &folded))),
        };
    }
    tree
}

/// A casing variant of `name` — the way real mods disagree on `Textures`
/// vs `textures` vs `TEXTURES` without anyone noticing.
fn mangle(rng: &mut Rng, name: &str) -> String {
    match rng.below(100) {
        0..=14 => name.to_uppercase(),
        15..=34 => {
            let mut chars = name.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => name.to_owned(),
            }
        }
        _ => name.to_owned(),
    }
}
