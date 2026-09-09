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
| Modified | Modified, same size, no sha1 clash | Noop, baseline refreshed | Same-size + drifted mtime is treated as metadata drift, not a dual edit — prevents keep-both from multiplying conflict copies forever (0.4.0 fix) | — |
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
| Created | Created, same content | Noop, baseline set | Both sides created the identical file independently | — |
| Created | Created, differs | Conflict (per `ConflictPolicy`) | Same conflict path as the baseline-present case | — |

## Directories (`decide_dir`)

No content comparison, no conflict — a directory is a container, not a value:

| LOCAL | REMOTE | Action | Notes |
| --- | --- | --- | --- |
| Created | (none on remote) | MkdirRemote | |
| (none on local) | Created | MkdirLocal | |
| Deleted | Unchanged | DeleteRemote (if `propagate_deletes`) else Noop | |
| Unchanged | Deleted | DeleteLocal (if `propagate_deletes`) else Noop | |
| anything else | | Noop, baseline kept from whichever side has an entry | Includes both-created (no mkdir race handling needed — mkdir is idempotent on both backends) |

## Conflict policy (`ConflictPolicy`, applies to the Conflict rows above)

| Policy | Behavior |
| --- | --- |
| `KeepBoth` (default) | Remote version wins the original name in the new baseline; local copy is renamed aside by the transfer layer. Never destroys either version. (`conflict_keep_both`, `streaming_keeps_both_on_conflict`) |
| `Newer` | Compares mtimes; whichever is newer wins (upload or download). If either side lacks an mtime, **falls back to `KeepBoth`** rather than guessing — same safety guarantee as the default. (`conflict_newer_local_wins`, `conflict_newer_remote_wins`, `conflict_newer_without_remote_mtime_falls_back_to_keep_both`) |
| `Skip` | Neither side is touched; baseline row is left as Noop until the user resolves it manually. (`conflict_skip_leaves_both_sides_untouched`) |

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
- **Same-size/drifted-mtime baseline refresh** (the 0.4.0 thrash fix) has no dedicated regression
  test under this name — it's exercised indirectly by whatever real-world scenario prompted the
  0.4.0 fix, but a named test would prevent silent regression.
- **Both-sides-created-identical-directory** and **both-sides-created-identical-file** have no
  dedicated test confirming the baseline is set correctly and no duplicate mkdir/upload is
  attempted.

## Type clash (cross-cutting, checked before either table)

If a path is a file on one side and a directory on the other, `decide()` skips it and emits a
warning/error event without touching either side or the baseline row — the row keeps being
flagged every run until a human resolves it by renaming or removing one side.
(`file_replaced_by_dir_is_surfaced_not_merged`)
