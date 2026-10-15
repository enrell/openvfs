# openvfs

**A layered virtual file system (VFS) for Rust.** No external dependencies, `#![forbid(unsafe_code)]`, MIT license.

openvfs gives you one file API over interchangeable storage that can be combined freely: memory, disk, mount tables
and *copy-on-write* layers with change tracking. Every backend passes the **same conformance and differential test
suites**, so your code behaves the same wherever the bytes live.

```rust
use openvfs::prelude::*;

let mounts = MountFs::new();
mounts.mount("/", MemoryFs::with_quota(64 << 20))?;                       // RAM root, up to 64 MiB
mounts.mount("/assets", ReadOnlyFs::new(PhysicalFs::new("./assets")?))?;  // host directory, read-only

let vfs = Vfs::new(mounts);
vfs.create_dir_all("/save/slot1")?;
vfs.write_atomic("/save/slot1/state.json", r#"{"level": 3}"#)?;          // never half-written
let logo = vfs.read("/assets/logo.png")?;
```

> Clean-room implementation: written from scratch, without consulting the code of other VFS libraries.

## Using it in other projects

The package is published on crates.io as `open-vfs`; the library is imported as `openvfs` (no dependencies, MSRV 1.88).
In your project's `Cargo.toml`:

```toml
[dependencies]
open-vfs = "0.1"   # in code: use openvfs::prelude::*;
```

Write your functions against `&Vfs` (or `&dyn FileSystem`): the backend becomes the caller's choice, and tests just
pass `Vfs::memory()`. The crate README ([`crates/openvfs/README.md`](crates/openvfs/README.md)) has a minimal example
and the limits to know about. The other workspace crates (`openvfs-mods`, `openvfs-winfsp`, `openvfs-hook`,
`openvfs-sandbox`) are applications/infrastructure (`publish = false`).

## Use case: game mod management

The workspace also contains a complete mod-management pipeline — the result an unmodified game needs is a directory
that *looks* merged by mod priority without ever touching the originals. Two tracks deliver it:

- **Deployment** ([`openvfs-mods`](crates/openvfs-mods)): profiles (an ordered, JSON-persisted mod list), a
  case-insensitive merged index (`FoldedPath` makes `Textures/` and `textures/` the same directory), a conflict
  report, and a **transactional deployer** that plants the winning files into the game directory as hardlinks
  (copy fallback), journaling every step so a crash mid-deploy can be resumed or rolled back. Game writes are
  captured into an `overwrite` directory, and `undeploy` restores the game directory byte-for-byte. Ships as a
  library plus the `openvfs-mods` CLI; see [`docs/recovery.md`](docs/recovery.md).
- **Real virtualization** ([`openvfs-winfsp`](crates/openvfs-winfsp), Windows): mounts any `FileSystem` as a real
  drive/directory via [WinFsp](https://winfsp.dev) — bound dynamically at runtime, so no SDK is needed to build.
- **Per-process interception** ([`openvfs-hook`](crates/openvfs-hook), Windows): a DLL that gives one process a
  merged view by detouring Win32 file APIs — the mechanism class that tools like Mod Organizer use, built
  clean-room. Foundation-grade; see [`docs/hook-spec.md`](docs/hook-spec.md) for the coverage matrix and risks.

## Architecture

```
                ┌───────────────────────────────┐
  your code ───▶│ Vfs  (std::fs-style API)      │  read, write_atomic, walk, copy_dir_all, try_exists…
                └──────────────┬────────────────┘
                               │ trait FileSystem: 7 primitives + 2 with default implementations
       ┌───────────┬───────────┼────────────┬──────────────┐
   MemoryFs    PhysicalFs   ReadOnlyFs    MountFs       OverlayFs
   RAM, quota, confined     wrapper       mount         writable upper over
   atomic      directory,                 table         read-only lowers,
   operations  link policy                              changes / commit / reset
```

| Component | Guarantees |
|---|---|
| [`VPath`](crates/openvfs/src/path.rs) | absolute, normalized, `..` stops at the root, case-sensitive comparison |
| [`VfsError`](crates/openvfs/src/error.rs) | typed variants; **always reports the path you passed**; converts to `io::Error` losslessly |
| [`MemoryFs`](crates/openvfs/src/memory.rs) | every operation is atomic and linearizable; byte quota; Unix-style handles (they survive `unlink` and rename) |
| [`PhysicalFs`](crates/openvfs/src/physical.rs) | confinement against hostile paths, Windows device names and symlinks (see [SECURITY.md](SECURITY.md)); listings sorted by name |
| [`MountFs`](crates/openvfs/src/mount.rs) | longest prefix wins; ancestors of mount points are virtual directories; errors rewritten to the outer path |
| [`OverlayFs`](crates/openvfs/src/overlay.rs) | lower layers are never written; linearizable operations; atomic copy-up; exact `changes()`; idempotent `commit` |
| [`ops`](crates/openvfs/src/ops.rs) | `write_atomic`, `create_dir_all`, recursive copy and `Walk` (ordered, lazy, with `max_depth` and `filter_entry`) |

## The contract

Documented on the [`FileSystem`](crates/openvfs/src/fs.rs) trait and verified by tests, point by point:

- `create_dir` and `open(create)` require the parent to exist; `remove_dir` only removes an empty directory; the root
  is untouchable.
- `rename` replaces a file at the destination, refuses to overwrite a directory, and refuses to move a directory into
  itself; renaming something to its own name does nothing.
- `copy_file` refuses to copy a file onto itself (instead of truncating it).
- A path that goes through a file (`/file.txt/x`) yields `NotADirectory`; a missing path yields `NotFound`.
- The order of the checks is the same in every backend, so the reported error is the same too.

## Use case: `openvfs-sandbox`

Edit a real directory with a preview and a guaranteed undo. The disk is the overlay's lower layer, and all edits stay
in memory until `commit`.

```bash
cargo run -p openvfs-sandbox -- ./my-project -c "replace fetch_user load_user --ext rs; diff"
```

```bash
cargo run -p openvfs-sandbox -- ./my-project --apply -f rename.txt
```

Use `-c`/`-f` for batch mode: without `--apply` it is only a dry run; any error aborts without touching the disk.
With no arguments it opens an interactive shell. The commands are
`ls tree cd cat find grep write append mkdir rm mv cp replace status diff commit discard`.

## Tests

```bash
cargo test --workspace
```

The integration suites form a single binary (`tests/it`), which makes both compiling and running much faster.
To run just one suite: `cargo test -p open-vfs --test it conformance::` (or `differential::`, `overlay::`, …).

There are **888 tests** (unit, integration, doctests and end-to-end CLI tests), organized in layers:

| Suite | What it proves |
|---|---|
| [`conformance`](crates/openvfs/tests/it/conformance.rs) | **56 contract rules × 11 configurations** (memory, disk, disk following links, overlay over memory, overlay over disk, overlay with a disk upper, two lowers with decoys, stacked overlay, mount, nested mount, and a minimal third-party backend that uses the default implementations) = 616 tests |
| [`differential`](crates/openvfs/tests/it/differential.rs) | random histories applied to a reference `MemoryFs` and to every configuration: the result, **the error variant and the path** must match at every step; the whole tree is compared periodically; lower layers must stay untouched |
| [`overlay`](crates/openvfs/tests/it/overlay.rs) | copy-on-write semantics and algebraic properties: `changes()` is exact and well classified; `commit` reproduces the merged view; `reset` restores the original |
| [`faults`](crates/openvfs/tests/it/faults.rs) | fault injection at **every possible point**, in two models (dead disk and transient failure): resumable commit, `write_atomic` never exposes partial data, copy-up and rename never corrupt the view, directory-rename rollback |
| [`concurrency`](crates/openvfs/tests/it/concurrency.rs) | parallel writers, exclusive `create_new`, `write_atomic` never observed half-done, appends are not lost, renames are atomic to listers, an overlay mutation "storm"; all under a deadlock watchdog |
| [`scaling`](crates/openvfs/tests/it/scaling.rs) | complexity contracts: listing a directory does not depend on the size of its subtree, resolving a path does not depend on the number of mounts; they fail if either becomes linear again |
| [`physical`](crates/openvfs/tests/it/physical.rs) | traversal attacks, device names, case aliases, symlinks and junctions (escapes, dangling, loops), special files |
| [`memory`](crates/openvfs/tests/it/memory.rs), [`mount`](crates/openvfs/tests/it/mount.rs), [`api`](crates/openvfs/tests/it/api.rs) | quota accounting, handle semantics, mount shadowing, compile-time `Send`/`Sync`, error mapping and `VPath` properties |
| [`cli`](crates/openvfs-sandbox/tests/cli.rs) | the real binary: exit codes, a dry run that leaves the disk alone, `--apply`, scripts, REPL, ANSI-free output in a pipe |
| `openvfs-mods` ([`tests/it`](crates/openvfs-mods/tests/it)) | index/profile/conflicts, deploy plan/apply/resume/rollback/undeploy/verify/capture, crash-resume at **every plan step**, a deployed-vs-virtual differential, priority-ordering properties, and the `openvfs-mods` CLI end-to-end |
| `openvfs-winfsp` ([`tests`](crates/openvfs-winfsp/tests)) | the callback bridge exercised without the driver; real-mount tests self-skip when WinFsp is not installed |
| `openvfs-hook` ([`tests`](crates/openvfs-hook/tests)) | end-to-end injection: a synthetic game sees the merged view only inside the hooked process (`OPENVFS_HOOK_E2E=1`) |

**Randomized tests are reproducible** (seeded PRNG, no dependencies) and scalable through environment variables:

```bash
OPENVFS_DIFF_CASES=3000 OPENVFS_DIFF_STEPS=200 OPENVFS_PROP_CASES=3000 cargo test --release -p open-vfs
```

```bash
OPENVFS_DIFF_SEED=1592590357 cargo test -p open-vfs --test it differential::
```

`OPENVFS_DIFF_THREADS` controls how many threads share each backend's histories (default: cores, at most 8).

A stuck test (deadlock) does not hang the run: the concurrent tests have their own watchdog and the whole process
aborts after `OPENVFS_TEST_DEADLINE_SECS` seconds (default 180; raise it for runs with a large `OPENVFS_DIFF_CASES`).

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs fmt, pedantic clippy and rustdoc with `-D warnings`,
the tests on Linux, macOS and Windows, the MSRV (1.88), and a nightly deep run of the randomized tests. Tagging `v*`
runs the [release workflow](.github/workflows/release.yml).

## Performance

The cost of each operation, with *n* nodes, *c* children, *d* components in the path and *m* mounts:

| Backend | Operation | Cost |
|---|---|---|
| `MemoryFs` | `metadata`, `open`, `create_dir`, `remove_*` of one node | O(log n) |
| `MemoryFs` | `read_dir` | O(c · log n), **independent** of what lies below the children |
| `MemoryFs` | `rename` / `remove_dir_all` of a directory | O(nodes in the subtree) |
| `MountFs` | resolving a path | O(d · log m), **independent** of how many mounts exist |
| `OverlayFs` | reads | concurrent (read lock); the cost is that of the layer that answers |
| `OverlayFs` | structural operations (`rename`, `remove`, copy-up) | serialized by one global lock |
| `PhysicalFs` | any operation, `Symlinks::Deny` | ~*d* system calls (one `lstat` per component) |
| `PhysicalFs` | any operation, `FollowWithinRoot` | typically one `canonicalize` |

The **independent** claims are checked by [`tests/it/scaling.rs`](crates/openvfs/tests/it/scaling.rs). For numbers on
your machine (they vary a lot with the disk and antivirus):

```bash
cargo run --release -p open-vfs --example bench
```

Measured on one Windows 11 machine, with noise of up to 2×: `MemoryFs::read_dir("/")` with two children used to cost
18 µs with 1 thousand nodes below and **1.8 ms** with 100 thousand; it now costs ~2–5 µs in both cases.
`MountFs::metadata` with 1024 mounts dropped from 9 µs to ~1.7 µs, practically the cost of a single mount. In
`PhysicalFs` the cost is dominated by system calls: on that machine each one costs ~16 µs, so `metadata` 8 levels deep
takes ~160 µs against ~30 µs for `std::fs`. On Linux, where a system call costs ~1 µs, the gap is much smaller, but
that has not been measured yet.

## Known limitations

- It is not a boundary against *concurrent* hostile processes on the host (TOCTOU). See [SECURITY.md](SECURITY.md).
- `PhysicalFs` with `Symlinks::Deny` pays one system call per path component; in very deep, frequently accessed trees
  this is the bottleneck (see [Performance](#performance)).
- `MemoryFs::rename` and `remove_dir_all` of a directory cost proportionally to the size of the subtree.
- `read_dir` returns a complete `Vec`; there is no streaming listing and no async API.
- `OverlayFs::commit` is idempotent but not atomic; a `rename` across mounts is copy + delete.
- Overlay whiteouts live in memory (`whiteouts()` lets you persist them externally).
- The virtual model has no symlinks, permissions or extended attributes.
- Case-insensitive hosts (Windows, macOS) keep that behavior inside `PhysicalFs`; `FoldedPath` and the
  `openvfs-mods` index/adapter provide case-insensitive lookup where a mod manager needs it.

## License

[MIT](LICENSE).
