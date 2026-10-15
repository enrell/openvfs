//! NTSTATUS codes and the [`VfsError`] → status mapping.
//!
//! This module is platform-independent: the constants are fixed by Windows and
//! the mapping is pure logic, so it compiles (and is unit-tested) everywhere.

use openvfs::VfsError;

/// NT status code type (`NTSTATUS`, a signed 32-bit value).
pub type NtStatus = i32;

pub const STATUS_SUCCESS: NtStatus = 0x0000_0000;
pub const STATUS_PENDING: NtStatus = 0x0000_0103;
pub const STATUS_REPARSE: NtStatus = 0x0000_0104;
pub const STATUS_BUFFER_OVERFLOW: NtStatus = -0x7FFF_FFFB; // 0x80000005
pub const STATUS_NO_MORE_FILES: NtStatus = -0x7FFF_FFFA; // 0x80000006
pub const STATUS_UNSUCCESSFUL: NtStatus = -0x3FFF_FFFF; // 0xC0000001
pub const STATUS_NOT_IMPLEMENTED: NtStatus = -0x3FFF_FFFE; // 0xC0000002
pub const STATUS_INVALID_INFO_CLASS: NtStatus = -0x3FFF_FFFD; // 0xC0000003
pub const STATUS_INVALID_HANDLE: NtStatus = -0x3FFF_FFF8; // 0xC0000008
pub const STATUS_INVALID_PARAMETER: NtStatus = -0x3FFF_FFF3; // 0xC000000D
pub const STATUS_NO_SUCH_DEVICE: NtStatus = -0x3FFF_FFF2; // 0xC000000E
pub const STATUS_NO_SUCH_FILE: NtStatus = -0x3FFF_FFF1; // 0xC000000F
pub const STATUS_INVALID_DEVICE_REQUEST: NtStatus = -0x3FFF_FFF0; // 0xC0000010
pub const STATUS_END_OF_FILE: NtStatus = -0x3FFF_FFEF; // 0xC0000011
pub const STATUS_MORE_PROCESSING_REQUIRED: NtStatus = -0x3FFF_FFE6; // 0xC0000016
pub const STATUS_ACCESS_DENIED: NtStatus = -0x3FFF_FFDE; // 0xC0000022
pub const STATUS_BUFFER_TOO_SMALL: NtStatus = -0x3FFF_FFDD; // 0xC0000023
pub const STATUS_OBJECT_NAME_INVALID: NtStatus = -0x3FFF_FFCD; // 0xC0000033
pub const STATUS_OBJECT_NAME_NOT_FOUND: NtStatus = -0x3FFF_FFCC; // 0xC0000034
pub const STATUS_OBJECT_NAME_COLLISION: NtStatus = -0x3FFF_FFCB; // 0xC0000035
pub const STATUS_OBJECT_PATH_NOT_FOUND: NtStatus = -0x3FFF_FFC6; // 0xC000003A
pub const STATUS_OBJECT_PATH_SYNTAX_BAD: NtStatus = -0x3FFF_FFC5; // 0xC000003B
pub const STATUS_SHARING_VIOLATION: NtStatus = -0x3FFF_FFBD; // 0xC0000043
pub const STATUS_DELETE_PENDING: NtStatus = -0x3FFF_FFAA; // 0xC0000056
pub const STATUS_PRIVILEGE_NOT_HELD: NtStatus = -0x3FFF_FFA4; // 0xC0000061
pub const STATUS_DISK_FULL: NtStatus = -0x3FFF_FF81; // 0xC000007F
pub const STATUS_INSUFFICIENT_RESOURCES: NtStatus = -0x3FFF_FF66; // 0xC000009A
pub const STATUS_MEDIA_WRITE_PROTECTED: NtStatus = -0x3FFF_FF5E; // 0xC00000A2
pub const STATUS_FILE_IS_A_DIRECTORY: NtStatus = -0x3FFF_FF46; // 0xC00000BA
pub const STATUS_NOT_SUPPORTED: NtStatus = -0x3FFF_FF45; // 0xC00000BB
pub const STATUS_NOT_SAME_DEVICE: NtStatus = -0x3FFF_FF2C; // 0xC00000D4
pub const STATUS_INTERNAL_ERROR: NtStatus = -0x3FFF_FF1B; // 0xC00000E5
pub const STATUS_UNEXPECTED_IO_ERROR: NtStatus = -0x3FFF_FF17; // 0xC00000E9
pub const STATUS_NOT_A_DIRECTORY: NtStatus = -0x3FFF_FEFD; // 0xC0000103
pub const STATUS_DIRECTORY_NOT_EMPTY: NtStatus = -0x3FFF_FEFF; // 0xC0000101
pub const STATUS_CANNOT_DELETE: NtStatus = -0x3FFF_FEDF; // 0xC0000121
pub const STATUS_FILE_CLOSED: NtStatus = -0x3FFF_FED8; // 0xC0000128

/// `true` when `status` is an NT success code (`STATUS_*` severity 0/1).
#[must_use]
pub const fn nt_success(status: NtStatus) -> bool {
    status >= 0
}

/// Maps a [`VfsError`] to the closest NTSTATUS.
///
/// `ancestor` chooses the variants used for "a path component was not a
/// directory" / "missing" when the error surfaced while resolving a *parent*
/// (`STATUS_OBJECT_PATH_NOT_FOUND`) rather than the final component
/// (`STATUS_OBJECT_NAME_NOT_FOUND`). Windows tools generally prefer the path
/// variant for missing parents and it makes diagnosis clearer.
#[must_use]
pub fn status_of(err: &VfsError, ancestor: bool) -> NtStatus {
    match err {
        VfsError::NotFound(_) => {
            if ancestor {
                STATUS_OBJECT_PATH_NOT_FOUND
            } else {
                STATUS_OBJECT_NAME_NOT_FOUND
            }
        }
        VfsError::AlreadyExists(_) => STATUS_OBJECT_NAME_COLLISION,
        VfsError::NotADirectory(_) => {
            if ancestor {
                STATUS_OBJECT_PATH_NOT_FOUND
            } else {
                STATUS_NOT_A_DIRECTORY
            }
        }
        VfsError::IsADirectory(_) => STATUS_FILE_IS_A_DIRECTORY,
        VfsError::DirectoryNotEmpty(_) => STATUS_DIRECTORY_NOT_EMPTY,
        VfsError::ReadOnly(_) => STATUS_MEDIA_WRITE_PROTECTED,
        VfsError::PermissionDenied(_) | VfsError::PathEscapesRoot(_) => STATUS_ACCESS_DENIED,
        VfsError::StorageFull(_) => STATUS_DISK_FULL,
        VfsError::CrossDevice { .. } => STATUS_NOT_SAME_DEVICE,
        VfsError::InvalidPath(_) => STATUS_OBJECT_NAME_INVALID,
        VfsError::InvalidInput(_) => STATUS_INVALID_PARAMETER,
        VfsError::Io { source, .. } => status_of_io(source.kind()),
        // `VfsError` is non_exhaustive — future variants degrade to a generic
        // I/O failure rather than breaking compilation.
        _ => STATUS_UNEXPECTED_IO_ERROR,
    }
}

/// Maps an [`std::io::ErrorKind`] to the closest NTSTATUS (used for handle I/O
/// failures that surface inside `Read`/`Write`/`Flush`).
#[must_use]
pub fn status_of_io(kind: std::io::ErrorKind) -> NtStatus {
    match kind {
        std::io::ErrorKind::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
        std::io::ErrorKind::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => STATUS_INVALID_PARAMETER,
        std::io::ErrorKind::WriteZero => STATUS_END_OF_FILE,
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => STATUS_DISK_FULL,
        std::io::ErrorKind::ReadOnlyFilesystem => STATUS_MEDIA_WRITE_PROTECTED,
        std::io::ErrorKind::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        std::io::ErrorKind::NotADirectory => STATUS_NOT_A_DIRECTORY,
        std::io::ErrorKind::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        std::io::ErrorKind::CrossesDevices => STATUS_NOT_SAME_DEVICE,
        _ => STATUS_UNEXPECTED_IO_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_ntdef() {
        assert_eq!(STATUS_SUCCESS, 0);
        assert_eq!(STATUS_BUFFER_OVERFLOW as u32, 0x8000_0005);
        assert_eq!(STATUS_UNSUCCESSFUL as u32, 0xC000_0001);
        assert_eq!(STATUS_ACCESS_DENIED as u32, 0xC000_0022);
        assert_eq!(STATUS_OBJECT_NAME_NOT_FOUND as u32, 0xC000_0034);
        assert_eq!(STATUS_DIRECTORY_NOT_EMPTY as u32, 0xC000_0101);
        assert_eq!(STATUS_FILE_IS_A_DIRECTORY as u32, 0xC000_00BA);
        assert_eq!(STATUS_MEDIA_WRITE_PROTECTED as u32, 0xC000_00A2);
        assert_eq!(STATUS_CANNOT_DELETE as u32, 0xC000_0121);
        assert_eq!(STATUS_SHARING_VIOLATION as u32, 0xC000_0043);
    }

    #[test]
    fn success_codes_are_non_negative() {
        assert!(nt_success(STATUS_SUCCESS));
        assert!(nt_success(STATUS_PENDING));
        // `NT_SUCCESS` is simply `>= 0`: "informational" severities like
        // BUFFER_OVERFLOW (0x80000005) do NOT count.
        assert!(!nt_success(STATUS_BUFFER_OVERFLOW));
        assert!(!nt_success(STATUS_ACCESS_DENIED));
    }

    #[test]
    fn vfs_error_mapping() {
        let p = openvfs::VPath::new("/x").unwrap();
        assert_eq!(status_of(&VfsError::NotFound(p.clone()), false), STATUS_OBJECT_NAME_NOT_FOUND);
        assert_eq!(status_of(&VfsError::NotFound(p.clone()), true), STATUS_OBJECT_PATH_NOT_FOUND);
        assert_eq!(status_of(&VfsError::AlreadyExists(p.clone()), false), STATUS_OBJECT_NAME_COLLISION);
        assert_eq!(status_of(&VfsError::ReadOnly(p.clone()), false), STATUS_MEDIA_WRITE_PROTECTED);
        assert_eq!(status_of(&VfsError::DirectoryNotEmpty(p.clone()), false), STATUS_DIRECTORY_NOT_EMPTY);
    }
}
