# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[SemVer](https://semver.org/).

## [0.1.2] — 2026-10-23

### Removed
- `openvfs-mods` (mod-management layer and CLI) — openvfs is the bridge between applications and a VFS;
  mod management belongs in an application built on it, not in this workspace.

### Added
- `VPath::folded()` / `FoldedPath` and `VPath::eq_ignore_case`: Unicode-lowercase keys that compare and hash
  case-insensitively while the original spelling is preserved for display and file access (`open-vfs`).
- **`openvfs-winfsp`** (Windows): mounts any `FileSystem` through WinFsp, bound dynamically at runtime — no SDK at
  build time; 20 filesystem callbacks implemented. The runtime DLL is discovered beyond `PATH` (registry
  `InstallDir`, the `WinFsp.Launcher` service image, `Program Files` and versioned `SxS` layouts), and directory
  mount points follow junction semantics — WinFsp creates and removes them. Real-mount tests run when the driver
  is installed and self-skip otherwise.
- `OverlayFs::hide`: records a whiteout without deleting anything, to replay a persisted `whiteouts()` set.
- **`openvfs-hook`** (Windows): per-process merged view via Win32 API detours (`ilhook` engine): `CreateFile*`,
  `Find*`, attributes, delete/rename/copy/move, writes captured to `overwrite`, whiteouts; injector + synthetic
  program verified end-to-end live. Foundation grade — see `docs/hook-spec.md` for coverage and remaining work.

- Read-only `PhysicalFs` instances cache metadata and listings, and `OverlayFs` caches merged lookups, dropping its
  caches whenever a write changes the view. A view over a hundred layers no longer walks every layer per lookup.
- `OPENVFS_WINFSP_LOG` (`openvfs-winfsp`): an opt-in log of the WinFsp callbacks, for diagnosing stalls, and the
  `replay` example, which replays such a log against the host logic with no driver and times each kind of call.

### Changed
- **Lookups and listings over many layers are fast enough to run a game from a mount.** A game started from a view
  of 113 mods sat without a window while the host burned a core: listing one 2,452-file folder took ~10 s. Now it
  takes ~0.13 s, and replaying the game's first minute of file-system calls went from 160 s to under 3 s:
  - a lookup in a read-only layer is answered from its cached parent: a path under a missing folder costs no system
    call, and a name the folder does not hold is ruled out from the folder's names, read once;
  - a missing path is classified from the walk that found it missing, instead of re-examining every ancestor with
    two failing system calls each (this was most of the cost: every lookup misses in the empty upper layer);
  - `ReadDirectory` lists a directory once per scan and serves every page from that snapshot, instead of listing,
    sorting and re-reading the metadata of the whole directory for every page the kernel asks for.

## [0.1.1] — 2026-10-15

### Fixed
- `PhysicalFs::read_dir` returns entries sorted by name. On Linux and macOS the on-disk order is arbitrary, which made
  operations such as `copy_tree` fail at different points than the other backends.
- Tests pass on Linux, macOS, Windows and Rust 1.88 (CI ran for the first time in this version).

## [0.1.0] — 2026-10-15

First version under the name **openvfs** (it was a prototype called "strata").

### Added
- Package ready to be used as a dependency: README and LICENSE inside the crate, `cargo package` validated and tested
  from an external project; `openvfs-sandbox` marked `publish = false`.
- `VfsFile::set_len` and `VfsFile::sync_all`; `Vfs::set_len`.
- `Vfs::write_atomic` / `ops::write_atomic`: readers see the old content or the new, never a mixture.
- `Vfs::try_exists`, which distinguishes "does not exist" from "could not be checked".
- `FileSystem::remove_dir_all` and `FileSystem::copy_file`, with default implementations and atomic or native versions
  in the backends.
- `MemoryFs::with_quota`, `used_bytes`, `quota`.
- `PhysicalFs::builder` with a link policy (`Symlinks::Deny` by default, or `Symlinks::FollowWithinRoot`).
- `Walk::max_depth` and `Walk::filter_entry` (prune subtrees).
- `OverlayFs::whiteouts`.
- `VfsError::PermissionDenied` and `VfsError::StorageFull` errors; `VfsError::kind()`.
- `Debug` for `dyn VfsFile`, `Borrow<str>` for `VPath`.

### Fixed
- **Data loss:** `copy(a, a)` truncated the file. It now returns `InvalidInput`.
- **Windows:** `CON`, `NUL.txt`, `COM1` etc. were accepted and writes went to the device. Names ending in `.` or a space
  created aliases. All are now rejected.
- **Overlay:** a failure during copy-up (disk full, quota, I/O) left a partial file shadowing the original.
- **Overlay:** renaming a lower-layer file over another deleted the destination before copying.
- **Overlay:** `metadata().read_only` was `true` for files the overlay can write.
- **Mount:** `rename` across mounts *merged* with an existing directory at the destination.
- **Consistency:** the same error had a different variant and path depending on the backend. There is now a single
  contract, covered by the conformance and differential suites.
- **Symlinks:** `walk` looped forever on circular links.

### Performance
- `MemoryFs::read_dir` cost O(subtree), listing every descendant and filtering out the children: listing `/` with two
  children took 1.8 ms with 100 thousand nodes below. It is now O(children · log n) and independent of subtree size.
- `MountFs` scanned the whole mount table in almost every operation; it now uses range queries and the cost does not
  depend on the number of mounts (9 µs → ~1.7 µs with 1024 mounts).
- `PhysicalFs` with `Symlinks::Deny` reuses the metadata fetched by the link check (one fewer system call in
  `metadata` and `open`).
- `examples/bench.rs`: dependency-free micro-benchmarks for every backend.
- `tests/it/scaling.rs`: complexity contracts (listing a directory does not depend on subtree size; resolving a path
  does not depend on the number of mounts).

### Test speed
- The 10 integration test files became **a single binary** (`tests/it/main.rs`; run one suite with
  `cargo test -p open-vfs --test it differential::`). They used to be 10 executables, each recompiling `common/`,
  linking separately and paying the antivirus scan on first run. Measured on Windows: rebuilding after a code edit
  dropped from ~17 s to ~2 s, the suites went from ~8 s sequentially to ~2 s in parallel, and a full
  `cargo test --workspace` from ~25 s to ~10 s.
- The differential test spreads each backend's histories across threads (`OPENVFS_DIFF_THREADS`, default: cores, at
  most 8). The CI `deep` run, with 3000 histories per backend, dropped from ~500 s to ~210 s; the default run from
  ~1.6 s to ~0.7 s. The first failure stops the other threads and is re-raised with the original message and the seed
  to replay.
- Since the suites now run together, a deadlock in any test (including those without their own watchdog) held up the
  whole binary. There is now a deadline for the whole process (`OPENVFS_TEST_DEADLINE_SECS`, default 180 s; the CI
  `deep` job uses 7200 s): when it expires the process aborts with exit code 101 and a message pointing at the
  suspect tests. The differential watchdog, previously fixed at 1 h, is now proportional to the work requested
  (~2 min by default).

### Fixed in tests
- `atomic_writes_are_never_observed_torn` was flaky (it failed in ~1 of 3 runs and could pass without exercising
  anything): the writer finished before the reader threads had started. It now only finishes after a minimum number of
  reads that really overlapped the writes.
- The overlay's concurrent rename test fails fast, with the observer's message, instead of waiting for the watchdog.

### Changed
- The overlay became linearizable: structural operations are serialized.
- `commit` and `changes` stream content instead of loading whole files into memory.
- MSRV 1.88.
