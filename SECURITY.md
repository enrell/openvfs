# Security and threat model

## What openvfs guarantees

**Paths do not escape the root.** Every path goes through `VPath`, which normalizes the input and resolves `..`
lexically, always stopping at the root. Backends never receive a `..`.

**`PhysicalFs` confines hostile paths to its root directory:**

| Attack | Defense | Test |
|---|---|---|
| `../../etc/passwd`, `\..\x` | `VPath` clamps at the root | `physical::traversal_never_leaves_the_root` |
| `C:\…`, `\\?\…` (prefixes) | a component must be a single `Normal` | `physical::windows_device_names_and_aliases_are_rejected` |
| `CON`, `NUL.txt`, `COM1`, `LPT¹` (Windows devices) | rejected with `InvalidPath` | same + unit test `windows_name_rules` |
| `file.txt:stream` (NTFS ADS), `<>:"\|?*` | rejected | same |
| `name.` / `name ` (Windows strips these and creates an alias) | rejected | same |
| symlink/junction pointing outside | `Symlinks::Deny` (default): `PermissionDenied`; `FollowWithinRoot`: `PathEscapesRoot` | `physical::deny_policy_…`, `physical::follow_policy_…` |
| dangling symlink (would create the file outside) | treated as an escape | same |
| symlink loop (`loop -> .`) | `Deny`: invisible; `Follow`: use `Walk::max_depth` | same |
| `remove_dir_all` on a directory containing a link | removes the link, never the target (`std::fs::remove_dir_all`) | `recursive_delete_removes_links_not_targets` |
| FIFO, socket, device inside the root | invisible (opening a FIFO would block) | `special_files_and_unaddressable_names_are_invisible` (Unix) |

**Bounded memory for untrusted input.** `MemoryFs::with_quota(bytes)` caps the total number of bytes. This applies to
writes, appends, `set_len`, copies and sparse writes. When the quota is exceeded the operation fails with
`StorageFull`.

**No silent data loss.** Copying a file onto itself is refused; it used to truncate it. In the overlay, copy-up and
rename write to a temporary name and then atomically `rename` into place, so a failure never leaves a partial file at
the real path.

## What openvfs does **not** guarantee

- **TOCTOU on the host.** `PhysicalFs` confinement applies to *paths*. A hostile concurrent process that swaps
  directories for symlinks between the check and the use can win the race. If that is your threat model, combine it
  with OS isolation (containers, namespaces, AppContainer, unprivileged accounts).
- **Permissions and ACLs.** The virtual model has no owner or access mode. `PhysicalFs` operates with the process's
  rights.
- **Atomicity across layers.** `OverlayFs::commit` is idempotent but not atomic. `Vfs::rename` across mounts is
  copy + delete.
- **Directory durability.** `write_atomic` calls `sync_all` on the file, but not on the parent directory.

## Reporting vulnerabilities

Open a private report on the repository (Security Advisories), not a public issue. Include a case that reproduces the
problem; ideally a test in the format of `crates/openvfs/tests/it/physical.rs`.
