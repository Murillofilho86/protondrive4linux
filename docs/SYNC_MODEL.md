# Sync model

How protondrive4linux decides what to sync, when, and why. This is the design behind `src/engine.rs` and `src/watcher.rs`. It is written for contributors; users only need the "How it works" section of the [README](../README.md).

## The constraint that shapes everything

protondrive4linux drives the official `proton-drive` CLI. That CLI has two properties that dictate the whole design:

1. **No change feed.** There is no "what changed on Proton since X" API. The only way to learn about a remote-side change (an edit made on another device) is to list the folder again and compare.
2. **A cache that serves stale listings.** A default run can show a file that another device already trashed. So every listing protondrive4linux makes runs with a throwaway `PROTON_DRIVE_CACHE_DIR`, and each concurrent scan worker gets its own, so parallel CLI processes never share (and corrupt) one cache.

Everything below follows from "local changes are cheap to detect, remote changes cost a re-walk."

## Baseline and the three-way merge

For each folder pair protondrive4linux keeps a **baseline**: the last state the two sides agreed on, stored per file in SQLite (`stats.db`, table `baseline`, keyed by `(pair, rel)`). A sync classifies every path on each side against the baseline as Created, Modified, Deleted, Unchanged, or Absent, then combines the two verdicts to pick an action (`decide` / `decide_dir` in `engine.rs`).

The baseline commit is **additive**: `commit_baseline` writes the rows that synced this run and deletes the rows explicitly marked removed. It never rewrites the whole pair. This is what makes a partial or scoped sync safe: it only ever touches the rows it actually reconciled.

## Detecting changes: shallow, folder-scoped syncs

The local side is watched live with a recursive filesystem watch (inotify via `notify-debouncer-full`). A change does **not** trigger a whole-pair sync. It triggers a **shallow reconcile of the one folder whose direct contents changed** (`sync_pair_shallow`): list just that folder (one non-recursive listing on each side), reconcile its direct children, create or remove immediate sub-folders, but do **not** descend.

Why shallow is correct and not lossy:

- The watch is recursive, so a change deeper in the tree arrives as its **own** event and reconciles its **own** folder. Walking the subtree on every change would redo work that the deeper events already cover.
- A directory event reconciles that directory; a file event (or a delete, where the path can no longer be stat-ed) reconciles the file's parent. Both converge on "the folder whose direct listing changed."
- Remote-only changes are **not** the watch's job. They are caught by the hot pass and the full walk (below).

Consequences that are intentional:

- Deleting a whole sub-folder locally trashes the remote folder (recoverably) and cleans that sub-folder's descendant rows out of the baseline (the shallow commit only removed the folder's own row, so descendants are pruned explicitly).
- A brand-new remote sub-folder's contents are created locally as an empty folder at the direct level and filled in by the full walk, since there is no local event to trigger their download.

## Startup order: quick wins first, full walk last

When the watcher starts it does not lead with the expensive full walk. It attaches the file watch **first** (so changes made during startup are captured, not lost in a blind window), then:

1. **Fresh local changes.** A fast local-only scan (`local_change_folders`, no network) finds folders that hold a new or changed file and shallow-syncs each. New local work uploads within seconds.
2. **Hot folders.** Folders with recent local activity (`hot_folders`) are shallow-synced next.
3. **Full walk.** Only then does the full reconcile run.

So syncing starts almost immediately and the full walk stops being a gate.

## The full walk: an adaptively paced safety net

The full walk scans both trees end to end. It is the only thing that catches remote-only changes across the whole tree, so it must run, but on a large tree it is expensive and pointless to run constantly. So it is **paced to its own cost**: after each full walk protondrive4linux measures how long it took and schedules the next one at roughly `duration * 6` (`FULL_WALK_MULTIPLIER`), floored at the configured `poll_interval` and capped so it always eventually runs. A 15-minute walk reruns about every 90 minutes; small, active folders stay fresh in between through the shallow and hot syncs.

The full walk **streams**: rather than scan the whole tree and only then apply, it walks folder-by-folder (a concurrent breadth-first walk, `run_sync_streaming`) and reconciles each folder as it is discovered, transferring immediately. So uploads and downloads overlap the walk instead of waiting for a full scan. Each folder is handled by the same shallow, direct-children primitive (`sync_pair_shallow_with_base`), and the walk descends into the child folders that reconcile reports (skipping any it just deleted or that are excluded).

Concurrency is a pool of workers (`scan_threads`, auto = CPU cores clamped to 2..8), each with its own Engine and cache dir. They share one read-only baseline snapshot and commit disjoint rows (SQLite serialises the writes), and the breadth-first, level-by-level structure means a parent folder is always created before its children are processed. A sequential reference implementation (`Engine::sync_pair_streaming`) is the tested oracle for the walk's semantics; an equivalence test asserts it reaches the same end state as the batch walk.

## The session gate: signed out is not the same as a keyring out of reach

When the `proton-drive` session is unusable every remote call fails, and a full walk would bury the one real cause under an error per folder. So the watcher probes the session (one cheap listing of the remote root) at startup, after any cycle that had errors, every five minutes while it works, and when a window nudges it (`auth_signal`), and keeps a gate: `Open`, `SignedOut` or `KeyringUnreachable`. While the gate is shut it does no sync work, cancels a running walk, says why once (`SyncEvent::Auth { signed_in: false }` or `SyncEvent::KeyringUnreachable`) and probes again - every 20 seconds for a sign-out, every minute for the keyring - until the probe succeeds, then says so (`Auth { signed_in: true }`) and re-walks soon, since anything may have changed meanwhile.

The two shut states differ because the fix does. `SignedOut` means there is no session and it takes a sign-in. `KeyringUnreachable` means the CLI could not reach the secret store its session lives in (locked, no D-Bus session in this login, headless): the session is usually intact, signing in again does nothing, and it ends by itself once the keyring answers - so the app says "waiting for your keyring", offers no sign-in, probes less often (a probe of a locked keyring can be what makes it ask for its password) and notifies only if it lasts five minutes. Telling them apart is `protoncli::session_problem`, from the CLI's message: it is the one place that knows the wording. An inconclusive probe (a network blip) never moves the gate in either direction.

## Data-safety invariants

These are the rules that must hold. Most exist because breaking one caused a real incident.

- **Positive-confirmation commit.** A file advances the baseline only after its transfer actually completes. A failed transfer, or an op a cancelled/killed run never reached, is dropped from the prospective baseline, leaving its previous row intact. Without this, a run cancelled mid-way recorded files it never transferred as "synced", and the next run then read those still-missing local files as deletions and propagated them to the remote.
- **Downloads arrive whole or not at all** (`src/staged.rs`). The CLI writes a download straight to its destination, and with the `remove` conflict strategy deletes the file already there first, so a download that died half way (disk full, the CLI killed at its timeout, a crash, a power cut) left a truncated file under the real name - which the next run read as the user's own edit and uploaded over the good copy. So the CLI is pointed at a private staging folder inside the destination's folder (`.protondrive4linux-<pid>-….part`; `*.part` is already in the ignore list, so it is never scanned, uploaded or deleted, and watcher events inside it are dropped), and the file is moved into place with one `rename` only after: it measures the size the listing promised (when it gave one; 0 means "unknown"), it has been `fsync`ed (so the baseline cannot say "synced" over bytes that never reached the disk), its modification time has been set (so a scan never sees it with the wrong one), and the local file is still what the plan saw - an edit made, or a file created, while the run was in flight is never overwritten, and meets the remote version as an ordinary conflict next run. Failure at any point leaves the previous local file exactly as it was and removes the staging folder; one left by a killed process is swept by the next scan of that folder (a minute after its owner is gone, or once it has outlived the CLI timeout).
- **Conflict copies never replace a file.** Setting the local version aside (`name (conflict <timestamp>).ext`) picks a name nothing has (a counter is appended if needed) and moves without `rename(2)`'s silent replace, so two conflicts on one file in one second, or a file the user already named that way, cannot overwrite an earlier copy.
- **Deletes are opt-in and recoverable.** They propagate only with `propagate_deletes`; remote deletes go to Proton trash, local deletes to the desktop trash (`local_delete = "remove"` unlinks permanently instead).
- **Incomplete views never delete.** If a folder listing fails, that run syncs without deletions rather than acting on a partial picture.
- **Missing roots are refused.** A vanished local root (unmounted drive) or a vanished remote base folder is refused for that run, not read as "everything was deleted".
- **A view the engine cannot trust never deletes.** The local scan used to drop the errors of its own walk, so a folder it could not open (permissions) looked empty and everything in it looked deleted; the batch scan now counts what it could not see (`scan_local` returns it) and, like a failed remote listing, suppresses deletes and renames when it is not zero. The shallow listing does the same for a `read_dir` that fails or breaks off half way. A pair root that comes back empty while the baseline knows at least five files in it (`EMPTY_ROOT_MIN_FILES`; an unmounted drive's mount point, a folder emptied by mistake - below that, deleting the last file or two of a small folder is just a deletion) holds deletions back too, in both the batch and the shallow root reconcile. A tracked *file* that is now a symlink, pipe, socket or device is carried forward untouched (neither uploaded nor trashed); a tracked *folder* that is now a symlink holds deletions back, and it is never descended into.
- **Nothing is written through a symlink.** `safe_join_to_write` refuses a destination whose parent folders include a link (it would land outside the sync root), for downloads, mkdirs, renames and conflict copies; a shallow reconcile of a folder that is or sits below a link does nothing. The sync root itself may be a link.
- **A baseline belongs to its folders.** It is only meaningful for the local and remote folders it was taken from, and keyed by pair name alone it used to outlive a change of either: point the pair at another folder and every path it knew read as deleted on the side that changed. `pair_roots` records both folders (`Stats::pin_pair_roots`, called by `state::load_baseline_for`, which every sync entry point uses); when they differ the pair's baseline and retry backoff are discarded in one transaction and the run is a first run - a union, deleting nothing. A pair with no record yet (first run, or a database from before this) is just recorded and keeps its baseline.
- **Missing baseline unions.** A first run or a wiped state dir unions both sides rather than mirroring, so a lost baseline cannot trigger a mass delete.
- **Scope containment.** A scoped or shallow sync only ever looks at paths inside its folder, so nothing outside can be seen as missing and deleted. An empty scope means the pair root, not a folder literally named "".
- **Excludes are invisible.** Excluded sub-paths are pruned from the scan itself (local and remote), never listed, downloaded, or deleted. Re-including a folder re-downloads it, so excluding can never delete remote data.
- **Conflicts keep both.** Both sides changed and differ, keep both (`name (conflict <timestamp>).ext`), never silently overwrite.

## The GUI is up to three processes

In tray mode the background tray owns the watcher and does the syncing in one process; the window is a separate process; and `--tray` is a thin supervisor that holds the tray lock and runs the real tray (`--tray-child`) as its child, starting it again after a crash (waits of 2, 5, 15, 30 and 30 seconds; it gives up after five restarts in five minutes - logging why and, if notifications are on, telling you with a desktop notification (at most once an hour) because nothing else would - and stops at once when the display session is gone or the tray quits). The release build aborts on a panic, a broken display connection kills a GTK app, and the kernel's OOM killer picks its own victims, so without the supervisor any of them silently ended background sync. The supervisor does no GTK, syncing or config watching, so it survives what takes the tray down, and the kernel stops the tray if the supervisor itself dies, so there is never a tray without the lock.

The window does not trust the snapshot blindly: every couple of seconds it checks that a live process holds the tray lock and that `status.json` is less than ten seconds old. A dead supervisor is started again by the window; a tray that is alive but has stopped updating gets a "not responding" banner; three starts that never come up become "could not start", with a Try again button. The decisions are plain functions in `src/tray_policy.rs`.

The window cannot see the tray's in-memory state directly, so the tray publishes a compact live snapshot to `status.json` a few times a second and the window reads it. That is how an open window shows the daemon scanning, syncing, and per-file transfers in real time. A CLI `protondrive4linux sync` also records its operations into the same activity store the window reads, so command-line runs show up there too.

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
