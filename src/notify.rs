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
//! - a signed-out session notifies on the transition into it, not while it lasts.

use std::collections::HashMap;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Conflict,
    Error,
    SignedOut,
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

    /// A private session bus, killed on drop.
    struct PrivateBus {
        child: std::process::Child,
        address: String,
    }
    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
    impl PrivateBus {
        /// `None` when there's no `dbus-daemon` to start (the test then skips).
        fn start() -> Option<Self> {
            use std::io::BufRead;
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take()?)
                .read_line(&mut line)
                .ok()?;
            let address = line.trim().to_string();
            (!address.is_empty()).then_some(PrivateBus { child, address })
        }
    }

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
}
