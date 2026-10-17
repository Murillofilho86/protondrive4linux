# Sync State Matrix

Closes M0-003. This is the actual decision table the engine implements — extracted by reading
`decide`/`decide_dir`/`classify` in `src/engine.rs`, not written from intent. Where a row has no
test citation, that is a real gap, not an oversight in this document — see § Coverage gaps.

## How to read this

Every path is classified independently on each side against **the same baseline row**
(`classify()` in `src/engine.rs`): `Created` (present now, absent from baseline), `Modified`
(present in both, content differs under `options.compare`), `Deleted` (absent now, present in
baseline), `Unchanged` (present in both, same content), or `Absent` (absent from both — the path
never reached this side of the baseline). Because both sides classify against **one shared
baseline entry**, only 13 of the 25 `(local, remote)` combinations are actually reachable:

- Baseline **has** a row for this path → each side is `Unchanged`, `Modified`, or `Deleted` (9
  combinations).
- Baseline **has no** row for this path → each side is `Created` or `Absent` (4 combinations;
  `Absent`×`Absent` never reaches the decision function at all, since keys are drawn from
  `local ∪ remote ∪ base`).

A directory hits `decide_dir` instead of the file table below (no content compare, no conflict —
see § Directories). A path that is a file on one side and a directory on the other is a type
clash: skipped and surfaced as an error, baseline row left untouched, **before** either table
below is consulted (`decide`, top of function).

## Files — baseline has a row for this path

| LOCAL | REMOTE | Action | Notes | Test |
| --- | --- | --- | --- | --- |
| Unchanged | Unchanged | Noop, baseline kept | | `idempotent_second_run` |
| Modified | Unchanged | Upload | | `modify_local_uploads` |
| Unchanged | Modified | Download | | (covered implicitly by streaming tests; no dedicated unit test) |
| Modified | Modified, same content | Noop, baseline refreshed | `same_content()` under the configured `compare` mode | — |
| Modified | Modified, same size, **digests equal** (the remote listed a SHA-1 and the local file hashes to it) | Noop, baseline refreshed | A touch on both sides, same bytes: only a digest can prove that. The local file is hashed just for this case (`digest_equal_sizes`) | `the_same_content_with_drifted_times_is_not_a_conflict_when_the_digest_proves_it` |
| Modified | Modified, same size, digests differ **or the remote listed none** | Conflict (per `ConflictPolicy`) | Same size is not same content: two edits of equal length (a counter, a flag) look exactly like a touch. This used to be refreshed as "metadata drift" (a 0.4.0 fix for conflict copies multiplying, which was really the `claimedSize` misparse); the next run then downloaded the remote over the local file and the local edit was gone from both sides | `two_different_edits_of_the_same_size_are_a_conflict_not_metadata_drift` |
| Modified | Modified, real content differs | Conflict (per `ConflictPolicy`) | See § Conflict policy | `conflict_keep_both`, `streaming_keeps_both_on_conflict` |
| Deleted | Unchanged | DeleteRemote (if `propagate_deletes`) else Noop, baseline kept | | `delete_local_propagate_trashes_remote`, `delete_local_no_propagate` |
| Unchanged | Deleted | DeleteLocal (if `propagate_deletes`) else Noop, baseline kept | | `delete_remote_propagate_removes_local` |
| Deleted | Deleted | Noop, no baseline row | Gone on both sides — nothing to reconcile | — |
| Deleted | Modified | Download, keep remote | Delete-vs-change never loses the surviving edit | — |
| Modified | Deleted | Upload, keep local | Delete-vs-change never loses the surviving edit | — |

## Files — baseline has no row for this path

| LOCAL | REMOTE | Action | Notes | Test |
| --- | --- | --- | --- | --- |
| Created | Absent | Upload ("new local") | | `initial_upload` |
| Absent | Created | Download ("new remote") | | `download_new_remote` |
| Created | Created, same content | Noop, baseline set | Both sides created the identical file independently (judged as the rows above) | — |
| Created | Created, differs | Conflict (per `ConflictPolicy`) | Same conflict path as the baseline-present case, including equal sizes without a proving digest: the first sync of a folder that already exists on both sides no longer replaces one of two different `notes.txt` | `files_made_independently_on_both_sides_with_equal_sizes_are_both_kept` |

## Directories (`decide_dir`)

No content comparison, no conflict — a directory is a container, not a value:

| LOCAL | REMOTE | Action | Notes |
| --- | --- | --- | --- |
| Created | (none on remote) | MkdirRemote | |
| (none on local) | Created | MkdirLocal | |
| Deleted | Unchanged | DeleteRemote (if `propagate_deletes`) else Noop | At execution, the remote folder is listed and must hold only files the baseline knows and that are unchanged (`folder_delete_hazard`); otherwise it is kept and its files reconcile on their own | `a_folder_removed_locally_keeps_remote_files_this_machine_never_saw` |
| Unchanged | Deleted | DeleteLocal (if `propagate_deletes`) else Noop | Same, locally: nothing inside may be new or edited here, and no exclude may sit inside it | `a_folder_removed_remotely_keeps_local_files_that_never_synced` |
| anything else | | Noop, baseline kept from whichever side has an entry | Includes both-created (no mkdir race handling needed — mkdir is idempotent on both backends) |

## Conflict policy (`ConflictPolicy`, applies to the Conflict rows above)

| Policy | Behavior |
| --- | --- |
| `KeepBoth` (default) | Remote version wins the original name in the new baseline; local copy is renamed aside by the transfer layer. Never destroys either version. (`conflict_keep_both`, `streaming_keeps_both_on_conflict`) |
| `Newer` | Compares mtimes; whichever is newer wins (upload or download). If either side lacks an mtime, **falls back to `KeepBoth`** rather than guessing — same safety guarantee as the default. (`conflict_newer_local_wins`, `conflict_newer_remote_wins`, `conflict_newer_without_remote_mtime_falls_back_to_keep_both`) |
| `Skip` | Neither side is touched; baseline row is left as Noop until the user resolves it manually. (`conflict_skip_leaves_both_sides_untouched`) |

**Every policy reports how it settled the conflict** (`SyncEvent::Conflict`, emitted only once the
resolution has actually completed - a failed transfer is re-detected next run and reports nothing
yet; a dry run reports nothing): `KeepBoth` -> `KeptBoth` with the preserved copy's name, `Newer` ->
`KeptLocal` / `KeptRemote`, `Skip` -> `Skipped` (re-reported on every run until the user settles
it). The events feed the conflict inbox (`conflicts` table in `stats.db`). A `Skipped` entry
auto-clears when its file finally syncs. (`keep_both_conflict_reports_the_preserved_copy`,
`newer_conflict_reports_which_side_won`, `skipped_conflict_is_reported_on_every_run_until_it_is_settled`,
`dry_run_reports_no_conflicts`, `a_conflict_whose_transfer_failed_is_not_reported_until_it_succeeds`)

## Renames (`detect_renames`, post-processing on the plan)

After the table above produces a plan, same-content delete+create pairs are collapsed into a
single rename/move op (`Action::RenameLocal`/`RenameRemote`) — see
`local_rename_becomes_remote_move`. Skipped entirely when the remote scan was incomplete, since a
"missing" source might just be in an unlisted folder (`incomplete_remote_scan_suppresses_delete`
covers the related suppression, not the rename skip itself directly).

## Coverage gaps

Rows above with no test citation, or "implicit" coverage only, are real gaps — not just missing
documentation:

- **`Unchanged`/`Modified` (download path) has no dedicated unit test** at the `decide()` level,
  only indirect coverage via the streaming full-walk tests.
- ~~`ConflictPolicy::Newer` and `ConflictPolicy::Skip` have no test at all~~ — closed:
  `conflict_newer_local_wins`, `conflict_newer_remote_wins`,
  `conflict_newer_without_remote_mtime_falls_back_to_keep_both`, and
  `conflict_skip_leaves_both_sides_untouched` in `tests/engine.rs` (M1-005).
- ~~Same-size/drifted-mtime baseline refresh has no dedicated regression test~~ — it turned out to
  be a data-loss bug, not a thrash fix (see the rows above), and is now covered by
  `two_different_edits_of_the_same_size_are_a_conflict_not_metadata_drift` and friends in
  `tests/data_loss.rs`.
- **Both-sides-created-identical-directory** and **both-sides-created-identical-file** have no
  dedicated test confirming the baseline is set correctly and no duplicate mkdir/upload is
  attempted.

## Type clash (cross-cutting, checked before either table)

If a path is a file on one side and a directory on the other, `decide()` skips it and emits a
warning/error event without touching either side or the baseline row — the row keeps being
flagged every run until a human resolves it by renaming or removing one side.
(`file_replaced_by_dir_is_surfaced_not_merged`)
