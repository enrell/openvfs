# openvfs-hook — per-process file virtualization on Windows

`crates/openvfs-hook` builds `openvfs_hook.dll`, an injectable DLL that makes
one selected process see a **merged view** of a directory: the game's own
files plus any number of read-only *mod layers*, with writes captured into a
separate *overwrite* directory. It is the Win32 equivalent of what the
`open-vfs` crate models in-process, applied to a program that does not know
the library exists.

This document is the behavioral contract: what is hooked, what semantics are
delivered, what is knowingly faked, and what remains before this is anything
like production-ready. It was implemented **clean-room** — from the publicly
documented/observable behavior of tools like Mod Organizer 2's USVFS (merged
mod view by priority, overwrite capture, per-process isolation), never from
their source code.

---

## 1. Architecture

```
                 ┌──────────────────────────┐
                 │        game.exe           │  (unmodified, single process)
                 │  kernel32!CreateFileW ────┼──┐
                 └──────────────────────────┘  │  inline detour
                                               ▼
                              ┌──────────────────────────────┐
                              │   openvfs_hook.dll (injected) │
                              │  guard (TLS recursion check)  │
                              │  routes: folded path map      │
                              │   ├─ overwrite/   (top)       │
                              │   ├─ mod layers[0..n]         │
                              │   └─ virtual_root (base)      │
                              └──────────────────────────────┘
```

- **Injector** (`openvfs-inject`): `CreateProcessW` with `CREATE_SUSPENDED`,
  `VirtualAllocEx` + `WriteProcessMemory` for the DLL path,
  `CreateRemoteThread` at `LoadLibraryW`. `DllMain` then runs the full
  install **synchronously**, so every hook is live before the game's main
  thread resumes. The injector waits for the child and propagates its exit
  code (a full NTSTATUS on crash).
- **DLL** (`crate-type = ["cdylib", "rlib"]`): on `DLL_PROCESS_ATTACH` it
  reads `OPENVFS_HOOK_CONFIG`, builds the route map, and installs detours.
  `openvfs_hook_install` / `openvfs_hook_uninstall` exports provide an
  explicit, idempotent alternative for injectors that prefer a second remote
  call. With no config the DLL loads inertly and hooks nothing.
- **Hook engine** (`engine`): [`ilhook`] 2.3.0 — a pure-Rust x64 inline
  detour library using `iced-x86` for instruction relocation (no C toolchain;
  `retour` needs `libudis86-sys`, `detour` is unmaintained). Each hooked
  entry point gets a `JmpToAddr` detour whose pre-jump routine records the
  trampoline address into a per-hook `AtomicUsize`; `HookPoint`s live for
  the process lifetime.
- **Routing** (`routes`): a standalone folded-path map — not `OverlayFs` —
  because detours must translate *absolute host paths* to other *host paths*
  with minimal syscalls; `OverlayFs` resolves `VPath`s component-wise through
  the `FileSystem` trait. Semantics intentionally mirror the library model.
- **Recursion guard** (`guard`): every detour starts by taking a TLS guard;
  internal operations (layer probing, copy-up, listing) then call the real
  `windows-sys` functions, which re-enter the patched stubs and pass straight
  through to the trampoline. Panics in detour logic are contained by
  `catch_unwind` and fall back to the original function.
- **Logging** (`log`): `OPENVFS_HOOK_LOG=<path>` appends to a file;
  `1`/`stderr` writes to stderr; unset/`0` disables. Cheap no-op when off.

### Config (`OPENVFS_HOOK_CONFIG`)

```json
{
  "virtual_root": "C:/Games/Foo/Data",
  "overwrite": "C:/users/me/mods/overwrite",
  "layers": [
    { "name": "textures-hd", "dir": "C:/users/me/mods/hd", "enabled": true },
    { "name": "fixes",       "dir": "C:/users/me/mods/fixes", "enabled": true }
  ]
}
```

`layers[0]` is the highest-priority mod. `overwrite` outranks all layers;
the real contents of `virtual_root` are the lowest-priority base. Missing or
disabled layers are ignored. Paths are compared case-insensitively
(Unicode-aware folding; exotic folding differences are a documented edge),
but names created in the overwrite layer keep the casing the app used.

## 2. Semantics

| Operation | Behavior |
|---|---|
| Read open | Resolves to highest-priority provider. Missing → `ERROR_FILE_NOT_FOUND`. |
| Write open / create / truncate dispositions | Captured into `overwrite/` (parents created). A lower-layer winner is copied up first so the write sees merged content; read-only bits are cleared on the copy. |
| `DeleteFile` on overwrite file | Real delete; whiteout written if a lower layer also provides it. |
| `DeleteFile` on layer/base file | `.ovh-deleted` marker created in `overwrite/<dir>/`; file vanishes from the merged view. Layers are never modified. |
| `CreateDirectory` | Merged-exists → `ERROR_ALREADY_EXISTS`; missing parent → `ERROR_PATH_NOT_FOUND`; otherwise created in `overwrite/`. |
| `RemoveDirectory` | Virtual directory must be empty; overwrite remnants (markers) are cleared, then lower providers hidden via a whiteout. |
| `FindFirstFile*`/`FindNextFile*` | Fully synthesized listing: merged names + `.`/`..`, wildcard match (DOS quirks approximated), attributes/sizes/times of the *winning* file. Nonexistent dir → `ERROR_PATH_NOT_FOUND`; no matches → `ERROR_FILE_NOT_FOUND`. |
| `GetFileAttributes*` | Attributes of the winning file. Missing → `INVALID_FILE_ATTRIBUTES` + `ERROR_FILE_NOT_FOUND`. |
| `SetFileAttributes` | Applied to overwrite copy; lower-layer files are copied up first. |
| `MoveFile*` within root | Rename in overwrite if already there; otherwise copy winner → overwrite destination + whiteout the source. |
| `MoveFile*` root → outside | Content copied out, source whited out (appears moved-away). |
| `MoveFile*` outside → root | Real move into `overwrite/`, then visible in the view. |
| `CopyFile*` | Source resolved through the view; destination in root lands in `overwrite/`. `fail_if_exists` honored against the merged view. |
| `FILE_FLAG_DELETE_ON_CLOSE` | Copies the winner up (so the open succeeds), whites out lowers; the overwrite copy dies on close. |
| Drive-relative paths (`C:foo`), `\\.\` device paths, paths outside the root | Passed through untouched. |
| Everything when `OPENVFS_HOOK_CONFIG` is unset/invalid | Nothing is hooked; the process is unaffected. |

Isolation: the route map is per-process (built inside the target at attach).
Only the hooked process sees the view; the mod and base directories on disk
are never modified.

## 3. API coverage

Win32 (`kernel32`/`kernelbase` — whichever module actually hosts the export;
deduped by resolved address):

| API | Status |
|---|---|
| `CreateFileW` / `CreateFileA` / `CreateFile2` | ✅ hooked |
| `FindFirstFileW` / `FindFirstFileA` | ✅ hooked |
| `FindFirstFileExW` | ✅ `FindExInfoStandard`/`Basic` + `NameMatch`/`LimitToDirectories`; other levels/ops pass through |
| `FindNextFileW` / `FindNextFileA` / `FindClose` | ✅ hooked (synthesized handle table) |
| `GetFileAttributesW/A`, `GetFileAttributesExW/A` | ✅ hooked |
| `DeleteFileW/A`, `CreateDirectoryW/A`, `RemoveDirectoryW/A` | ✅ hooked |
| `SetFileAttributesW/A` | ✅ hooked (copy-up into overwrite) |
| `MoveFileW/A`, `MoveFileExW/A` | ✅ hooked (minimal; see limitations) |
| `CopyFileW/A` | ✅ hooked |
| `CopyFileEx*`, `MoveFileWithProgress*`, `CopyFile2`, `CreateFileTransacted*`, `MoveFileTransacted*` | ⬜ future — pass through today |
| `GetFileAttributesTransacted*`, `FindFirstFileExA`, `FindFirstStreamW`, `FindFirstFileNameW`, `GetFileInformationByHandle*`, `GetFinalPathNameByHandle*`, `GetFullPathName*` | ⬜ future — **not** hooked; handles/paths may show real locations |
| `ReadDirectoryChangesW`, `FindFirstChangeNotification*` | ⬜ future — watchers see real dirs only |
| `NtCreateFile`, `NtOpenFile`, `NtQueryDirectoryFile`, `NtQueryFullAttributesFile`, `NtDeleteFile`, `NtSetInformationFile`, `NtReadFile` (virtualized reads) | ⬜ tier-2 — ntapi hooking is the next big coverage win |
| `GetOverlappedResult` / IOCP reads | ⬜ future — async opens are redirected but completion plumbing isn't virtualized specially |
| 32-bit (`i686`) targets | ⬜ out of scope — `ilhook` supports x86, so the engine is portable, but the crate is `x86_64`-gated |
| Child process spawn (`CreateProcessW/A`, `CreateProcessAsUserW`, `ShellExecute*`) | ⬜ future — documented propagation plan below |

### Known limitations / edge cases

- **Enumeration approximation**: `.`/`..` are synthesized when the pattern
  matches them; `*`/`?`/DOS aliases (`<`, `>`, `"`) handled by an
  approximation of `RtlIsNameInExpression`, including the trailing-`.*`
  rule. Attributes/timestamps come from the winning real file (`.`,`..` may
  borrow the root's attrs). `WIN32_FIND_DATAA` names are converted through
  the system ANSI codepage.
- **Wildcards inside directory components** of a pattern resolve to nothing
  (Windows rejects those anyway).
- **`MoveFileW` rename of a whited-out source into existence** and
  cross-`MOVEFILE_DELAY_UNTIL_REBOOT` semantics are approximations.
- **`GetFullPathName`/`GetFinalPathNameByHandle`** are not virtualized, so a
  process inspecting its own absolute path may see overwrite-layer paths.
- **Symlinks/junctions** inside layers resolve by `std::fs` probing — a link
  that escapes the layer root is followed (same as normal FS semantics for
  the *real* path) but never created by the layer itself.
- **Sharing violations** propagate from the real open; two live handles that
  disagree about the view can observe transient states (e.g. a write-open
  captured into overwrite while another handle still reads the lower file is
  consistent, but a lower file whited-out mid-write leaves the write-opened
  copy already deleted on close — matches "delete wins" semantics).
- **Thread-safety of installation**: hooks are installed while the app is
  suspended (injector path), so no thread is executing patched code.
  `openvfs_hook_install` on a *running* process does not suspend other
  threads — ilhook's `ThreadCallback` suspension support is a future hookup.
- **Performance**: every hooked call does a normalized-path prefix check;
  under-root calls probe several directories. Fine for a game's data dir;
  not measured under heavy churn.
- **Transient overwrite ghosts**: an aborted process can leave copies /
  markers that change the next run's view (`.ovh-deleted` files are visible
  on disk, never through the view). Cleanup tooling is future work.

## 4. Child-process propagation (planned, not implemented)

The spec-of-record for propagation when it lands:

- Hook `CreateProcessW/A`, `CreateProcessAsUserW` in the parent.
- When a child is spawned, the parent-side hook creates it suspended, writes
  `OPENVFS_HOOK_CONFIG` (pointing at the same config) into the child's
  environment block, remote-injects this DLL exactly like `openvfs-inject`,
  then resumes — so the view is inherited transitively.
- Opt-in via a config flag (`"inherit_children": true`), default off.
- Today: children see the real filesystem.

## 5. Risks & intended scope

- **Anti-cheat**: inline hooking of `kernel32` in a game process is exactly
  what anti-cheat systems flag. This crate must **never** ship into
  anti-cheat-protected titles; it is intended for single-player mod tooling,
  tests, and offline tooling only.
- **Code signing**: injected unsigned DLLs into signed games may violate ToS
  or trip integrity checks; a production fork should be signed and should
  consider a documented non-invasive mode.
- **Fragility**: prologue detouring depends on instruction boundaries of a
  foreign binary; a Windows update that changes kernelbase prologues could
  break a hook (ilhook validates instruction decode, and our detours fall
  back to the real function on any internal panic — a failed hook fails open,
  not closed).
- **Maintenance**: API coverage is a treadmill — new games use new surfaces
  (NT native APIs, IOCP, direct syscalls). The tier-2 list above is where
  most coverage bugs will live.
- **Privilege**: injecting into processes with different integrity levels or
  elevated targets is unsupported; the injector handles the same-user,
  same-integrity case.

## 6. Remaining work before "production"

1. ntapi tier: `NtCreateFile`/`NtOpenFile`/`NtQueryDirectoryFile`/
   `NtDeleteFile`/`NtSetInformationFile`/`NtQueryFullAttributesFile` hooks —
   many games bypass Win32 here.
2. `CreateProcess` family hooks for child inheritance (§4).
3. `GetFileInformationByHandle*`/`GetFinalPathNameByHandle*`/`GetFullPathName*`
   so path introspection can't reveal real locations.
4. `FindFirstFileExA`, `CopyFileExW/A`, `MoveFileWithProgressW`,
   `FindFirstStreamW`, directory-change notifications.
5. Handle-level mapping (virtual handle → real handle bookkeeping) instead of
   path-level-only routing — needed for `NtReadFile`-level fidelity.
6. Runtime attach support via ilhook `ThreadCallback` (suspend threads while
   patching) + unload story (`openvfs_hook_uninstall` exists but is only safe
   at quiescence).
7. 32-bit support (ilhook x86 already exists; mostly build plumbing).
8. Async/IOCP-aware reads, `ReadDirectoryChanges*` virtualization.
9. Overwrite hygiene tools: stale-marker garbage collection, `.ovh-deleted`
   namespace escaping for apps that literally create such files.
10. Fuzzing/property tests against a real Windows volume for enumeration
    parity (pattern quirks, long paths, Unicode folding edge cases).

## 7. Testing

- `cargo test -p openvfs-hook` runs unit tests (wildcard/access/parent logic)
  and the e2e's *baseline* half (synth-game without injection).
- `OPENVFS_HOOK_E2E=1 cargo test -p openvfs-hook --test e2e` additionally
  injects `openvfs_hook.dll` into a spawned `synth-game` and asserts the
  merged view, write capture, whiteouts, and layer preservation on disk.
  If the injector itself fails (AV/policy), the test logs a skip instead of
  failing; a failure *inside* the hooked child still fails the test.

[`ilhook`]: https://crates.io/crates/ilhook
