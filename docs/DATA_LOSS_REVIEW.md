# Data-loss review (M9-003)

The checkpoint before a release candidate: could this program lose or damage a
user's files, and does it say so when it cannot be sure? This is what was
looked at, what was found, what was fixed, and - as importantly - what is
still a risk and what could not be checked.

Reviewed on 2026-10-17 against `master` at 0.4.3 plus the unreleased work (the
tray supervisor, the keyring wait). Issue #42; the fixes are PRs #85, #86 and
#87, and the scenario tests are in `tests/data_loss.rs`, `tests/disaster/` and
`tests/engine.rs`.

## Method

1. Read the engine (`src/engine.rs`), the CLI adapter (`src/protoncli.rs`), the
   baseline store (`src/stats.rs`, `src/state.rs`) and the local-delete path
   (`src/trash.rs`) asking one question of every place that writes, replaces or
   deletes a file on either side: *what is the worst thing that can have
   happened to the data it is acting on, between the moment it was looked at and
   the moment it is changed?*
2. Turn every answer that could lose data into a test, and **keep only the ones
   that fail on the code as it was**. Eleven did. A test that passes on the old
   code proves nothing, so each fix below has one that fails without it.
3. Fix, then **mutation-check**: break the fix on purpose and see that the test
   that names it fails (and only that). Twice this caught a test that was blind:
   a disaster test that killed only the parent process and so let the orphaned
   CLI finish its file, and a test of the folder-delete guard that never reached
   the code it was written for.
4. State what the tests cannot show.

## What was found, and what happens now

Each row was reproduced by a failing test first. "Delete" findings only matter
with `propagate_deletes` on (it is off by default), and both sides' deletes are
recoverable (the desktop trash, Proton's online trash) unless `local_delete =
"remove"` - which is why none of them is the same as the other kind of loss, the
one that has no trash to recover from: an overwrite.

| # | What went wrong | Loss | Now | PR |
|---|---|---|---|---|
| 1 | A download that died half way (disk full, the CLI killed at its timeout, a crash, a power cut) left a truncated file under its real name, or replaced the good local copy with a fragment | the local copy | Every download is written to a private `.protondrive4linux-….part` folder next to the destination and moved into place with one rename after it measures the promised size, has been `fsync`ed, and the local file is still what the plan saw (`src/staged.rs`) | #85 |
| 2 | A download that exited 0 having written too little was recorded as synced; the next run read it as the user's edit and **uploaded it over the good copy on the remote** | the remote copy | The size check above; the file is never put in place | #85 |
| 3 | A file edited (or created) after the scan and before a download landed was overwritten | the user's edit | The local file is re-checked at the moment of the rename; if it changed, the download is dropped and the two meet as a conflict next run | #85 |
| 4 | A conflict copy (`name (conflict <time>).ext`) overwrote an existing file of the same name (two conflicts in one second, or a file the user had named that way) | an earlier copy | A name nothing has, and a move that cannot replace | #85 |
| 5 | Two different edits of **the same size** at different times were taken for "metadata drift": the baseline was refreshed, and the next run downloaded the remote over the local file | the local edit, on both sides | A conflict (both kept) unless both sides' SHA-1 agree. Also fixes the first sync of a folder that exists on both sides | #87 |
| 6 | Deleting a folder deleted everything in it, including files that had never synced (new, edited, or put there by another device); the shallow walk the watcher runs never looked inside | files, in either direction | Before a folder is removed the baseline is checked against everything in it, locally or on the remote; anything unknown keeps the folder | #87 |
| 7 | A delete + create of the same size and second looked like a rename: the old content was moved to the new name and the new content never uploaded | the new file's content, on the remote | Confirmed against the remote's SHA-1 when it lists one | #87 |
| 8 | A pair pointed at another local or remote folder kept the old folder's baseline: everything read as deleted on the side that changed | the other side's copy | The folders a pair lives in are recorded; when either changes the baseline is discarded and the run is a union | #86 |
| 9 | A pair root that came back empty (an unmounted drive's mount point, a folder emptied by mistake) read as "every file deleted" | the remote copy | Deletions held back when at least five files are on record | #86 |
| 10 | The batch scan (`sync`, "Sync now") dropped the errors of its own walk: a folder it could not open looked empty | the remote copy | Counted as "could not see"; deletes and renames are suppressed. Same for a listing that breaks off | #86 |
| 11 | A tracked file or folder replaced by a symlink read as deleted (and a linked folder would have been followed); a download could be written **through a symlink to outside the sync root** | the remote copy; and a write outside the root | Carried forward untouched / deletes held back; symlinks are never followed and nothing is written through one | #86 |

One more, found while fixing: watcher events inside an excluded path (a user
exclude, a junk name, a download's own staging folder) queued a sync that cost a
CLI listing to find nothing. They are dropped now.

## The scenarios of the issue

What happens in each, and the test that shows it. "Review" means it could only
be argued from the code, not run.

| Scenario | What happens | Shown by |
|---|---|---|
| **`kill -9` / SIGTERM** during a sync | Nothing is lost locally. The baseline only advances for transfers that finished, so a killed run re-detects the rest. Downloads in flight are in staging folders, never under a real name; they are swept once their process is gone | `sigkill_mid_sync_recovers_cleanly`, `sigterm_mid_sync_recovers_cleanly`, `sigkill_mid_download_never_leaves_a_partial_file_under_its_real_name`, `sigkill_mid_download_over_existing_files_keeps_each_old_or_new_never_half`, `sigterm_mid_download_…` (these kill the whole process group, so the CLI dies mid-write too) |
| **Power loss** | The baseline is SQLite in WAL mode with `synchronous = FULL`, so a commit is on disk when it returns, and a downloaded file is `fsync`ed (and its folder) *before* the baseline can say it is synced: the bytes cannot be missing behind a row that says they are there. A trash or a conflict-copy move is one journaled rename (or link + unlink) | Review, plus `the_baseline_is_committed_durably` pins the SQLite setting. **Not runnable here**: a real power cut needs hardware |
| **Network loss** | Every call fails, nothing is deleted or changed, local edits wait and go up when it is back, remote changes come down, and an edit on both sides becomes a conflict. A CLI call that hangs is killed at `cli_timeout_secs` | `offline_then_reconnect_reconciles_correctly`, `crashing_cli_does_not_corrupt_state`, `a_cli_that_hits_its_timeout_mid_download_leaves_no_partial_file_and_no_junk`, `tests/protoncli_timeout.rs` |
| **Disk full** | A download that fails part way leaves the old local file as it was, no partial file, no conflict copy and no junk uploaded; the retry succeeds. A disk too full for the baseline to be written only means the next run re-detects (the file is then seen as equal on both sides and nothing is transferred) | `a_download_that_dies_half_way_leaves_nothing_under_the_final_name`, `a_download_that_dies_over_an_existing_file_keeps_the_old_copy`. The failure is simulated (a download that writes part of the file and errors); a real full disk was not created |
| **Permission denied** | An unreadable folder never makes its files look deleted; an unreadable file is an error and the remote copy is untouched; an unwritable destination fails the download cleanly | `an_unreadable_local_folder_never_deletes_what_it_holds`, `a_file_that_cannot_be_read_is_an_error_and_never_touches_the_remote_copy`, `a_download_into_a_folder_that_cannot_be_written_fails_cleanly` (skipped when run as root) |
| **Remote unavailable** | A failed listing, a missing remote base folder, and a missing subfolder all suppress deletes for the run; a remote that lists most of the tree as gone deletes at most the guard's allowance locally (to the desktop trash) and is reported | `incomplete_remote_scan_suppresses_delete`, `vanished_remote_base_suppresses_delete`, `shallow_remote_list_failure_suppresses_delete`, `shallow_remote_notfound_suppresses_delete`. **Not covered**: a listing that *succeeds* but is wrong (an empty array from an API glitch) is believed, now bounded by the mass-deletion guard - see the limits below |
| **Local unavailable** | A missing root refuses the pair; an empty root with files on record holds deletions back; a folder that is now a symlink holds them back | `missing_local_root_refuses_delete`, `an_empty_local_root_never_deletes_the_remote`, `a_tracked_folder_replaced_by_a_symlink_is_not_a_deletion_and_is_never_followed` |
| **Conflict** | Both versions are kept (the default); `newer` and `skip` are tested; a conflict copy never replaces anything; a conflict whose transfer fails half way loses nothing and converges. Equal sizes are no longer taken for equal content | the `conflict_*` tests of `tests/engine.rs`, `a_conflict_copy_never_overwrites_a_file_that_already_has_its_name`, `a_conflict_whose_download_fails_loses_nothing_and_converges`, `two_different_edits_of_the_same_size_are_a_conflict_not_metadata_drift` |
| **Rename** | A real rename is a move (and not a re-upload); a look-alike (same size and second, other bytes) is not, when the remote lists a digest | `local_rename_becomes_remote_move`, `a_real_rename_is_still_a_move_even_with_digests`, `a_new_file_with_the_size_and_time_of_a_deleted_one_is_not_a_rename`. Only the batch scan detects renames; the watcher's shallow walk sees a delete and a create |
| **Large file** | A download is staged and verified however big it is. The limit is time: a transfer that does not finish within `cli_timeout_secs` (1800 by default) is killed, retried with backoff, and killed again - nothing is lost or left behind, but a file that takes longer than that never arrives. Raise `cli.cli_timeout_secs` for a slow link and big files | `a_cli_that_hits_its_timeout_mid_download_leaves_no_partial_file_and_no_junk`. A multi-gigabyte transfer was not run |
| **Large tree** | 3000 files in 210 folders converge in both directions with both walks, and a further pass changes nothing (in a one-off measurement, re-planning 6000 files took 0.4 s against the in-memory remote) | `a_large_tree_converges_in_both_directions_with_both_walks_and_then_does_nothing`. Not run at hundreds of thousands of files, nor against a real CLI's listing time |

## Limits that remain

Decisions or work, not fixes - none of these is hidden by the tests above.

1. ~~**No mass-deletion circuit breaker.**~~ **Added after the review** (the one
   decision it left open): with `propagate_deletes` on, one run may delete at most
   the larger of `max_delete_files` (100) and `max_delete_percent` (20 %) of the
   files a pair tracks; more is held back whole - the transfers carry on - and
   reported with both ways out (`sync --allow-mass-delete`, `sync --resync`). A
   full walk counts the files on disk first, so a tree that has mostly gone
   deletes nothing; a walk spends one allowance across all its folders; the
   remote being emptied is bounded to the allowance. What it does not do: a
   deletion *spread over many runs*, each under the allowance (a user deleting
   half a library over an afternoon, one debounced batch at a time), is not one
   event to it. The window shows a held run as a banner on the Activity page
   with **Restore them** / **Delete them**, the tray sends one notification, and
   the hold stands until a run over the whole folder finds nothing to hold (a
   pass over a few edited folders does not clear it). Tests: the
   `a_deletion_far_bigger_than_the_allowance_…` family in `tests/data_loss.rs`,
   and `tests/controller_held.rs` for what a frontend sees and can do.
2. **A listing that succeeds but is wrong is believed.** An empty folder
   listing is a real "everything was deleted there". If the CLI or the API ever
   returned one for a folder that is not empty, the local files go to the
   desktop trash (recoverable).
3. **`size+mtime` is a heuristic.** An edit that keeps the size and lands in the
   same whole second as the previous modification is not seen; a remote entry
   with no modification time is compared by size alone. `compare = "sha1"` has
   neither blind spot. Equal sizes with different times and no digest on the
   remote become a conflict copy rather than a guess.
4. **Renames are only checked when the remote lists a SHA-1** (real cli-drive
   0.8 listings do carry `claimedDigests`; the fixture in `protoncli.rs` is from
   a real file). Without one, size + second still decide.
5. **The timeout kills the CLI process, not its process group.** If the real
   `proton-drive` is a wrapper that starts something else, that something may
   outlive the kill and keep writing; into staging, which is then gone, so it
   fails harmlessly - but an orphaned *upload* may still finish.
6. **Two runs at once are not prevented.** One process serialises its own runs,
   but the window's "Sync now" and the tray's watcher are separate processes
   with no lock between them. The primitives are made race-tolerant (staged
   installs, the local-unchanged check, no-clobber conflict names, idempotent
   deletes), which turns the worst outcome into a duplicate conflict copy, not
   into a loss.
7. **Non-UTF-8 file names** are skipped by the batch scan and converted lossily
   by the shallow one: such a file is not synced (or fails to upload), never
   deleted.
8. **The built-in ignore list is silent.** Anything matching it (`*~`, `*.tmp`,
   `*.part`, `Thumbs.db`, `.DS_Store`, a folder named `x.tmp`, ...) is never
   backed up, and a folder with such a name hides everything under it. That is
   the point of the list, but it is a way for data to be absent that nothing
   reports.
9. **A download needs room for both copies** while it runs (the old one used to
   be deleted first).
10. **The Newer conflict policy overwrites the older side** (the remote's goes to
    Proton's online trash; the local one is replaced). Documented, opt-in,
    `keep-both` is the default.

## What this review could not check

- **The real `proton-drive` CLI.** There is no account here. Everything above
  runs against an in-memory remote and a stand-in CLI (`fake-proton-drive`)
  that is as faithful as what is known of the real one (it now keeps the
  modification time on upload, as the real `claimedModificationTime` does).
  Not exercised: that the real CLI downloads into an existing empty folder and
  names the file as the remote does (that is how the destination was already
  used, and a lone file under another name is accepted); that `claimedSize`
  always equals the delivered size (a file for which it does not now fails its
  download with a clear message rather than being installed); and what a
  locked or absent keyring makes it print beyond the prefix the code matches.
- **Real power loss and a real full disk.** Reasoned and simulated, as above.
- **A very large tree or file** on a real connection.

## Re-running it

```sh
cargo test --all-features                      # everything
cargo test --test data_loss                    # the review's scenarios, in-process
cargo test --test disaster                     # real processes, real signals
cargo test --test engine                       # the decision table
```

A change to the engine's merge, delete or download paths should be judged by
whether a test in `tests/data_loss.rs` would still fail without it - the
property to keep is that every guard here has a test that fails when the guard
is removed.
