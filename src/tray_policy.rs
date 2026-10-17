//! What keeps background sync alive when the tray daemon dies, and what an open
//! window says about it.
//!
//! In tray mode the tray daemon owns the watcher: when it dies, sync stops, and
//! until now nothing noticed. It could die on a broken display connection (the
//! compositor or panel restarting), on a panic (the release build aborts on
//! panic), or because the kernel's OOM killer picked it; nothing started it
//! again, and an open window kept drawing the last `status.json` as if it were
//! live.
//!
//! So `--tray` is now a thin supervisor that holds the tray lock and runs the
//! real tray (`--tray-child`), starting it again after a crash; and the window
//! checks that background sync is alive and updating, and says so when it isn't.
//! The numbers (2, 5, 15, 30, 30 seconds between restarts; five restarts in five
//! minutes before giving up; ten seconds without an update before calling it
//! unresponsive) are from upstream NeutronSync 0.5.1.
//!
//! Nothing here spawns a process, installs a signal handler or reads a file. The
//! decisions are plain functions of the facts they are given, and the supervisor
//! loop is written against [`Supervised`], so the tests exercise every branch
//! with a scripted stand-in. The real thing (`--tray` in the GUI binary) supplies
//! the process handling.

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How a tray child stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEnd {
    Code(i32),
    Signal(i32),
}

impl ChildEnd {
    /// Short text for the log line.
    pub fn describe(self) -> String {
        match self {
            ChildEnd::Code(code) => format!("exit code {code}"),
            ChildEnd::Signal(signal) => format!("signal {signal}"),
        }
    }
}

/// What the supervisor does after a child stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Exit code 0: the tray's Quit, or there was nothing to run.
    StopClean,
    /// The display session is gone, so a restart would only spin after logout.
    StopSessionGone,
    /// Already restarted [`MAX_RESTARTS_IN_WINDOW`] times inside the window.
    StopRestartCap,
    /// Start the child again after `delay_secs`.
    Restart { delay_secs: u64 },
}

/// Pause before each restart inside the window: 2s, 5s, 15s, 30s, then 30s.
const RESTART_BACKOFF_SECS: [u64; 5] = [2, 5, 15, 30, 30];

/// Restarts allowed inside [`RESTART_WINDOW`]; the next one stops the supervisor.
pub const MAX_RESTARTS_IN_WINDOW: u32 = 5;

/// How far back restarts count towards the cap.
pub const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);

/// `restarts_in_window` is how many restarts were already made inside the
/// window, not counting the one being decided.
pub fn supervisor_action(
    end: ChildEnd,
    session_present: bool,
    restarts_in_window: u32,
) -> SupervisorAction {
    // Exit 0 is the tray's Quit (or "no config, nothing to run"), whatever else
    // is going on: that is a request, not a failure.
    if matches!(end, ChildEnd::Code(0)) {
        return SupervisorAction::StopClean;
    }
    if !session_present {
        return SupervisorAction::StopSessionGone;
    }
    if restarts_in_window >= MAX_RESTARTS_IN_WINDOW {
        return SupervisorAction::StopRestartCap;
    }
    let index = (restarts_in_window as usize).min(RESTART_BACKOFF_SECS.len() - 1);
    SupervisorAction::Restart {
        delay_secs: RESTART_BACKOFF_SECS[index],
    }
}

/// The restarts made recently, so the cap means "five in five minutes" and not
/// "five, ever": a tray that crashes once a day is restarted every time.
#[derive(Debug, Default)]
pub struct RestartLog {
    at: Vec<Instant>,
}

impl RestartLog {
    /// How many restarts happened within [`RESTART_WINDOW`] of `now`.
    pub fn count(&mut self, now: Instant) -> u32 {
        self.at
            .retain(|t| now.saturating_duration_since(*t) < RESTART_WINDOW);
        self.at.len() as u32
    }

    pub fn record(&mut self, now: Instant) {
        self.at.push(now);
    }
}

/// What the supervisor needs from the outside world.
pub trait Supervised {
    /// Is there still a desktop session for the tray to live in?
    fn session_present(&mut self) -> bool;
    /// Start the tray, wait for it to end, and say how it ended. `Err` when it
    /// could not be started at all.
    fn run_child(&mut self) -> std::io::Result<ChildEnd>;
    /// Wait `delay`; `true` if the supervisor was told to stop in the meantime.
    fn pause(&mut self, delay: Duration) -> bool;
    /// Has the supervisor itself been asked to stop (SIGTERM, SIGINT)?
    fn stop_requested(&mut self) -> bool;
    fn now(&mut self) -> Instant;
    /// A line for the sync log.
    fn note(&mut self, line: &str);
}

/// Why the supervisor stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The tray quit on its own request (Quit, or nothing to run).
    Quit,
    /// The supervisor was told to stop.
    Stopped,
    /// The display session is gone.
    SessionGone,
    /// Gave up: too many restarts in a short time.
    TooManyRestarts,
}

/// Run the tray under supervision until it quits, the supervisor is told to
/// stop, the session goes away, or it keeps failing.
pub fn supervise(s: &mut impl Supervised) -> Outcome {
    let mut restarts = RestartLog::default();
    loop {
        if s.stop_requested() {
            return Outcome::Stopped;
        }
        if !s.session_present() {
            s.note("background sync stopped: the display session is gone");
            return Outcome::SessionGone;
        }
        let (end, how) = match s.run_child() {
            Ok(end) => {
                // Told to stop while it ran: the child was told too, and ending
                // is exactly what it was asked to do, not a crash.
                if s.stop_requested() {
                    return Outcome::Stopped;
                }
                (end, format!("tray {}", end.describe()))
            }
            // Counts as a crash: it will fail the same way, so the cap ends it.
            Err(e) => (ChildEnd::Code(1), format!("could not start the tray: {e}")),
        };
        let now = s.now();
        let session = s.session_present();
        match supervisor_action(end, session, restarts.count(now)) {
            SupervisorAction::StopClean => return Outcome::Quit,
            SupervisorAction::StopSessionGone => {
                s.note("background sync stopped: the display session is gone");
                return Outcome::SessionGone;
            }
            SupervisorAction::StopRestartCap => {
                s.note(&format!(
                    "background sync stopped ({how}); that was {MAX_RESTARTS_IN_WINDOW} \
                     restarts in {} minutes, so it is not starting it again",
                    RESTART_WINDOW.as_secs() / 60
                ));
                return Outcome::TooManyRestarts;
            }
            SupervisorAction::Restart { delay_secs } => {
                s.note(&format!(
                    "background sync stopped ({how}); starting it again in {delay_secs}s"
                ));
                restarts.record(now);
                if s.pause(Duration::from_secs(delay_secs)) {
                    return Outcome::Stopped;
                }
            }
        }
    }
}

/// What an open window should tell the user about background sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonBanner {
    /// Not running: the window has started it, or is about to.
    Restarting,
    /// Running, but its status hasn't been updated lately.
    NotResponding,
    /// The window started it [`MAX_WINDOW_SPAWN_ATTEMPTS`] times and it never
    /// came up.
    CouldNotStart,
}

/// Failed `--tray` starts from the window before it stops trying.
pub const MAX_WINDOW_SPAWN_ATTEMPTS: u32 = 3;

/// A status older than this (or missing for this long) means "not updating".
pub const STATUS_STALE_SECS: f64 = 10.0;

/// What the window knows when it checks on background sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LivenessInput {
    /// A live process holds the tray lock.
    pub alive: bool,
    /// `status.json` is old or missing (see [`status_is_stale`]).
    pub status_stale: bool,
    /// Starts this window has already made without the tray coming up.
    pub attempts: u32,
    /// It has been long enough since the last start to judge it.
    pub spawn_due: bool,
    /// The window started the tray a moment ago and it hasn't had time to take
    /// its lock yet - not worth alarming anyone over.
    pub starting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowLiveness {
    pub banner: Option<DaemonBanner>,
    /// The window should start `--tray` now.
    pub spawn: bool,
    /// The new count of failed starts.
    pub attempts: u32,
}

/// Decide what the window shows and whether it starts the tray. A start counts
/// as failed only once `spawn_due` says the wait has passed and the lock is
/// still dead; the third failure becomes [`DaemonBanner::CouldNotStart`].
pub fn window_liveness(i: LivenessInput) -> WindowLiveness {
    if i.alive {
        return WindowLiveness {
            banner: i.status_stale.then_some(DaemonBanner::NotResponding),
            spawn: false,
            attempts: 0,
        };
    }
    if i.starting {
        return WindowLiveness {
            banner: None,
            spawn: false,
            attempts: i.attempts,
        };
    }
    let out = |banner, spawn, attempts| WindowLiveness {
        banner: Some(banner),
        spawn,
        attempts,
    };
    if i.attempts >= MAX_WINDOW_SPAWN_ATTEMPTS {
        return if i.spawn_due {
            out(DaemonBanner::CouldNotStart, false, i.attempts)
        } else {
            out(DaemonBanner::Restarting, false, i.attempts)
        };
    }
    if i.spawn_due {
        return out(DaemonBanner::Restarting, true, i.attempts + 1);
    }
    out(DaemonBanner::Restarting, false, i.attempts)
}

/// True when a periodic check is due. `last_check_secs < 0` means "never":
/// check at once.
pub fn check_due(now_secs: f64, last_check_secs: f64, interval_secs: f64) -> bool {
    last_check_secs < 0.0 || now_secs - last_check_secs >= interval_secs
}

/// `status.json` is stale when it is older than [`STATUS_STALE_SECS`], or when
/// it has been missing for that long (a daemon that has only just started
/// hasn't written one yet, which is not the same as one that stopped).
pub fn status_is_stale(age_secs: Option<f64>, missing_for_secs: Option<f64>) -> bool {
    age_secs.is_some_and(|age| age > STATUS_STALE_SECS)
        || missing_for_secs.is_some_and(|age| age > STATUS_STALE_SECS)
}

/// `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`, or `$WAYLAND_DISPLAY` itself when it is
/// an absolute path.
pub fn wayland_socket_path(runtime: Option<&str>, display: Option<&str>) -> Option<PathBuf> {
    let display = display.filter(|d| !d.is_empty())?;
    let path = PathBuf::from(display);
    if path.is_absolute() {
        return Some(path);
    }
    let runtime = runtime.filter(|r| !r.is_empty())?;
    Some(PathBuf::from(runtime).join(path))
}

/// The socket for `$DISPLAY`: `:0` and `:0.0` are `/tmp/.X11-unix/X0`.
pub fn x11_socket_path(display: &str) -> Option<PathBuf> {
    let rest = display.trim().rsplit_once(':')?.1;
    let screen = rest.split('.').next()?.trim();
    if screen.is_empty() || !screen.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(PathBuf::from(format!("/tmp/.X11-unix/X{screen}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // --- the restart decision ---------------------------------------------------

    #[test]
    fn a_clean_exit_is_the_users_quit_and_is_never_restarted() {
        for (session, restarts) in [(true, 0), (false, 4), (true, 9)] {
            assert_eq!(
                supervisor_action(ChildEnd::Code(0), session, restarts),
                SupervisorAction::StopClean
            );
        }
    }

    #[test]
    fn restarts_back_off_and_then_hit_the_cap() {
        let delays: Vec<_> = (0..5)
            .map(|n| supervisor_action(ChildEnd::Signal(9), true, n))
            .collect();
        assert_eq!(
            delays,
            [2, 5, 15, 30, 30].map(|delay_secs| SupervisorAction::Restart { delay_secs })
        );
        assert_eq!(
            supervisor_action(ChildEnd::Signal(9), true, 5),
            SupervisorAction::StopRestartCap
        );
        // Far past the cap is no different.
        assert_eq!(
            supervisor_action(ChildEnd::Code(1), true, 50),
            SupervisorAction::StopRestartCap
        );
    }

    #[test]
    fn without_a_session_it_stops_instead_of_spinning() {
        assert_eq!(
            supervisor_action(ChildEnd::Code(1), false, 0),
            SupervisorAction::StopSessionGone
        );
        assert_eq!(
            supervisor_action(ChildEnd::Signal(6), false, 3),
            SupervisorAction::StopSessionGone
        );
    }

    #[test]
    fn only_restarts_inside_the_window_count() {
        let t0 = Instant::now();
        let mut log = RestartLog::default();
        assert_eq!(log.count(t0), 0);
        for s in [0, 10, 20] {
            log.record(t0 + Duration::from_secs(s));
        }
        assert_eq!(log.count(t0 + Duration::from_secs(30)), 3);
        // The first one is just outside five minutes; the other two are not.
        assert_eq!(log.count(t0 + Duration::from_secs(300 + 5)), 2);
        assert_eq!(log.count(t0 + Duration::from_secs(300 + 25)), 0);
    }

    // --- the supervisor loop, against a scripted stand-in ------------------------

    struct Script {
        ends: VecDeque<std::io::Result<ChildEnd>>,
        session: Vec<bool>,
        stop_after_run: bool,
        stop_on_pause: bool,
        clock: Instant,
        step: Duration,
        runs: u32,
        pauses: Vec<u64>,
        notes: Vec<String>,
    }

    impl Script {
        fn new(ends: Vec<std::io::Result<ChildEnd>>) -> Script {
            Script {
                ends: ends.into(),
                session: vec![],
                stop_after_run: false,
                stop_on_pause: false,
                clock: Instant::now(),
                step: Duration::from_secs(1),
                runs: 0,
                pauses: vec![],
                notes: vec![],
            }
        }
    }

    impl Supervised for Script {
        fn session_present(&mut self) -> bool {
            if self.session.is_empty() {
                true
            } else {
                self.session.remove(0)
            }
        }
        fn run_child(&mut self) -> std::io::Result<ChildEnd> {
            self.runs += 1;
            self.ends
                .pop_front()
                .expect("the script ran out of children")
        }
        fn pause(&mut self, delay: Duration) -> bool {
            self.pauses.push(delay.as_secs());
            self.clock += delay;
            self.stop_on_pause
        }
        fn stop_requested(&mut self) -> bool {
            self.stop_after_run && self.runs > 0
        }
        fn now(&mut self) -> Instant {
            self.clock += self.step;
            self.clock
        }
        fn note(&mut self, line: &str) {
            self.notes.push(line.to_string());
        }
    }

    #[test]
    fn a_crash_is_restarted_after_the_backoff_and_a_quit_ends_it() {
        let mut s = Script::new(vec![
            Ok(ChildEnd::Signal(9)),
            Ok(ChildEnd::Code(1)),
            Ok(ChildEnd::Code(0)),
        ]);
        assert_eq!(supervise(&mut s), Outcome::Quit);
        assert_eq!(s.runs, 3);
        assert_eq!(s.pauses, [2, 5], "the backoff steps up");
        assert_eq!(s.notes.len(), 2);
        assert!(
            s.notes[0].contains("signal 9") && s.notes[0].contains("in 2s"),
            "{:?}",
            s.notes
        );
        assert!(
            s.notes[1].contains("exit code 1") && s.notes[1].contains("in 5s"),
            "{:?}",
            s.notes
        );
    }

    #[test]
    fn a_tray_that_keeps_failing_is_given_up_on_after_five_restarts() {
        let mut s = Script::new((0..10).map(|_| Ok(ChildEnd::Code(1))).collect());
        assert_eq!(supervise(&mut s), Outcome::TooManyRestarts);
        assert_eq!(s.runs, 6, "the first run plus five restarts");
        assert_eq!(s.pauses, [2, 5, 15, 30, 30]);
        let last = s.notes.last().unwrap();
        assert!(last.contains("5 restarts in 5 minutes"), "{last}");
        assert!(last.contains("not starting it again"), "{last}");
    }

    #[test]
    fn crashes_spread_out_over_time_never_reach_the_cap() {
        // Each crash is followed by a long quiet stretch: the window forgets them.
        let mut s = Script::new(
            (0..20)
                .map(|_| Ok(ChildEnd::Signal(11)))
                .chain([Ok(ChildEnd::Code(0))])
                .collect(),
        );
        s.step = Duration::from_secs(400);
        assert_eq!(supervise(&mut s), Outcome::Quit);
        assert_eq!(s.runs, 21);
        assert!(
            s.pauses.iter().all(|&d| d == 2),
            "each is a first restart: {:?}",
            s.pauses
        );
    }

    #[test]
    fn a_tray_that_cannot_even_start_is_a_crash_that_ends_at_the_cap() {
        let mut s = Script::new(
            (0..10)
                .map(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no such file",
                    ))
                })
                .collect(),
        );
        assert_eq!(supervise(&mut s), Outcome::TooManyRestarts);
        assert!(
            s.notes[0].contains("could not start the tray: no such file"),
            "{:?}",
            s.notes
        );
    }

    #[test]
    fn the_session_going_away_stops_it_whether_before_or_after_the_crash() {
        // Gone before the first start.
        let mut s = Script::new(vec![]);
        s.session = vec![false];
        assert_eq!(supervise(&mut s), Outcome::SessionGone);
        assert_eq!(s.runs, 0);
        assert!(s.notes[0].contains("display session is gone"));

        // Present to start, gone when the child dies (the usual logout).
        let mut s = Script::new(vec![Ok(ChildEnd::Code(1))]);
        s.session = vec![true, false];
        assert_eq!(supervise(&mut s), Outcome::SessionGone);
        assert_eq!(s.runs, 1);
        assert!(s.pauses.is_empty());
    }

    #[test]
    fn being_told_to_stop_ends_it_without_counting_as_a_crash() {
        // Stopped while the child ran: the child ending is what was asked for.
        let mut s = Script::new(vec![Ok(ChildEnd::Signal(15))]);
        s.stop_after_run = true;
        assert_eq!(supervise(&mut s), Outcome::Stopped);
        assert!(s.notes.is_empty(), "{:?}", s.notes);

        // Stopped while waiting to restart.
        let mut s = Script::new(vec![Ok(ChildEnd::Code(1))]);
        s.stop_on_pause = true;
        assert_eq!(supervise(&mut s), Outcome::Stopped);
        assert_eq!(s.runs, 1, "it did not start another");
    }

    // --- what the window says -----------------------------------------------------

    fn live(alive: bool, stale: bool, attempts: u32, due: bool, starting: bool) -> WindowLiveness {
        window_liveness(LivenessInput {
            alive,
            status_stale: stale,
            attempts,
            spawn_due: due,
            starting,
        })
    }

    #[test]
    fn a_live_updating_tray_needs_no_banner_and_resets_the_attempts() {
        let l = live(true, false, 2, true, false);
        assert_eq!((l.banner, l.spawn, l.attempts), (None, false, 0));
    }

    #[test]
    fn a_live_tray_that_stopped_updating_is_not_responding_not_restarted() {
        let l = live(true, true, 0, true, false);
        assert_eq!(l.banner, Some(DaemonBanner::NotResponding));
        assert!(
            !l.spawn,
            "it is there; starting a second one would only be refused"
        );
    }

    #[test]
    fn a_dead_tray_is_started_and_the_window_says_so() {
        let l = live(false, true, 0, true, false);
        assert_eq!(l.banner, Some(DaemonBanner::Restarting));
        assert!(l.spawn);
        assert_eq!(l.attempts, 1);
        // Not again until the wait has passed.
        let l = live(false, true, 1, false, false);
        assert_eq!(
            (l.banner, l.spawn, l.attempts),
            (Some(DaemonBanner::Restarting), false, 1)
        );
    }

    #[test]
    fn three_starts_that_never_take_become_could_not_start() {
        let l = live(false, true, 2, true, false);
        assert_eq!((l.spawn, l.attempts), (true, 3), "the third try");
        // After the third, still waiting out its ten seconds: still "restarting".
        let l = live(false, true, 3, false, false);
        assert_eq!((l.banner, l.spawn), (Some(DaemonBanner::Restarting), false));
        // Then it is judged.
        let l = live(false, true, 3, true, false);
        assert_eq!(
            (l.banner, l.spawn),
            (Some(DaemonBanner::CouldNotStart), false)
        );
        // It stays that way until something resets the count.
        assert_eq!(
            live(false, true, 9, true, false).banner,
            Some(DaemonBanner::CouldNotStart)
        );
    }

    #[test]
    fn a_tray_the_window_has_just_started_is_given_a_moment_before_anyone_is_alarmed() {
        let l = live(false, true, 1, false, true);
        assert_eq!((l.banner, l.spawn, l.attempts), (None, false, 1));
        // But once it is alive, the grace no longer matters.
        assert_eq!(live(true, false, 1, false, true).banner, None);
    }

    #[test]
    fn staleness_needs_an_old_file_or_one_missing_for_a_while() {
        assert!(!status_is_stale(Some(0.5), None));
        assert!(
            !status_is_stale(Some(10.0), None),
            "exactly ten seconds is not over it"
        );
        assert!(status_is_stale(Some(10.5), None));
        assert!(
            !status_is_stale(None, Some(3.0)),
            "a daemon that is just starting"
        );
        assert!(status_is_stale(None, Some(12.0)));
        assert!(!status_is_stale(None, None));
    }

    #[test]
    fn checks_run_at_their_interval_and_the_first_is_immediate() {
        assert!(check_due(5.0, -1.0, 2.0));
        assert!(!check_due(5.0, 4.0, 2.0));
        assert!(check_due(6.0, 4.0, 2.0));
        assert!(check_due(7.0, 4.0, 2.0));
    }

    // --- the display session -----------------------------------------------------

    #[test]
    fn the_wayland_socket_is_in_the_runtime_dir_unless_the_display_is_a_path() {
        assert_eq!(
            wayland_socket_path(Some("/run/user/1000"), Some("wayland-1")),
            Some(PathBuf::from("/run/user/1000/wayland-1"))
        );
        assert_eq!(
            wayland_socket_path(None, Some("/tmp/wl.sock")),
            Some(PathBuf::from("/tmp/wl.sock"))
        );
        assert_eq!(wayland_socket_path(Some("/run/user/1000"), None), None);
        assert_eq!(wayland_socket_path(Some("/run/user/1000"), Some("")), None);
        assert_eq!(wayland_socket_path(None, Some("wayland-1")), None);
        assert_eq!(wayland_socket_path(Some(""), Some("wayland-1")), None);
    }

    #[test]
    fn the_x11_socket_is_named_after_the_display_number() {
        for d in [":0", ":0.0", "localhost:0", " :0 "] {
            assert_eq!(
                x11_socket_path(d),
                Some(PathBuf::from("/tmp/.X11-unix/X0")),
                "{d:?}"
            );
        }
        assert_eq!(
            x11_socket_path(":12.1"),
            Some(PathBuf::from("/tmp/.X11-unix/X12"))
        );
        for d in ["", "no-colon", ":", ":x", ":0a"] {
            assert_eq!(x11_socket_path(d), None, "{d:?}");
        }
    }
}
