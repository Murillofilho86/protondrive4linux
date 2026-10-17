//! Desktop notifications (M5-005): tell the user about the few things worth
//! interrupting them for - a conflict, a sync problem that persists, a signed-out
//! session - without nagging.
//!
//! The deciding is a pure state machine ([`Notifier`], driven by an injected
//! clock so it's testable): it watches the app's [`OverallStatus`] and the
//! conflicts recorded since it last looked, and returns what to announce.
//! Delivery ([`send`]) is a single `org.freedesktop.Notifications.Notify` call
//! over the session bus, which every desktop's notification daemon implements.
//!
//! Anti-spam, since a sync client that cries wolf gets muted:
//! - a sync problem only notifies once it has lasted [`ERROR_GRACE_SECS`] - a
//!   rate-limit blip that clears on its first retry never does - then at most
//!   once per episode, and episodes closer than [`ERROR_COOLDOWN_SECS`] to the
//!   last notice stay quiet;
//! - conflicts arriving together are batched into one notice, and notices are at
//!   least [`CONFLICT_COOLDOWN_SECS`] apart (the rest wait and join the next);
//! - a signed-out session notifies on the transition into it, not while it lasts;
//! - a keyring the CLI can't reach is, at login, usually one its owner is about
//!   to unlock, so that only notifies once it has lasted [`KEYRING_GRACE_SECS`],
//!   then once per episode;
//! - the tray supervisor giving up on a tray that keeps dying notifies at most
//!   once an hour ([`announce_give_up`]).

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::events::ConflictResolution;
use crate::service::OverallStatus;
use crate::stats::ConflictRecord;

/// How long a sync problem must persist before it's worth a notification.
pub const ERROR_GRACE_SECS: i64 = 90;
/// Minimum gap between two sync-problem notifications.
pub const ERROR_COOLDOWN_SECS: i64 = 600;
/// How long to wait for more conflicts before announcing the batch.
pub const CONFLICT_BATCH_SECS: i64 = 3;
/// Minimum gap between two conflict notifications.
pub const CONFLICT_COOLDOWN_SECS: i64 = 30;
/// How long the keyring must stay out of reach before it's worth a notification:
/// well past the minute it takes to type a password into the unlock prompt.
pub const KEYRING_GRACE_SECS: i64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Conflict,
    Error,
    SignedOut,
    /// The CLI has been unable to reach its keyring for a while.
    WaitingForKeyring,
    /// The tray supervisor stopped restarting a tray that keeps failing.
    BackgroundSyncStopped,
}

/// What to show: a one-line summary and a short body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub kind: NoticeKind,
    pub summary: String,
    pub body: String,
}

/// Decides what deserves a notification, given the app's state over time.
pub struct Notifier {
    prev: OverallStatus,
    error_since: Option<i64>,
    error_notified: bool,
    last_error_notice: Option<i64>,
    keyring_since: Option<i64>,
    keyring_notified: bool,
    pending: Vec<ConflictRecord>,
    pending_since: Option<i64>,
    last_conflict_notice: Option<i64>,
}

impl Notifier {
    /// Start watching from `initial` - the state the app is in right now, which
    /// is not itself announced.
    pub fn new(initial: OverallStatus) -> Self {
        Notifier {
            prev: initial,
            error_since: None,
            error_notified: false,
            last_error_notice: None,
            keyring_since: None,
            keyring_notified: false,
            pending: Vec::new(),
            pending_since: None,
            last_conflict_notice: None,
        }
    }

    /// Advance to `now` given the current overall `status`, its sentence
    /// `line`, and any conflicts recorded since the last call. Returns the
    /// notifications to show now (usually none).
    pub fn tick(
        &mut self,
        now: i64,
        status: OverallStatus,
        line: &str,
        new_conflicts: Vec<ConflictRecord>,
    ) -> Vec<Notification> {
        let mut out = Vec::new();

        if status == OverallStatus::SignedOut && self.prev != OverallStatus::SignedOut {
            out.push(Notification {
                kind: NoticeKind::SignedOut,
                summary: "Signed out of Proton".into(),
                body: "Syncing is stopped until you sign in again.".into(),
            });
        }

        if status == OverallStatus::WaitingForKeyring {
            let since = *self.keyring_since.get_or_insert(now);
            if !self.keyring_notified && now - since >= KEYRING_GRACE_SECS {
                out.push(Notification {
                    kind: NoticeKind::WaitingForKeyring,
                    summary: "Waiting for your keyring".into(),
                    body: "Syncing is paused because the keyring can't be reached. Unlock it, \
                           or start one in this session, and syncing resumes by itself."
                        .into(),
                });
                self.keyring_notified = true;
            }
        } else {
            self.keyring_since = None;
            self.keyring_notified = false;
        }

        if status == OverallStatus::Error {
            let since = *self.error_since.get_or_insert(now);
            let cooled = self
                .last_error_notice
                .is_none_or(|t| now - t >= ERROR_COOLDOWN_SECS);
            if !self.error_notified && now - since >= ERROR_GRACE_SECS && cooled {
                out.push(Notification {
                    kind: NoticeKind::Error,
                    summary: "Sync problem".into(),
                    body: line.to_string(),
                });
                self.error_notified = true;
                self.last_error_notice = Some(now);
            }
        } else {
            self.error_since = None;
            self.error_notified = false;
        }

        if !new_conflicts.is_empty() {
            if self.pending.is_empty() {
                self.pending_since = Some(now);
            }
            self.pending.extend(new_conflicts);
        }
        if let Some(since) = self.pending_since {
            let cooled = self
                .last_conflict_notice
                .is_none_or(|t| now - t >= CONFLICT_COOLDOWN_SECS);
            if now - since >= CONFLICT_BATCH_SECS && cooled {
                out.push(conflict_notice(&self.pending));
                self.pending.clear();
                self.pending_since = None;
                self.last_conflict_notice = Some(now);
            }
        }

        self.prev = status;
        out
    }
}

fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// One notification for a batch of conflicts.
fn conflict_notice(batch: &[ConflictRecord]) -> Notification {
    if let [c] = batch {
        let body = match c.resolution {
            ConflictResolution::KeptBoth => "Both versions were kept. See the Conflicts page.",
            ConflictResolution::KeptLocal => {
                "Your version was newer, so it replaced the one in Proton Drive."
            }
            ConflictResolution::KeptRemote => {
                "The Proton Drive version was newer, so it replaced your local one."
            }
            ConflictResolution::Skipped => {
                "Both sides changed and nothing was touched. It needs your decision."
            }
        };
        return Notification {
            kind: NoticeKind::Conflict,
            summary: format!("Conflict: {}", file_name(&c.rel)),
            body: body.into(),
        };
    }

    const SHOWN: usize = 3;
    let mut lines: Vec<String> = batch
        .iter()
        .take(SHOWN)
        .map(|c| file_name(&c.rel).to_string())
        .collect();
    if batch.len() > SHOWN {
        lines.push(format!("…and {} more", batch.len() - SHOWN));
    }
    let undecided = batch
        .iter()
        .filter(|c| c.resolution == ConflictResolution::Skipped)
        .count();
    if undecided > 0 {
        lines.push(format!("{undecided} need your decision."));
    }
    Notification {
        kind: NoticeKind::Conflict,
        summary: format!("{} files conflicted", batch.len()),
        body: lines.join("\n"),
    }
}

/// Minimum gap between two "background sync stopped" notices.
pub const GIVE_UP_COOLDOWN_SECS: i64 = 3600;

/// File in the state folder holding when the last such notice was shown.
pub const GIVE_UP_MARKER: &str = "tray-gave-up";

/// What the user is told when the tray supervisor gives up. It names the way back
/// (opening the app starts background sync again) and where the reason is.
pub fn background_sync_stopped_notice() -> Notification {
    Notification {
        kind: NoticeKind::BackgroundSyncStopped,
        summary: "Background sync stopped".into(),
        body: "The tray kept failing, so protondrive4linux stopped restarting it. \
               Open the app to try again; the sync log has the reason."
            .into(),
    }
}

/// Tell the user the tray supervisor gave up restarting the tray: the one moment
/// nothing else says that sync is no longer running. At most once per
/// [`GIVE_UP_COOLDOWN_SECS`] (so reopening the app and failing again doesn't
/// nag), and only if notifications are `enabled`. `deliver` shows the notice and
/// says whether it was shown; the time is recorded only when it was, so a
/// desktop with no notification daemon at that moment is tried again next time.
/// Returns whether a notice was shown.
pub fn announce_give_up(
    state_dir: &Path,
    enabled: bool,
    now: i64,
    deliver: impl FnOnce(&Notification) -> bool,
) -> bool {
    if !enabled {
        return false;
    }
    let marker = state_dir.join(GIVE_UP_MARKER);
    let last: Option<i64> = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    // Quiet only while the last notice is recent; one stamped in the future (a
    // clock that went backwards) must not silence this for hours.
    if last.is_some_and(|t| (0..GIVE_UP_COOLDOWN_SECS).contains(&(now - t))) {
        return false;
    }
    if !deliver(&background_sync_stopped_notice()) {
        return false;
    }
    let _ = std::fs::write(&marker, now.to_string());
    true
}

/// Run `f` on its own thread and wait at most `timeout` for it; `false` if it
/// said so or didn't finish in time (the thread is left to finish on its own).
fn run_within(timeout: Duration, f: impl FnOnce() -> bool + Send + 'static) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout).unwrap_or(false)
}

/// [`send`], but waiting for it (up to `timeout`) and saying whether the notice
/// was handed over - for a process that is about to exit, where [`send_async`]'s
/// detached thread would be killed before it could send anything.
pub fn send_within(n: &Notification, timeout: Duration) -> bool {
    let n = n.clone();
    run_within(timeout, move || send(&n).is_ok())
}

/// Show `n` through the desktop's notification daemon.
pub fn send(n: &Notification) -> zbus::Result<u32> {
    send_via(&zbus::blocking::Connection::session()?, n)
}

/// [`send`] over an explicit bus connection (so a test can use a private bus).
fn send_via(conn: &zbus::blocking::Connection, n: &Notification) -> zbus::Result<u32> {
    use zbus::zvariant::Value;

    let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
    // Normal urgency (1): none of these is an emergency, and critical ones
    // stay on screen until dismissed.
    hints.insert("urgency", Value::U8(1));
    hints.insert("desktop-entry", Value::from("protondrive4linux-gui"));
    let reply = conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "protondrive4linux",
            0u32,                // replaces_id: 0 = a new notification
            "protondrive4linux", // icon theme name (installed with the package)
            n.summary.as_str(),
            n.body.as_str(),
            Vec::<&str>::new(), // actions
            hints,
            -1i32, // expire_timeout: the server's default
        ),
    )?;
    reply.body().deserialize::<u32>()
}

/// [`send`] on its own thread, ignoring failure: a missing or slow
/// notification daemon must never stall the tray's loop or a window frame, and
/// there is nobody to tell if it's absent.
pub fn send_async(n: Notification) {
    std::thread::spawn(move || {
        let _ = send(&n);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a notification daemon receives from `Notify`.
    #[derive(Clone, Debug, Default, PartialEq)]
    struct Received {
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        urgency: Option<u8>,
        desktop_entry: Option<String>,
        expire_timeout: i32,
    }

    struct MockDaemon(std::sync::Arc<std::sync::Mutex<Vec<Received>>>);

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl MockDaemon {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app_name: String,
            replaces_id: u32,
            app_icon: String,
            summary: String,
            body: String,
            actions: Vec<String>,
            hints: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            let urgency = hints
                .get("urgency")
                .and_then(|v| u8::try_from(v.clone()).ok());
            let desktop_entry = hints
                .get("desktop-entry")
                .and_then(|v| String::try_from(v.clone()).ok());
            let mut got = self.0.lock().unwrap();
            got.push(Received {
                app_name,
                replaces_id,
                app_icon,
                summary,
                body,
                actions,
                urgency,
                desktop_entry,
                expire_timeout,
            });
            got.len() as u32
        }
    }

    use crate::testbus::PrivateBus;

    #[test]
    fn notify_call_reaches_a_real_notification_daemon_intact() {
        // `send_async` swallows every error, so a malformed `Notify` call would
        // silently show nothing, forever. Prove the wire format end to end:
        // a private bus, with a mock org.freedesktop.Notifications on it.
        let Some(bus) = PrivateBus::start() else {
            eprintln!("skipping: no dbus-daemon available");
            return;
        };
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let _daemon = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .serve_at(
                "/org/freedesktop/Notifications",
                MockDaemon(received.clone()),
            )
            .unwrap()
            .build()
            .unwrap();

        let client = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let note = Notification {
            kind: NoticeKind::Conflict,
            summary: "Conflict: notes.txt".into(),
            body: "Both versions were kept.\nSee the Conflicts page.".into(),
        };
        let id = send_via(&client, &note).expect("Notify must succeed");
        assert_eq!(id, 1, "the id the daemon returned comes back");

        let got = received.lock().unwrap().clone();
        assert_eq!(
            got,
            [Received {
                app_name: "protondrive4linux".into(),
                replaces_id: 0,
                app_icon: "protondrive4linux".into(),
                summary: "Conflict: notes.txt".into(),
                body: "Both versions were kept.\nSee the Conflicts page.".into(),
                actions: vec![],
                urgency: Some(1),
                desktop_entry: Some("protondrive4linux-gui".into()),
                expire_timeout: -1,
            }]
        );
    }

    #[test]
    fn sending_with_no_notification_daemon_is_an_error_not_a_hang() {
        let Some(bus) = PrivateBus::start() else {
            eprintln!("skipping: no dbus-daemon available");
            return;
        };
        let client = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let note = Notification {
            kind: NoticeKind::Error,
            summary: "x".into(),
            body: "y".into(),
        };
        assert!(send_via(&client, &note).is_err());
    }

    fn conflict(rel: &str, resolution: ConflictResolution) -> ConflictRecord {
        ConflictRecord {
            id: 1,
            ts: 0,
            pair: "docs".into(),
            rel: rel.into(),
            resolution,
            copy_rel: None,
            dismissed: false,
        }
    }

    fn kinds(v: &[Notification]) -> Vec<NoticeKind> {
        v.iter().map(|n| n.kind).collect()
    }

    #[test]
    fn nothing_to_say_when_everything_is_fine() {
        let mut n = Notifier::new(OverallStatus::Synced);
        for t in 0..300 {
            assert!(n.tick(t, OverallStatus::Synced, "ok", vec![]).is_empty());
            assert!(n.tick(t, OverallStatus::Syncing, "busy", vec![]).is_empty());
        }
    }

    #[test]
    fn signed_out_notifies_on_the_transition_only() {
        let mut n = Notifier::new(OverallStatus::Synced);
        let out = n.tick(0, OverallStatus::SignedOut, "x", vec![]);
        assert_eq!(kinds(&out), [NoticeKind::SignedOut]);
        // Staying signed out is not news.
        for t in 1..100 {
            assert!(n.tick(t, OverallStatus::SignedOut, "x", vec![]).is_empty());
        }
        // Signing back in and out again is.
        assert!(n.tick(100, OverallStatus::Synced, "x", vec![]).is_empty());
        assert_eq!(
            kinds(&n.tick(101, OverallStatus::SignedOut, "x", vec![])),
            [NoticeKind::SignedOut]
        );
    }

    #[test]
    fn a_keyring_that_stays_out_of_reach_notifies_once_after_the_grace_period() {
        let mut n = Notifier::new(OverallStatus::Synced);
        // The usual login: the keyring is unlocked within moments. Silence.
        for t in 0..KEYRING_GRACE_SECS {
            assert!(
                n.tick(t, OverallStatus::WaitingForKeyring, "x", vec![])
                    .is_empty(),
                "t={t}"
            );
        }
        let out = n.tick(
            KEYRING_GRACE_SECS,
            OverallStatus::WaitingForKeyring,
            "x",
            vec![],
        );
        assert_eq!(kinds(&out), [NoticeKind::WaitingForKeyring]);
        assert_eq!(out[0].summary, "Waiting for your keyring");
        assert!(
            !out[0].body.to_lowercase().contains("sign in"),
            "it must not send the user to sign in: {}",
            out[0].body
        );
        // The same episode never repeats, however long it lasts.
        for t in (KEYRING_GRACE_SECS + 1)..(KEYRING_GRACE_SECS + 5000) {
            assert!(n
                .tick(t, OverallStatus::WaitingForKeyring, "x", vec![])
                .is_empty());
        }
    }

    #[test]
    fn a_keyring_unlocked_in_time_never_notifies_and_the_next_wait_starts_afresh() {
        let mut n = Notifier::new(OverallStatus::Synced);
        for t in 0..KEYRING_GRACE_SECS {
            assert!(n
                .tick(t, OverallStatus::WaitingForKeyring, "x", vec![])
                .is_empty());
        }
        assert!(n
            .tick(KEYRING_GRACE_SECS, OverallStatus::Synced, "ok", vec![])
            .is_empty());
        // Out of reach again: the clock restarts from here, not from before.
        let again = KEYRING_GRACE_SECS + 10;
        assert!(n
            .tick(again, OverallStatus::WaitingForKeyring, "x", vec![])
            .is_empty());
        assert!(n
            .tick(
                again + KEYRING_GRACE_SECS - 1,
                OverallStatus::WaitingForKeyring,
                "x",
                vec![]
            )
            .is_empty());
        assert_eq!(
            kinds(&n.tick(
                again + KEYRING_GRACE_SECS,
                OverallStatus::WaitingForKeyring,
                "x",
                vec![]
            )),
            [NoticeKind::WaitingForKeyring]
        );
    }

    #[test]
    fn waiting_for_the_keyring_is_not_announced_as_a_sign_out() {
        // Whatever the keyring is doing, a plain waiting state never produces
        // the "Signed out of Proton" notice, immediately or later.
        let mut n = Notifier::new(OverallStatus::Synced);
        for t in 0..(2 * KEYRING_GRACE_SECS) {
            let out = n.tick(t, OverallStatus::WaitingForKeyring, "x", vec![]);
            assert!(!kinds(&out).contains(&NoticeKind::SignedOut), "t={t}");
        }
        // But if the keyring answers and the session turns out to be gone, that
        // is a sign-out and says so.
        assert_eq!(
            kinds(&n.tick(
                2 * KEYRING_GRACE_SECS,
                OverallStatus::SignedOut,
                "x",
                vec![]
            )),
            [NoticeKind::SignedOut]
        );
    }

    #[test]
    fn already_signed_out_at_startup_is_not_announced() {
        // The state at creation is the baseline, not an event.
        let mut n = Notifier::new(OverallStatus::SignedOut);
        assert!(n.tick(0, OverallStatus::SignedOut, "x", vec![]).is_empty());
    }

    #[test]
    fn a_brief_problem_never_notifies() {
        let mut n = Notifier::new(OverallStatus::Synced);
        // A rate-limit blip: failing for under the grace period, then fine.
        for t in 0..ERROR_GRACE_SECS {
            assert!(n.tick(t, OverallStatus::Error, "blip", vec![]).is_empty());
        }
        assert!(n
            .tick(ERROR_GRACE_SECS, OverallStatus::Synced, "ok", vec![])
            .is_empty());
        // ...and the next problem starts its grace period afresh.
        assert!(n
            .tick(ERROR_GRACE_SECS + 1, OverallStatus::Error, "blip", vec![])
            .is_empty());
    }

    #[test]
    fn a_persistent_problem_notifies_once_with_the_status_line() {
        let mut n = Notifier::new(OverallStatus::Synced);
        assert!(n
            .tick(0, OverallStatus::Error, "docs: boom", vec![])
            .is_empty());
        let out = n.tick(ERROR_GRACE_SECS, OverallStatus::Error, "docs: boom", vec![]);
        assert_eq!(
            out,
            [Notification {
                kind: NoticeKind::Error,
                summary: "Sync problem".into(),
                body: "docs: boom".into(),
            }]
        );
        // The same episode never repeats, however long it lasts.
        for t in (ERROR_GRACE_SECS + 1)..(ERROR_GRACE_SECS + 5000) {
            assert!(n
                .tick(t, OverallStatus::Error, "docs: boom", vec![])
                .is_empty());
        }
    }

    #[test]
    fn a_flapping_problem_is_rate_limited_across_episodes() {
        let mut n = Notifier::new(OverallStatus::Synced);
        n.tick(0, OverallStatus::Error, "x", vec![]);
        assert_eq!(
            n.tick(ERROR_GRACE_SECS, OverallStatus::Error, "x", vec![])
                .len(),
            1
        );
        // It clears, then fails again - lasting past the grace period, but
        // still inside the cooldown since the last notice.
        let t0 = ERROR_GRACE_SECS + 10;
        n.tick(t0, OverallStatus::Synced, "ok", vec![]);
        n.tick(t0 + 1, OverallStatus::Error, "x", vec![]);
        assert!(n
            .tick(t0 + 1 + ERROR_GRACE_SECS, OverallStatus::Error, "x", vec![])
            .is_empty());
        // Once the cooldown has passed, a still-ongoing episode is announced.
        let late = ERROR_GRACE_SECS + ERROR_COOLDOWN_SECS;
        assert_eq!(
            n.tick(late, OverallStatus::Error, "x", vec![]).len(),
            1,
            "past the cooldown the ongoing problem is finally announced"
        );
    }

    #[test]
    fn a_single_conflict_is_announced_after_the_batch_window() {
        let mut n = Notifier::new(OverallStatus::Synced);
        let c = vec![conflict("sub/notes.txt", ConflictResolution::KeptBoth)];
        assert!(n.tick(10, OverallStatus::Synced, "ok", c).is_empty());
        assert!(n
            .tick(
                10 + CONFLICT_BATCH_SECS - 1,
                OverallStatus::Synced,
                "ok",
                vec![]
            )
            .is_empty());
        let out = n.tick(
            10 + CONFLICT_BATCH_SECS,
            OverallStatus::Synced,
            "ok",
            vec![],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, NoticeKind::Conflict);
        assert_eq!(out[0].summary, "Conflict: notes.txt");
        assert_eq!(
            out[0].body,
            "Both versions were kept. See the Conflicts page."
        );
        // Announced once, then quiet.
        assert!(n.tick(100, OverallStatus::Synced, "ok", vec![]).is_empty());
    }

    #[test]
    fn each_resolution_has_its_own_wording() {
        let body = |r| conflict_notice(&[conflict("a.txt", r)]).body;
        assert!(body(ConflictResolution::KeptLocal).contains("Your version was newer"));
        assert!(body(ConflictResolution::KeptRemote).contains("Proton Drive version was newer"));
        assert!(body(ConflictResolution::Skipped).contains("needs your decision"));
    }

    #[test]
    fn conflicts_arriving_together_are_one_notification() {
        let mut n = Notifier::new(OverallStatus::Synced);
        n.tick(
            0,
            OverallStatus::Synced,
            "ok",
            vec![conflict("a.txt", ConflictResolution::KeptBoth)],
        );
        n.tick(
            1,
            OverallStatus::Synced,
            "ok",
            vec![
                conflict("b.txt", ConflictResolution::Skipped),
                conflict("c.txt", ConflictResolution::KeptLocal),
                conflict("d/e.txt", ConflictResolution::Skipped),
                conflict("f.txt", ConflictResolution::KeptBoth),
            ],
        );
        let out = n.tick(CONFLICT_BATCH_SECS, OverallStatus::Synced, "ok", vec![]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].summary, "5 files conflicted");
        assert_eq!(
            out[0].body,
            "a.txt\nb.txt\nc.txt\n…and 2 more\n2 need your decision."
        );
    }

    #[test]
    fn conflict_notices_are_spaced_out_and_nothing_is_lost() {
        let mut n = Notifier::new(OverallStatus::Synced);
        n.tick(
            0,
            OverallStatus::Synced,
            "ok",
            vec![conflict("a.txt", ConflictResolution::KeptBoth)],
        );
        assert_eq!(
            n.tick(CONFLICT_BATCH_SECS, OverallStatus::Synced, "ok", vec![])
                .len(),
            1
        );
        // Another conflict right after: held back until the cooldown ends...
        let t = CONFLICT_BATCH_SECS + 1;
        n.tick(
            t,
            OverallStatus::Synced,
            "ok",
            vec![conflict("b.txt", ConflictResolution::KeptBoth)],
        );
        assert!(n
            .tick(t + CONFLICT_BATCH_SECS, OverallStatus::Synced, "ok", vec![])
            .is_empty());
        // ...then announced, not dropped.
        let out = n.tick(
            CONFLICT_BATCH_SECS + CONFLICT_COOLDOWN_SECS,
            OverallStatus::Synced,
            "ok",
            vec![],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].summary, "Conflict: b.txt");
    }

    // --- the supervisor giving up ----------------------------------------------

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "protondrive4linux-notify-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_give_up_notice_says_what_stopped_and_how_to_get_it_back() {
        let n = background_sync_stopped_notice();
        assert_eq!(n.kind, NoticeKind::BackgroundSyncStopped);
        assert_eq!(n.summary, "Background sync stopped");
        assert!(n.body.contains("Open the app to try again"), "{}", n.body);
        assert!(n.body.contains("sync log"), "{}", n.body);
    }

    #[test]
    fn a_give_up_is_announced_once_an_hour_and_only_when_notifications_are_on() {
        let dir = temp_dir("giveup");
        let shown = std::cell::Cell::new(0);
        let deliver = |_: &Notification| {
            shown.set(shown.get() + 1);
            true
        };

        // Switched off: nothing is shown and nothing is recorded.
        assert!(!announce_give_up(&dir, false, 1_000, deliver));
        assert_eq!(shown.get(), 0);
        assert!(!dir.join(GIVE_UP_MARKER).exists());

        assert!(announce_give_up(&dir, true, 1_000, deliver));
        assert_eq!(shown.get(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join(GIVE_UP_MARKER)).unwrap(),
            "1000"
        );

        // Failing again soon after (the app was reopened and the tray died
        // again) stays quiet, right up to the end of the hour.
        assert!(!announce_give_up(&dir, true, 1_000 + 60, deliver));
        assert!(!announce_give_up(
            &dir,
            true,
            1_000 + GIVE_UP_COOLDOWN_SECS - 1,
            deliver
        ));
        assert_eq!(shown.get(), 1);

        // After the hour it speaks again, and the clock restarts from there.
        assert!(announce_give_up(
            &dir,
            true,
            1_000 + GIVE_UP_COOLDOWN_SECS,
            deliver
        ));
        assert_eq!(shown.get(), 2);
        assert!(!announce_give_up(
            &dir,
            true,
            1_000 + GIVE_UP_COOLDOWN_SECS + 1,
            deliver
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_notice_that_could_not_be_shown_is_not_counted_so_the_next_give_up_tries_again() {
        let dir = temp_dir("giveup-undelivered");
        assert!(!announce_give_up(&dir, true, 5_000, |_| false));
        assert!(
            !dir.join(GIVE_UP_MARKER).exists(),
            "nothing was shown, nothing recorded"
        );
        assert!(announce_give_up(&dir, true, 5_001, |_| true));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_marker_from_the_future_or_gibberish_does_not_silence_it() {
        let dir = temp_dir("giveup-marker");
        // A clock that went backwards since the last notice.
        std::fs::write(dir.join(GIVE_UP_MARKER), "999999999").unwrap();
        assert!(announce_give_up(&dir, true, 1_000, |_| true));
        std::fs::write(dir.join(GIVE_UP_MARKER), "not a time").unwrap();
        assert!(announce_give_up(&dir, true, 1_000, |_| true));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn waiting_for_a_notice_is_bounded_and_reports_what_happened() {
        assert!(run_within(Duration::from_secs(5), || true));
        assert!(!run_within(Duration::from_secs(5), || false));
        // A notification daemon that never answers must not hold up an exiting
        // supervisor.
        let started = std::time::Instant::now();
        let slow = run_within(Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(3));
            true
        });
        assert!(!slow);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
