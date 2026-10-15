//! A merged, case-insensitive index over an ordered set of named layers.
//!
//! A mod manager shows one virtual tree assembled from many directories —
//! the installed mods and the base game — where the *highest priority* layer
//! wins any file several provide. [`MergedIndex`] answers "who serves this
//! path?": it walks every layer once, keys every node by its folded path (so
//! `Textures/` and `textures/` really do merge — see [`VPath::folded`]), and
//! from then on answers lookups, listings and [`crate::ConflictReport`]s
//! without touching the file systems again.
//!
//! # Consistency model
//!
//! The index is a **snapshot**, not a live view. Mods are expected not to
//! change while a profile is in use — the manager rebuilds when the mod set
//! changes — so there is no filesystem watching. After changing a layer
//! anyway (e.g. installing a mod into it), call [`MergedIndex::invalidate`]
//! and then [`MergedIndex::rebuild`]; queries keep answering from the last
//! snapshot until the rebuild finishes.
//!
//! # What counts as a directory
//!
//! Mirrors [`openvfs::OverlayFs`]: a directory exists in the merged
//! namespace if any layer lists it *or* it is the ancestor of a listed node
//! (a mod whose data is a bare file list still "contains" every parent of
//! its files). Providers remember whether they were `listed` or only implied
//! — see [`Provider::listed`].
//!
//! One edge case, kept identical to the overlay: a path *under* a file that a
//! lower layer provides is still reachable (`/x` a file in layer 0 does not
//! hide `/x/y` in layer 1). In a coherent mod set this never happens.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;
use std::sync::Arc;

use openvfs::{DirEntry, FileSystem, FileType, Metadata, VPath};

use crate::conflicts::ConflictReport;
use crate::error::Result;

/// One named layer of a [`MergedIndex`]; `layers[0]` has the highest priority.
///
/// Cheap to clone: the file system is shared.
#[derive(Clone)]
pub struct Layer {
    name: Arc<str>,
    fs: Arc<dyn FileSystem>,
}

impl Layer {
    /// A layer called `name`, backed by `fs`.
    pub fn new(name: impl Into<String>, fs: Arc<dyn FileSystem>) -> Self {
        Self { name: Arc::from(name.into().as_str()), fs }
    }

    /// The layer's name (typically a mod name, or `"base game"`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file system holding this layer's files.
    #[must_use]
    pub fn fs(&self) -> &Arc<dyn FileSystem> {
        &self.fs
    }
}

impl fmt::Debug for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layer").field("name", &self.name).finish_non_exhaustive()
    }
}

/// One layer's claim on a node of the merged namespace.
///
/// Providers of a node are ordered by layer priority; the first is the
/// *winner*, the rest are the *losers* (shadowed providers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    layer: usize,
    name: Arc<str>,
    path: VPath,
    file_type: FileType,
    listed: bool,
}

impl Provider {
    /// The layer's index in [`MergedIndex::layers`]; `0` is highest priority.
    #[must_use]
    pub fn layer(&self) -> usize {
        self.layer
    }

    /// The layer's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The real, original-cased path at which the layer stores the node.
    #[must_use]
    pub fn path(&self) -> &VPath {
        &self.path
    }

    /// Whether the node is a file or a directory in this layer.
    #[must_use]
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// `true` for regular files.
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.file_type == FileType::File
    }

    /// `true` for directories.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Dir
    }

    /// `true` when the layer's own listing produced this node; `false` when
    /// it was materialized as the ancestor of a listed node (an *implied*
    /// parent directory).
    #[must_use]
    pub fn listed(&self) -> bool {
        self.listed
    }
}

/// A borrowed view of one node of a [`MergedIndex`]: its folded path and
/// every layer providing it.
#[derive(Debug, Clone, Copy)]
pub struct IndexEntry<'a> {
    folded: &'a str,
    providers: &'a [Provider],
}

impl<'a> IndexEntry<'a> {
    /// The folded path this entry lives at (see [`VPath::folded`]).
    #[must_use]
    pub fn folded_path(&self) -> &'a str {
        self.folded
    }

    /// Every layer providing the node, highest priority first.
    #[must_use]
    pub fn providers(&self) -> &'a [Provider] {
        self.providers
    }

    /// The layer serving the node: the highest-priority provider.
    #[must_use]
    pub fn winner(&self) -> &'a Provider {
        &self.providers[0] // slots are never stored empty
    }

    /// The shadowed providers, in priority order.
    #[must_use]
    pub fn losers(&self) -> &'a [Provider] {
        &self.providers[1..]
    }
}

/// One snapshot of the merged namespace over an ordered list of [`Layer`]s.
///
/// Build once with [`MergedIndex::build`], then query freely; all queries are
/// answered from the snapshot alone. See the module docs for the consistency
/// model and the directory-existence rule.
pub struct MergedIndex {
    layers: Vec<Layer>,
    /// Folded path → providers (never empty, sorted by layer index).
    /// Invariant: every folded ancestor of a key is itself a key.
    nodes: BTreeMap<String, Vec<Provider>>,
    /// Set by `invalidate`, cleared by a successful `rebuild`.
    stale: bool,
}

impl MergedIndex {
    /// Walks every layer and builds the index. `layers[0]` is highest priority.
    ///
    /// Any layer whose root cannot be listed fails the build.
    pub fn build(layers: Vec<Layer>) -> Result<Self> {
        let mut nodes = BTreeMap::new();
        Self::scan(&layers, &mut nodes)?;
        Ok(Self { layers, nodes, stale: false })
    }

    /// The layers, highest priority first.
    #[must_use]
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Number of nodes (files and directories) in the merged namespace.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// `true` when no layer contributes any node.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Marks the snapshot out of date. Queries keep answering from it until
    /// [`MergedIndex::rebuild`] runs — `invalidate` is a hint, not a fence.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// `true` after [`MergedIndex::invalidate`] until a successful rebuild.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Re-walks every layer and replaces the snapshot atomically.
    ///
    /// On error the previous snapshot is kept intact, so the index is never
    /// observed half-built.
    pub fn rebuild(&mut self) -> Result<()> {
        let mut fresh = BTreeMap::new();
        Self::scan(&self.layers, &mut fresh)?;
        self.nodes = fresh;
        self.stale = false;
        Ok(())
    }

    /// Computes who wins every path more than one layer provides.
    #[must_use]
    pub fn conflicts(&self) -> ConflictReport {
        ConflictReport::collect(self)
    }

    /// Iterates every indexed node, in folded-path order.
    pub fn iter(&self) -> impl Iterator<Item = IndexEntry<'_>> {
        self.nodes.iter().map(|(folded, providers)| IndexEntry { folded, providers })
    }

    /// Every layer providing `path`, highest priority first (`[0]` wins).
    #[must_use]
    pub fn providers(&self, path: &VPath) -> Option<&[Provider]> {
        self.nodes.get(path.as_str().to_lowercase().as_str()).map(Vec::as_slice)
    }

    /// The layer serving `path`: the highest-priority provider.
    #[must_use]
    pub fn winner(&self, path: &VPath) -> Option<&Provider> {
        self.providers(path).and_then(|p| p.first())
    }

    /// The real, original-cased path the winning layer stores `path` at —
    /// the address a later deployment step needs.
    #[must_use]
    pub fn real_path(&self, path: &VPath) -> Option<&VPath> {
        self.winner(path).map(Provider::path)
    }

    /// Index-only metadata: the winner's file type, `len` 0, no mtime, and
    /// `read_only` set (the index never writes).
    ///
    /// Sizes and timestamps are not tracked on purpose — recording them would
    /// make every build stat every file. For real metadata, open the winning
    /// layer: `index.layers()[w.layer()].fs().metadata(w.path())`.
    #[must_use]
    pub fn metadata(&self, path: &VPath) -> Option<Metadata> {
        if path.is_root() {
            return Some(Metadata::dir().with_read_only(true));
        }
        let winner = self.winner(path)?;
        let meta = match winner.file_type() {
            FileType::File => Metadata::file(0),
            FileType::Dir => Metadata::dir(),
        };
        Some(meta.with_read_only(true))
    }

    /// The merged listing of `dir`'s direct children: one entry per folded
    /// child name, spelled the way the *winning* layer spells it. `None`
    /// when `dir` is not a directory in the merged namespace.
    ///
    /// The returned `DirEntry::path`s keep `dir`'s spelling joined with each
    /// winning child's real name.
    #[must_use]
    pub fn read_dir(&self, dir: &VPath) -> Option<Vec<DirEntry>> {
        let meta = self.metadata(dir)?;
        if !meta.is_dir() {
            return None;
        }
        let folded = dir.as_str().to_lowercase();
        let prefix = if dir.is_root() { String::from("/") } else { format!("{folded}/") };
        let mut entries = Vec::new();
        for (key, providers) in self
            .nodes
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(&prefix))
        {
            let rest = &key[prefix.len()..];
            if rest.contains('/') {
                continue; // a deeper descendant, not a direct child
            }
            let winner = &providers[0];
            let name = winner.path().file_name().unwrap_or(rest);
            entries.push(DirEntry {
                name: name.to_owned(),
                path: dir.join(name).unwrap_or_else(|_| winner.path().clone()),
                file_type: winner.file_type(),
            });
        }
        Some(entries)
    }

    /// Re-walks the layers into `nodes`.
    fn scan(layers: &[Layer], nodes: &mut BTreeMap<String, Vec<Provider>>) -> Result<()> {
        for (index, layer) in layers.iter().enumerate() {
            let mut stack = vec![VPath::root()];
            while let Some(dir) = stack.pop() {
                for entry in layer.fs.read_dir(&dir)? {
                    if entry.is_dir() {
                        stack.push(entry.path.clone());
                    }
                    insert(nodes, layers, index, entry.path, entry.file_type, true);
                }
            }
        }
        Ok(())
    }
}

/// Records `path` as provided by `layer`, materializing implied parent
/// directories for that layer first.
fn insert(
    nodes: &mut BTreeMap<String, Vec<Provider>>,
    layers: &[Layer],
    layer: usize,
    path: VPath,
    kind: FileType,
    listed: bool,
) {
    let mut ancestors: Vec<VPath> = path.ancestors().skip(1).collect();
    ancestors.pop(); // the root needs no provider
    for ancestor in ancestors.into_iter().rev() {
        add(nodes, layers, layer, ancestor, FileType::Dir, false);
    }
    add(nodes, layers, layer, path, kind, listed);
}

/// Adds one provider to a slot, or upgrades an implied provider of the same
/// layer to a listed one. Providers stay sorted by layer index because
/// [`MergedIndex::scan`] inserts layers in priority order.
fn add(
    nodes: &mut BTreeMap<String, Vec<Provider>>,
    layers: &[Layer],
    layer: usize,
    path: VPath,
    kind: FileType,
    listed: bool,
) {
    let folded = path.as_str().to_lowercase();
    let provider = || Provider { layer, name: Arc::clone(&layers[layer].name), path, file_type: kind, listed };
    match nodes.entry(folded) {
        std::collections::btree_map::Entry::Vacant(vacant) => {
            vacant.insert(vec![provider()]);
        }
        std::collections::btree_map::Entry::Occupied(mut occupied) => {
            let providers = occupied.get_mut();
            debug_assert!(providers.iter().all(|p| p.layer <= layer), "layers must insert in order");
            match providers.iter_mut().find(|p| p.layer == layer) {
                // An explicit listing upgrades an implied entry, never the reverse.
                Some(p) if listed && !p.listed => *p = provider(),
                Some(_) => {}
                None => providers.push(provider()),
            }
        }
    }
}

impl fmt::Debug for MergedIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MergedIndex")
            .field("layers", &self.layers.len())
            .field("nodes", &self.nodes.len())
            .field("stale", &self.stale)
            .finish()
    }
}
