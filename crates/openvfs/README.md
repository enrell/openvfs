# openvfs

A layered, pluggable virtual file system for Rust. No dependencies, `#![forbid(unsafe_code)]`, MIT.

One `std::fs`-style API over interchangeable storage: memory (with a byte quota), a confined host
directory, read-only wrappers, mount tables and copy-on-write overlays with change tracking. Every
backend passes the same conformance and differential test suites, so code behaves the same wherever
the bytes live. Files are plain `Read + Write + Seek` objects.

```toml
[dependencies]
open-vfs = "0.1"   # the library is imported as `openvfs`: use openvfs::prelude::*;
```

```rust
use openvfs::prelude::*;

fn main() -> openvfs::Result<()> {
    let mounts = MountFs::new();
    mounts.mount("/", MemoryFs::with_quota(64 << 20))?;                      // RAM root, 64 MiB cap
    mounts.mount("/assets", ReadOnlyFs::new(PhysicalFs::new("./assets")?))?; // host dir, read-only

    let vfs = Vfs::new(mounts);
    vfs.create_dir_all("/save/slot1")?;
    vfs.write_atomic("/save/slot1/state.json", r#"{"level": 3}"#)?; // never half-written
    Ok(())
}
```

## Typical uses

- **Test doubles:** make your code take a `Vfs` and hand it a `MemoryFs` in tests.
- **Dry runs and undo:** `OverlayFs::new(MemoryFs::new(), PhysicalFs::new(dir)?)` edits in memory;
  `changes()` previews, `commit()` applies, `reset()` discards.
- **Layered assets / mods:** several read-only lower layers under one writable upper layer.
- **Bounded scratch space** for untrusted input via `MemoryFs::with_quota`.

Write against `&dyn FileSystem` (or `Vfs`) in your own APIs and the backend becomes a deployment choice.

## Know the limits

- `PhysicalFs` confines *paths*; it is not a sandbox against a hostile concurrent process (TOCTOU).
- Synchronous only; `read_dir` returns a `Vec`. In async code use `spawn_blocking`.
- No permissions, symlinks (in the virtual model), xattrs or locking. `OverlayFs::commit` is idempotent, not atomic.
- Tested on Windows only so far; Linux/macOS are covered by the CI workflow but have not been run.

Full documentation, architecture, performance notes and the threat model are in the repository
(`README.md`, `SECURITY.md`). Run `cargo doc --open` for the API.
