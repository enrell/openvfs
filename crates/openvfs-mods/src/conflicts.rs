//! Who wins what: the conflict report over a [`MergedIndex`].
//!
//! A *conflict* is a folded path that more than one enabled layer provides.
//! For each of them the report names the winner (the highest-priority
//! provider) and the ordered losers — the information a mod manager shows as
//! "mod A overwrites files from mod B" — plus per-mod totals of files won
//! and lost.

use std::fmt;

use openvfs::{FileType, VPath};

use crate::index::{IndexEntry, MergedIndex, Provider};

/// How many files a single mod wins and loses.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FileCount {
    /// Files this mod serves although other mods provide them too.
    pub won: usize,
    /// Files this mod provides that another mod serves instead.
    pub lost: usize,
}

/// One contested path: every layer providing it, highest priority first.
#[derive(Debug, Clone)]
pub struct ConflictEntry {
    /// The folded path (the report's sort key).
    folded: String,
    providers: Vec<Provider>,
}

impl ConflictEntry {
    /// The folded contested path (see [`VPath::folded`]).
    #[must_use]
    pub fn folded_path(&self) -> &str {
        &self.folded
    }

    /// The contested path in the winner's original spelling.
    #[must_use]
    pub fn path(&self) -> &VPath {
        self.winner().path()
    }

    /// The layer serving the path: the highest-priority provider.
    #[must_use]
    pub fn winner(&self) -> &Provider {
        &self.providers[0] // a conflict always has at least two providers
    }

    /// The shadowed providers, in priority order.
    #[must_use]
    pub fn losers(&self) -> &[Provider] {
        &self.providers[1..]
    }

    /// Every provider of the path, highest priority first.
    #[must_use]
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// What the winner serves: file or directory.
    #[must_use]
    pub fn file_type(&self) -> FileType {
        self.winner().file_type()
    }
}

/// The conflicts of a [`MergedIndex`], sorted by folded path.
///
/// Implied parent directories count too — two mods whose files share
/// `textures/` *both* provide it — so a large report of directories is
/// normal; [`ConflictReport::file_conflicts`] filters to the entries most
/// UIs want.
#[derive(Debug, Clone)]
pub struct ConflictReport {
    entries: Vec<ConflictEntry>,
    /// Files won/lost per layer, by layer index.
    counts: Vec<FileCount>,
    /// Layer names by index, for the display summary.
    layer_names: Vec<String>,
}

impl ConflictReport {
    /// Collects every index node with two or more providers.
    ///
    /// Per-mod [`FileCount`]s cover only entries whose *winner* is a file:
    /// a contested file is won by the winner's mod and lost by every other
    /// provider, whatever kind the losing entries are.
    pub(crate) fn collect(index: &MergedIndex) -> Self {
        let mut counts = vec![FileCount::default(); index.layers().len()];
        let layer_names = index.layers().iter().map(|l| l.name().to_owned()).collect();
        let entries: Vec<ConflictEntry> = index
            .iter()
            .filter(|entry| entry.providers().len() > 1)
            .map(|entry| Self::entry(entry, &mut counts))
            .collect();
        Self { entries, counts, layer_names }
    }

    fn entry(entry: IndexEntry<'_>, counts: &mut [FileCount]) -> ConflictEntry {
        let providers = entry.providers().to_vec();
        if providers[0].is_file() {
            counts[providers[0].layer()].won += 1;
            for loser in &providers[1..] {
                counts[loser.layer()].lost += 1;
            }
        }
        ConflictEntry { folded: entry.folded_path().to_owned(), providers }
    }

    /// Every contested path, sorted by folded path.
    #[must_use]
    pub fn entries(&self) -> &[ConflictEntry] {
        &self.entries
    }

    /// Only the entries where the winner is a file — the classic
    /// "mod A overwrites mod B's file" view.
    pub fn file_conflicts(&self) -> impl Iterator<Item = &ConflictEntry> {
        self.entries.iter().filter(|e| e.file_type() == FileType::File)
    }

    /// `true` when no path is provided by more than one layer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of contested paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Files won/lost per layer, indexed by layer position
    /// (see [`crate::MergedIndex::layers`]).
    #[must_use]
    pub fn file_counts(&self) -> &[FileCount] {
        &self.counts
    }
}

impl fmt::Display for ConflictReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = self.file_conflicts().count();
        writeln!(f, "{} conflicting paths ({file} files):", self.entries.len())?;
        for entry in &self.entries {
            let losers = entry.losers().iter().map(|p| format!("{:?}", p.name())).collect::<Vec<_>>().join(", ");
            write!(f, "  {}: {:?} over {losers}", entry.path(), entry.winner().name())?;
            if entry.file_type() == FileType::Dir {
                f.write_str(" (dir)")?;
            }
            writeln!(f)?;
        }
        for (layer, count) in self.counts.iter().enumerate() {
            if count.won > 0 || count.lost > 0 {
                writeln!(f, "  {:?}: {} files won, {} lost", self.layer_names[layer], count.won, count.lost)?;
            }
        }
        Ok(())
    }
}
