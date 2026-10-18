//! `protondrive4linux-gui --screenshots <dir>`: draw every page from example
//! data and write each one as a PNG - the pictures in the README and in
//! `docs/screenshots.md`, reproducible with one command instead of one-off
//! captures that go stale as the app changes.
//!
//! It is self-contained on purpose. It builds its own configuration in memory
//! and works in a scratch folder, so it never reads or writes your config, state,
//! themes or desktop entries, never calls the `proton-drive` CLI and never
//! touches the network: every folder, file and conflict on screen is made up.
//! `scripts/screenshots.sh` also runs it with a scratch `HOME` (inside a private
//! X server when `xvfb-run` is there), but the guarantee is the mode's own.
//!
//! The window is a fixed [`WINDOW_W`] x [`WINDOW_H`] points, forced to one pixel
//! per point, so the pictures come out the same size on every machine; `--scale
//! 2` draws everything at twice the size for sharper images (it needs a screen
//! with room for a window that big, which a virtual X server has).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use neutronsync::config::{self, Config, Pair, ThemePref};
use neutronsync::datefmt::now_epoch;
use neutronsync::events::ConflictResolution;
use neutronsync::service::{
    AccountState, ActivityItem, ActivityKind, ActivityOp, AppState, HeldDeletes, PairState, Phase,
    Progress,
};
use neutronsync::stats::ConflictRecord;
use neutronsync::themes::Catalog;

use super::{App, Nav};

/// The window, in points. Short enough for a 1366x768 laptop screen once the
/// desktop's panels have taken their share (a compositor shrinks a window that
/// doesn't fit the space left): the pictures are for thumbnails, and every
/// page still shows what it has to.
pub const WINDOW_W: f32 = 1100.0;
pub const WINDOW_H: f32 = 660.0;

/// Where the desktop's config folder is, as far as the pictures are concerned.
/// The Settings page prints the themes folder and reads the "launch at login"
/// entry from there; a made-up one keeps the pictures from showing the machine
/// they were made on, and from depending on it.
const NEUTRAL_CONFIG_HOME: &str = "/home/you/.config";

/// Frames a scene is drawn, and the time it is on screen, before it is
/// photographed: enough for the layout, the fonts and the progress animation to
/// settle.
const SETTLE_FRAMES: u32 = 10;
const SETTLE_TIME: Duration = Duration::from_millis(500);
/// A scene that has not produced its picture by now is given up on.
const SCENE_TIMEOUT: Duration = Duration::from_secs(30);

/// One page as it is photographed.
struct Scene {
    name: &'static str,
    nav: Nav,
    light: bool,
    state: AppState,
    conflicts: Vec<ConflictRecord>,
}

// --- the example data -------------------------------------------------------------

fn pair(
    name: &str,
    local: &str,
    remote: &str,
    phase: Phase,
    last_synced: Option<i64>,
    files: usize,
) -> PairState {
    PairState {
        name: name.into(),
        local: local.into(),
        remote: remote.into(),
        phase,
        progress: Progress::default(),
        scanned_folders: 0,
        current_op: None,
        last_error: None,
        last_synced,
        tracked: files,
        retrying: 0,
        held: None,
    }
}

fn op(action: &str, path: &str, pair: &str) -> ActivityOp {
    ActivityOp {
        action: action.into(),
        path: path.into(),
        pair: pair.into(),
        ok: true,
        error: None,
    }
}

fn done(ts: i64, action: &str, path: &str, pair: &str) -> ActivityItem {
    ActivityItem {
        ts,
        kind: ActivityKind::Sync,
        text: format!("{action} {path}"),
        op: Some(op(action, path, pair)),
    }
}

fn account() -> AccountState {
    AccountState {
        checked: true,
        binary_found: true,
        signed_in: true,
        keyring_unreachable: false,
        version: "cli-drive@0.8.0 (/usr/bin/proton-drive)".into(),
        checking: false,
    }
}

/// The recent feed, oldest first (the page shows newest first).
fn feed(now: i64) -> VecDeque<ActivityItem> {
    [
        done(
            now - 900,
            "download",
            "Contracts/Lease (signed).pdf",
            "Documents",
        ),
        done(now - 780, "upload", "Budget 2026.xlsx", "Documents"),
        done(now - 700, "upload", "Quarterly report.pdf", "Documents"),
        done(now - 610, "upload", "Lisbon trip/IMG_2038.jpg", "Photos"),
        done(now - 560, "upload", "Lisbon trip/IMG_2039.jpg", "Photos"),
        done(now - 500, "download", "Lisbon trip/IMG_2040.jpg", "Photos"),
        done(
            now - 330,
            "upload",
            "notes/meeting-2026-10-14.md",
            "Documents",
        ),
        done(now - 240, "delete-remote", "old-draft.docx", "Documents"),
        done(now - 190, "upload", "app/src/main.rs", "Projects"),
        done(now - 150, "upload", "app/README.md", "Projects"),
    ]
    .into_iter()
    .collect()
}

fn conflicts(now: i64) -> Vec<ConflictRecord> {
    let rec = |id, ago, pair: &str, rel: &str, resolution, copy: Option<&str>| ConflictRecord {
        id,
        ts: now - ago,
        pair: pair.into(),
        rel: rel.into(),
        resolution,
        copy_rel: copy.map(str::to_string),
        dismissed: false,
    };
    vec![
        rec(
            4,
            1_800,
            "Documents",
            "Budget 2026.xlsx",
            ConflictResolution::Skipped,
            None,
        ),
        rec(
            3,
            7_200,
            "Documents",
            "Contracts/Lease (signed).pdf",
            ConflictResolution::KeptBoth,
            Some("Contracts/Lease (signed) (conflict 2026-10-16 18-02).pdf"),
        ),
        rec(
            2,
            93_600,
            "Photos",
            "Lisbon trip/notes.txt",
            ConflictResolution::KeptLocal,
            None,
        ),
        rec(
            1,
            180_000,
            "Projects",
            "app/config.toml",
            ConflictResolution::KeptRemote,
            None,
        ),
    ]
}

/// The state everything starts from: three folders, all synced, the feed above.
fn idle(now: i64) -> AppState {
    AppState {
        pairs: vec![
            pair(
                "Documents",
                "~/Documents",
                "/my-files/Documents",
                Phase::Synced,
                Some(now - 130),
                4_733,
            ),
            pair(
                "Photos",
                "~/Pictures/Camera",
                "/my-files/Photos",
                Phase::Synced,
                Some(now - 130),
                2_118,
            ),
            pair(
                "Projects",
                "~/Projects",
                "/my-files/Projects",
                Phase::Synced,
                Some(now - 130),
                912,
            ),
        ],
        account: account(),
        activity: feed(now),
        active: vec![],
        busy: false,
        watching: true,
        signed_out: false,
        keyring_unreachable: false,
    }
}

fn scenes(now: i64) -> Vec<Scene> {
    let open_conflicts = conflicts(now);
    let scene = |name, nav, light, state: AppState, conflicts: Vec<ConflictRecord>| Scene {
        name,
        nav,
        light,
        state,
        conflicts,
    };

    // Mid-sync: Photos is uploading, with files in flight.
    let mut syncing = idle(now);
    syncing.busy = true;
    syncing.pairs[1].phase = Phase::Syncing;
    syncing.pairs[1].progress = Progress {
        done: 37,
        total: 120,
    };
    syncing.pairs[1].current_op = Some("upload Lisbon trip/IMG_2043.jpg".into());
    syncing.active = vec![
        op("upload", "Lisbon trip/IMG_2043.jpg", "Photos"),
        op("upload", "Lisbon trip/IMG_2044.jpg", "Photos"),
        op("download", "Contracts/Lease (signed).pdf", "Documents"),
    ];

    // Folders with something to see: one being watched, one syncing, one with
    // files waiting to retry.
    let mut folders = idle(now);
    folders.pairs[1].phase = Phase::Syncing;
    folders.pairs[1].progress = Progress {
        done: 37,
        total: 120,
    };
    folders.pairs[2].retrying = 3;
    folders.busy = true;

    // Not signed in yet: the first screen of a fresh install.
    let mut signed_out = idle(now);
    signed_out.account.signed_in = false;
    signed_out.signed_out = true;
    signed_out.watching = false;

    // Signed in, but the CLI can't reach the keyring the session is kept in.
    let mut keyring = idle(now);
    keyring.account.signed_in = false;
    keyring.account.keyring_unreachable = true;
    keyring.keyring_unreachable = true;
    keyring.watching = false;

    // The mass-deletion guard held a deletion back: most of Photos is gone from
    // this computer (a card pulled out, a folder emptied by accident), and the
    // deletions that would have followed on Proton Drive wait for a decision.
    let mut held = idle(now);
    let hold = HeldDeletes {
        remote: 1_640,
        local: 0,
        tracked: 2_118,
        at: now - 20,
    };
    let text = neutronsync::events::held_deletes_text("Photos", 1_640, 0, 2_118);
    held.pairs[1].phase = Phase::Error;
    held.pairs[1].held = Some(hold);
    held.pairs[1].last_error = Some(text.clone());
    held.activity.push_back(ActivityItem {
        ts: now - 20,
        kind: ActivityKind::Error,
        text: format!("Photos: {text}"),
        op: None,
    });

    let mut idle_folders = idle(now);
    idle_folders.pairs[2].retrying = 3;

    vec![
        scene("activity-syncing", Nav::Activity, false, syncing, vec![]),
        scene("activity", Nav::Activity, false, idle(now), vec![]),
        scene("folders", Nav::Folders, false, folders, vec![]),
        scene(
            "conflicts",
            Nav::Conflicts,
            false,
            idle(now),
            open_conflicts.clone(),
        ),
        scene("settings", Nav::Settings, false, idle(now), vec![]),
        scene("account", Nav::Account, false, idle(now), vec![]),
        scene("sign-in", Nav::Activity, false, signed_out, vec![]),
        scene("keyring", Nav::Activity, false, keyring, vec![]),
        scene("held", Nav::Activity, false, held, vec![]),
        scene("activity-light", Nav::Activity, true, idle(now), vec![]),
        scene("folders-light", Nav::Folders, true, idle_folders, vec![]),
        scene(
            "conflicts-light",
            Nav::Conflicts,
            true,
            idle(now),
            open_conflicts,
        ),
    ]
}

/// A configuration that exists only for the screenshots: three made-up folders,
/// a CLI that is not there (so nothing can be run, let alone reach the network),
/// and a state folder inside `scratch`.
fn example_config(scratch: &Path) -> Config {
    let mut cfg = config::parse_str("", None).expect("the empty config is valid");
    cfg.binary = scratch.join("no-proton-drive").display().to_string();
    cfg.state_dir = scratch.join("state");
    cfg.pairs = [
        ("Documents", "~/Documents", "/my-files/Documents"),
        ("Photos", "~/Pictures/Camera", "/my-files/Photos"),
        ("Projects", "~/Projects", "/my-files/Projects"),
    ]
    .iter()
    .map(|(name, local, remote)| Pair {
        name: (*name).into(),
        local: PathBuf::from(local),
        remote: (*remote).into(),
        auto: true,
        exclude: vec![],
    })
    .collect();
    cfg.auto_sync = true;
    cfg.run_in_tray = true;
    cfg.notifications = true;
    cfg.check_on_launch = false;
    cfg.file_manager = false;
    cfg.theme = ThemePref::Dark;
    cfg
}

// --- the script ---------------------------------------------------------------------

/// Walks the window through the scenes: puts the app in a scene's state, waits
/// for it to settle, asks egui for a screenshot, writes it, moves on.
pub struct Script {
    dir: PathBuf,
    scale: f32,
    scenes: Vec<Scene>,
    index: usize,
    /// When the current scene started being drawn.
    since: Option<Instant>,
    frames: u32,
    asked: bool,
    written: usize,
    problems: Vec<String>,
    /// The scratch folder, removed when the script ends.
    scratch: PathBuf,
}

impl Script {
    fn new(dir: PathBuf, scale: f32, scenes: Vec<Scene>, scratch: PathBuf) -> Script {
        Script {
            dir,
            scale,
            scenes,
            index: 0,
            since: None,
            frames: 0,
            asked: false,
            written: 0,
            problems: Vec::new(),
            scratch,
        }
    }

    /// The size a picture should have, given the scale.
    fn expected_size(&self) -> [usize; 2] {
        [
            (WINDOW_W * self.scale).round() as usize,
            (WINDOW_H * self.scale).round() as usize,
        ]
    }

    fn save(&mut self, image: &egui::ColorImage) {
        let name = self.scenes[self.index].name;
        let path = self.dir.join(format!("{name}.png"));
        let rgb: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|c| [c.r(), c.g(), c.b()])
            .collect();
        match write_png(&path, image.size[0] as u32, image.size[1] as u32, &rgb) {
            Ok(()) => self.written += 1,
            Err(e) => self
                .problems
                .push(format!("{name}: could not write {}: {e}", path.display())),
        }
        let want = self.expected_size();
        if image.size != want {
            self.problems.push(format!(
                "{name}: the window came out {}x{} instead of {}x{} (a tiling window manager, \
                 or a screen too small for it)",
                image.size[0], image.size[1], want[0], want[1]
            ));
        }
    }

    fn advance(&mut self) {
        self.index += 1;
        self.since = None;
    }

    fn finish(&self) -> ! {
        let _ = std::fs::remove_dir_all(&self.scratch);
        eprintln!(
            "protondrive4linux: wrote {} of {} screenshots to {}",
            self.written,
            self.scenes.len(),
            self.dir.display()
        );
        for p in &self.problems {
            eprintln!("protondrive4linux: {p}");
        }
        std::process::exit(if self.problems.is_empty() { 0 } else { 1 });
    }

    /// One frame of the script. Returns the state the window should draw.
    fn step(&mut self, app: &mut App, ctx: &egui::Context) -> Option<AppState> {
        // A picture that arrived is the current scene's: save it and move on.
        let arrived = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot {
                    user_data, image, ..
                } => user_data
                    .data
                    .as_ref()
                    .and_then(|d| d.downcast_ref::<usize>())
                    .map(|n| (*n, image.clone())),
                _ => None,
            })
        });
        if let Some((n, image)) = arrived {
            if n == self.index {
                self.save(&image);
                self.advance();
            }
        }
        if self.index >= self.scenes.len() {
            self.finish();
        }

        let scene = &self.scenes[self.index];
        let (name, nav, light) = (scene.name, scene.nav, scene.light);
        let (conflicts, state) = (scene.conflicts.clone(), scene.state.clone());
        let started = *self.since.get_or_insert_with(|| {
            // First frame of the scene: put the app in its state. The page has
            // no fade-in, the theme is explicit (never "follow the system"),
            // and the conflict list is the scene's, not a database's.
            app.nav = nav;
            app.nav_t = 1.0;
            app.cfg.theme = if light {
                ThemePref::Light
            } else {
                ThemePref::Dark
            };
            app.conflict_count = conflicts.len();
            app.conflicts = conflicts;
            app.show_resolved = false;
            self.frames = 0;
            self.asked = false;
            Instant::now()
        });
        self.frames += 1;

        if !self.asked && self.frames >= SETTLE_FRAMES && started.elapsed() >= SETTLE_TIME {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                self.index,
            )));
            self.asked = true;
        }
        if started.elapsed() > SCENE_TIMEOUT {
            self.problems.push(format!(
                "{name}: no picture arrived within {}s",
                SCENE_TIMEOUT.as_secs()
            ));
            self.advance();
        }
        Some(state)
    }
}

/// Write `rgb` (8-bit, three bytes a pixel) as a PNG.
fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgb).map_err(|e| e.to_string())
}

// --- the window -------------------------------------------------------------------

impl App {
    /// The app as the screenshot mode draws it: example data, a scratch folder,
    /// the bundled themes only, and nothing started.
    pub(super) fn for_screenshots(
        cc: &eframe::CreationContext<'_>,
        dir: PathBuf,
        scale: f32,
    ) -> App {
        super::install_theme(&cc.egui_ctx);
        let scratch =
            std::env::temp_dir().join(format!("protondrive4linux-shots-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&scratch);
        let cfg = example_config(&scratch);
        let mut app = App::assemble(
            cfg,
            scratch.join("protondrive4linux.toml"),
            true,
            false,
            None,
            None,
            // An empty folder: the themes that ship with the app and none of yours.
            Catalog::load(&scratch.join("themes")),
        );
        cc.egui_ctx.set_zoom_factor(scale);
        // The scenes bring their own conflicts and their own theme list.
        app.conflicts_polled = f64::INFINITY;
        app.themes_scanned = f64::INFINITY;
        app.shot = Some(Script::new(dir, scale, scenes(now_epoch()), scratch));
        app
    }

    /// In screenshot mode, advance the script and return the state to draw this
    /// frame in place of the real one; `None` in normal use.
    pub(super) fn drive_shots(&mut self, ctx: &egui::Context) -> Option<AppState> {
        let mut script = self.shot.take()?;
        ctx.request_repaint();
        let state = script.step(self, ctx);
        self.shot = Some(script);
        state
    }
}

/// Open the window and photograph every scene into `dir`.
pub fn run(dir: PathBuf, scale: f32) -> eframe::Result<()> {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("protondrive4linux: cannot create {}: {e}", dir.display());
        std::process::exit(2);
    }
    eprintln!(
        "protondrive4linux: drawing the example scenes into {} (made-up data only; nothing of yours is read)",
        dir.display()
    );
    // One pixel per point, so the pictures are the same size on every screen.
    std::env::set_var("WINIT_X11_SCALE_FACTOR", "1");
    std::env::set_var("XDG_CONFIG_HOME", NEUTRAL_CONFIG_HOME);
    // If what the Settings page prints is no longer derived from that, a picture
    // would show the folder this machine really uses: refuse to make any.
    let themes = neutronsync::themes::themes_dir();
    if !themes.starts_with(NEUTRAL_CONFIG_HOME) {
        eprintln!(
            "protondrive4linux: the themes folder ({}) is no longer derived from XDG_CONFIG_HOME; \
             the Settings picture would show a real path, so none are made",
            themes.display()
        );
        std::process::exit(2);
    }
    let size = [WINDOW_W * scale, WINDOW_H * scale];
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_min_inner_size(size)
        .with_max_inner_size(size)
        .with_title("protondrive4linux (screenshots)")
        .with_app_id("protondrive4linux")
        // No title bar: a compositor that adds one takes its height out of the
        // picture (a 700-point window came out 648 high).
        .with_decorations(false);
    // X11 (or XWayland) when there is one: a dialog-type window floats in a
    // tiling compositor instead of being squeezed into a column, so it keeps the
    // size it asked for.
    let x11 = std::env::var_os("DISPLAY").is_some();
    if x11 {
        viewport = viewport.with_window_type(egui::X11WindowType::Dialog);
    }
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    if x11 {
        use winit::platform::x11::EventLoopBuilderExtX11;
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
    eframe::run_native(
        "protondrive4linux",
        options,
        Box::new(move |cc| Ok(Box::new(App::for_screenshots(cc, dir, scale)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn every_scene_has_its_own_file_name_and_the_set_covers_every_page() {
        let all = scenes(NOW);
        let mut names: Vec<_> = all.iter().map(|s| s.name).collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "a name used twice would overwrite a picture"
        );
        for n in &names {
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{n} is not a plain file name"
            );
        }
        // The pages people look at, and the light theme for the ones that show
        // the most colour.
        for want in [
            "activity",
            "folders",
            "conflicts",
            "settings",
            "account",
            "sign-in",
            "keyring",
            "held",
        ] {
            assert!(names.contains(&want), "no scene named {want}");
        }
        assert!(all.iter().any(|s| s.light));
        assert!(all.iter().any(|s| !s.light));
    }

    #[test]
    fn only_the_conflicts_scenes_carry_conflicts_and_only_the_syncing_one_is_busy() {
        for s in scenes(NOW) {
            assert_eq!(
                !s.conflicts.is_empty(),
                s.nav == Nav::Conflicts,
                "{}: conflicts on a page that doesn't show them, or none on the page that does",
                s.name
            );
        }
        let busy: Vec<_> = scenes(NOW)
            .into_iter()
            .filter(|s| s.state.busy)
            .map(|s| s.name)
            .collect();
        assert_eq!(busy, ["activity-syncing", "folders"]);
    }

    #[test]
    fn the_example_times_read_the_same_whenever_it_is_run() {
        // Relative to now, so "2 minutes ago" stays "2 minutes ago".
        let state = idle(NOW);
        for p in &state.pairs {
            assert_eq!(p.last_synced, Some(NOW - 130));
        }
        let newest = state.activity.iter().map(|a| a.ts).max().unwrap();
        assert!(
            NOW - newest >= 120,
            "the newest row is recent but not 'just now'"
        );
        // And the same at any other time.
        assert_eq!(
            idle(NOW + 1_000_000).pairs[0].last_synced,
            Some(NOW + 1_000_000 - 130)
        );
    }

    #[test]
    fn no_scene_shows_a_page_that_prints_where_this_machine_keeps_its_files() {
        // About prints the config and state folders; those are in a scratch
        // folder here, and a picture of them would be of this machine.
        for s in scenes(NOW) {
            assert!(s.nav != Nav::About, "{} would print real paths", s.name);
        }
    }

    #[test]
    fn only_the_sign_in_and_keyring_scenes_show_the_gate_and_each_says_why() {
        for s in scenes(NOW) {
            let gate = s.state.account.checked
                && !(s.state.account.binary_found && s.state.account.signed_in);
            assert_eq!(gate, matches!(s.name, "sign-in" | "keyring"), "{}", s.name);
            // The two reasons never overlap, and a scene says it the way the
            // app would: the account check and the state agree.
            assert!(
                !(s.state.signed_out && s.state.keyring_unreachable),
                "{}",
                s.name
            );
            assert_eq!(
                s.state.account.keyring_unreachable, s.state.keyring_unreachable,
                "{}",
                s.name
            );
            assert_eq!(s.state.signed_out, s.name == "sign-in", "{}", s.name);
        }
    }

    #[test]
    fn the_held_scene_is_a_hold_the_activity_page_asks_about() {
        let none = std::collections::HashMap::new();
        let held = scenes(NOW)
            .into_iter()
            .find(|s| s.name == "held")
            .expect("a held scene");
        let (pair, h, others) = crate::held_to_show(&held.state, &none).expect("a folder is held");
        assert_eq!((pair, others), ("Photos", 0));
        assert_eq!((h.files(), h.percent()), (1_640, 78));
        // Every other scene is an ordinary one: nothing asks for a decision.
        for s in scenes(NOW).into_iter().filter(|s| s.name != "held") {
            assert!(crate::held_to_show(&s.state, &none).is_none(), "{}", s.name);
        }
    }

    #[test]
    fn the_example_config_cannot_reach_anything_real() {
        let scratch = std::path::PathBuf::from("/tmp/protondrive4linux-shots-test");
        let cfg = example_config(&scratch);
        assert!(
            cfg.state_dir.starts_with(&scratch),
            "state stays in the scratch folder"
        );
        // A CLI that isn't there: nothing can be run, so nothing reaches Proton.
        assert!(Path::new(&cfg.binary).starts_with(&scratch));
        assert!(!Path::new(&cfg.binary).exists());
        assert!(!cfg.check_on_launch, "no update check");
        assert!(!cfg.file_manager, "no writes to the file manager's folders");
        assert_eq!(cfg.theme, ThemePref::Dark);
        assert_eq!(cfg.pairs.len(), 3);
        assert!(cfg.source_path.is_none(), "no file to save back to");
        // The folder names are made up, not anybody's real ones.
        for p in &cfg.pairs {
            assert!(p.local.starts_with("~"), "{}", p.local.display());
        }
    }

    #[test]
    fn a_picture_is_written_as_a_png_that_reads_back_the_same() {
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-shots-png-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        let rgb: Vec<u8> = (0..4 * 3 * 3).map(|i| (i * 7) as u8).collect(); // 4x3 pixels
        write_png(&path, 4, 3, &rgb).unwrap();

        let decoder =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (4, 3));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(&buf[..info.buffer_size()], &rgb[..]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_expected_picture_size_follows_the_scale() {
        let script = |scale| Script::new(PathBuf::new(), scale, vec![], PathBuf::new());
        assert_eq!(script(1.0).expected_size(), [1100, 660]);
        assert_eq!(script(2.0).expected_size(), [2200, 1320]);
    }
}
