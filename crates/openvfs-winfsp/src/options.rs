//! Mount-time options for [`crate::Mount`].

/// Options accepted by [`crate::Mount::mount`].
///
/// Defaults are chosen to behave like a plain local disk:
/// a 1 TiB volume reported half full, writable when the backend is.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MountOptions {
    /// Volume label reported to Windows (≤ 31 UTF-16 units used).
    /// Default: `"OpenVFS"`.
    pub volume_label: String,
    /// File system name reported by `GetVolumeInformation` calls that ask for
    /// it (≤ 15 UTF-16 units used). Default: `"OpenVFS"`.
    pub file_system_name: String,
    /// Total capacity to report, in bytes. Purely cosmetic — backends may be
    /// unbounded or grow on demand. Default: 1 TiB.
    pub total_size: u64,
    /// Free space to report, in bytes (clamped to `total_size`).
    /// Default: `total_size / 2`.
    pub free_size: u64,
    /// Volume serial number. Default: `0x4F504656` ("OPVF").
    pub volume_serial: u32,
    /// Force the mount read-only even when the backend accepts writes
    /// (`STATUS_MEDIA_WRITE_PROTECTED` on writes). The backend's own
    /// [`openvfs::FileSystem::is_read_only`] also applies.
    pub read_only: bool,
    /// WinFsp dispatcher thread count; `0` = WinFsp's default (recommended).
    pub dispatcher_threads: u32,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self {
            volume_label: "OpenVFS".to_owned(),
            file_system_name: "OpenVFS".to_owned(),
            total_size: 1 << 40, // 1 TiB
            free_size: 1 << 39,  // 512 GiB
            volume_serial: 0x4F50_4656,
            read_only: false,
            dispatcher_threads: 0,
        }
    }
}

impl MountOptions {
    /// Fluent [`MountOptions::volume_label`].
    #[must_use]
    pub fn volume_label(mut self, label: impl Into<String>) -> Self {
        self.volume_label = label.into();
        self
    }

    /// Fluent [`MountOptions::file_system_name`].
    #[must_use]
    pub fn file_system_name(mut self, name: impl Into<String>) -> Self {
        self.file_system_name = name.into();
        self
    }

    /// Fluent [`MountOptions::total_size`] / [`MountOptions::free_size`].
    /// `free_size` is clamped to `total_size` at mount time.
    #[must_use]
    pub fn reported_sizes(mut self, total: u64, free: u64) -> Self {
        self.total_size = total;
        self.free_size = free;
        self
    }

    /// Fluent [`MountOptions::volume_serial`].
    #[must_use]
    pub fn volume_serial(mut self, serial: u32) -> Self {
        self.volume_serial = serial;
        self
    }

    /// Fluent [`MountOptions::read_only`].
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Fluent [`MountOptions::dispatcher_threads`].
    #[must_use]
    pub fn dispatcher_threads(mut self, threads: u32) -> Self {
        self.dispatcher_threads = threads;
        self
    }
}
