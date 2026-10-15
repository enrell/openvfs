//! Random file-system operations and their canonical outcomes.
//!
//! The differential suite applies the same random history to a reference
//! [`MemoryFs`] and to every other backend; the canonical outcome strings
//! (results *and* error variants with their paths) must match exactly.

use std::sync::Arc;

use openvfs::{MemoryFs, Vfs, VfsError};

use super::{Rng, Tree, snapshot};

/// A small name alphabet maximizes collisions (same path touched repeatedly).
/// `ç` keeps non-ASCII names in every history.
const NAMES: [&str; 3] = ["a", "b", "ç"];

#[derive(Debug, Clone)]
pub enum Op {
    Write(String, Vec<u8>),
    WriteAtomic(String, Vec<u8>),
    Append(String, Vec<u8>),
    SetLen(String, u64),
    CreateDir(String),
    CreateDirAll(String),
    RemoveFile(String),
    RemoveDir(String),
    RemoveDirAll(String),
    Rename(String, String),
    Copy(String, String),
    CopyTree(String, String),
    Read(String),
    ReadDir(String),
    Stat(String),
}

impl Op {
    pub fn is_mutation(&self) -> bool {
        !matches!(self, Self::Read(_) | Self::ReadDir(_) | Self::Stat(_))
    }
}

pub fn random_path(rng: &mut Rng) -> String {
    if rng.chance(4) {
        return "/".to_owned();
    }
    let depth = 1 + rng.below(3);
    let mut path = String::new();
    for _ in 0..depth {
        path.push('/');
        let name: &&str = rng.pick(&NAMES);
        path.push_str(name);
    }
    path
}

pub fn random_op(rng: &mut Rng) -> Op {
    let p = random_path(rng);
    match rng.below(100) {
        0..14 => Op::Write(p, rng.bytes(40)),
        14..19 => Op::WriteAtomic(p, rng.bytes(40)),
        19..24 => Op::Append(p, rng.bytes(16)),
        24..28 => Op::SetLen(p, rng.below(48) as u64),
        28..38 => Op::CreateDir(p),
        38..46 => Op::CreateDirAll(p),
        46..53 => Op::RemoveFile(p),
        53..58 => Op::RemoveDir(p),
        58..62 => Op::RemoveDirAll(p),
        62..72 => Op::Rename(p, random_path(rng)),
        72..78 => Op::Copy(p, random_path(rng)),
        78..81 => Op::CopyTree(p, random_path(rng)),
        81..88 => Op::Read(p),
        88..94 => Op::ReadDir(p),
        _ => Op::Stat(p),
    }
}

/// Variant name plus reported path: the parts of an error every backend must agree on.
pub fn describe(err: &VfsError) -> String {
    let debug = format!("{err:?}");
    let variant: String = debug.chars().take_while(char::is_ascii_alphanumeric).collect();
    match err.path() {
        Some(path) => format!("{variant}({path})"),
        None => variant,
    }
}

fn outcome<T>(result: openvfs::Result<T>, ok: impl FnOnce(T) -> String) -> String {
    match result {
        Ok(value) => ok(value),
        Err(e) => format!("Err {}", describe(&e)),
    }
}

/// Applies `op` and renders the result canonically.
pub fn apply(vfs: &Vfs, op: &Op) -> String {
    let unit = |()| "Ok".to_owned();
    match op {
        Op::Write(p, data) => outcome(vfs.write(p, data), unit),
        Op::WriteAtomic(p, data) => outcome(vfs.write_atomic(p, data), unit),
        Op::Append(p, data) => outcome(vfs.append(p, data), unit),
        Op::SetLen(p, len) => outcome(vfs.set_len(p, *len), unit),
        Op::CreateDir(p) => outcome(vfs.create_dir(p), unit),
        Op::CreateDirAll(p) => outcome(vfs.create_dir_all(p), unit),
        Op::RemoveFile(p) => outcome(vfs.remove_file(p), unit),
        Op::RemoveDir(p) => outcome(vfs.remove_dir(p), unit),
        Op::RemoveDirAll(p) => outcome(vfs.remove_dir_all(p), unit),
        Op::Rename(a, b) => outcome(vfs.rename(a, b), unit),
        Op::Copy(a, b) => outcome(vfs.copy(a, b), |n| format!("Ok {n} bytes")),
        Op::CopyTree(a, b) => outcome(vfs.copy_dir_all(a, b), unit),
        Op::Read(p) => outcome(vfs.read(p), |bytes| format!("Ok {bytes:?}")),
        Op::ReadDir(p) => outcome(vfs.read_dir(p), |entries| {
            let listing: Vec<String> =
                entries.iter().map(|e| if e.is_dir() { format!("{}/", e.name) } else { e.name.clone() }).collect();
            format!("Ok {listing:?}")
        }),
        Op::Stat(p) => outcome(vfs.metadata(p), |m| format!("Ok {:?} {}", m.file_type, m.len)),
    }
}

/// A random but valid starting tree, built by running random mutations.
pub fn random_tree(rng: &mut Rng, steps: usize) -> Tree {
    let vfs = Vfs::from(Arc::new(MemoryFs::new()));
    for _ in 0..steps {
        let op = random_op(rng);
        if op.is_mutation() {
            let _ = apply(&vfs, &op);
        }
    }
    snapshot(&**vfs.backend())
}
