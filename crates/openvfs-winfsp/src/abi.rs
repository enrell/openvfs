//! The WinFsp user-mode ABI as pure data: `repr(C)` struct layouts, the
//! `FSP_FILE_SYSTEM_INTERFACE` vtable, and function-pointer types.
//!
//! These declarations are re-derived from the documented public ABI
//! (`inc/winfsp/winfsp.h`, `inc/winfsp/fsctl.h`, WinFsp 2.x) — no WinFsp code
//! is copied or linked. Everything here is platform-independent so layout
//! assertions and the `host` logic can be unit-tested on any machine, with or
//! without the driver installed. Only `sys.rs` (dynamic loading) is
//! Windows-specific.
//!
//! Calling convention: the WinFsp ABI uses the C convention (`extern "C"`) for
//! both its exports and the callbacks it invokes — identical to
//! `extern "system"` on x86_64/aarch64 but distinct on x86, so `extern "C"` is
//! used throughout.

use std::ffi::c_void;

use crate::status::NtStatus;

// These aliases deliberately use the Windows type names — the declarations
// below are a line-for-line mirror of the public ABI.
#[allow(non_camel_case_types)]
pub type NTSTATUS = NtStatus;
pub type PVOID = *mut c_void;
#[allow(non_camel_case_types)]
pub type PWSTR = *mut u16;
#[allow(non_camel_case_types)]
pub type PCWSTR = *const u16;
#[allow(non_camel_case_types)]
pub type BOOLEAN = u8;
#[allow(non_camel_case_types)]
pub type UINT16 = u16;
#[allow(non_camel_case_types)]
pub type UINT32 = u32;
#[allow(non_camel_case_types)]
pub type UINT64 = u64;
#[allow(non_camel_case_types)]
pub type ULONG = u32;
#[allow(non_camel_case_types)]
pub type SIZE_T = usize;

// ---------------------------------------------------------------------------
// Volume parameters (FSP_FSCTL_VOLUME_PARAMS — 504 bytes, versioned).
// ---------------------------------------------------------------------------

/// `FSP_FSCTL_VOLUME_PARAMS`. `version == size_of::<Self>()` selects the V1
/// tail (per-class timeouts etc.); `0` means "V0 layout" (456 bytes).
///
/// The single `flags` word packs the ABI's bitfields LSB-first, in documented
/// order: attributes (bits 0-9), kernel flags (10-15), user-mode flags
/// (16-23), extra kernel flags (24-31).
#[repr(C)]
pub struct FspFsctlVolumeParams {
    pub version: UINT16,
    pub sector_size: UINT16,
    pub sectors_per_allocation_unit: UINT16,
    pub max_component_length: UINT16,
    pub volume_creation_time: UINT64,
    pub volume_serial_number: UINT32,
    pub transact_timeout: UINT32,
    pub irp_timeout: UINT32,
    pub irp_capacity: UINT32,
    pub file_info_timeout: UINT32,
    pub flags: UINT32,
    pub prefix: [u16; 192],
    pub file_system_name: [u16; 16],
    // ---- V1 extension (only read when `version == 504`) ----
    pub flags2: UINT32,
    pub volume_info_timeout: UINT32,
    pub dir_info_timeout: UINT32,
    pub security_timeout: UINT32,
    pub stream_info_timeout: UINT32,
    pub ea_timeout: UINT32,
    pub fsext_control_code: UINT32,
    pub reserved32: [UINT32; 1],
    pub reserved64: [UINT64; 2],
}

// `[u16; 192]` predates const-generic `Default`, so this is a manual impl.
impl Default for FspFsctlVolumeParams {
    fn default() -> Self {
        Self {
            version: 0,
            sector_size: 0,
            sectors_per_allocation_unit: 0,
            max_component_length: 0,
            volume_creation_time: 0,
            volume_serial_number: 0,
            transact_timeout: 0,
            irp_timeout: 0,
            irp_capacity: 0,
            file_info_timeout: 0,
            flags: 0,
            prefix: [0; 192],
            file_system_name: [0; 16],
            flags2: 0,
            volume_info_timeout: 0,
            dir_info_timeout: 0,
            security_timeout: 0,
            stream_info_timeout: 0,
            ea_timeout: 0,
            fsext_control_code: 0,
            reserved32: [0; 1],
            reserved64: [0; 2],
        }
    }
}

// `flags` bit positions (documented ABI order).
pub const VP_CASE_SENSITIVE_SEARCH: UINT32 = 1 << 0;
pub const VP_CASE_PRESERVED_NAMES: UINT32 = 1 << 1;
pub const VP_UNICODE_ON_DISK: UINT32 = 1 << 2;
pub const VP_PERSISTENT_ACLS: UINT32 = 1 << 3;
pub const VP_REPARSE_POINTS: UINT32 = 1 << 4;
pub const VP_REPARSE_POINTS_ACCESS_CHECK: UINT32 = 1 << 5;
pub const VP_NAMED_STREAMS: UINT32 = 1 << 6;
pub const VP_HARD_LINKS: UINT32 = 1 << 7;
pub const VP_EXTENDED_ATTRIBUTES: UINT32 = 1 << 8;
pub const VP_READ_ONLY_VOLUME: UINT32 = 1 << 9;
pub const VP_POST_CLEANUP_WHEN_MODIFIED_ONLY: UINT32 = 1 << 10;
pub const VP_PASS_QUERY_DIRECTORY_PATTERN: UINT32 = 1 << 11;
pub const VP_ALWAYS_USE_DOUBLE_BUFFERING: UINT32 = 1 << 12;
pub const VP_PASS_QUERY_DIRECTORY_FILE_NAME: UINT32 = 1 << 13;
pub const VP_FLUSH_AND_PURGE_ON_CLEANUP: UINT32 = 1 << 14;
pub const VP_DEVICE_CONTROL: UINT32 = 1 << 15;
pub const VP_UM_FILE_CONTEXT_IS_USER_CONTEXT2: UINT32 = 1 << 16;
pub const VP_UM_FILE_CONTEXT_IS_FULL_CONTEXT: UINT32 = 1 << 17;
pub const VP_UM_NO_REPARSE_POINTS_DIR_CHECK: UINT32 = 1 << 18;
pub const VP_ALLOW_OPEN_IN_KERNEL_MODE: UINT32 = 1 << 24;
pub const VP_CASE_PRESERVED_EXTENDED_ATTRIBUTES: UINT32 = 1 << 25;
pub const VP_WSL_FEATURES: UINT32 = 1 << 26;
pub const VP_DIRECTORY_MARKER_AS_NEXT_OFFSET: UINT32 = 1 << 27;
pub const VP_SUPPORTS_POSIX_UNLINK_RENAME: UINT32 = 1 << 29;
pub const VP_POST_DISPOSITION_WHEN_NECESSARY_ONLY: UINT32 = 1 << 30;

/// `FSP_FSCTL_VOLUME_INFO` — filled by `GetVolumeInfo` (88 bytes).
#[repr(C)]
#[derive(Default)]
pub struct FspFsctlVolumeInfo {
    pub total_size: UINT64,
    pub free_size: UINT64,
    pub volume_label_length: UINT16, // in bytes
    pub volume_label: [u16; 32],
}

/// `FSP_FSCTL_FILE_INFO` — per-node metadata block (72 bytes).
#[repr(C)]
#[derive(Clone, Default)]
pub struct FspFsctlFileInfo {
    pub file_attributes: UINT32,
    pub reparse_tag: UINT32,
    pub allocation_size: UINT64,
    pub file_size: UINT64,
    pub creation_time: UINT64,
    pub last_access_time: UINT64,
    pub last_write_time: UINT64,
    pub change_time: UINT64,
    pub index_number: UINT64,
    pub hard_links: UINT32,
    pub ea_size: UINT32,
}

/// `FSP_FSCTL_DIR_INFO` header (104 bytes); the UTF-16 file name starts at
/// `file_name_buf` and `size` covers header + name bytes.
#[repr(C)]
#[derive(Clone, Default)]
pub struct FspFsctlDirInfo {
    pub size: UINT16,
    pub file_info: FspFsctlFileInfo,
    /// Union with a 24-byte padding field; `next_offset` is unused by the
    /// `FspFileSystemAddDirInfo` copy helper (it only reads `size` bytes).
    pub next_offset: UINT64,
    pub _union_pad: [u8; 16],
    pub file_name_buf: [u16; 0],
}

/// A stack-allocated `FSP_FSCTL_DIR_INFO` with room for a 255-unit UTF-16 name
/// (the maximum component length we advertise).
#[repr(C)]
pub struct DirInfoBuf {
    pub info: FspFsctlDirInfo,
    pub name: [u16; 255],
}

impl DirInfoBuf {
    /// A zeroed record.
    #[must_use]
    pub fn new() -> Self {
        Self {
            info: FspFsctlDirInfo {
                size: 0,
                file_info: FspFsctlFileInfo::default(),
                next_offset: 0,
                _union_pad: [0; 16],
                file_name_buf: [],
            },
            name: [0; 255],
        }
    }

    /// Fills the record from a `FileInfo` and a UTF-16 name (names longer than
    /// 255 units are truncated — unreachable through `MaxComponentLength`).
    pub fn set(&mut self, file_info: &FspFsctlFileInfo, name: &[u16]) {
        let name_len = name.len().min(255);
        self.info.file_info = file_info.clone();
        self.name[..name_len].copy_from_slice(&name[..name_len]);
        self.info.size = (size_of::<FspFsctlDirInfo>() + name_len * 2) as UINT16;
    }
}

impl Default for DirInfoBuf {
    fn default() -> Self {
        Self::new()
    }
}

/// Allocation-unit rounding for reported `AllocationSize` (512-byte sectors).
#[must_use]
pub const fn allocation_size(file_size: u64) -> u64 {
    file_size.div_ceil(512) * 512
}

// ---------------------------------------------------------------------------
// The interface (vtable) — FSP_FILE_SYSTEM_INTERFACE, WinFsp 2.x: exactly 64
// slots; the first 33 are named, the last 31 are reserved (NULL).
// ---------------------------------------------------------------------------

macro_rules! fsp_cb {
    ($name:ident, ($($arg:ty),*) -> $ret:ty) => {
        /// Callback signature (see `FSP_FILE_SYSTEM_INTERFACE`).
        pub type $name = unsafe extern "C" fn($($arg),*) -> $ret;
    };
}

fsp_cb!(GetVolumeInfoFn, (*mut FspFileSystem, *mut FspFsctlVolumeInfo) -> NTSTATUS);
fsp_cb!(SetVolumeLabelFn, (*mut FspFileSystem, PWSTR, *mut FspFsctlVolumeInfo) -> NTSTATUS);
fsp_cb!(GetSecurityByNameFn, (*mut FspFileSystem, PWSTR, *mut UINT32, PVOID, *mut SIZE_T) -> NTSTATUS);
fsp_cb!(
    CreateFn,
    (*mut FspFileSystem, PWSTR, UINT32, UINT32, UINT32, PVOID, UINT64, *mut PVOID, *mut FspFsctlFileInfo) -> NTSTATUS
);
fsp_cb!(OpenFn, (*mut FspFileSystem, PWSTR, UINT32, UINT32, *mut PVOID, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(OverwriteFn, (*mut FspFileSystem, PVOID, UINT32, BOOLEAN, UINT64, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(CleanupFn, (*mut FspFileSystem, PVOID, PWSTR, ULONG) -> ());
fsp_cb!(CloseFn, (*mut FspFileSystem, PVOID) -> ());
fsp_cb!(ReadFn, (*mut FspFileSystem, PVOID, PVOID, UINT64, ULONG, *mut ULONG) -> NTSTATUS);
fsp_cb!(
    WriteFn,
    (*mut FspFileSystem, PVOID, PVOID, UINT64, ULONG, BOOLEAN, BOOLEAN, *mut ULONG, *mut FspFsctlFileInfo) -> NTSTATUS
);
fsp_cb!(FlushFn, (*mut FspFileSystem, PVOID, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(GetFileInfoFn, (*mut FspFileSystem, PVOID, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(
    SetBasicInfoFn,
    (*mut FspFileSystem, PVOID, UINT32, UINT64, UINT64, UINT64, UINT64, *mut FspFsctlFileInfo) -> NTSTATUS
);
fsp_cb!(SetFileSizeFn, (*mut FspFileSystem, PVOID, UINT64, BOOLEAN, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(CanDeleteFn, (*mut FspFileSystem, PVOID, PWSTR) -> NTSTATUS);
fsp_cb!(RenameFn, (*mut FspFileSystem, PVOID, PWSTR, PWSTR, BOOLEAN) -> NTSTATUS);
fsp_cb!(GetSecurityFn, (*mut FspFileSystem, PVOID, PVOID, *mut SIZE_T) -> NTSTATUS);
fsp_cb!(SetSecurityFn, (*mut FspFileSystem, PVOID, UINT32, PVOID) -> NTSTATUS);
fsp_cb!(ReadDirectoryFn, (*mut FspFileSystem, PVOID, PWSTR, PWSTR, PVOID, ULONG, *mut ULONG) -> NTSTATUS);
fsp_cb!(
    ResolveReparsePointsFn,
    (*mut FspFileSystem, PWSTR, UINT32, BOOLEAN, PVOID, PVOID, *mut SIZE_T) -> NTSTATUS
);
fsp_cb!(GetReparsePointFn, (*mut FspFileSystem, PVOID, PWSTR, PVOID, *mut SIZE_T) -> NTSTATUS);
fsp_cb!(SetReparsePointFn, (*mut FspFileSystem, PVOID, PWSTR, PVOID, SIZE_T) -> NTSTATUS);
fsp_cb!(DeleteReparsePointFn, (*mut FspFileSystem, PVOID, PWSTR, PVOID, SIZE_T) -> NTSTATUS);
fsp_cb!(GetStreamInfoFn, (*mut FspFileSystem, PVOID, PVOID, ULONG, *mut ULONG) -> NTSTATUS);
fsp_cb!(GetDirInfoByNameFn, (*mut FspFileSystem, PVOID, PWSTR, *mut FspFsctlDirInfo) -> NTSTATUS);
fsp_cb!(
    ControlFn,
    (*mut FspFileSystem, PVOID, UINT32, PVOID, ULONG, PVOID, ULONG, *mut ULONG) -> NTSTATUS
);
fsp_cb!(SetDeleteFn, (*mut FspFileSystem, PVOID, PWSTR, BOOLEAN) -> NTSTATUS);
fsp_cb!(
    CreateExFn,
    (*mut FspFileSystem, PWSTR, UINT32, UINT32, UINT32, PVOID, UINT64, PVOID, ULONG, BOOLEAN, *mut PVOID, *mut FspFsctlFileInfo) -> NTSTATUS
);
fsp_cb!(
    OverwriteExFn,
    (*mut FspFileSystem, PVOID, UINT32, BOOLEAN, UINT64, PVOID, ULONG, *mut FspFsctlFileInfo) -> NTSTATUS
);
fsp_cb!(GetEaFn, (*mut FspFileSystem, PVOID, PVOID, ULONG, *mut ULONG) -> NTSTATUS);
fsp_cb!(SetEaFn, (*mut FspFileSystem, PVOID, PVOID, ULONG, *mut FspFsctlFileInfo) -> NTSTATUS);
fsp_cb!(Obsolete0Fn, () -> NTSTATUS);
fsp_cb!(DispatcherStoppedFn, (*mut FspFileSystem, BOOLEAN) -> ());

/// The vtable handed to `FspFileSystemCreate`. `None` slots are NULL — WinFsp
/// checks for NULL before calling every optional operation.
#[repr(C)]
pub struct FspFileSystemInterface {
    pub get_volume_info: Option<GetVolumeInfoFn>,
    pub set_volume_label: Option<SetVolumeLabelFn>,
    pub get_security_by_name: Option<GetSecurityByNameFn>,
    pub create: Option<CreateFn>,
    pub open: Option<OpenFn>,
    pub overwrite: Option<OverwriteFn>,
    pub cleanup: Option<CleanupFn>,
    pub close: Option<CloseFn>,
    pub read: Option<ReadFn>,
    pub write: Option<WriteFn>,
    pub flush: Option<FlushFn>,
    pub get_file_info: Option<GetFileInfoFn>,
    pub set_basic_info: Option<SetBasicInfoFn>,
    pub set_file_size: Option<SetFileSizeFn>,
    pub can_delete: Option<CanDeleteFn>,
    pub rename: Option<RenameFn>,
    pub get_security: Option<GetSecurityFn>,
    pub set_security: Option<SetSecurityFn>,
    pub read_directory: Option<ReadDirectoryFn>,
    pub resolve_reparse_points: Option<ResolveReparsePointsFn>,
    pub get_reparse_point: Option<GetReparsePointFn>,
    pub set_reparse_point: Option<SetReparsePointFn>,
    pub delete_reparse_point: Option<DeleteReparsePointFn>,
    pub get_stream_info: Option<GetStreamInfoFn>,
    pub get_dir_info_by_name: Option<GetDirInfoByNameFn>,
    pub control: Option<ControlFn>,
    pub set_delete: Option<SetDeleteFn>,
    pub create_ex: Option<CreateExFn>,
    pub overwrite_ex: Option<OverwriteExFn>,
    pub get_ea: Option<GetEaFn>,
    pub set_ea: Option<SetEaFn>,
    pub obsolete0: Option<Obsolete0Fn>,
    pub dispatcher_stopped: Option<DispatcherStoppedFn>,
    pub reserved: [Option<unsafe extern "C" fn() -> NTSTATUS>; 31],
}

// ---------------------------------------------------------------------------
// FSP_FILE_SYSTEM — only the prefix we touch is declared; the object is
// allocated and owned by the DLL. (Full size is 792 bytes on x64.)
// ---------------------------------------------------------------------------

/// `FSP_FILE_SYSTEM` prefix: `version` at offset 0, `user_context` follows at
/// pointer alignment. Everything after `user_context` is owned by the DLL.
#[repr(C)]
pub struct FspFileSystem {
    pub version: UINT16,
    /// Free-form pointer for the FS implementation — we store `*const Host`.
    pub user_context: PVOID,
}

// ---------------------------------------------------------------------------
// Compile-time ABI verification — these run on any host, no driver required.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_the_published_abi() {
        // The same numbers the WinFsp headers static_assert:
        assert_eq!(size_of::<FspFsctlVolumeParams>(), 504, "FSP_FSCTL_VOLUME_PARAMS");
        assert_eq!(size_of::<FspFsctlVolumeInfo>(), 88, "FSP_FSCTL_VOLUME_INFO");
        assert_eq!(size_of::<FspFsctlFileInfo>(), 72, "FSP_FSCTL_FILE_INFO");
        assert_eq!(size_of::<FspFsctlDirInfo>(), 104, "FSP_FSCTL_DIR_INFO");
        assert_eq!(
            size_of::<FspFileSystemInterface>(),
            64 * size_of::<*mut c_void>(),
            "FSP_FILE_SYSTEM_INTERFACE must be 64 fn pointers"
        );
        // FileNameBuf follows the 24-byte union: header is exactly 104 bytes.
        let d = DirInfoBuf::new();
        let base = &raw const d as usize;
        assert_eq!((&raw const d.info.file_name_buf) as usize - base, 104);
        // user_context sits at pointer alignment (8 on 64-bit).
        assert!(size_of::<FspFileSystem>() >= 8 + size_of::<PVOID>());
    }

    #[test]
    fn dir_info_buf_records_size() {
        let mut b = DirInfoBuf::new();
        let name: Vec<u16> = "file.txt".encode_utf16().collect();
        b.set(&FspFsctlFileInfo { file_size: 5, ..Default::default() }, &name);
        assert_eq!(b.info.size as usize, 104 + name.len() * 2);
        assert_eq!(b.info.file_info.file_size, 5);
        // A name longer than the buffer is truncated, never overflows.
        let long_name: Vec<u16> = vec![b'a' as u16; 400];
        b.set(&FspFsctlFileInfo::default(), &long_name);
        assert_eq!(b.info.size as usize, 104 + 255 * 2);
    }

    #[test]
    fn allocation_rounding() {
        assert_eq!(allocation_size(0), 0);
        assert_eq!(allocation_size(1), 512);
        assert_eq!(allocation_size(512), 512);
        assert_eq!(allocation_size(513), 1024);
    }
}
