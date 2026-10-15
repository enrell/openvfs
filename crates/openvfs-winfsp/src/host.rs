//! The `openvfs` ↔ WinFsp bridge: every file-system operation as plain Rust.
//!
//! This module contains the semantics of each `FSP_FILE_SYSTEM_INTERFACE`
//! callback, operating on already-decoded arguments ([`VPath`], `u32` masks,
//! byte buffers) and returning `NTSTATUS` codes. Raw-pointer marshalling,
//! `catch_unwind` and `extern "C"` trampolines live in `cb.rs` (Windows only);
//! this module is platform-independent so the logic is unit-testable without
//! the driver.
//!
//! ## Lifetime / handle model
//!
//! `UmFileContextIsUserContext2` is set on the volume, so every `Create`/`Open`
//! yields a fresh per-open [`Handle`] (the "file descriptor" role) which the
//! callback layer turns into the `FileContext` pointer. The kernel-side FSD
//! tracks file *nodes* by normalized name itself and performs share-access
//! checks — we do not need a node table.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use openvfs::{FileSystem, Metadata, OpenOptions, VPath, VfsError, VfsFile};

use crate::abi::{DirInfoBuf, FspFsctlFileInfo, FspFsctlVolumeInfo, allocation_size};
use crate::attrs::{self, AttrMap};
use crate::options::MountOptions;
use crate::pathx::{WinPath, fold, fold_path, is_after_marker, win_to_vpath};
use crate::security::permissive_security_descriptor;
use crate::status::{NtStatus, status_of, status_of_io};
use crate::status::{
    STATUS_ACCESS_DENIED, STATUS_CANNOT_DELETE, STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_HANDLE, STATUS_INVALID_PARAMETER, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};

// NT-specific access bits that appear in `GrantedAccess` (winternl values).
const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_APPEND_DATA: u32 = 0x0004;
const FILE_READ_EA: u32 = 0x0008;
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_EXECUTE: u32 = 0x0020;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;

// `CreateOptions` bits (NtCreateFile).
pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
#[allow(dead_code)]
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

// `Cleanup` flag bits (FspCleanup*).
pub const CLEANUP_DELETE: u32 = 0x01;
pub const CLEANUP_SET_ALLOCATION_SIZE: u32 = 0x02;
pub const CLEANUP_SET_ARCHIVE_BIT: u32 = 0x10;
pub const CLEANUP_SET_LAST_ACCESS_TIME: u32 = 0x20;
pub const CLEANUP_SET_LAST_WRITE_TIME: u32 = 0x40;
pub const CLEANUP_SET_CHANGE_TIME: u32 = 0x80;

/// Maximum UTF-16 code units in a file name component (`MaxComponentLength`).
pub const MAX_COMPONENT_LEN: usize = 255;

/// An open instance ("file descriptor" role — see module docs).
pub struct Handle {
    /// The resolved, real-cased virtual path. Interior-mutable so `rename`
    /// (which changes the file's path) can keep the handle pointing at the
    /// file — matching the Windows semantics that handles follow the file.
    pub path: RwLock<VPath>,
    /// `true` for directory handles (no `VfsFile` behind them).
    pub is_dir: bool,
    /// The backend file handle, when this descriptor carries I/O access.
    /// `None` for directories and for metadata-only opens (e.g. a handle with
    /// only `FILE_READ_ATTRIBUTES`/`DELETE` rights).
    pub file: Option<Mutex<Box<dyn VfsFile>>>,
    /// All writes go to EOF (`FILE_APPEND_DATA`-only opens).
    pub append: bool,
}

impl Handle {
    /// The current resolved path (rename-aware).
    fn path(&self) -> VPath {
        read(&self.path).clone()
    }
}

/// Per-mount shared state. Everything is lock-protected; the WinFsp
/// dispatcher calls us from multiple threads concurrently.
pub struct Host {
    fs: Arc<dyn FileSystem>,
    /// Whole-volume read-only (mount option or backend `is_read_only()`).
    read_only: bool,
    label: RwLock<Vec<u16>>,
    total_size: u64,
    free_size: u64,
    attrs: AttrMap,
    /// folded path → resolved path. A *hint* cache only: hits are re-validated
    /// with `metadata` before use, so out-of-band backend changes can't
    /// resurrect stale names.
    fold_cache: RwLock<HashMap<String, VPath>>,
    /// Live handle pointers (as `usize`) so unmount can reclaim leaked ones.
    live_handles: Mutex<HashSet<usize>>,
    /// The permissive self-relative security descriptor we report.
    security: Vec<u8>,
    /// Set by `DispatcherStopped`.
    stopped: AtomicBool,
}

impl Host {
    /// Wraps `fs` for mounting. Fails early when the root isn't a directory.
    pub fn new(fs: Arc<dyn FileSystem>, opts: &MountOptions) -> Result<Self, VfsError> {
        let meta = fs.metadata(&VPath::root())?;
        if !meta.is_dir() {
            return Err(VfsError::NotADirectory(VPath::root()));
        }
        let epoch = attrs::filetime_now();
        Ok(Self {
            read_only: opts.read_only || fs.is_read_only(),
            label: RwLock::new(opts.volume_label.encode_utf16().take(31).collect()),
            total_size: opts.total_size,
            free_size: opts.free_size.min(opts.total_size),
            attrs: AttrMap::new(epoch),
            fold_cache: RwLock::new(HashMap::new()),
            live_handles: Mutex::new(HashSet::new()),
            security: permissive_security_descriptor(),
            stopped: AtomicBool::new(false),
            fs,
        })
    }

    /// `true` once the dispatcher reported being stopped.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    // ------------------------------------------------------------------
    // Handle registry (raw pointers live only in `cb.rs`; here we count them).
    // ------------------------------------------------------------------

    /// Remembers a live `FileContext` pointer (for unmount-time reclamation).
    pub fn register_handle(&self, ctx: usize) {
        lock(&self.live_handles).insert(ctx);
    }

    /// Forgets a `FileContext` pointer (at `Close` time).
    pub fn unregister_handle(&self, ctx: usize) {
        lock(&self.live_handles).remove(&ctx);
    }

    /// Returns every still-registered handle pointer (leak cleanup at unmount).
    pub fn drain_handles(&self) -> Vec<usize> {
        lock(&self.live_handles).drain().collect()
    }

    // ------------------------------------------------------------------
    // Path resolution
    // ------------------------------------------------------------------

    /// Normalizes a WinFsp name (`\dir\file`) into a [`VPath`].
    fn vpath(name: &str) -> Result<VPath, NtStatus> {
        match win_to_vpath(name) {
            WinPath::Path(p) => Ok(p),
            WinPath::Invalid => Err(STATUS_OBJECT_NAME_INVALID),
        }
    }

    /// Resolves `want` to the real-cased path of an *existing* node.
    ///
    /// Fast path: exact (case-sensitive) metadata hit. Slow path: per-component
    /// case-folded directory scans, so `Textures\a.dds` finds `textures\A.dds`.
    pub fn resolve(&self, want: &VPath) -> Result<VPath, NtStatus> {
        match self.fs.metadata(want) {
            Ok(_) => {
                self.cache_fold(want);
                return Ok(want.clone());
            }
            Err(e) if missing(&e) => {}
            Err(e) => return Err(status_of(&e, false)),
        }

        let folded = fold_path(want);
        if let Some(hit) = read(&self.fold_cache).get(&folded).cloned()
            && self.fs.metadata(&hit).is_ok()
        {
            return Ok(hit);
        }
        write(&self.fold_cache).remove(&folded);

        self.folded_walk(want)
    }

    /// Component-by-component case-insensitive resolution.
    fn folded_walk(&self, want: &VPath) -> Result<VPath, NtStatus> {
        let mut cur = VPath::root();
        let comps: Vec<&str> = want.components().collect();
        let last = comps.len().saturating_sub(1);
        for (i, comp) in comps.iter().enumerate() {
            let direct = cur.join(comp).map_err(|_| STATUS_OBJECT_NAME_INVALID)?;
            match self.fs.metadata(&direct) {
                Ok(_) => {
                    cur = direct;
                    continue;
                }
                Err(e) if missing(&e) => {}
                Err(e) => return Err(status_of(&e, i != last)),
            }
            // Exact miss — scan the directory for a case-folded match.
            let entries = self.fs.read_dir(&cur).map_err(|e| status_of(&e, true))?;
            let want_fold = fold(comp);
            match entries.iter().find(|e| e.name != *comp && fold(&e.name) == want_fold) {
                Some(e) => cur = e.path.clone(),
                None => {
                    return Err(if i == last { STATUS_OBJECT_NAME_NOT_FOUND } else { STATUS_OBJECT_PATH_NOT_FOUND });
                }
            }
        }
        write(&self.fold_cache).insert(fold_path(want), cur.clone());
        Ok(cur)
    }

    /// Resolves for creation: the parent is folded to its real path, while the
    /// final component keeps its requested spelling (that's the casing the
    /// user wants the new file to have).
    fn resolve_for_create(&self, want: &VPath) -> Result<VPath, NtStatus> {
        let Some(name) = want.file_name() else {
            return Err(STATUS_CANNOT_DELETE); // root can never be created
        };
        let parent = want.parent().unwrap_or_else(VPath::root);
        let real_parent = self.resolve(&parent)?;
        real_parent.join(name).map_err(|_| STATUS_OBJECT_NAME_INVALID)
    }

    /// Forgets cached fold hints under `path` (subtree included).
    fn evict_fold(&self, path: &VPath) {
        let folded = fold_path(path);
        let prefix = format!("{folded}/");
        write(&self.fold_cache).retain(|k, _| *k != folded && !k.starts_with(&prefix));
    }

    fn cache_fold(&self, real: &VPath) {
        write(&self.fold_cache).insert(fold_path(real), real.clone());
    }

    // ------------------------------------------------------------------
    // Metadata plumbing
    // ------------------------------------------------------------------

    fn metadata(&self, path: &VPath) -> Result<Metadata, NtStatus> {
        self.fs.metadata(path).map_err(|e| status_of(&e, false))
    }

    /// Builds the `FSP_FSCTL_FILE_INFO` for `path` — backend metadata merged
    /// with the session attribute side-map.
    fn file_info(&self, path: &VPath, meta: &Metadata) -> FspFsctlFileInfo {
        let a = self.attrs.get_or_fill(path, meta);
        let mut fa = a.attributes & !attrs::FILE_ATTRIBUTE_DIRECTORY;
        if meta.is_dir() {
            fa |= attrs::FILE_ATTRIBUTE_DIRECTORY;
        }
        if self.read_only || meta.read_only {
            fa |= attrs::FILE_ATTRIBUTE_READONLY;
        }
        let size = if meta.is_file() { meta.len } else { 0 };
        FspFsctlFileInfo {
            file_attributes: fa,
            reparse_tag: 0,
            allocation_size: allocation_size(size),
            file_size: size,
            creation_time: a.creation_time,
            last_access_time: a.last_access_time,
            last_write_time: a.last_write_time,
            change_time: a.change_time,
            index_number: a.index_number,
            hard_links: 0,
            ea_size: 0,
        }
    }

    /// `(path, meta)` for an existing node, folded-resolved.
    fn stat(&self, name: &str) -> Result<(VPath, Metadata), NtStatus> {
        let want = Self::vpath(name)?;
        let real = self.resolve(&want)?;
        let meta = self.metadata(&real)?;
        Ok((real, meta))
    }

    /// Whether the volume rejects writes at all.
    fn check_writable(&self) -> Result<(), NtStatus> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        Ok(())
    }

    /// `true` when the node itself is marked read-only — either by the
    /// backend (`Metadata::read_only`) or by a `FILE_ATTRIBUTE_READONLY` flag
    /// parked in the session side-map.
    fn file_readonly(&self, path: &VPath, meta: &Metadata) -> bool {
        meta.read_only || self.attrs.get_or_fill(path, meta).attributes & attrs::FILE_ATTRIBUTE_READONLY != 0
    }

    // ------------------------------------------------------------------
    // Handle registry ops
    // ------------------------------------------------------------------

    /// Decodes the `GrantedAccess` mask into openvfs [`OpenOptions`].
    /// `create` marks the create-on-missing case.
    fn open_options(granted: u32, create: bool) -> OpenOptions {
        let read = granted & (FILE_READ_DATA | FILE_READ_EA | FILE_EXECUTE | GENERIC_READ | GENERIC_ALL) != 0;
        // FILE_APPEND_DATA without FILE_WRITE_DATA is the classic append-only
        // open; FILE_WRITE_DATA alone permits positioned writes.
        let append = granted & (FILE_APPEND_DATA | GENERIC_WRITE | GENERIC_ALL) != 0
            && granted & (FILE_WRITE_DATA | FILE_READ_DATA) == 0;
        let write = granted
            & (FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | FILE_WRITE_EA
                | FILE_WRITE_ATTRIBUTES
                | GENERIC_WRITE
                | GENERIC_ALL)
            != 0;
        let mut o = OpenOptions::new();
        o.create = create;
        o.write = write || create; // create needs write through the trait
        o.append = append;
        o.read = read || !o.is_write();
        o
    }

    /// Creates an I/O handle for `path` if `options` asks for data access.
    fn open_backend_file(
        &self,
        path: &VPath,
        granted: u32,
        create: bool,
    ) -> Result<Option<Mutex<Box<dyn VfsFile>>>, NtStatus> {
        let has_data_access = granted
            & (FILE_READ_DATA
                | FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | FILE_EXECUTE
                | FILE_READ_EA
                | FILE_WRITE_EA
                | GENERIC_READ
                | GENERIC_WRITE
                | GENERIC_ALL
                | GENERIC_EXECUTE)
            != 0;
        if !has_data_access && !create {
            return Ok(None); // attribute/delete-only handle
        }
        let opts = Self::open_options(granted, create);
        let file = self.fs.open(path, &opts).map_err(|e| status_of(&e, false))?;
        Ok(Some(Mutex::new(file)))
    }

    fn make_handle(&self, path: VPath, is_dir: bool, file: Option<Mutex<Box<dyn VfsFile>>>, granted: u32) -> Handle {
        Handle {
            path: RwLock::new(path),
            is_dir,
            file,
            append: granted & FILE_APPEND_DATA != 0 && granted & (FILE_WRITE_DATA | FILE_READ_DATA) == 0,
        }
    }

    // ------------------------------------------------------------------
    // Interface operations (called from `cb.rs` trampolines)
    // ------------------------------------------------------------------

    /// `GetVolumeInfo`.
    pub fn get_volume_info(&self) -> FspFsctlVolumeInfo {
        let mut vi =
            FspFsctlVolumeInfo { total_size: self.total_size, free_size: self.free_size, ..Default::default() };
        let label = read(&self.label);
        let n = label.len().min(32);
        vi.volume_label[..n].copy_from_slice(&label[..n]);
        vi.volume_label_length = (n * 2) as u16;
        vi
    }

    /// `SetVolumeLabel`. In-memory only (the VFS has no volume store).
    pub fn set_volume_label(&self, label: &[u16]) -> Result<FspFsctlVolumeInfo, NtStatus> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        *write(&self.label) = label.iter().take(31).copied().collect();
        Ok(self.get_volume_info())
    }

    /// `GetSecurityByName`: returns `FILE_ATTRIBUTE_*` for a name, folded.
    ///
    /// The security-descriptor part is deliberately answered with size 0 —
    /// WinFsp then skips the (meaningless-for-us) `AccessCheck`, which is both
    /// faster and the honest model for an ACL-free VFS. `GetSecurity` still
    /// reports a permissive descriptor for tools that read ACLs.
    pub fn get_security_by_name(&self, name: &str) -> Result<u32, NtStatus> {
        let (real, meta) = self.stat(name)?;
        let info = self.file_info(&real, &meta);
        Ok(info.file_attributes)
    }

    /// `Create` — new file or directory. Returns the handle + file info.
    pub fn create(
        &self,
        name: &str,
        create_options: u32,
        granted: u32,
        file_attributes: u32,
    ) -> Result<(Handle, FspFsctlFileInfo), NtStatus> {
        self.check_writable()?;
        let want = Self::vpath(name)?;
        if self.resolve(&want).is_ok() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        let real = self.resolve_for_create(&want)?;

        if create_options & FILE_DIRECTORY_FILE != 0 {
            self.fs.create_dir(&real).map_err(|e| status_of(&e, false))?;
        } else {
            // Reject a file create where the target resolves to an existing
            // dir is impossible here (resolve() above found nothing), so just
            // create+open through the backend.
        }

        let now = attrs::filetime_now();
        let file = if create_options & FILE_DIRECTORY_FILE != 0 {
            None
        } else {
            self.open_backend_file(&real, granted, true)?
        };
        let meta = self.metadata(&real)?;
        self.attrs.update(&real, &meta, |a| {
            a.attributes = attrs::sanitize_attributes(file_attributes, meta.is_dir());
            a.creation_time = now;
            a.last_access_time = now;
            a.last_write_time = now;
            a.change_time = now;
        });
        self.cache_fold(&real);
        let info = self.file_info(&real, &meta);
        Ok((self.make_handle(real, meta.is_dir(), file, granted), info))
    }

    /// `Open` — existing file or directory.
    pub fn open(&self, name: &str, create_options: u32, granted: u32) -> Result<(Handle, FspFsctlFileInfo), NtStatus> {
        let (real, meta) = self.stat(name)?;
        let wants_dir = create_options & FILE_DIRECTORY_FILE != 0;
        let must_file = create_options & FILE_NON_DIRECTORY_FILE != 0;
        if meta.is_dir() && must_file {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        if meta.is_dir() && !wants_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        if meta.is_file() && wants_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        if meta.is_file() {
            // A write-capable handle on a read-only file/volume is denied —
            // like NTFS, `FILE_ATTRIBUTE_READONLY` blocks write opens.
            if granted
                & (FILE_WRITE_DATA
                    | FILE_APPEND_DATA
                    | FILE_WRITE_EA
                    | FILE_WRITE_ATTRIBUTES
                    | GENERIC_WRITE
                    | GENERIC_ALL)
                != 0
            {
                self.check_writable()?;
                if self.file_readonly(&real, &meta) {
                    return Err(STATUS_ACCESS_DENIED);
                }
            }
        }
        // Directories never get a backend `VfsFile` — the trait has no
        // directory-handle concept; `read_dir` is driven per-request.
        let file = if meta.is_dir() { None } else { self.open_backend_file(&real, granted, false)? };
        let info = self.file_info(&real, &meta);
        Ok((self.make_handle(real, meta.is_dir(), file, granted), info))
    }

    /// `Overwrite` — truncate an open file to empty (Windows still reports the
    /// allocation size it requested; we don't preallocate since file size is
    /// the meaningful extent for backends).
    pub fn overwrite(
        &self,
        h: &Handle,
        file_attributes: u32,
        replace_attributes: bool,
        _allocation_size: u64,
    ) -> Result<FspFsctlFileInfo, NtStatus> {
        self.check_writable()?;
        if h.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let Some(file) = &h.file else { return Err(STATUS_INVALID_HANDLE) };
        let path = h.path();
        let meta = self.metadata(&path)?;
        if self.file_readonly(&path, &meta) {
            return Err(STATUS_ACCESS_DENIED);
        }
        lock(file).set_len(0).map_err(|e| status_of_io(e.kind()))?;
        let meta = self.metadata(&path)?; // post-truncate state for FileInfo
        let now = attrs::filetime_now();
        self.attrs.update(&path, &meta, |a| {
            a.attributes = if replace_attributes {
                attrs::sanitize_attributes(file_attributes, false)
            } else {
                attrs::sanitize_attributes(file_attributes | a.attributes, false)
            };
            a.last_write_time = now;
            a.change_time = now;
        });
        Ok(self.file_info(&path, &meta))
    }

    /// `Cleanup` — not the last close; performs the deferred delete and applies
    /// cheap timestamp side-effects. Errors cannot be reported (void callback).
    pub fn cleanup(&self, h: &Handle, flags: u32) {
        let path = h.path();
        if flags & CLEANUP_DELETE != 0 {
            // Cleanup cannot report errors (void callback) — the authoritative
            // checks already ran in CanDelete/SetDelete.
            let _removed = if h.is_dir { self.fs.remove_dir(&path) } else { self.fs.remove_file(&path) };
            self.attrs.remove(&path);
            self.evict_fold(&path);
        }
        if flags
            & (CLEANUP_SET_LAST_ACCESS_TIME
                | CLEANUP_SET_LAST_WRITE_TIME
                | CLEANUP_SET_CHANGE_TIME
                | CLEANUP_SET_ARCHIVE_BIT)
            != 0
            && let Ok(meta) = self.fs.metadata(&path)
        {
            let now = attrs::filetime_now();
            self.attrs.update(&path, &meta, |a| {
                if flags & CLEANUP_SET_ARCHIVE_BIT != 0 && !meta.is_dir() {
                    a.attributes |= attrs::FILE_ATTRIBUTE_ARCHIVE;
                }
                if flags & CLEANUP_SET_LAST_ACCESS_TIME != 0 {
                    a.last_access_time = now;
                }
                if flags & CLEANUP_SET_LAST_WRITE_TIME != 0 {
                    a.last_write_time = now;
                }
                if flags & CLEANUP_SET_CHANGE_TIME != 0 {
                    a.change_time = now;
                }
            });
        }
    }

    /// `Read` into `buf`; returns bytes read.
    pub fn read(&self, h: &Handle, buf: &mut [u8], offset: u64) -> Result<usize, NtStatus> {
        if h.is_dir {
            return Err(STATUS_INVALID_DEVICE_REQUEST);
        }
        let Some(file) = &h.file else { return Err(STATUS_INVALID_HANDLE) };
        let mut f = lock(file);
        f.seek(SeekFrom::Start(offset)).map_err(|e| status_of_io(e.kind()))?;
        let n = f.read(buf).map_err(|e| status_of_io(e.kind()))?;
        Ok(n)
    }

    /// `Write` from `buf`; returns (bytes written, fresh file info).
    pub fn write(
        &self,
        h: &Handle,
        buf: &[u8],
        offset: u64,
        to_end: bool,
        constrained: bool,
    ) -> Result<(usize, FspFsctlFileInfo), NtStatus> {
        if h.is_dir {
            return Err(STATUS_INVALID_DEVICE_REQUEST);
        }
        let Some(file) = &h.file else { return Err(STATUS_ACCESS_DENIED) };
        let mut f = lock(file);
        let len = if to_end || h.append {
            f.seek(SeekFrom::End(0)).map_err(|e| status_of_io(e.kind()))?;
            buf.len()
        } else {
            f.seek(SeekFrom::Start(offset)).map_err(|e| status_of_io(e.kind()))?;
            if constrained {
                // Constrained I/O must not grow the file.
                let end = f.seek(SeekFrom::End(0)).map_err(|e| status_of_io(e.kind()))?;
                let _restored = f.seek(SeekFrom::Start(offset)).map_err(|e| status_of_io(e.kind()));
                let room = end.saturating_sub(offset);
                usize::try_from(room.min(buf.len() as u64)).unwrap_or(usize::MAX)
            } else {
                buf.len()
            }
        };
        let n = f.write(&buf[..len]).map_err(|e| status_of_io(e.kind()))?;
        drop(f);
        let path = h.path();
        let meta = self.metadata(&path)?;
        let now = attrs::filetime_now();
        self.attrs.update(&path, &meta, |a| {
            a.last_write_time = now;
            a.change_time = now;
        });
        Ok((n, self.file_info(&path, &meta)))
    }

    /// `Flush`. `h == None` means a whole-volume flush.
    pub fn flush(&self, h: Option<&Handle>) -> Result<Option<FspFsctlFileInfo>, NtStatus> {
        let Some(h) = h else { return Ok(None) };
        if let Some(file) = &h.file {
            lock(file).sync_all().map_err(|e| status_of_io(e.kind()))?;
            let path = h.path();
            let meta = self.metadata(&path)?;
            return Ok(Some(self.file_info(&path, &meta)));
        }
        Ok(None)
    }

    /// `GetFileInfo`.
    pub fn get_file_info(&self, h: &Handle) -> Result<FspFsctlFileInfo, NtStatus> {
        let path = h.path();
        let meta = self.metadata(&path)?;
        Ok(self.file_info(&path, &meta))
    }

    /// `SetBasicInfo` — attributes + file times (0 / INVALID sentinel = keep).
    pub fn set_basic_info(
        &self,
        h: &Handle,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        change_time: u64,
    ) -> Result<FspFsctlFileInfo, NtStatus> {
        if self.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let path = h.path();
        let meta = self.metadata(&path)?;
        self.attrs.update(&path, &meta, |a| {
            if file_attributes != attrs::INVALID_FILE_ATTRIBUTES {
                a.attributes = attrs::sanitize_attributes(file_attributes, meta.is_dir());
            }
            if creation_time != 0 {
                a.creation_time = creation_time;
            }
            if last_access_time != 0 {
                a.last_access_time = last_access_time;
            }
            if last_write_time != 0 {
                a.last_write_time = last_write_time;
            }
            if change_time != 0 {
                a.change_time = change_time;
            }
        });
        Ok(self.file_info(&path, &meta))
    }

    /// `SetFileSize` / `SetAllocationSize`.
    ///
    /// Allocation is virtual: we only act when shrinking the allocation below
    /// the file size (which truncates the file, per NTFS rules) or when the
    /// EOF size itself is set.
    pub fn set_file_size(&self, h: &Handle, new_size: u64, is_allocation: bool) -> Result<FspFsctlFileInfo, NtStatus> {
        self.check_writable()?;
        if h.is_dir {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let Some(file) = &h.file else { return Err(STATUS_INVALID_HANDLE) };
        let path = h.path();
        let meta = self.metadata(&path)?;
        if self.file_readonly(&path, &meta) {
            return Err(STATUS_ACCESS_DENIED);
        }
        if is_allocation {
            // Shrinking the allocation below the file size truncates the
            // file (NTFS rules); growing it is a no-op for us — allocation
            // is virtual.
            if new_size < meta.len {
                lock(file).set_len(new_size).map_err(|e| status_of_io(e.kind()))?;
            }
        } else {
            lock(file).set_len(new_size).map_err(|e| status_of_io(e.kind()))?;
        }
        let meta = self.metadata(&path)?;
        Ok(self.file_info(&path, &meta))
    }

    /// Shared `CanDelete` / `SetDelete(true)` validation.
    fn validate_delete(&self, h: &Handle) -> Result<(), NtStatus> {
        self.check_writable()?;
        let path = h.path();
        let meta = self.metadata(&path)?;
        if self.file_readonly(&path, &meta) {
            return Err(STATUS_CANNOT_DELETE);
        }
        if meta.is_dir() && !self.fs.read_dir(&path).map_err(|e| status_of(&e, false))?.is_empty() {
            return Err(STATUS_DIRECTORY_NOT_EMPTY);
        }
        Ok(())
    }

    /// `CanDelete`.
    pub fn can_delete(&self, h: &Handle) -> Result<(), NtStatus> {
        self.validate_delete(h)
    }

    /// `SetDelete`. `delete == false` clears a pending delete (always fine).
    pub fn set_delete(&self, h: &Handle, delete: bool) -> Result<(), NtStatus> {
        if delete { self.validate_delete(h) } else { Ok(()) }
    }

    /// `Rename` — the handle follows the file, so `h.path` is updated.
    pub fn rename(&self, h: &Handle, new_name: &str, replace_if_exists: bool) -> Result<(), NtStatus> {
        self.check_writable()?;
        let want = Self::vpath(new_name)?;
        // Existing destination? Without replace rights it is a collision.
        let target = self.resolve(&want);
        if let Ok(existing) = &target {
            if !replace_if_exists {
                return Err(STATUS_OBJECT_NAME_COLLISION);
            }
            let meta = self.metadata(existing)?;
            if meta.is_dir() {
                return Err(STATUS_ACCESS_DENIED); // can't overwrite a directory
            }
            if meta.read_only {
                return Err(STATUS_CANNOT_DELETE);
            }
        }
        let from = h.path();
        let to = self.resolve_for_create(&want)?;
        self.fs.rename(&from, &to).map_err(|e| status_of(&e, false))?;
        *write(&h.path) = to.clone();
        self.attrs.rename(&from, &to);
        self.evict_fold(&from);
        self.cache_fold(&to);
        Ok(())
    }

    /// `GetSecurity` — the permissive descriptor (see `security.rs`).
    pub fn get_security(&self) -> &[u8] {
        &self.security
    }

    /// `ReadDirectory`. Enumerates `.`/`..` then real entries in our stable
    /// order; `emit` maps to `FspFileSystemAddDirInfo`. When `emit` returns
    /// false the caller's buffer is full — we stop and let the FSD re-issue
    /// with a marker. Returns `true` when the listing was fully written (the
    /// caller then appends the EOF marker via `AddDirInfo(NULL)`).
    pub fn read_directory(
        &self,
        h: &Handle,
        marker: Option<&str>,
        emit: &mut dyn FnMut(&mut DirInfoBuf) -> bool,
    ) -> Result<bool, NtStatus> {
        if !h.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let dir = h.path();
        let entries = self.fs.read_dir(&dir).map_err(|e| status_of(&e, false))?;

        // One record per entry: (name-to-report, path-of-node). Skip entries
        // whose metadata disappeared mid-listing — a listing is a snapshot.
        let mut items: Vec<(String, VPath, Metadata)> = Vec::with_capacity(entries.len() + 2);
        let self_meta = self.metadata(&dir)?;
        items.push((".".to_owned(), dir.clone(), self_meta.clone()));
        let parent = dir.parent().unwrap_or_else(VPath::root);
        let parent_meta = self.metadata(&parent).unwrap_or_else(|_| self_meta.clone());
        items.push(("..".to_owned(), parent, parent_meta));
        for e in entries {
            if let Ok(m) = self.fs.metadata(&e.path) {
                items.push((e.name, e.path, m));
            }
        }
        // Stable enumeration order for all real entries (dots stay first only
        // on the first page — they sort before everything anyway).
        items[2..].sort_by(|a, b| (fold(&a.0), a.0.as_str()).cmp(&(fold(&b.0), b.0.as_str())));

        let mut buf = DirInfoBuf::new();
        for (name, path, meta) in &items {
            if let Some(m) = marker
                && !is_after_marker(name, m)
            {
                continue;
            }
            let info = self.file_info(path, meta);
            buf.set(&info, &name.encode_utf16().collect::<Vec<u16>>());
            if !emit(&mut buf) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `GetDirInfoByName` — single-entry lookup relative to a dir handle.
    pub fn get_dir_info_by_name(&self, h: &Handle, name: &str) -> Result<DirInfoBuf, NtStatus> {
        if !h.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let dir = h.path();
        // `name` is a single component (FSP_FSCTL guaranteed); reject anything
        // shaped otherwise defensively.
        if name.contains(['\\', '/']) {
            return Err(STATUS_OBJECT_NAME_INVALID);
        }
        let entries = self.fs.read_dir(&dir).map_err(|e| status_of(&e, false))?;
        let folded = fold(name);
        let found =
            entries.iter().find(|e| e.name == name).or_else(|| entries.iter().find(|e| fold(&e.name) == folded));
        let Some(e) = found else { return Err(STATUS_OBJECT_NAME_NOT_FOUND) };
        let meta = self.fs.metadata(&e.path).map_err(|e| status_of(&e, false))?;
        let info = self.file_info(&e.path, &meta);
        let mut buf = DirInfoBuf::new();
        buf.set(&info, &e.name.encode_utf16().collect::<Vec<u16>>());
        Ok(buf)
    }

    /// `DispatcherStopped` — records the dispatcher's terminal state.
    pub fn dispatcher_stopped(&self, _normally: bool) {
        self.stopped.store(true, Ordering::Release);
    }
}

/// `true` for "missing" errors that trigger the folded lookup.
fn missing(e: &VfsError) -> bool {
    matches!(e, VfsError::NotFound(_) | VfsError::NotADirectory(_))
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock<T>(l: &Mutex<T>) -> MutexGuard<'_, T> {
    l.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
