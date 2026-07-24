# Sync model

How NeutronSync decides what to sync, when, and why. This is the design behind
`src/engine.rs` and `src/watcher.rs`. It is written for contributors; users only
need the "How it works" section of the [README](../README.md).

## The constraint that shapes everything

NeutronSync drives the official `proton-drive` CLI. That CLI has two properties
that dictate the whole design:

1. **No change feed.** There is no "what changed on Proton since X" API. The
   only way to learn about a remote-side change (an edit made on another device)
   is to list the folder again and compare.
2. **A cache that serves stale listings.** A default run can show a file that
   another device already trashed. So every listing NeutronSync makes runs with
   a throwaway `PROTON_DRIVE_CACHE_DIR`, and each concurrent scan worker gets its
   own, so parallel CLI processes never share (and corrupt) one cache.

Everything below follows from "local changes are cheap to detect, remote changes
cost a re-walk."

## Baseline and the three-way merge

For each folder pair NeutronSync keeps a **baseline**: the last state the two
sides agreed on, stored per file in SQLite (`stats.db`, table `baseline`, keyed
by `(pair, rel)`). A sync classifies every path on each side against the baseline
as Created, Modified, Deleted, Unchanged, or Absent, then combines the two
verdicts to pick an action (`decide` / `decide_dir` in `engine.rs`).

The baseline commit is **additive**: `commit_baseline` writes the rows that
synced this run and deletes the rows explicitly marked removed. It never rewrites
the whole pair. This is what makes a partial or scoped sync safe: it only ever
touches the rows it actually reconciled.

## Detecting changes: shallow, folder-scoped syncs

The local side is watched live with a recursive filesystem watch (inotify via
`notify-debouncer-full`). A change does **not** trigger a whole-pair sync. It
triggers a **shallow reconcile of the one folder whose direct contents changed**
(`sync_pair_shallow`): list just that folder (one non-recursive listing on each
side), reconcile its direct children, create or remove immediate sub-folders, but
do **not** descend.

Why shallow is correct and not lossy:

- The watch is recursive, so a change deeper in the tree arrives as its **own**
  event and reconciles its **own** folder. Walking the subtree on every change
  would redo work that the deeper events already cover.
- A directory event reconciles that directory; a file event (or a delete, where
  the path can no longer be stat-ed) reconciles the file's parent. Both converge
  on "the folder whose direct listing changed."
- Remote-only changes are **not** the watch's job. They are caught by the hot
  pass and the full walk (below).

Consequences that are intentional:

- Deleting a whole sub-folder locally trashes the remote folder (recoverably) and
  cleans that sub-folder's descendant rows out of the baseline (the shallow
  commit only removed the folder's own row, so descendants are pruned explicitly).
- A brand-new remote sub-folder's contents are created locally as an empty folder
  at the direct level and filled in by the full walk, since there is no local
  event to trigger their download.

## Startup order: quick wins first, full walk last

When the watcher starts it does not lead with the expensive full walk. It attaches
the file watch **first** (so changes made during startup are captured, not lost in
a blind window), then:

1. **Fresh local changes.** A fast local-only scan (`local_change_folders`, no
   network) finds folders that hold a new or changed file and shallow-syncs each.
   New local work uploads within seconds.
2. **Hot folders.** Folders with recent local activity (`hot_folders`) are
   shallow-synced next.
3. **Full walk.** Only then does the full reconcile run.

So syncing starts almost immediately and the full walk stops being a gate.

## The full walk: an adaptively paced safety net

The full walk scans both trees end to end. It is the only thing that catches
remote-only changes across the whole tree, so it must run, but on a large tree it
is expensive and pointless to run constantly. So it is **paced to its own cost**:
after each full walk NeutronSync measures how long it took and schedules the next
one at roughly `duration * 6` (`FULL_WALK_MULTIPLIER`), floored at the configured
`poll_interval` and capped so it always eventually runs. A 15-minute walk reruns
about every 90 minutes; small, active folders stay fresh in between through the
shallow and hot syncs.

The remote scan itself is concurrent: a pool of workers (`scan_threads`, auto =
CPU cores clamped to 2..8) lists folders in parallel, each with its own cache dir.
The reconcile/apply loop is single-threaded so two syncs of the same pair never
overlap; downloads run through a small concurrent pool.

## Data-safety invariants

These are the rules that must hold. Most exist because breaking one caused a real
incident.

- **Positive-confirmation commit.** A file advances the baseline only after its
  transfer actually completes. A failed transfer, or an op a cancelled/killed run
  never reached, is dropped from the prospective baseline, leaving its previous
  row intact. Without this, a run cancelled mid-way recorded files it never
  transferred as "synced", and the next run then read those still-missing local
  files as deletions and propagated them to the remote.
- **Deletes are opt-in and recoverable.** They propagate only with
  `propagate_deletes`; remote deletes go to Proton trash, local deletes to the
  desktop trash (`local_delete = "remove"` unlinks permanently instead).
- **Incomplete views never delete.** If a folder listing fails, that run syncs
  without deletions rather than acting on a partial picture.
- **Missing roots are refused.** A vanished local root (unmounted drive) or a
  vanished remote base folder is refused for that run, not read as "everything
  was deleted".
- **Missing baseline unions.** A first run or a wiped state dir unions both sides
  rather than mirroring, so a lost baseline cannot trigger a mass delete.
- **Scope containment.** A scoped or shallow sync only ever looks at paths inside
  its folder, so nothing outside can be seen as missing and deleted. An empty
  scope means the pair root, not a folder literally named "".
- **Excludes are invisible.** Excluded sub-paths are pruned from the scan itself
  (local and remote), never listed, downloaded, or deleted. Re-including a folder
  re-downloads it, so excluding can never delete remote data.
- **Conflicts keep both.** Both sides changed and differ, keep both
  (`name (conflict <timestamp>).ext`), never silently overwrite.

## The GUI is two processes

In tray mode the background tray daemon owns the watcher and does the syncing in
one process; the window is a separate process. The window cannot see the daemon's
in-memory state directly, so the daemon publishes a compact live snapshot to
`status.json` a few times a second and the window reads it. That is how an open
window shows the daemon scanning, syncing, and per-file transfers in real time.
A CLI `neutronsync sync` also records its operations into the same activity store
the window reads, so command-line runs show up there too.

## Config knobs

| Key | Meaning |
| --- | --- |
| `poll_interval` | Floor for the adaptive full-walk interval (seconds). |
| `scan_interval` | How often the hot-folder pass runs (seconds). |
| `debounce` | Filesystem-event debounce window (seconds). |
| `scan_threads` | Concurrent remote-scan workers (0 = auto, capped at 8). |
| `download_threads` | Concurrent downloads (0 = auto, kept modest). |
| `propagate_deletes` | Whether deletions cross to the other side. |
| `local_delete` | `trash` (recoverable) or `remove` (permanent) for local deletes. |
| `conflict` | `keep-both` (default), `newer`, or `skip`. |
| `compare` | `size+mtime` (default), `size`, or `sha1`. |
