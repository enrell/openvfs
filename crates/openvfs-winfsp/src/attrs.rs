//! Per-path Windows attributes and timestamps the [`openvfs::FileSystem`]
//! trait can't express, kept in a session-local side map.
//!
//! `openvfs::Metadata` carries `file_type`, `len`, `modified` and `read_only`.
//! Windows also wants a full `FILE_ATTRIBUTE_*` word (hidden/system/archive/…)
//! and four file times (creation/access/write/change). Those live here,
//! keyed by the *resolved* (real-cased) [`VPath`]. Entries are created when a
//! node is first reported or created through the mount, mutated by
//! `SetBasicInfo`/`Create`, moved by `Rename`, and dropped on delete. They are
//! session state: nothing is written back to the backend.
//!
//! Platform-independent and unit-tested without the driver.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use openvfs::{Metadata, VPath};

// Selected `FILE_ATTRIBUTE_*` bits (winnt.h). Only the ones we can express.
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
pub const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
pub const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x2000;
/// `SetBasicInfo` uses this sentinel for "don't touch the attributes".
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

/// 100-ns intervals between 1601-01-01 and 1970-01-01 (the FILETIME epoch).
const FILETIME_EPOCH_DIFF: u64 = 11_644_473_600;

/// Converts a [`SystemTime`] to a Windows `FILETIME` (100-ns ticks since 1601).
/// Times before 1601 clamp to 0.
#[must_use]
pub fn filetime_of(t: SystemTime) -> u64 {
    const TICKS_PER_SEC: u64 = 10_000_000;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() + FILETIME_EPOCH_DIFF) * TICKS_PER_SEC + u64::from(d.subsec_nanos() / 100),
        Err(e) => {
            // `t` precedes 1970: subtract the distance, clamping at the
            // FILETIME epoch (pre-1601 — only representable on Unix hosts).
            let d = e.duration();
            let before = d.as_secs() * TICKS_PER_SEC + u64::from(d.subsec_nanos() / 100);
            (FILETIME_EPOCH_DIFF * TICKS_PER_SEC).saturating_sub(before)
        }
    }
}

/// Current time as a `FILETIME`.
#[must_use]
pub fn filetime_now() -> u64 {
    filetime_of(SystemTime::now())
}

/// Attributes + times for one path.
#[derive(Debug, Clone, Copy)]
pub struct Attrs {
    /// `FILE_ATTRIBUTE_*` bits. `FILE_ATTRIBUTE_DIRECTORY` is never stored —
    /// it always derives from the backend's file type.
    pub attributes: u32,
    /// FILETIME stamps; 0 for "unset" is handled at fill time.
    pub creation_time: u64,
    /// Last access time.
    pub last_access_time: u64,
    /// Last write time.
    pub last_write_time: u64,
    /// Change time (metadata or content change).
    pub change_time: u64,
    /// Stable per-path index number reported in `FileInfo.IndexNumber`.
    pub index_number: u64,
}

impl Attrs {
    fn fresh(index: u64, meta: &Metadata, default_time: u64) -> Self {
        let write = meta.modified.map_or(default_time, filetime_of);
        Self {
            attributes: default_attributes(meta),
            creation_time: write,
            last_access_time: write,
            last_write_time: write,
            change_time: write,
            index_number: index,
        }
    }
}

/// The default attribute word for a freshly-seen node: `ARCHIVE` for files,
/// nothing extra for directories (the `DIRECTORY` bit is ORed in at fill time).
/// `FILE_ATTRIBUTE_NORMAL` supplied by a caller collapses to the same defaults.
#[must_use]
pub fn default_attributes(meta: &Metadata) -> u32 {
    if meta.is_dir() { 0 } else { FILE_ATTRIBUTE_ARCHIVE }
}

/// Sanitizes a caller-provided attribute word (e.g. from `Create` or
/// `SetBasicInfo`): the directory bit is dropped (type comes from the backend)
/// and `NORMAL` means "no attributes".
#[must_use]
pub fn sanitize_attributes(attributes: u32, is_dir: bool) -> u32 {
    let mut a = attributes & !FILE_ATTRIBUTE_DIRECTORY;
    if a == FILE_ATTRIBUTE_NORMAL {
        a = 0;
    }
    if !is_dir && a == 0 {
        a = FILE_ATTRIBUTE_ARCHIVE;
    }
    a
}

/// Session side map of Windows-only metadata.
pub struct AttrMap {
    map: RwLock<HashMap<VPath, Attrs>>,
    next_index: AtomicU64,
    /// Timestamp used when the backend reports no `modified` time.
    default_time: u64,
}

impl AttrMap {
    /// A new, empty map. `default_time` is a FILETIME applied to nodes whose
    /// backend doesn't track modification time (typically the mount time).
    #[must_use]
    pub fn new(default_time: u64) -> Self {
        Self { map: RwLock::new(HashMap::new()), next_index: AtomicU64::new(1), default_time }
    }

    /// Reads the stored attrs, filling in defaults from `meta` when the path
    /// has none yet. This is what makes the map transparent for trees that
    /// were populated outside the mount.
    pub fn get_or_fill(&self, path: &VPath, meta: &Metadata) -> Attrs {
        if let Some(a) = read(&self.map).get(path) {
            return *a;
        }
        let index = self.next_index.fetch_add(1, Ordering::Relaxed);
        let attrs = Attrs::fresh(index, meta, self.default_time);
        write(&self.map).insert(path.clone(), attrs);
        attrs
    }

    /// Stores `attrs` for `path` (after Create/SetBasicInfo/Overwrite).
    pub fn set(&self, path: &VPath, attrs: Attrs) {
        write(&self.map).insert(path.clone(), attrs);
    }

    /// Applies `f` to the stored attrs of `path`, creating a default entry from
    /// `meta` first when absent.
    pub fn update(&self, path: &VPath, meta: &Metadata, f: impl FnOnce(&mut Attrs)) {
        let mut attrs = self.get_or_fill(path, meta);
        f(&mut attrs);
        self.set(path, attrs);
    }

    /// Forgets `path` (after a delete).
    pub fn remove(&self, path: &VPath) {
        write(&self.map).remove(path);
    }

    /// Re-keys every stored entry from `from` onto `to`, including descendants
    /// for directory renames.
    pub fn rename(&self, from: &VPath, to: &VPath) {
        let mut map = write(&self.map);
        // `starts_with` is component-wise and covers both `from` itself and
        // everything below it.
        let keys: Vec<VPath> = map.keys().filter(|k| k.starts_with(from)).cloned().collect();
        for k in keys {
            if let Some(a) = map.remove(&k) {
                let rest = k.strip_prefix(from).unwrap_or_else(VPath::root);
                // `join` is always relative, so `/x` lands under `to`; the
                // root remainder maps to `to` itself.
                let nk = to.join(rest.as_str()).unwrap_or_else(|_| to.clone());
                map.insert(nk, a);
            }
        }
    }

    /// Removes every entry (unmount-time teardown).
    pub fn clear(&self) {
        write(&self.map).clear();
    }
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn filetime_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        // 1700000000 + 11644473600 = 13344473600 s → *1e7
        assert_eq!(filetime_of(t), 13_344_473_600 * 10_000_000);
        assert_eq!(filetime_of(UNIX_EPOCH), FILETIME_EPOCH_DIFF * 10_000_000);
    }

    #[test]
    fn filetime_handles_pre_epoch() {
        // 1969-12-31 23:59:59 — one second before the Unix epoch.
        assert_eq!(filetime_of(UNIX_EPOCH - Duration::from_secs(1)), (FILETIME_EPOCH_DIFF - 1) * 10_000_000);
    }

    #[test]
    fn sanitize_rules() {
        assert_eq!(sanitize_attributes(FILE_ATTRIBUTE_NORMAL, false), FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(sanitize_attributes(0, false), FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(sanitize_attributes(0, true), 0);
        assert_eq!(sanitize_attributes(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY, true), FILE_ATTRIBUTE_HIDDEN);
        assert_eq!(sanitize_attributes(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_DIRECTORY, false), FILE_ATTRIBUTE_HIDDEN);
    }

    #[test]
    fn map_lifecycle() {
        let m = AttrMap::new(filetime_now());
        let p = VPath::new("/a").unwrap();
        let meta = Metadata::file(0);
        let a = m.get_or_fill(&p, &meta);
        assert_eq!(a.attributes, FILE_ATTRIBUTE_ARCHIVE);
        assert_eq!(m.get_or_fill(&p, &meta).index_number, a.index_number);

        m.update(&p, &meta, |x| x.attributes = FILE_ATTRIBUTE_HIDDEN);
        assert_eq!(m.get_or_fill(&p, &meta).attributes, FILE_ATTRIBUTE_HIDDEN);

        m.rename(&p, &VPath::new("/b").unwrap());
        assert_eq!(m.get_or_fill(&VPath::new("/b").unwrap(), &meta).attributes, FILE_ATTRIBUTE_HIDDEN);

        m.remove(&VPath::new("/b").unwrap());
        // fresh default again (new index)
        assert_eq!(m.get_or_fill(&VPath::new("/b").unwrap(), &meta).attributes, FILE_ATTRIBUTE_ARCHIVE);
    }

    #[test]
    fn rename_moves_descendants() {
        let m = AttrMap::new(0);
        let meta = Metadata::dir();
        let dir = VPath::new("/d").unwrap();
        let child = VPath::new("/d/c").unwrap();
        m.update(&dir, &meta, |a| a.attributes = 9);
        m.update(&child, &Metadata::file(0), |a| a.attributes = 7);
        m.rename(&dir, &VPath::new("/e").unwrap());
        assert_eq!(m.get_or_fill(&VPath::new("/e").unwrap(), &meta).attributes, 9);
        assert_eq!(m.get_or_fill(&VPath::new("/e/c").unwrap(), &Metadata::file(0)).attributes, 7);
    }
}
