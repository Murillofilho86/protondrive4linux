//! File-manager integration (M5-004), opt-in via `file_manager = true`.
//!
//! Your synced folders show up where you already look for folders, with no
//! virtual filesystem involved - it's all just a shortcut to the real local
//! folder:
//!
//! - **Sidebar bookmarks** in `~/.config/gtk-3.0/bookmarks`, the file the GTK
//!   file managers (Nautilus, Nemo, Thunar, PCManFM...) read for their sidebar.
//! - **Nautilus Scripts** (`~/.local/share/nautilus/scripts/`): two entries in
//!   the right-click -> Scripts menu, to sync now and to open the conflicts.
//!
//! (Per-file status emblems are a separate piece: see [`crate::emblems`].)
//!
//! Everything it writes is marked as ours, and *only* ours is ever changed or
//! removed: a user's own bookmarks and scripts are carried through untouched,
//! even one that happens to have the same name. Applying is idempotent, so it
//! is safe to run at startup, on a config reload and when a switch flips.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::{config_home, data_home, Config};

/// Label suffix that marks a bookmark as managed by us ("docs (Proton Drive)").
pub const BOOKMARK_SUFFIX: &str = " (Proton Drive)";

/// First line of every script we write, so we only ever delete our own.
const SCRIPT_MARK: &str = "# managed by protondrive4linux (Settings -> File manager)";

/// Where the GTK file managers keep their sidebar bookmarks.
pub fn bookmarks_path() -> PathBuf {
    config_home().join("gtk-3.0").join("bookmarks")
}

/// Where Nautilus looks for its Scripts menu entries.
pub fn scripts_dir() -> PathBuf {
    data_home().join("nautilus").join("scripts")
}

/// Percent-encode a path as the `file://` URI GTK expects: unreserved
/// characters and `/` pass through, everything else (spaces, non-ASCII, `%`...)
/// becomes `%XX` per UTF-8 byte.
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One line of a bookmarks file: `<uri>[ <label>]`.
fn split_line(line: &str) -> (&str, Option<&str>) {
    match line.split_once(' ') {
        Some((uri, label)) => (uri, Some(label)),
        None => (line, None),
    }
}

fn is_ours(line: &str) -> bool {
    split_line(line)
        .1
        .is_some_and(|l| l.ends_with(BOOKMARK_SUFFIX))
}

/// `existing` (the text of a bookmarks file) with our entries replaced by
/// `desired` (`(folder, pair name)`): every line we manage is dropped and one is
/// added per desired folder, unless the person already bookmarked that exact
/// folder themselves (we don't duplicate their entry - or own it). Their lines
/// keep their order and content; ours go last.
pub fn merge_bookmarks(existing: &str, desired: &[(PathBuf, String)]) -> String {
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim().is_empty() && !is_ours(l))
        .map(str::to_string)
        .collect();
    for (path, name) in desired {
        let uri = file_uri(path);
        if lines.iter().any(|l| split_line(l).0 == uri) {
            continue;
        }
        lines.push(format!("{uri} {name}{BOOKMARK_SUFFIX}"));
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

/// Write `text` to `path` atomically (temp file in the same directory, then
/// rename), keeping the existing file's permissions.
fn write_atomic(path: &Path, text: &str, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("pd4l-tmp-{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    let mode = mode.or_else(|| {
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    });
    if let Some(mode) = mode {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(&tmp, path)
}

/// Bring the bookmarks file at `path` in line with `desired`. `true` if the
/// file changed (nothing is written when it already matches).
pub fn sync_bookmarks(path: &Path, desired: &[(PathBuf, String)]) -> std::io::Result<bool> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let merged = merge_bookmarks(&existing, desired);
    // What `existing` already amounts to, minus blank lines and with a trailing
    // newline: if that is what we'd write, there's nothing to do (and turning
    // the integration off never creates a file that wasn't there).
    let normalized: String = existing
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| format!("{l}\n"))
        .collect();
    if merged == normalized {
        return Ok(false);
    }
    write_atomic(path, &merged, None)?;
    Ok(true)
}

/// The folders to bookmark: every configured pair whose local folder exists.
pub fn desired_bookmarks(cfg: &Config) -> Vec<(PathBuf, String)> {
    cfg.pairs
        .iter()
        .filter(|p| p.local.is_dir())
        .map(|p| (p.local.clone(), p.name.clone()))
        .collect()
}

/// A sibling of the running executable if there is one (a package puts the CLI
/// and the GUI side by side), else the bare name for `$PATH` to resolve.
fn exe_next_to_us(name: &str) -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

/// Single-quote `s` for a POSIX shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The scripts we install: file name (what the Scripts menu shows) and body.
fn scripts() -> Vec<(&'static str, String)> {
    vec![
        (
            "Proton Drive - Sync now",
            format!(
                "#!/bin/sh\n{SCRIPT_MARK}\nexec {} sync\n",
                sh_quote(&exe_next_to_us("protondrive4linux"))
            ),
        ),
        (
            "Proton Drive - Conflicts",
            format!(
                "#!/bin/sh\n{SCRIPT_MARK}\nexec {} --page conflicts\n",
                sh_quote(&exe_next_to_us("protondrive4linux-gui"))
            ),
        ),
    ]
}

/// Install (`enabled`) or remove our Nautilus scripts in `dir`. Only files
/// carrying our marker are ever replaced or deleted: a script of the same name
/// that the person wrote themselves is left alone. `true` if anything changed.
pub fn sync_scripts(dir: &Path, enabled: bool) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mut changed = false;
    for (name, body) in scripts() {
        let path = dir.join(name);
        let existing = std::fs::read_to_string(&path).ok();
        let ours = existing.as_deref().is_some_and(|t| t.contains(SCRIPT_MARK));
        if enabled {
            if existing.is_some() && !ours {
                continue; // theirs: don't touch it
            }
            let executable = std::fs::metadata(&path)
                .map(|m| m.permissions().mode() & 0o111 == 0o111)
                .unwrap_or(false);
            if existing.as_deref() == Some(body.as_str()) && executable {
                continue;
            }
            write_atomic(&path, &body, Some(0o755))?;
            changed = true;
        } else if ours {
            std::fs::remove_file(&path)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// What [`apply`] did, for a log line or a toast.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub bookmarks_changed: bool,
    pub scripts_changed: bool,
    /// Things that couldn't be written (a read-only config dir, ...): never
    /// fatal - the integration is a convenience.
    pub errors: Vec<String>,
}

/// Where [`apply_in`] reads and writes (so a test can use a temp dir).
pub struct Paths {
    pub bookmarks: PathBuf,
    pub scripts: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Paths {
            bookmarks: bookmarks_path(),
            scripts: scripts_dir(),
        }
    }
}

/// Make the file manager match `cfg`: when `file_manager` is on, bookmark the
/// synced folders and install the scripts; when it's off, remove whatever we
/// installed. Idempotent and best-effort.
pub fn apply(cfg: &Config) -> Applied {
    apply_in(cfg, &Paths::system())
}

pub fn apply_in(cfg: &Config, paths: &Paths) -> Applied {
    let mut out = Applied::default();
    let desired = if cfg.file_manager {
        desired_bookmarks(cfg)
    } else {
        Vec::new()
    };
    match sync_bookmarks(&paths.bookmarks, &desired) {
        Ok(changed) => out.bookmarks_changed = changed,
        Err(e) => out.errors.push(format!("bookmarks: {e}")),
    }
    match sync_scripts(&paths.scripts, cfg.file_manager) {
        Ok(changed) => out.scripts_changed = changed,
        Err(e) => out.errors.push(format!("scripts: {e}")),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-fileman-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn want(path: &str, name: &str) -> (PathBuf, String) {
        (PathBuf::from(path), name.to_string())
    }

    // --- URIs ---------------------------------------------------------------

    #[test]
    fn paths_become_percent_encoded_file_uris() {
        let uri = |p: &str| file_uri(Path::new(p));
        assert_eq!(uri("/home/u/Documents"), "file:///home/u/Documents");
        assert_eq!(uri("/home/u/My Docs"), "file:///home/u/My%20Docs");
        assert_eq!(uri("/home/u/100%"), "file:///home/u/100%25");
        assert_eq!(uri("/home/u/a#b?c"), "file:///home/u/a%23b%3Fc");
        // UTF-8, byte by byte
        assert_eq!(uri("/home/u/Áudio"), "file:///home/u/%C3%81udio");
        assert_eq!(uri("/a-b_c.d~e"), "file:///a-b_c.d~e");
    }

    // --- bookmarks ----------------------------------------------------------

    #[test]
    fn our_bookmarks_are_added_after_the_users_own() {
        let merged = merge_bookmarks(
            "file:///home/u/Nextcloud\nfile:///home/u/Work Work Folder\n",
            &[
                want("/home/u/Documents", "docs"),
                want("/home/u/My Photos", "photos"),
            ],
        );
        assert_eq!(
            merged,
            "file:///home/u/Nextcloud\n\
             file:///home/u/Work Work Folder\n\
             file:///home/u/Documents docs (Proton Drive)\n\
             file:///home/u/My%20Photos photos (Proton Drive)\n"
        );
    }

    #[test]
    fn merging_is_idempotent() {
        let desired = [want("/home/u/Documents", "docs")];
        let once = merge_bookmarks("file:///home/u/Nextcloud\n", &desired);
        assert_eq!(merge_bookmarks(&once, &desired), once);
    }

    #[test]
    fn a_removed_or_renamed_pair_is_updated_and_nothing_else_is() {
        let start = merge_bookmarks(
            "file:///home/u/Nextcloud\n",
            &[want("/a", "one"), want("/b", "two")],
        );
        // "two" removed, "one" renamed
        let next = merge_bookmarks(&start, &[want("/a", "uno")]);
        assert_eq!(
            next,
            "file:///home/u/Nextcloud\nfile:///a uno (Proton Drive)\n"
        );
        // everything off: only the user's line remains
        assert_eq!(merge_bookmarks(&next, &[]), "file:///home/u/Nextcloud\n");
        assert_eq!(merge_bookmarks("", &[]), "");
    }

    #[test]
    fn a_folder_the_user_already_bookmarked_is_not_duplicated_or_taken_over() {
        let user = "file:///home/u/Documents My own name\n";
        let merged = merge_bookmarks(user, &[want("/home/u/Documents", "docs")]);
        assert_eq!(merged, user);
        // ...and turning the integration off leaves their entry alone.
        assert_eq!(merge_bookmarks(&merged, &[]), user);
    }

    #[test]
    fn a_users_label_that_merely_resembles_ours_is_still_only_ours_if_it_ends_with_the_suffix() {
        let user = "file:///x Proton Drive notes\nfile:///y Backup (Proton Drive)\n";
        // "Backup (Proton Drive)" carries our suffix, so it counts as ours -
        // the one documented trade-off of identifying our lines by label.
        assert_eq!(merge_bookmarks(user, &[]), "file:///x Proton Drive notes\n");
    }

    #[test]
    fn syncing_the_file_creates_it_and_is_a_no_op_the_second_time() {
        let dir = temp_dir("bm");
        let path = dir.join("gtk-3.0").join("bookmarks");
        let desired = [want("/home/u/Documents", "docs")];

        assert!(
            sync_bookmarks(&path, &desired).unwrap(),
            "first call writes"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "file:///home/u/Documents docs (Proton Drive)\n"
        );
        assert!(
            !sync_bookmarks(&path, &desired).unwrap(),
            "second call changes nothing"
        );

        // Removing everything of ours empties it, and a missing file stays missing.
        assert!(sync_bookmarks(&path, &[]).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let none = dir.join("never").join("bookmarks");
        assert!(!sync_bookmarks(&none, &[]).unwrap());
        assert!(!none.exists(), "turning it off must not create files");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn syncing_keeps_the_existing_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("bm-mode");
        let path = dir.join("bookmarks");
        std::fs::write(&path, "file:///keep\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        sync_bookmarks(&path, &[want("/a", "x")]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- scripts ------------------------------------------------------------

    #[test]
    fn scripts_are_installed_executable_and_marked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("scripts");
        assert!(sync_scripts(&dir, true).unwrap());
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["Proton Drive - Conflicts", "Proton Drive - Sync now"]
        );
        for n in &names {
            let p = dir.join(n);
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(body.starts_with("#!/bin/sh\n"), "{n}");
            assert!(body.contains(SCRIPT_MARK), "{n}");
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o755,
                "{n}"
            );
        }
        assert!(
            std::fs::read_to_string(dir.join("Proton Drive - Conflicts"))
                .unwrap()
                .contains("--page conflicts")
        );
        // Installing again changes nothing.
        assert!(!sync_scripts(&dir, true).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabling_removes_only_our_scripts_and_never_a_users_namesake() {
        let dir = temp_dir("scripts-off");
        sync_scripts(&dir, true).unwrap();
        // The person's own script that happens to share a name with one of ours,
        // plus an unrelated one.
        let theirs = dir.join("Proton Drive - Conflicts");
        std::fs::write(&theirs, "#!/bin/sh\necho mine\n").unwrap();
        std::fs::write(dir.join("unrelated"), "#!/bin/sh\n").unwrap();

        // Enabling must not overwrite their namesake...
        sync_scripts(&dir, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
        // ...and disabling removes ours but leaves theirs and the unrelated one.
        assert!(sync_scripts(&dir, false).unwrap());
        assert!(!dir.join("Proton Drive - Sync now").exists());
        assert!(theirs.exists());
        assert!(dir.join("unrelated").exists());
        assert!(!sync_scripts(&dir, false).unwrap(), "already off");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- applying a whole config ---------------------------------------------

    /// A config with these pairs, written to a file and loaded for real.
    fn config_with(dir: &Path, file_manager: bool, pairs: &[(&str, &Path)]) -> Config {
        let mut toml = format!("[options]\nfile_manager = {file_manager}\n");
        for (name, local) in pairs {
            toml += &format!(
                "[[pair]]\nname = {name:?}\nlocal = {:?}\nremote = {name:?}\n",
                local.to_string_lossy()
            );
        }
        let path = dir.join("c.toml");
        std::fs::write(&path, toml).unwrap();
        crate::config::load(Some(path.to_str().unwrap())).unwrap()
    }

    #[test]
    fn apply_follows_the_switch_and_the_pairs() {
        let root = temp_dir("apply");
        let docs = root.join("My Docs");
        let photos = root.join("photos");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&photos).unwrap();
        let missing = root.join("unmounted"); // a pair whose folder isn't there
        let paths = Paths {
            bookmarks: root.join("cfg/gtk-3.0/bookmarks"),
            scripts: root.join("data/nautilus/scripts"),
        };
        std::fs::create_dir_all(paths.bookmarks.parent().unwrap()).unwrap();
        std::fs::write(&paths.bookmarks, "file:///home/u/Nextcloud\n").unwrap();

        // Off (the default): nothing is touched, nothing is created.
        let off = config_with(
            &root,
            false,
            &[("docs", &docs), ("photos", &photos), ("gone", &missing)],
        );
        let done = apply_in(&off, &paths);
        assert_eq!(done, Applied::default());
        assert!(!paths.scripts.exists());
        assert_eq!(
            std::fs::read_to_string(&paths.bookmarks).unwrap(),
            "file:///home/u/Nextcloud\n"
        );

        // On: the existing folders are bookmarked (the missing one is skipped)
        // and the scripts appear.
        let on = config_with(
            &root,
            true,
            &[("docs", &docs), ("photos", &photos), ("gone", &missing)],
        );
        let done = apply_in(&on, &paths);
        assert!(done.bookmarks_changed && done.scripts_changed && done.errors.is_empty());
        let bm = std::fs::read_to_string(&paths.bookmarks).unwrap();
        assert!(bm.starts_with("file:///home/u/Nextcloud\n"));
        assert!(
            bm.contains(&format!("{} docs (Proton Drive)", file_uri(&docs))),
            "{bm}"
        );
        assert!(
            bm.contains(&format!("{} photos (Proton Drive)", file_uri(&photos))),
            "{bm}"
        );
        assert!(!bm.contains("gone"), "{bm}");
        assert_eq!(std::fs::read_dir(&paths.scripts).unwrap().count(), 2);

        // Applying again changes nothing.
        assert_eq!(apply_in(&on, &paths), Applied::default());

        // Remove a pair: its bookmark goes, the other stays.
        let fewer = config_with(&root, true, &[("photos", &photos)]);
        assert!(apply_in(&fewer, &paths).bookmarks_changed);
        let bm = std::fs::read_to_string(&paths.bookmarks).unwrap();
        assert!(!bm.contains("docs") && bm.contains("photos"), "{bm}");

        // Off again: ours is gone, the user's line and folders are untouched.
        let done = apply_in(&off, &paths);
        assert!(done.bookmarks_changed && done.scripts_changed);
        assert_eq!(
            std::fs::read_to_string(&paths.bookmarks).unwrap(),
            "file:///home/u/Nextcloud\n"
        );
        assert_eq!(std::fs::read_dir(&paths.scripts).unwrap().count(), 0);
        assert!(
            docs.is_dir() && photos.is_dir(),
            "never touches the synced folders"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failure_is_reported_not_fatal() {
        let root = temp_dir("apply-err");
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // The "bookmarks file" is a directory: reading it fails.
        let blocked = root.join("bookmarks");
        std::fs::create_dir_all(&blocked).unwrap();
        let paths = Paths {
            bookmarks: blocked,
            scripts: root.join("scripts"),
        };
        let on = config_with(&root, true, &[("docs", &docs)]);
        let done = apply_in(&on, &paths);
        assert_eq!(done.errors.len(), 1, "{:?}", done.errors);
        assert!(done.errors[0].starts_with("bookmarks:"));
        assert!(done.scripts_changed, "the scripts half still went through");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn script_paths_are_shell_quoted() {
        assert_eq!(sh_quote("/usr/bin/x"), "'/usr/bin/x'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote("a b;rm -rf /"), "'a b;rm -rf /'");
    }
}
