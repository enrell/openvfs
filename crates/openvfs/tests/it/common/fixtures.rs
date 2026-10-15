//! Every backend configuration the suites run against.
//!
//! Each builder receives the initial contents and returns a file system that
//! shows exactly those contents — however it is assembled underneath.

use std::any::Any;
use std::sync::Arc;

use openvfs::{
    DirEntry, FileSystem, MemoryFs, Metadata, MountFs, OpenOptions, OverlayFs, PhysicalFs, Symlinks, VPath, VfsFile,
};

use super::{Entry, TempDir, Tree, populate};

/// A file system under test, plus what must stay alive while it is used.
pub struct Fixture {
    pub fs: Arc<dyn FileSystem>,
    /// Lower layers and the contents they must keep (overlays never write them).
    pub immutable: Vec<(Arc<dyn FileSystem>, Tree)>,
    /// Kept only so their `Drop` runs after the file system is gone.
    #[allow(dead_code)]
    guards: Vec<Box<dyn Any>>,
}

impl Fixture {
    fn plain(fs: Arc<dyn FileSystem>) -> Self {
        Self { fs, immutable: Vec::new(), guards: Vec::new() }
    }

    fn guard(mut self, guard: impl Any) -> Self {
        self.guards.push(Box::new(guard));
        self
    }
}

pub type Builder = fn(&Tree) -> Fixture;

fn mem_with(initial: &Tree) -> Arc<dyn FileSystem> {
    let fs: Arc<dyn FileSystem> = Arc::new(MemoryFs::new());
    populate(&*fs, initial);
    fs
}

fn disk_with(initial: &Tree, symlinks: Symlinks) -> (Arc<dyn FileSystem>, TempDir) {
    let dir = TempDir::new("fixture");
    let fs: Arc<dyn FileSystem> = Arc::new(PhysicalFs::builder(dir.path()).symlinks(symlinks).build().unwrap());
    populate(&*fs, initial);
    (fs, dir)
}

/// Plain in-memory storage.
pub fn memory(initial: &Tree) -> Fixture {
    Fixture::plain(mem_with(initial))
}

/// A host directory, default (deny) symlink policy.
pub fn physical(initial: &Tree) -> Fixture {
    let (fs, dir) = disk_with(initial, Symlinks::Deny);
    Fixture::plain(fs).guard(dir)
}

/// A host directory that follows in-root symlinks (different code path).
pub fn physical_follow(initial: &Tree) -> Fixture {
    let (fs, dir) = disk_with(initial, Symlinks::FollowWithinRoot);
    Fixture::plain(fs).guard(dir)
}

fn overlay(upper: Arc<dyn FileSystem>, lowers: Vec<(Arc<dyn FileSystem>, Tree)>) -> Fixture {
    let fs = Arc::new(OverlayFs::with_layers(upper, lowers.iter().map(|(fs, _)| Arc::clone(fs)).collect()));
    Fixture { fs, immutable: lowers, guards: Vec::new() }
}

/// Memory upper over a memory lower holding the contents.
pub fn overlay_memory(initial: &Tree) -> Fixture {
    overlay(Arc::new(MemoryFs::new()), vec![(mem_with(initial), initial.clone())])
}

/// Memory upper over a host directory holding the contents.
pub fn overlay_on_disk(initial: &Tree) -> Fixture {
    let (lower, dir) = disk_with(initial, Symlinks::Deny);
    overlay(Arc::new(MemoryFs::new()), vec![(lower, initial.clone())]).guard(dir)
}

/// A *host directory* as the upper layer, over a memory lower.
pub fn overlay_disk_upper(initial: &Tree) -> Fixture {
    let dir = TempDir::new("fixture-upper");
    let upper: Arc<dyn FileSystem> = Arc::new(PhysicalFs::new(dir.path()).unwrap());
    overlay(upper, vec![(mem_with(initial), initial.clone())]).guard(dir)
}

/// Contents split across two lowers. The low-priority layer also holds stale
/// "decoy" versions of files that the high-priority layer must shadow.
pub fn overlay_two_lowers(initial: &Tree) -> Fixture {
    let (mut high, mut low) = (Tree::new(), Tree::new());
    for (i, (path, entry)) in initial.iter().enumerate() {
        match entry {
            Entry::Dir => {
                high.insert(path.clone(), Entry::Dir);
                low.insert(path.clone(), Entry::Dir);
            }
            Entry::File(_) if i % 2 == 0 => {
                high.insert(path.clone(), entry.clone());
                low.insert(path.clone(), Entry::File(b"stale decoy".to_vec()));
            }
            Entry::File(_) => {
                low.insert(path.clone(), entry.clone());
            }
        }
    }
    overlay(Arc::new(MemoryFs::new()), vec![(mem_with(&high), high), (mem_with(&low), low)])
}

/// An overlay whose lower is itself an overlay (with an empty upper).
pub fn overlay_stacked(initial: &Tree) -> Fixture {
    let inner: Arc<dyn FileSystem> = Arc::new(OverlayFs::new(MemoryFs::new(), mem_with(initial)));
    overlay(Arc::new(MemoryFs::new()), vec![(inner, initial.clone())])
}

/// A memory backend mounted at `/` of a mount table.
pub fn mounted(initial: &Tree) -> Fixture {
    let mounts = MountFs::new();
    mounts.mount_arc("/", mem_with(initial)).unwrap();
    Fixture::plain(Arc::new(mounts))
}

/// A mount table mounted inside a mount table (exercises path rebasing twice).
pub fn mounted_nested(initial: &Tree) -> Fixture {
    let inner = MountFs::new();
    inner.mount_arc("/", mem_with(initial)).unwrap();
    let outer = MountFs::new();
    outer.mount("/", inner).unwrap();
    Fixture::plain(Arc::new(outer))
}

/// A third-party-style backend implementing *only* the seven required trait
/// methods, so the default `remove_dir_all` / `copy_file` / `exists` are tested.
pub fn minimal(initial: &Tree) -> Fixture {
    Fixture::plain(Arc::new(MinimalFs(mem_with(initial))))
}

pub struct MinimalFs(pub Arc<dyn FileSystem>);

impl FileSystem for MinimalFs {
    fn metadata(&self, path: &VPath) -> openvfs::Result<Metadata> {
        self.0.metadata(path)
    }
    fn read_dir(&self, path: &VPath) -> openvfs::Result<Vec<DirEntry>> {
        self.0.read_dir(path)
    }
    fn create_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.create_dir(path)
    }
    fn remove_file(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.remove_file(path)
    }
    fn remove_dir(&self, path: &VPath) -> openvfs::Result<()> {
        self.0.remove_dir(path)
    }
    fn rename(&self, from: &VPath, to: &VPath) -> openvfs::Result<()> {
        self.0.rename(from, to)
    }
    fn open(&self, path: &VPath, options: &OpenOptions) -> openvfs::Result<Box<dyn VfsFile>> {
        self.0.open(path, options)
    }
}

/// Every writable configuration. Conformance and differential suites iterate this.
#[allow(unused_macros)]
macro_rules! for_each_fixture {
    ($callback:ident) => {
        $callback!(
            memory,
            physical,
            physical_follow,
            overlay_memory,
            overlay_on_disk,
            overlay_disk_upper,
            overlay_two_lowers,
            overlay_stacked,
            mounted,
            mounted_nested,
            minimal,
        );
    };
}
#[allow(unused_imports)]
pub(crate) use for_each_fixture;
