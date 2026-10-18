//! Sync status in the file manager (M5-003): small emblems on your synced
//! folders - a check when it's up to date, arrows while it syncs, a warning
//! for a conflict, and so on.
//!
//! They are GVfs metadata (`metadata::emblems`), the standard way a program
//! tells the GNOME-family file managers (Nautilus, Nemo, Caja) to overlay an
//! icon on a file - no extension, no plugin, nothing to install. File managers
//! that don't read GVfs metadata (COSMIC Files, Dolphin, Thunar) simply show
//! nothing.
//!
//! Marking every file would mean a process per file, so only the few that need
//! it are marked: each pair's root folder (its overall state), the files that
//! have a problem (a conflict, or a transfer waiting to retry) with the
//! folders on the way down to them (so you can find them), and excluded
//! folders. A file with no emblem is simply in sync.
//!
//! Like the rest of the file-manager integration this is opt-in (`file_manager`
//! in the config), and it only ever touches paths it marked itself: what it set
//! is remembered on disk, so it can be cleared after a crash or when the switch
//! goes off, and nothing else's emblems are read or changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Pair;
use crate::service::{AppState, Phase};
use crate::stats::ConflictRecord;

/// The sync state a path can be in, from least to most urgent: `max` of two
/// statuses is the one worth showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileStatus {
    /// Up to date.
    Synced,
    /// Left out of the sync on purpose.
    Excluded,
    /// Being scanned or transferred right now.
    Syncing,
    /// A transfer failed and is waiting out its retry backoff.
    Pending,
    /// Changed on both sides; see the Conflicts page.
    Conflict,
    /// Can't reach Proton: the session is signed out, or the keyring it lives
    /// in can't be reached.
    Offline,
    /// The last run had errors.
    Error,
}

impl FileStatus {
    /// The freedesktop emblem icon name for this status (several statuses
    /// share one: the icon theme has a small vocabulary).
    pub fn emblem(self) -> &'static str {
        match self {
            FileStatus::Synced => "emblem-default",
            FileStatus::Excluded => "emblem-readonly",
            FileStatus::Syncing | FileStatus::Pending => "emblem-synchronizing",
            FileStatus::Conflict | FileStatus::Offline => "emblem-important",
            FileStatus::Error => "emblem-unreadable",
        }
    }

    /// Whether a folder containing a path in this status should show it too
    /// (a problem deep down must be findable from the top; an excluded
    /// subfolder says nothing about its parent).
    fn propagates(self) -> bool {
        self >= FileStatus::Pending
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Synced => "synced",
            FileStatus::Excluded => "excluded",
            FileStatus::Syncing => "syncing",
            FileStatus::Pending => "pending",
            FileStatus::Conflict => "conflict",
            FileStatus::Offline => "offline",
            FileStatus::Error => "error",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "synced" => FileStatus::Synced,
            "excluded" => FileStatus::Excluded,
            "syncing" => FileStatus::Syncing,
            "pending" => FileStatus::Pending,
            "conflict" => FileStatus::Conflict,
            "offline" => FileStatus::Offline,
            "error" => FileStatus::Error,
            _ => return None,
        })
    }
}

/// Never mark more than this many paths: roots first, then the most urgent.
pub const MAX_PATHS: usize = 400;

/// What [`desired`] looks at.
pub struct Inputs<'a> {
    pub pairs: &'a [Pair],
    pub state: &'a AppState,
    /// Unresolved conflicts.
    pub conflicts: &'a [ConflictRecord],
    /// Per pair: files waiting to retry, as `rel -> (attempts, next_at)`.
    pub retry: &'a HashMap<String, HashMap<String, (u32, i64)>>,
    pub now: i64,
}

/// `root.join(rel)`, but only if `rel` stays inside `root`: a relative path of
/// plain components. (A remote-controlled name must never be able to point an
/// emblem at somewhere else - `join` with an absolute path replaces the root.)
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel);
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

/// The emblem every path should carry right now.
pub fn desired(inp: &Inputs) -> BTreeMap<PathBuf, FileStatus> {
    let mut out: BTreeMap<PathBuf, FileStatus> = BTreeMap::new();
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();

    fn put(out: &mut BTreeMap<PathBuf, FileStatus>, path: PathBuf, st: FileStatus) {
        let e = out.entry(path).or_insert(st);
        *e = (*e).max(st);
    }

    for pair in inp.pairs {
        let root = &pair.local;
        if !root.is_dir() {
            continue;
        }
        let mut worst = FileStatus::Synced;

        // A problem: the path itself, and the folders down to it.
        let mut mark = |out: &mut BTreeMap<PathBuf, FileStatus>, rel: &str, st: FileStatus| {
            let Some(path) = inside(root, rel) else {
                return;
            };
            if st.propagates() {
                worst = worst.max(st);
                for anc in path.ancestors().skip(1) {
                    if anc == root.as_path() || !anc.starts_with(root) {
                        break;
                    }
                    put(out, anc.to_path_buf(), st);
                }
            }
            put(out, path, st);
        };

        for c in inp.conflicts.iter().filter(|c| c.pair == pair.name) {
            mark(&mut out, &c.rel, FileStatus::Conflict);
            if let Some(copy) = &c.copy_rel {
                mark(&mut out, copy, FileStatus::Conflict);
            }
        }
        if let Some(waiting) = inp.retry.get(&pair.name) {
            for (rel, (_, next_at)) in waiting {
                if *next_at > inp.now {
                    mark(&mut out, rel, FileStatus::Pending);
                }
            }
        }
        for ex in &pair.exclude {
            if let Some(path) = inside(root, ex) {
                if path.exists() {
                    put(&mut out, path, FileStatus::Excluded);
                }
            }
        }

        // The root carries the pair's overall state.
        let run = match inp.state.pairs.iter().find(|p| p.name == pair.name) {
            Some(p) if p.phase == Phase::Error => FileStatus::Error,
            Some(p) if matches!(p.phase, Phase::Scanning | Phase::Syncing) => FileStatus::Syncing,
            _ => FileStatus::Synced,
        };
        let mut root_status = run.max(worst);
        if inp.state.signed_out || inp.state.keyring_unreachable {
            root_status = root_status.max(FileStatus::Offline);
        }
        put(&mut out, root.clone(), root_status);
        roots.insert(root.clone());
    }

    if out.len() > MAX_PATHS {
        // Keep the roots, then the most urgent, then a stable order.
        let mut all: Vec<(PathBuf, FileStatus)> = out.into_iter().collect();
        all.sort_by(|(pa, sa), (pb, sb)| {
            roots
                .contains(pb)
                .cmp(&roots.contains(pa))
                .then(sb.cmp(sa))
                .then(pa.cmp(pb))
        });
        all.truncate(MAX_PATHS);
        out = all.into_iter().collect();
    }
    out
}

/// Sets and clears the emblem on a path. A trait so the diffing can be tested
/// without touching any real file's metadata.
pub trait Setter {
    /// `true` if the emblem was set.
    fn set(&self, path: &Path, emblem: &str) -> bool;
    fn clear(&self, path: &Path) -> bool;
}

/// Sets emblems through `gio set` (part of GLib, which the app already relies
/// on for the trash).
pub struct GioSetter {
    gio: PathBuf,
}

impl GioSetter {
    pub fn new() -> Option<Self> {
        crate::trash::which_gio().map(|gio| GioSetter { gio })
    }

    fn run(&self, args: &[&str], path: &Path) -> bool {
        Command::new(&self.gio)
            .arg("set")
            .args(args)
            .arg(path)
            .arg("metadata::emblems")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl Setter for GioSetter {
    fn set(&self, path: &Path, emblem: &str) -> bool {
        // `gio set -t stringv PATH KEY VALUE`: the value goes after the key.
        Command::new(&self.gio)
            .args(["set", "-t", "stringv"])
            .arg(path)
            .arg("metadata::emblems")
            .arg(emblem)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn clear(&self, path: &Path) -> bool {
        self.run(&["-t", "unset"], path)
    }
}

/// Keeps the emblems on disk in step with [`desired`], remembering what it set.
pub struct Emblems {
    applied: BTreeMap<PathBuf, FileStatus>,
    /// Sets that failed (a path that vanished, a filesystem GVfs can't track):
    /// not retried every tick, only if what's wanted changes.
    failed: BTreeSet<(PathBuf, FileStatus)>,
    store: Option<PathBuf>,
}

impl Emblems {
    /// Start with what a previous run left marked (so it can be cleared).
    pub fn load(state_dir: &Path) -> Self {
        let store = state_dir.join("emblems.json");
        let applied = std::fs::read(&store)
            .ok()
            .and_then(|b| serde_json::from_slice::<BTreeMap<String, String>>(&b).ok())
            .map(|m| {
                m.into_iter()
                    .filter_map(|(p, s)| Some((PathBuf::from(p), FileStatus::parse(&s)?)))
                    .collect()
            })
            .unwrap_or_default();
        Emblems {
            applied,
            failed: BTreeSet::new(),
            store: Some(store),
        }
    }

    /// For tests: nothing remembered, nothing persisted.
    pub fn in_memory() -> Self {
        Emblems {
            applied: BTreeMap::new(),
            failed: BTreeSet::new(),
            store: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }

    fn save(&self) {
        let Some(store) = &self.store else { return };
        let map: BTreeMap<String, &str> = self
            .applied
            .iter()
            .map(|(p, s)| (p.to_string_lossy().into_owned(), s.as_str()))
            .collect();
        let Ok(json) = serde_json::to_vec(&map) else {
            return;
        };
        let tmp = store.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, store);
        }
    }

    /// Make the emblems match `want`, doing at most `budget` operations (the
    /// rest happen on later calls, so a huge change never stalls the caller).
    /// Returns how many operations ran.
    pub fn update(
        &mut self,
        want: &BTreeMap<PathBuf, FileStatus>,
        setter: &dyn Setter,
        budget: usize,
    ) -> usize {
        let mut ops = 0;
        let stale: Vec<PathBuf> = self
            .applied
            .keys()
            .filter(|p| !want.contains_key(*p))
            .cloned()
            .collect();
        for path in stale {
            if ops >= budget {
                break;
            }
            // Gone from what's wanted: forget it either way (a clear that
            // fails means the file is gone, and there's nothing left to clear).
            setter.clear(&path);
            self.applied.remove(&path);
            ops += 1;
        }
        for (path, st) in want {
            if ops >= budget {
                break;
            }
            if self.applied.get(path) == Some(st) || self.failed.contains(&(path.clone(), *st)) {
                continue;
            }
            if setter.set(path, st.emblem()) {
                self.applied.insert(path.clone(), *st);
            } else {
                self.failed.insert((path.clone(), *st));
            }
            ops += 1;
        }
        self.failed.retain(|(p, s)| want.get(p) == Some(s));
        if ops > 0 {
            self.save();
        }
        ops
    }

    /// Remove every emblem we set (the switch went off, or we're uninstalling).
    pub fn clear_all(&mut self, setter: &dyn Setter, budget: usize) -> usize {
        self.update(&BTreeMap::new(), setter, budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::PairState;
    use std::cell::RefCell;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-emblems-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pair(name: &str, local: &Path, exclude: &[&str]) -> Pair {
        Pair {
            name: name.into(),
            local: local.to_path_buf(),
            remote: format!("/my-files/{name}"),
            auto: true,
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn state_of(pairs: &[(&str, Phase)]) -> AppState {
        AppState {
            pairs: pairs
                .iter()
                .map(|(n, ph)| PairState {
                    name: n.to_string(),
                    local: String::new(),
                    remote: String::new(),
                    phase: *ph,
                    progress: Default::default(),
                    scanned_folders: 0,
                    current_op: None,
                    last_error: None,
                    last_synced: Some(1),
                    tracked: 0,
                    retrying: 0,
                    held: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn conflict(pair: &str, rel: &str, copy: Option<&str>) -> ConflictRecord {
        ConflictRecord {
            id: 1,
            ts: 0,
            pair: pair.into(),
            rel: rel.into(),
            resolution: crate::events::ConflictResolution::KeptBoth,
            copy_rel: copy.map(str::to_string),
            dismissed: false,
        }
    }

    fn run(
        pairs: &[Pair],
        state: &AppState,
        conflicts: &[ConflictRecord],
        retry: &HashMap<String, HashMap<String, (u32, i64)>>,
    ) -> BTreeMap<PathBuf, FileStatus> {
        desired(&Inputs {
            pairs,
            state,
            conflicts,
            retry,
            now: 1000,
        })
    }

    // --- what to mark -------------------------------------------------------

    #[test]
    fn a_quiet_pair_is_just_a_checked_root() {
        let root = temp_dir("quiet");
        let want = run(
            &[pair("docs", &root, &[])],
            &state_of(&[("docs", Phase::Synced)]),
            &[],
            &HashMap::new(),
        );
        assert_eq!(want, BTreeMap::from([(root.clone(), FileStatus::Synced)]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_root_follows_the_pair_phase() {
        let root = temp_dir("phase");
        let p = [pair("docs", &root, &[])];
        for (phase, want) in [
            (Phase::Idle, FileStatus::Synced),
            (Phase::Scanning, FileStatus::Syncing),
            (Phase::Syncing, FileStatus::Syncing),
            (Phase::Error, FileStatus::Error),
        ] {
            let got = run(&p, &state_of(&[("docs", phase)]), &[], &HashMap::new());
            assert_eq!(got[&root], want, "{phase:?}");
        }
        // Signed out lifts a calm root to Offline, but never hides an error.
        let mut st = state_of(&[("docs", Phase::Synced)]);
        st.signed_out = true;
        assert_eq!(
            run(&p, &st, &[], &HashMap::new())[&root],
            FileStatus::Offline
        );
        st.pairs[0].phase = Phase::Error;
        assert_eq!(run(&p, &st, &[], &HashMap::new())[&root], FileStatus::Error);
        // A keyring the CLI can't reach stops sync just the same.
        let mut st = state_of(&[("docs", Phase::Synced)]);
        st.keyring_unreachable = true;
        assert_eq!(
            run(&p, &st, &[], &HashMap::new())[&root],
            FileStatus::Offline
        );
        st.pairs[0].phase = Phase::Error;
        assert_eq!(run(&p, &st, &[], &HashMap::new())[&root], FileStatus::Error);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_conflict_marks_the_file_its_copy_and_the_folders_down_to_them() {
        let root = temp_dir("conflict");
        let want = run(
            &[pair("docs", &root, &[])],
            &state_of(&[("docs", Phase::Synced)]),
            &[conflict(
                "docs",
                "a/b/notes.txt",
                Some("a/b/notes (conflict 1).txt"),
            )],
            &HashMap::new(),
        );
        let at = |rel: &str| want.get(&root.join(rel)).copied();
        assert_eq!(at("a/b/notes.txt"), Some(FileStatus::Conflict));
        assert_eq!(at("a/b/notes (conflict 1).txt"), Some(FileStatus::Conflict));
        assert_eq!(at("a/b"), Some(FileStatus::Conflict));
        assert_eq!(at("a"), Some(FileStatus::Conflict));
        // ...and the root reflects the worst thing below it.
        assert_eq!(want[&root], FileStatus::Conflict);
        assert_eq!(want.len(), 5);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_files_still_in_backoff_are_pending() {
        let root = temp_dir("retry");
        let retry = HashMap::from([(
            "docs".to_string(),
            HashMap::from([
                ("waiting.txt".to_string(), (2, 5000)), // retries after `now`
                ("due.txt".to_string(), (2, 900)),      // window already over
            ]),
        )]);
        let want = run(
            &[pair("docs", &root, &[])],
            &state_of(&[("docs", Phase::Synced)]),
            &[],
            &retry,
        );
        assert_eq!(want[&root.join("waiting.txt")], FileStatus::Pending);
        assert!(!want.contains_key(&root.join("due.txt")));
        assert_eq!(want[&root], FileStatus::Pending);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn excluded_folders_are_marked_only_when_they_exist_and_dont_taint_their_parent() {
        let root = temp_dir("excl");
        std::fs::create_dir_all(root.join("caches/inner")).unwrap();
        let want = run(
            &[pair("docs", &root, &["caches/inner", "never-created"])],
            &state_of(&[("docs", Phase::Synced)]),
            &[],
            &HashMap::new(),
        );
        assert_eq!(want[&root.join("caches/inner")], FileStatus::Excluded);
        assert!(!want.contains_key(&root.join("never-created")));
        assert!(
            !want.contains_key(&root.join("caches")),
            "parent is not excluded"
        );
        assert_eq!(want[&root], FileStatus::Synced);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_local_folder_gets_nothing_and_other_pairs_are_unaffected() {
        let root = temp_dir("multi");
        let gone = root.join("unmounted");
        let want = run(
            &[pair("a", &root, &[]), pair("b", &gone, &[])],
            &state_of(&[("a", Phase::Synced), ("b", Phase::Error)]),
            &[conflict("b", "x.txt", None)],
            &HashMap::new(),
        );
        assert_eq!(want, BTreeMap::from([(root.clone(), FileStatus::Synced)]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_remote_controlled_name_can_never_aim_an_emblem_outside_the_pair() {
        let root = temp_dir("escape");
        let want = run(
            &[pair("docs", &root, &["../outside"])],
            &state_of(&[("docs", Phase::Synced)]),
            &[
                conflict("docs", "../../etc/passwd", None),
                conflict("docs", "/etc/shadow", Some("a/../../b")),
                conflict("docs", "", None),
            ],
            &HashMap::new(),
        );
        assert_eq!(want, BTreeMap::from([(root.clone(), FileStatus::Synced)]));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_huge_number_of_problems_is_capped_keeping_roots_and_the_most_urgent() {
        let root = temp_dir("cap");
        let conflicts: Vec<ConflictRecord> = (0..MAX_PATHS + 50)
            .map(|i| conflict("docs", &format!("f{i}.txt"), None))
            .collect();
        let retry = HashMap::from([(
            "docs".to_string(),
            HashMap::from([("slow.txt".to_string(), (1, 9_999))]),
        )]);
        let want = run(
            &[pair("docs", &root, &[])],
            &state_of(&[("docs", Phase::Error)]),
            &conflicts,
            &retry,
        );
        assert_eq!(want.len(), MAX_PATHS);
        assert_eq!(
            want[&root],
            FileStatus::Error,
            "the root always survives the cap"
        );
        assert!(
            !want.contains_key(&root.join("slow.txt")),
            "Pending is less urgent than Conflict, so it's what gets dropped"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn statuses_have_distinct_urgency_and_known_emblem_names() {
        use FileStatus::*;
        let order = [Synced, Excluded, Syncing, Pending, Conflict, Offline, Error];
        assert!(order.windows(2).all(|w| w[0] < w[1]));
        for s in order {
            assert!(s.emblem().starts_with("emblem-"));
            assert_eq!(FileStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(FileStatus::parse("nonsense"), None);
    }

    // --- applying -----------------------------------------------------------

    /// Records what it was asked to do; `fail` makes `set` refuse those paths.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        fail: RefCell<BTreeSet<PathBuf>>,
    }
    impl Setter for Fake {
        fn set(&self, path: &Path, emblem: &str) -> bool {
            self.calls
                .borrow_mut()
                .push(format!("set {} {emblem}", path.display()));
            !self.fail.borrow().contains(path)
        }
        fn clear(&self, path: &Path) -> bool {
            self.calls
                .borrow_mut()
                .push(format!("clear {}", path.display()));
            true
        }
    }

    fn want(items: &[(&str, FileStatus)]) -> BTreeMap<PathBuf, FileStatus> {
        items.iter().map(|(p, s)| (PathBuf::from(p), *s)).collect()
    }

    #[test]
    fn only_what_changed_is_touched() {
        let setter = Fake::default();
        let mut e = Emblems::in_memory();

        assert_eq!(
            e.update(
                &want(&[("/r", FileStatus::Synced), ("/r/a", FileStatus::Conflict)]),
                &setter,
                100
            ),
            2
        );
        assert_eq!(
            *setter.calls.borrow(),
            ["set /r emblem-default", "set /r/a emblem-important"]
        );

        // Same again: nothing to do.
        setter.calls.borrow_mut().clear();
        assert_eq!(
            e.update(
                &want(&[("/r", FileStatus::Synced), ("/r/a", FileStatus::Conflict)]),
                &setter,
                100
            ),
            0
        );
        assert!(setter.calls.borrow().is_empty());

        // The root starts syncing, the conflict is resolved: one set, one clear.
        assert_eq!(
            e.update(&want(&[("/r", FileStatus::Syncing)]), &setter, 100),
            2
        );
        assert_eq!(
            *setter.calls.borrow(),
            ["clear /r/a", "set /r emblem-synchronizing"],
            "stale ones are cleared first"
        );
    }

    #[test]
    fn a_big_change_is_spread_over_calls_by_the_budget() {
        let setter = Fake::default();
        let mut e = Emblems::in_memory();
        let all = want(&[
            ("/r/1", FileStatus::Conflict),
            ("/r/2", FileStatus::Conflict),
            ("/r/3", FileStatus::Conflict),
        ]);
        assert_eq!(e.update(&all, &setter, 2), 2);
        assert_eq!(e.update(&all, &setter, 2), 1, "the rest on the next call");
        assert_eq!(e.update(&all, &setter, 2), 0);
    }

    #[test]
    fn a_set_that_fails_is_not_hammered_every_call_but_is_retried_if_the_want_changes() {
        let setter = Fake::default();
        setter.fail.borrow_mut().insert(PathBuf::from("/r/a"));
        let mut e = Emblems::in_memory();
        let w = want(&[("/r/a", FileStatus::Conflict)]);
        assert_eq!(e.update(&w, &setter, 10), 1, "tried once");
        assert_eq!(
            e.update(&w, &setter, 10),
            0,
            "not again while nothing changed"
        );
        // A different status for the same path is a new attempt.
        let w2 = want(&[("/r/a", FileStatus::Pending)]);
        assert_eq!(e.update(&w2, &setter, 10), 1);
        // And once it works, it sticks.
        setter.fail.borrow_mut().clear();
        let w3 = want(&[("/r/a", FileStatus::Error)]);
        assert_eq!(e.update(&w3, &setter, 10), 1);
        assert_eq!(e.update(&w3, &setter, 10), 0);
    }

    #[test]
    fn clear_all_removes_everything_it_set() {
        let setter = Fake::default();
        let mut e = Emblems::in_memory();
        e.update(
            &want(&[("/r", FileStatus::Synced), ("/r/a", FileStatus::Error)]),
            &setter,
            10,
        );
        assert!(!e.is_empty());
        setter.calls.borrow_mut().clear();
        assert_eq!(e.clear_all(&setter, 10), 2);
        assert!(e.is_empty());
        assert_eq!(setter.calls.borrow().len(), 2);
        assert!(setter
            .calls
            .borrow()
            .iter()
            .all(|c| c.starts_with("clear ")));
        assert_eq!(e.clear_all(&setter, 10), 0, "nothing left to clear");
    }

    #[test]
    fn what_was_set_survives_a_restart_so_it_can_be_cleared() {
        let dir = temp_dir("persist");
        let setter = Fake::default();
        {
            let mut e = Emblems::load(&dir);
            e.update(
                &want(&[("/r", FileStatus::Synced), ("/r/a", FileStatus::Conflict)]),
                &setter,
                10,
            );
        }
        // A new process: it knows what the old one left behind...
        let mut again = Emblems::load(&dir);
        assert!(!again.is_empty());
        setter.calls.borrow_mut().clear();
        // ...so what's no longer wanted is cleared, and what still is isn't redone.
        assert_eq!(
            again.update(&want(&[("/r", FileStatus::Synced)]), &setter, 10),
            1
        );
        assert_eq!(*setter.calls.borrow(), ["clear /r/a"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_store_is_just_an_empty_one() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("emblems.json"), b"not json {").unwrap();
        assert!(Emblems::load(&dir).is_empty());
        std::fs::write(dir.join("emblems.json"), br#"{"/r":"synced","/x":"bogus"}"#).unwrap();
        let e = Emblems::load(&dir);
        assert_eq!(
            e.applied.len(),
            1,
            "an unknown status is dropped, not guessed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- the real thing -----------------------------------------------------

    /// What `gio` reports for the emblems on `path`, if it can say.
    fn emblems_on(gio: &Path, path: &Path) -> Option<String> {
        let out = Command::new(gio)
            .args(["info", "-a", "metadata::emblems"])
            .arg(path)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
            l.trim()
                .strip_prefix("metadata::emblems:")
                .map(|v| v.trim().to_string())
        })
    }

    #[test]
    fn the_real_gio_setter_sets_and_clears_gvfs_emblems() {
        let Some(setter) = GioSetter::new() else {
            eprintln!("skipping: no gio");
            return;
        };
        // GVfs metadata only works on a real filesystem (not tmpfs /tmp), so use
        // a scratch folder under $HOME - and skip where GVfs isn't running.
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            eprintln!("skipping: no $HOME");
            return;
        };
        let dir = home.join(format!(
            ".cache/protondrive4linux-emblem-test-{}",
            std::process::id()
        ));
        if std::fs::create_dir_all(&dir).is_err() {
            eprintln!("skipping: cannot create {}", dir.display());
            return;
        }
        let file = dir.join("probe.txt");
        std::fs::write(&file, "x").unwrap();
        let gio = crate::trash::which_gio().unwrap();

        if !setter.set(&file, "emblem-important") || emblems_on(&gio, &file).is_none() {
            eprintln!("skipping: GVfs metadata isn't available here");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        assert_eq!(
            emblems_on(&gio, &file).as_deref(),
            Some("[emblem-important]")
        );
        // Replacing, not appending.
        assert!(setter.set(&file, "emblem-default"));
        assert_eq!(emblems_on(&gio, &file).as_deref(), Some("[emblem-default]"));
        assert!(setter.clear(&file));
        assert_eq!(emblems_on(&gio, &file), None);
        // A path with spaces and a leading dash in a component is passed as one argument.
        let odd = dir.join("-weird name $(x).txt");
        std::fs::write(&odd, "x").unwrap();
        assert!(setter.set(&odd, "emblem-default"));
        assert_eq!(emblems_on(&gio, &odd).as_deref(), Some("[emblem-default]"));
        setter.clear(&odd);
        std::fs::remove_dir_all(&dir).ok();
    }
}
