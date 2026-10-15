# Deployment recovery

`openvfs-mods` deploys a profile's merged mod view into the game directory
as real files (hardlinks or copies). Everything needed to undo or finish
that work lives in one **state directory** chosen with `--state`. This
guide covers what is in it, what a crash leaves behind, and how to get the
game directory back — by command or by hand.

## State directory layout

```text
<state_dir>/
  journal.jsonl    write-ahead log, one JSON record per line — exists only
                   while a deployment is interrupted or in flight
  manifest.json    the record of a *completed* deployment
  backup/          originals moved out of the game dir, keyed by their
                   original relative path ("/x" → backup/x)
    stray/           preserved non-originals displaced from deployed paths
```

Keep the state dir **outside** the game directory, on the same volume so
backups can rename (copy fallback exists, but same-volume is faster and
safer). Scans deliberately skip it either way.

## A crash during deploy

`deploy` journals each step before and after running it. Whatever killed
the process, the game dir is in one of two shapes:

- **Journal still exists** → a deployment is *in flight*. Two options:
  - `openvfs-mods resume --state DIR` — finishes the plan. Every op is
    idempotent, so re-running the interrupted step converges; the manifest
    is written when the last op completes and the journal is removed.
  - `openvfs-mods rollback --state DIR` — undoes every journaled step in
    reverse and drops the journal. The game dir returns to its pre-deploy
    state; a manifest from an *earlier* completed deployment is kept.
- **Only `manifest.json`** → the deployment completed. `undeploy` is the
  way back, not `rollback`.

Rule of thumb: journal ⇒ resume or rollback; manifest ⇒ undeploy;
neither ⇒ nothing was ever deployed. `status` prints which world you are
in. `deploy` refuses to run while a journal exists — resolve the
interrupted one first.

Torn or corrupted journal lines (a crash mid-`fsync`, partial sector
writes) are tolerated: the reader keeps the complete records, and
resume/rollback converge from them.

## The manifest

`manifest.json` is JSON for a reason — you can read it when the manager
cannot answer:

- `game_dir` — the directory this deployment describes.
- `overwrite_dir` — where captures default to.
- `created` — unix timestamp of when the deployment finished.
- `entries[]` — every expected file: `path` (deployed spelling),
  `folded` (its case-folded identity), `kind` (`deployed` = placed by us,
  `base` = a game file we recognized), `method` (`link`/`copy`), `sha256`
  and `size` at deploy time, `source` (the mod file behind a deployed
  entry), `backup` (the displaced original's path under `backup/`).
- `dirs[]` — directories the deployment created; undeploy removes exactly
  these, and only when empty.
- `foreign[]` — files present at deploy time that the merged tree does not
  provide, kept with their hashes (see `--foreign`).

## Hand-edited files: verify and capture

If you (or Steam, or the game) changed files under the deployment:

- `openvfs-mods verify <profile> --state DIR` re-hashes the manifest's
  entries against disk and scans for files it does not know. Exit code 0
  means clean; 1 lists the drift: `missing`, `modified`, `wrongtype` for
  manifest paths and `foreign` for unknown files. Note that files kept at
  deploy time under `--foreign keep` report as `foreign` — that is the
  manifest saying "not mine", not necessarily a problem.
- `openvfs-mods capture <profile> --state DIR` is the reconciler: it moves
  every non-manifest file (new writes, changed deployed files) into the
  overwrite directory at the same relative path, then repairs deployed
  entries from their mod sources. What the report means:
  - `moved` — writes now safe in the overwrite dir;
  - `repaired` — deployed files re-materialized from a clean source;
  - `corrupted` — a write went *through* a hardlink and mutated the mod
    file itself (see below); only re-installing the mod restores it;
  - `lost` — a base-game file changed or vanished; the write is captured,
    but the original bytes are gone;
  - `kept_foreign` — known-foreign files left in place (hash unchanged).
    A foreign file whose bytes *changed* is treated as a write and moved.

## Steam / "verify integrity of game files"

Steam's verify sees the deployed hardlinks as game files — usually byte
correct, so it does nothing; but if it decides a file is wrong it
**replaces** it: a rename-over unlinks the hardlink (mod file stays
pristine, deployment quietly loses the file) while an in-place rewrite
hits the shared inode and mutates the mod. Either way, afterwards:

```
openvfs-mods verify  <profile> --state DIR   # see what Steam changed
openvfs-mods capture <profile> --state DIR   # restore deployed entries,
                                             # save real writes to overwrite
```

`capture` repairs anything whose mod source is still clean.

## Full drive mid-deploy

If a copy op fails with `ENOSPC`, the deploy stops with the journal in
flight — nothing is silently half-applied. Free space, then `resume`.
A rollback to the pre-deploy state works too. Backup renames never
enlarge the tree, so they cannot hit the full-drive case; only `Copy`
and link-fallback copies can.

## The hardlink caveat (read before deploying)

A hardlinked deployed file *is* the mod's file — same inode. A game that
writes a file **in place** mutates the mod itself; the original bytes are
gone before `capture` runs (capture detects it by hash and reports the
file `corrupted`). For directories the game writes into — save paths,
config dirs — deploy with:

```
--mode copy              # copy everything (slow, uses real space)
--copy-under /saves      # copy only under that folded path prefix
```

Games that rewrite via rename (write temp, rename over) are harmless:
they replace the link, not the inode, and `capture` then repairs the
deployment.

## Manual recovery (no working binary)

Everything above is plain files; you can undo a deployment by hand:

1. Delete every `entries[]` file with `kind = "deployed"` — each `path` is
   relative to `game_dir`. A file there may be a hardlink to a mod file;
   deleting it only drops the link, the mod copy is safe. Skip any whose
   current contents no longer match `sha256` — that is a game write; move
   it to the overwrite dir instead of deleting.
2. Move everything under `backup/` **except `backup/stray/`** back to its
   path under the game dir — `backup/x/y` → `<game_dir>/x/y`. These are
   displaced originals.
3. Remove the dirs listed in `dirs[]` if they are now empty.
4. Delete `manifest.json`, `journal.jsonl` if present, then `backup/`
   itself, then the state dir.

`backup/stray/` holds unexpected occupants (files that appeared between
planning and apply, or second claimants to a backed-up path). They are
never originals and never auto-restored — sort them out by hand or delete
them deliberately.

If the journal exists but the manifest does not, the deployment never
finished: it is safe to delete `journal.jsonl` and every `backup/` entry
that corresponds to nothing in the game dir — or better, run `rollback`
from any working build instead.
