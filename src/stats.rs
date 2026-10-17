//! Change-frequency stats, backed by SQLite, used to find "hot" folders so the
//! watcher can scan busy subtrees more often than the full tree.
//!
//! Each local/remote change to a file bumps two folders: the file's containing
//! subfolder AND its direct parent (a change deep in a tree keeps both the leaf
//! and one level up warm). `hot_folders` then returns the folders with the most
//! recent activity, which the tiered scanner re-lists on the short interval.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::events::ConflictResolution;
use crate::models::Entry;

/// Stamped into the DB's `PRAGMA user_version` on every successful open. Bump
/// this if a future change to the `baseline`/`pair_state` schema would make an
/// older or newer build misinterpret existing rows (not needed for additive,
/// backward-compatible changes like the `state`/`error` columns below, which
/// use `ALTER TABLE` migrations instead).
const SCHEMA_VERSION: i64 = 1;

pub struct Stats {
    conn: Mutex<Connection>,
}

/// A recorded file operation, for restoring the activity feed on launch.
#[derive(Clone, Debug)]
pub struct OpRecord {
    pub ts: i64,
    pub pair: String,
    pub action: String,
    pub path: String,
    pub ok: bool,
    /// Why it failed, when it did. `None` for successful ops.
    pub error: Option<String>,
}

/// One entry in the conflict inbox (M1-005): a file that changed on both sides
/// and how it was settled.
#[derive(Clone, Debug)]
pub struct ConflictRecord {
    pub id: i64,
    /// Epoch seconds the conflict was settled (or first seen, for `Skipped`).
    pub ts: i64,
    pub pair: String,
    /// The path that conflicted, relative to the pair root.
    pub rel: String,
    pub resolution: ConflictResolution,
    /// For `KeptBoth`: the preserved local copy's path.
    pub copy_rel: Option<String>,
    /// The user has acknowledged it (or, for `Skipped`, the file synced).
    pub dismissed: bool,
}

/// One `conflicts` row (columns: id, ts, pair, rel, resolution, copy_rel,
/// dismissed). A row written by a newer build with a resolution this one
/// doesn't know yields `None` - skipped rather than guessed at.
fn conflict_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Option<ConflictRecord>> {
    let resolution: String = r.get(4)?;
    let Some(resolution) = ConflictResolution::parse(&resolution) else {
        return Ok(None);
    };
    Ok(Some(ConflictRecord {
        id: r.get(0)?,
        ts: r.get(1)?,
        pair: r.get(2)?,
        rel: r.get(3)?,
        resolution,
        copy_rel: r.get(5)?,
        dismissed: r.get::<_, i64>(6)? != 0,
    }))
}

impl Stats {
    /// Open (creating if needed) the stats DB under `state_dir`.
    pub fn open(state_dir: &Path) -> Result<Self> {
        // stats.db and the status.json this directory also holds record file
        // paths and content hashes for everything synced, so keep the
        // directory private to the user (matches logger.rs's log directory).
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(state_dir);
        }
        let path = state_dir.join("stats.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("opening stats db {}", path.display()))?;

        // A version of 0 means either a brand-new file (nothing to conflict
        // with) or one written before this check existed (same schema shape,
        // safe to adopt going forward — stamped below). Anything else that
        // isn't our own version is a schema we don't know how to read
        // correctly; refuse rather than risk misinterpreting rows a
        // different version wrote under different assumptions.
        let existing_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .with_context(|| format!("reading schema version of {}", path.display()))?;
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            anyhow::bail!(
                "{} has schema version {existing_version}, but this build expects \
                 {SCHEMA_VERSION}. Refusing to open it rather than risk misreading its \
                 baseline as empty. Back it up and remove it to start a fresh baseline \
                 (a missing baseline unions both sides rather than deleting anything), or \
                 use a build that matches this version.",
                path.display()
            );
        }

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS events (
                 pair   TEXT    NOT NULL,
                 folder TEXT    NOT NULL,
                 ts     INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_pair_ts ON events(pair, ts);
             CREATE TABLE IF NOT EXISTS ops (
                 pair   TEXT    NOT NULL,
                 action TEXT    NOT NULL,
                 path   TEXT    NOT NULL,
                 ok     INTEGER NOT NULL,
                 ts     INTEGER NOT NULL,
                 error  TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_ops_ts ON ops(ts);
             CREATE TABLE IF NOT EXISTS baseline (
                 pair      TEXT    NOT NULL,
                 rel       TEXT    NOT NULL,
                 is_dir    INTEGER NOT NULL,
                 size      INTEGER NOT NULL,
                 mtime     INTEGER,
                 sha1      TEXT,
                 remote_id TEXT,
                 state     TEXT    NOT NULL DEFAULT 'synced',
                 PRIMARY KEY (pair, rel)
             );
             CREATE TABLE IF NOT EXISTS pair_state (
                 pair        TEXT PRIMARY KEY,
                 last_synced INTEGER
             );
             CREATE TABLE IF NOT EXISTS retry_state (
                 pair     TEXT    NOT NULL,
                 rel      TEXT    NOT NULL,
                 attempts INTEGER NOT NULL,
                 next_at  INTEGER NOT NULL,
                 PRIMARY KEY (pair, rel)
             );
             CREATE TABLE IF NOT EXISTS conflicts (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 pair       TEXT    NOT NULL,
                 rel        TEXT    NOT NULL,
                 ts         INTEGER NOT NULL,
                 resolution TEXT    NOT NULL,
                 copy_rel   TEXT,
                 dismissed  INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_conflicts_open ON conflicts(pair, rel, dismissed);",
        )
        .context("initialising stats schema")?;
        // Migrate older DBs that predate the per-file state column. Existing
        // rows were fully synced, so 'synced' is the right default.
        let _ = conn.execute(
            "ALTER TABLE baseline ADD COLUMN state TEXT NOT NULL DEFAULT 'synced'",
            [],
        );
        // Migrate DBs whose ops table predates the error column. Older failed
        // rows keep a NULL reason: the message was never recorded, and the only
        // copy is the text log.
        let _ = conn.execute("ALTER TABLE ops ADD COLUMN error TEXT", []);
        // Drop any rows a previous build left in a transient "pending" state:
        // their stored metadata reflected a transfer that had not completed, so
        // it can't be trusted. Deleting them makes the next run re-detect and
        // re-sync those files safely (never as a deletion).
        let _ = conn.execute(
            "DELETE FROM baseline WHERE state IN ('pending_up', 'pending_down')",
            [],
        );
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .with_context(|| format!("stamping schema version on {}", path.display()))?;
        Ok(Stats {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory DB, for tests.
    #[cfg(test)]
    fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE events (pair TEXT NOT NULL, folder TEXT NOT NULL, ts INTEGER NOT NULL);
             CREATE TABLE ops (pair TEXT NOT NULL, action TEXT NOT NULL, path TEXT NOT NULL, ok INTEGER NOT NULL, ts INTEGER NOT NULL, error TEXT);
             CREATE TABLE retry_state (pair TEXT NOT NULL, rel TEXT NOT NULL, attempts INTEGER NOT NULL, next_at INTEGER NOT NULL, PRIMARY KEY (pair, rel));
             CREATE TABLE conflicts (id INTEGER PRIMARY KEY AUTOINCREMENT, pair TEXT NOT NULL, rel TEXT NOT NULL, ts INTEGER NOT NULL, resolution TEXT NOT NULL, copy_rel TEXT, dismissed INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE pair_state (pair TEXT PRIMARY KEY, last_synced INTEGER);
             CREATE TABLE baseline (pair TEXT NOT NULL, rel TEXT NOT NULL, is_dir INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER, sha1 TEXT, remote_id TEXT, state TEXT NOT NULL DEFAULT 'synced', PRIMARY KEY (pair, rel));",
        )?;
        Ok(Stats {
            conn: Mutex::new(conn),
        })
    }

    /// Record a change to `rel_file` (POSIX path relative to the pair root) at
    /// epoch `ts`. Bumps the containing subfolder and its direct parent.
    pub fn record_change(&self, pair: &str, rel_file: &str, ts: i64) -> Result<()> {
        let folder = folder_of(rel_file);
        let conn = self.conn.lock().unwrap();
        insert(&conn, pair, folder, ts)?;
        if let Some(parent) = parent_of(folder) {
            insert(&conn, pair, parent, ts)?;
        }
        Ok(())
    }

    /// Folders for `pair` with at least `threshold` changes since `now - window`,
    /// hottest first. Folder "" is the pair root.
    pub fn hot_folders(
        &self,
        pair: &str,
        window_secs: i64,
        threshold: i64,
        now: i64,
    ) -> Result<Vec<String>> {
        let since = now - window_secs;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT folder, COUNT(*) c FROM events
             WHERE pair = ?1 AND ts >= ?2
             GROUP BY folder HAVING c >= ?3
             ORDER BY c DESC, folder ASC",
        )?;
        let rows = stmt.query_map(params![pair, since, threshold], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Drop events older than `now - keep_secs` so the DB doesn't grow forever.
    pub fn prune(&self, keep_secs: i64, now: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM events WHERE ts < ?1", params![now - keep_secs])?;
        Ok(())
    }

    /// Persist a completed file operation for the activity history. `error` is
    /// the reason a failed op failed, so "what broke and why" is one query.
    pub fn record_op(
        &self,
        pair: &str,
        action: &str,
        path: &str,
        ok: bool,
        error: Option<&str>,
        ts: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ops(pair, action, path, ok, error, ts) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![pair, action, path, ok as i64, error, ts],
        )?;
        Ok(())
    }

    /// The most recent `limit` operations, newest first.
    pub fn recent_ops(&self, limit: usize) -> Result<Vec<OpRecord>> {
        self.query_ops("SELECT ts, pair, action, path, ok, error FROM ops", limit)
    }

    /// The most recent `limit` FAILED operations, newest first. This is the
    /// "what is broken right now" query.
    pub fn recent_failures(&self, limit: usize) -> Result<Vec<OpRecord>> {
        self.query_ops(
            "SELECT ts, pair, action, path, ok, error FROM ops WHERE ok = 0",
            limit,
        )
    }

    fn query_ops(&self, select: &str, limit: usize) -> Result<Vec<OpRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!("{select} ORDER BY rowid DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(OpRecord {
                ts: r.get(0)?,
                pair: r.get(1)?,
                action: r.get(2)?,
                path: r.get(3)?,
                ok: r.get::<_, i64>(4)? != 0,
                error: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Keep only the newest `keep` operation rows.
    pub fn prune_ops(&self, keep: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM ops WHERE rowid NOT IN (SELECT rowid FROM ops ORDER BY rowid DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        Ok(())
    }

    // -- sync baseline (per-pair three-way-merge snapshot) ------------------

    /// The stored baseline for `pair` (empty map if none), keyed by POSIX rel path.
    pub fn load_baseline(&self, pair: &str) -> Result<BTreeMap<String, Entry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rel, is_dir, size, mtime, sha1, remote_id FROM baseline WHERE pair = ?1",
        )?;
        let rows = stmt.query_map(params![pair], |r| {
            let rel: String = r.get(0)?;
            Ok(Entry {
                path: rel,
                is_dir: r.get::<_, i64>(1)? != 0,
                size: r.get::<_, i64>(2)? as u64,
                mtime: r.get::<_, Option<i64>>(3)?,
                sha1: r.get::<_, Option<String>>(4)?,
                remote_id: r.get::<_, Option<String>>(5)?,
            })
        })?;
        let mut out = BTreeMap::new();
        for e in rows {
            let e = e?;
            out.insert(e.path.clone(), e);
        }
        Ok(out)
    }

    /// Replace the whole baseline for `pair` atomically.
    pub fn save_baseline(&self, pair: &str, entries: &BTreeMap<String, Entry>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM baseline WHERE pair = ?1", params![pair])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO baseline(pair, rel, is_dir, size, mtime, sha1, remote_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (rel, e) in entries {
                ins.execute(params![
                    pair,
                    rel,
                    e.is_dir as i64,
                    e.size as i64,
                    e.mtime,
                    e.sha1,
                    e.remote_id
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Per-file state for `pair` (rel -> "synced" | "pending_down" | "pending_up").
    pub fn baseline_states(&self, pair: &str) -> Result<HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT rel, state FROM baseline WHERE pair = ?1")?;
        let rows = stmt.query_map(params![pair], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut m = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            m.insert(k, v);
        }
        Ok(m)
    }

    /// Incremental, additive baseline commit: upsert every entry as `synced`
    /// and drop the rows for files that were genuinely deleted/renamed away.
    /// Only files whose transfer SUCCEEDED this run are present in `entries`
    /// (the caller omits failed ones), so a failed transfer leaves its previous
    /// row intact and the file is simply retried next run. Unlisted/unseen rows
    /// are never touched — never a wholesale replace — so a partial scan is safe.
    pub fn commit_baseline(
        &self,
        pair: &str,
        entries: &BTreeMap<String, Entry>,
        removed: &HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO baseline(pair, rel, is_dir, size, mtime, sha1, remote_id, state)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'synced')
                 ON CONFLICT(pair, rel) DO UPDATE SET
                   is_dir=?3, size=?4, mtime=?5, sha1=?6, remote_id=?7, state='synced'",
            )?;
            for (rel, e) in entries {
                ins.execute(params![
                    pair,
                    rel,
                    e.is_dir as i64,
                    e.size as i64,
                    e.mtime,
                    e.sha1,
                    e.remote_id,
                ])?;
            }
            let mut del = tx.prepare("DELETE FROM baseline WHERE pair = ?1 AND rel = ?2")?;
            for rel in removed {
                del.execute(params![pair, rel])?;
            }
            // A skipped conflict is settled once its file finally syncs (the user
            // made the two sides agree, or switched the policy) or disappears:
            // take it out of the inbox. Looked up first - there are normally
            // none, and a big commit shouldn't pay a statement per file for it.
            let open: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT rel FROM conflicts
                     WHERE pair = ?1 AND resolution = 'skipped' AND dismissed = 0",
                )?;
                let rows = stmt.query_map(params![pair], |r| r.get::<_, String>(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            if !open.is_empty() {
                let mut dismiss = tx.prepare(
                    "UPDATE conflicts SET dismissed = 1
                     WHERE pair = ?1 AND rel = ?2 AND resolution = 'skipped' AND dismissed = 0",
                )?;
                for rel in open {
                    if entries.contains_key(&rel) || removed.contains(&rel) {
                        dismiss.execute(params![pair, rel])?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Per-file retry backoff state for `pair` (rel -> (consecutive failures,
    /// epoch seconds it's next eligible to be retried)). A path with no row
    /// has never failed, or already recovered (M4-004).
    pub fn retry_state(&self, pair: &str) -> Result<HashMap<String, (u32, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT rel, attempts, next_at FROM retry_state WHERE pair = ?1")?;
        let rows = stmt.query_map(params![pair], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)? as u32, r.get::<_, i64>(2)?),
            ))
        })?;
        let mut m = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            m.insert(k, v);
        }
        Ok(m)
    }

    /// Apply one run's retry outcomes: clear the backoff row for every path
    /// that succeeded this run (a no-op if it never had one), and upsert the
    /// new `(attempts, next_at)` for every path that failed again.
    pub fn apply_retry_updates(
        &self,
        pair: &str,
        upserts: &[(String, u32, i64)],
        clears: &[String],
    ) -> Result<()> {
        if upserts.is_empty() && clears.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO retry_state(pair, rel, attempts, next_at) VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(pair, rel) DO UPDATE SET attempts=?3, next_at=?4",
            )?;
            for (rel, attempts, next_at) in upserts {
                ins.execute(params![pair, rel, *attempts as i64, next_at])?;
            }
            let mut del = tx.prepare("DELETE FROM retry_state WHERE pair = ?1 AND rel = ?2")?;
            for rel in clears {
                del.execute(params![pair, rel])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record how a conflict was settled. A `Skipped` conflict is re-detected on
    /// every run until the user settles it, so while one is still open for the
    /// same file this keeps that first entry (and its time) instead of piling up
    /// a duplicate per run.
    pub fn record_conflict(
        &self,
        pair: &str,
        rel: &str,
        resolution: ConflictResolution,
        copy: Option<&str>,
        ts: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        if resolution == ConflictResolution::Skipped {
            let open: i64 = conn.query_row(
                "SELECT COUNT(*) FROM conflicts
                 WHERE pair = ?1 AND rel = ?2 AND resolution = 'skipped' AND dismissed = 0",
                params![pair, rel],
                |r| r.get(0),
            )?;
            if open > 0 {
                return Ok(());
            }
        }
        conn.execute(
            "INSERT INTO conflicts(pair, rel, ts, resolution, copy_rel) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![pair, rel, ts, resolution.as_str(), copy],
        )?;
        Ok(())
    }

    /// Conflict inbox entries, newest first. Only unresolved ones unless
    /// `include_dismissed` (the history view).
    pub fn conflicts(&self, include_dismissed: bool, limit: usize) -> Result<Vec<ConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, pair, rel, resolution, copy_rel, dismissed FROM conflicts
             WHERE dismissed = 0 OR ?1
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![include_dismissed, limit as i64], conflict_row)?;
        Ok(rows
            .filter_map(|r| r.transpose())
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Conflicts recorded after `after_id`, oldest first - what a notifier has
    /// not yet seen. Includes ones already dismissed: they're new either way.
    pub fn conflicts_since(&self, after_id: i64, limit: usize) -> Result<Vec<ConflictRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, pair, rel, resolution, copy_rel, dismissed FROM conflicts
             WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after_id, limit as i64], conflict_row)?;
        Ok(rows
            .filter_map(|r| r.transpose())
            .collect::<std::result::Result<_, _>>()?)
    }

    /// The highest conflict id so far (0 if none): where a notifier starts, so
    /// it only announces conflicts that happen after it began watching.
    pub fn max_conflict_id(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM conflicts", [], |r| {
                r.get(0)
            })?,
        )
    }

    /// How many conflicts are still unresolved (the inbox badge).
    pub fn conflict_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM conflicts WHERE dismissed = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Acknowledge one conflict (it stays in the history).
    pub fn dismiss_conflict(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conflicts SET dismissed = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Acknowledge every open conflict.
    pub fn dismiss_all_conflicts(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE conflicts SET dismissed = 1 WHERE dismissed = 0", [])?;
        Ok(())
    }

    /// Keep only the newest `keep` acknowledged conflicts; open ones are never
    /// pruned, however many there are.
    pub fn prune_conflicts(&self, keep: usize) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM conflicts WHERE dismissed = 1 AND id NOT IN
             (SELECT id FROM conflicts WHERE dismissed = 1 ORDER BY id DESC LIMIT ?1)",
            params![keep as i64],
        )?;
        Ok(())
    }

    /// Whether `pair` has any baseline rows (i.e. it has synced before).
    pub fn has_baseline(&self, pair: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM baseline WHERE pair = ?1",
            params![pair],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Last successful sync time for `pair`, if any.
    pub fn last_synced(&self, pair: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT last_synced FROM pair_state WHERE pair = ?1",
                params![pair],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Drop every row for `pair` (baseline, per-file state, hotness, activity).
    /// Called when a folder is removed so a later folder of the same name can't
    /// inherit stale baseline state.
    pub fn forget_pair(&self, pair: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for tbl in [
            "baseline",
            "pair_state",
            "events",
            "ops",
            "retry_state",
            "conflicts",
        ] {
            conn.execute(&format!("DELETE FROM {tbl} WHERE pair = ?1"), params![pair])?;
        }
        Ok(())
    }

    /// Wipe all protondrive4linux metadata (every table). Local files are untouched.
    pub fn wipe(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM baseline; DELETE FROM pair_state; DELETE FROM events; DELETE FROM ops; \
             DELETE FROM retry_state; DELETE FROM conflicts;",
        )?;
        Ok(())
    }

    /// Record `pair`'s last successful sync time.
    pub fn set_last_synced(&self, pair: &str, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pair_state(pair, last_synced) VALUES(?1, ?2)
             ON CONFLICT(pair) DO UPDATE SET last_synced = ?2",
            params![pair, ts],
        )?;
        Ok(())
    }
}

fn insert(conn: &Connection, pair: &str, folder: &str, ts: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO events(pair, folder, ts) VALUES(?1, ?2, ?3)",
        params![pair, folder, ts],
    )?;
    Ok(())
}

/// The folder containing `rel_file` ("" for a file at the pair root).
fn folder_of(rel_file: &str) -> &str {
    match rel_file.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// The direct parent of `folder`, or None if `folder` is already the root.
fn parent_of(folder: &str) -> Option<&str> {
    if folder.is_empty() {
        None
    } else {
        Some(match folder.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_and_parent() {
        assert_eq!(folder_of("a/b/c.txt"), "a/b");
        assert_eq!(folder_of("c.txt"), "");
        assert_eq!(parent_of("a/b"), Some("a"));
        assert_eq!(parent_of("a"), Some(""));
        assert_eq!(parent_of(""), None);
    }

    #[test]
    fn records_subfolder_and_parent() {
        let s = Stats::memory().unwrap();
        // one change deep in a/b -> bumps "a/b" and "a"
        s.record_change("p", "a/b/c.txt", 100).unwrap();
        let hot = s.hot_folders("p", 1000, 1, 200).unwrap();
        assert!(hot.contains(&"a/b".to_string()));
        assert!(hot.contains(&"a".to_string()));
        assert!(!hot.contains(&"".to_string()));
    }

    #[test]
    fn hotness_threshold_and_window() {
        let s = Stats::memory().unwrap();
        for _ in 0..3 {
            s.record_change("p", "docs/x.txt", 100).unwrap(); // folder "docs" x3, root x3
        }
        s.record_change("p", "misc/y.txt", 100).unwrap(); // folder "misc" x1
                                                          // threshold 3 in a wide window -> only "docs" and "" (root, 4 events) qualify
        let hot = s.hot_folders("p", 10_000, 3, 200).unwrap();
        assert!(hot.contains(&"docs".to_string()));
        assert!(hot.contains(&"".to_string())); // root accumulated 4
        assert!(!hot.contains(&"misc".to_string()));
        // outside the window -> nothing
        let old = s.hot_folders("p", 10, 1, 1_000_000).unwrap();
        assert!(old.is_empty());
    }

    #[test]
    fn retry_state_round_trips_and_clears() {
        let s = Stats::memory().unwrap();
        assert!(s.retry_state("p").unwrap().is_empty());

        s.apply_retry_updates(
            "p",
            &[("a.txt".into(), 1, 100), ("b.txt".into(), 2, 200)],
            &[],
        )
        .unwrap();
        let state = s.retry_state("p").unwrap();
        assert_eq!(state.get("a.txt"), Some(&(1, 100)));
        assert_eq!(state.get("b.txt"), Some(&(2, 200)));

        // A later failure for the same path overwrites, not accumulates.
        s.apply_retry_updates("p", &[("a.txt".into(), 2, 150)], &[])
            .unwrap();
        assert_eq!(s.retry_state("p").unwrap().get("a.txt"), Some(&(2, 150)));

        // Clearing (a success) removes the row entirely.
        s.apply_retry_updates("p", &[], &["a.txt".into()]).unwrap();
        let state = s.retry_state("p").unwrap();
        assert!(!state.contains_key("a.txt"));
        assert!(state.contains_key("b.txt"), "unrelated row must survive");
    }

    #[test]
    fn retry_state_is_isolated_per_pair() {
        let s = Stats::memory().unwrap();
        s.apply_retry_updates("p1", &[("a.txt".into(), 1, 100)], &[])
            .unwrap();
        s.apply_retry_updates("p2", &[("a.txt".into(), 5, 999)], &[])
            .unwrap();
        assert_eq!(s.retry_state("p1").unwrap().get("a.txt"), Some(&(1, 100)));
        assert_eq!(s.retry_state("p2").unwrap().get("a.txt"), Some(&(5, 999)));
    }

    #[test]
    fn conflicts_are_listed_newest_first_and_can_be_dismissed() {
        let s = Stats::memory().unwrap();
        s.record_conflict(
            "p",
            "a.txt",
            ConflictResolution::KeptBoth,
            Some("a (conflict x).txt"),
            100,
        )
        .unwrap();
        s.record_conflict("p", "b.txt", ConflictResolution::KeptLocal, None, 200)
            .unwrap();
        assert_eq!(s.conflict_count().unwrap(), 2);

        let open = s.conflicts(false, 10).unwrap();
        assert_eq!(
            open.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(),
            ["b.txt", "a.txt"],
            "newest first"
        );
        assert_eq!(open[1].resolution, ConflictResolution::KeptBoth);
        assert_eq!(open[1].copy_rel.as_deref(), Some("a (conflict x).txt"));

        // Dismissed ones leave the inbox but stay in the history.
        s.dismiss_conflict(open[0].id).unwrap();
        assert_eq!(s.conflict_count().unwrap(), 1);
        assert_eq!(s.conflicts(false, 10).unwrap().len(), 1);
        let all = s.conflicts(true, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().find(|c| c.rel == "b.txt").unwrap().dismissed);

        s.dismiss_all_conflicts().unwrap();
        assert_eq!(s.conflict_count().unwrap(), 0);
        assert_eq!(s.conflicts(true, 1).unwrap().len(), 1, "limit applies");
    }

    #[test]
    fn conflicts_since_returns_only_newer_rows_oldest_first() {
        let s = Stats::memory().unwrap();
        assert_eq!(s.max_conflict_id().unwrap(), 0, "empty table");
        assert!(s.conflicts_since(0, 10).unwrap().is_empty());

        for (i, rel) in ["a", "b", "c"].iter().enumerate() {
            s.record_conflict("p", rel, ConflictResolution::KeptBoth, None, i as i64)
                .unwrap();
        }
        let all = s.conflicts_since(0, 10).unwrap();
        assert_eq!(
            all.iter().map(|c| c.rel.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "oldest first, unlike the inbox"
        );

        // A notifier that has seen up to the second row gets only the third...
        let seen = all[1].id;
        let newer = s.conflicts_since(seen, 10).unwrap();
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].rel, "c");
        // ...including ones already acknowledged, which are new to it all the same.
        s.dismiss_conflict(newer[0].id).unwrap();
        assert_eq!(s.conflicts_since(seen, 10).unwrap().len(), 1);

        assert_eq!(s.max_conflict_id().unwrap(), all[2].id);
        assert_eq!(s.conflicts_since(all[2].id, 10).unwrap().len(), 0);
        assert_eq!(s.conflicts_since(0, 2).unwrap().len(), 2, "limit applies");
    }

    #[test]
    fn a_skipped_conflict_is_not_duplicated_while_it_stays_open() {
        let s = Stats::memory().unwrap();
        for ts in [100, 200, 300] {
            s.record_conflict("p", "a.txt", ConflictResolution::Skipped, None, ts)
                .unwrap();
        }
        let open = s.conflicts(false, 10).unwrap();
        assert_eq!(open.len(), 1, "re-detected each run, one inbox entry");
        assert_eq!(open[0].ts, 100, "keeps when it was first seen");

        // Once acknowledged, a fresh detection is a new entry.
        s.dismiss_conflict(open[0].id).unwrap();
        s.record_conflict("p", "a.txt", ConflictResolution::Skipped, None, 400)
            .unwrap();
        assert_eq!(s.conflicts(false, 10).unwrap().len(), 1);
        assert_eq!(s.conflicts(true, 10).unwrap().len(), 2);

        // Other resolutions are history, never deduped.
        for ts in [1, 2] {
            s.record_conflict("p", "b.txt", ConflictResolution::KeptBoth, Some("c"), ts)
                .unwrap();
        }
        assert_eq!(s.conflicts(false, 10).unwrap().len(), 3);
    }

    fn entry(rel: &str) -> Entry {
        Entry {
            path: rel.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_skipped_conflict_clears_when_its_file_finally_syncs() {
        let s = Stats::memory().unwrap();
        s.record_conflict("p", "a.txt", ConflictResolution::Skipped, None, 1)
            .unwrap();
        s.record_conflict("p", "gone.txt", ConflictResolution::Skipped, None, 2)
            .unwrap();
        s.record_conflict("p", "other.txt", ConflictResolution::Skipped, None, 3)
            .unwrap();
        s.record_conflict("p", "kept.txt", ConflictResolution::KeptBoth, None, 4)
            .unwrap();
        s.record_conflict("q", "a.txt", ConflictResolution::Skipped, None, 5)
            .unwrap();

        // A commit that writes a.txt and removes gone.txt settles those two -
        // but not other.txt (untouched), another pair's a.txt, or the
        // already-applied keep-both history entry.
        let mut entries = BTreeMap::new();
        entries.insert("a.txt".to_string(), entry("a.txt"));
        entries.insert("kept.txt".to_string(), entry("kept.txt"));
        let removed: HashSet<String> = ["gone.txt".to_string()].into();
        s.commit_baseline("p", &entries, &removed).unwrap();

        let mut open: Vec<(String, String)> = s
            .conflicts(false, 10)
            .unwrap()
            .into_iter()
            .map(|c| (c.pair, c.rel))
            .collect();
        open.sort();
        assert_eq!(
            open,
            [
                ("p".to_string(), "kept.txt".to_string()),
                ("p".to_string(), "other.txt".to_string()),
                ("q".to_string(), "a.txt".to_string()),
            ]
        );
    }

    #[test]
    fn pruning_conflicts_never_drops_open_ones() {
        let s = Stats::memory().unwrap();
        for i in 0..5 {
            s.record_conflict("p", &format!("f{i}"), ConflictResolution::KeptBoth, None, i)
                .unwrap();
        }
        // f0..f2 acknowledged, f3/f4 still open.
        for c in s.conflicts(true, 10).unwrap() {
            if ["f0", "f1", "f2"].contains(&c.rel.as_str()) {
                s.dismiss_conflict(c.id).unwrap();
            }
        }
        s.prune_conflicts(1).unwrap();
        let rels: Vec<String> = s
            .conflicts(true, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.rel)
            .collect();
        assert!(rels.contains(&"f3".to_string()) && rels.contains(&"f4".to_string()));
        assert!(
            rels.contains(&"f2".to_string()),
            "newest dismissed row kept"
        );
        assert!(!rels.contains(&"f0".to_string()) && !rels.contains(&"f1".to_string()));
    }

    #[test]
    fn forgetting_a_pair_clears_its_conflicts_and_retry_state() {
        let s = Stats::memory().unwrap();
        s.record_conflict("gone", "a.txt", ConflictResolution::Skipped, None, 1)
            .unwrap();
        s.record_conflict("stays", "a.txt", ConflictResolution::Skipped, None, 1)
            .unwrap();
        s.apply_retry_updates("gone", &[("a.txt".into(), 1, 10)], &[])
            .unwrap();
        s.forget_pair("gone").unwrap();
        assert_eq!(s.conflicts(true, 10).unwrap().len(), 1);
        assert!(s.retry_state("gone").unwrap().is_empty());

        s.apply_retry_updates("stays", &[("a.txt".into(), 1, 10)], &[])
            .unwrap();
        s.wipe().unwrap();
        assert_eq!(s.conflict_count().unwrap(), 0);
        assert!(s.retry_state("stays").unwrap().is_empty());
    }

    #[test]
    fn open_refuses_a_corrupted_file_instead_of_silently_treating_it_as_empty() {
        // M1-003: a stats.db that isn't a valid SQLite file at all (disk
        // corruption, a truncated write, garbage) must fail loudly, not be
        // silently read as "no baseline" - the caller decides what "no
        // baseline" safely means (union, never mirror); Stats::open must not
        // make that call itself by swallowing the error.
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-test-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stats.db"),
            b"not a sqlite database, just garbage bytes",
        )
        .unwrap();

        let err = match Stats::open(&dir) {
            Ok(_) => panic!("a garbage file must not open as an empty DB"),
            Err(e) => e,
        };
        // rusqlite's own message ("file is not a database"), not a message we
        // invented - proves the failure comes from SQLite actually rejecting
        // the file, not from a check that could itself be bypassed.
        assert!(
            format!("{err:#}").to_lowercase().contains("not a database"),
            "unexpected error: {err:#}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_refuses_an_incompatible_schema_version() {
        // M1-003: a stats.db stamped with a schema version this build doesn't
        // know must be refused, not silently read under the wrong assumptions.
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-test-schemaver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A normal open creates and stamps the current version.
        Stats::open(&dir).unwrap();
        // Simulate a DB written by some other (future or foreign) schema
        // version.
        Connection::open(dir.join("stats.db"))
            .unwrap()
            .execute_batch("PRAGMA user_version = 999")
            .unwrap();

        let err = match Stats::open(&dir) {
            Ok(_) => panic!("a foreign schema version must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("schema version"),
            "unexpected error: {err:#}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
