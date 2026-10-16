# Changelog

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project follows
[SemVer](https://semver.org/).

## [Unreleased]

### Added
- `VPath::folded()` / `FoldedPath` and `VPath::eq_ignore_case`: Unicode-lowercase keys that compare and hash
  case-insensitively while the original spelling is preserved for display and file access (`open-vfs`).
- **`openvfs-mods`** — the mod-management layer:
  - `MergedIndex`: a snapshot over ordered layers (`layers[0]` wins) keyed by folded path — `Textures/` and
    `textures/` merge — answering lookups, listings and provenance without touching the file systems; explicit
    `invalidate`/`rebuild`; ~700 ms over 100 thousand files in 500 layers.
  - `CaseInsensitiveFs`: a `FileSystem` adapter resolving each component case-insensitively, preserving real casing.
  - `Profile`: an ordered mod list (enable/disable/reorder, JSON persistence) that assembles an `OverlayFs` with an
    `overwrite` upper; missing mods are skipped with diagnostics, a missing game directory is an error.
  - `ConflictReport`: winner + ordered losers for every contested path, with per-mod counts.
  - `deploy`: plans the diff between the game directory and the merged index, then applies it transactionally —
    hardlinks with cross-volume copy fallback, a write-ahead JSONL journal (resume/rollback after a crash),
    a SHA-256 manifest, originals preserved under `backup/` for byte-for-byte `undeploy`, drift detection via
    `verify`, and `capture` of game writes into the overwrite directory. `PlanOptions::copy_under` for subtrees
    the game writes in place (see the hardlink caveat in the module docs).
  - Structured logging via the `log` facade and an `openvfs-mods` CLI: `profile`, `conflicts`, `deploy`,
    `undeploy`, `verify`, `capture`, `status`, `resume`, `rollback`, `game`.
- **`openvfs-winfsp`** (Windows): mounts any `FileSystem` through WinFsp, bound dynamically at runtime — no SDK at
  build time; 20 filesystem callbacks implemented. The runtime DLL is discovered beyond `PATH` (registry
  `InstallDir`, the `WinFsp.Launcher` service image, `Program Files` and versioned `SxS` layouts), and directory
  mount points follow junction semantics — WinFsp creates and removes them. Real-mount tests run when the driver
  is installed and self-skip otherwise.
- `openvfs-mods mount <file> <dir-or-drive>` (Windows): serves the profile's merged view as a real filesystem —
  every process sees it without deployment or injection; writes land in `overwrite`, deletes persist in a
  `<profile>.whiteouts` sidecar across mounts.
- `OverlayFs::hide`: records a whiteout without deleting anything, to replay a persisted `whiteouts()` set.
- **`openvfs-hook`** (Windows): per-process merged view via Win32 API detours (`ilhook` engine): `CreateFile*`,
  `Find*`, attributes, delete/rename/copy/move, writes captured to `overwrite`, whiteouts; injector + synthetic
  game verified end-to-end live. Foundation grade — see `docs/hook-spec.md` for coverage and remaining work.
- `docs/recovery.md`: journal/manifest layout and recovery procedures for the deployment engine.

### Fixed
- `openvfs-mods` deploy: a deployed file displacing a *directory* recorded no backup (unrestorable), and a file
  displaced by a created directory had no manifest entry to hang one on — undeploy now restores both.
- `openvfs-mods` CLI: profile paths stored relative depended on the caller's CWD; they are absolutized on load.
- `PhysicalFs::read_dir` returns entries sorted by name. On Linux and macOS the on-disk order is arbitrary, which made
  operations such as `copy_tree` fail at different points than the other backends.
- Tests pass on Linux, macOS, Windows and Rust 1.88 (CI ran for the first time in this version).

### Tests
- Deployment fault injection at every plan position (crash → resume converges / rollback restores), a
  deployed-vs-virtual differential suite, and randomized priority-ordering properties.
- WinFsp real-mount suite (`mount_windows`): roundtrip and OS-vs-VFS differential through the actual driver,
  verified against WinFsp 2.1+ (SxS layout).
- ~890 tests total.

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
