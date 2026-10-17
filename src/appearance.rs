//! Which theme the desktop wants: light or dark (M5-001's "System" setting).
//!
//! winit doesn't report the system theme on Wayland (`egui` sees `None`, so
//! "follow the system" would quietly mean "always dark"), so ask the desktop
//! directly: the `org.freedesktop.appearance` / `color-scheme` setting of the
//! settings portal, which GNOME, KDE, COSMIC and the rest all provide.
//! Read-only; nothing is changed on the system.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use zbus::zvariant::Value;

/// The portal's `color-scheme` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    NoPreference,
    Dark,
    Light,
}

impl ColorScheme {
    fn from_portal(n: u32) -> Self {
        match n {
            1 => ColorScheme::Dark,
            2 => ColorScheme::Light,
            // 0 is "no preference"; unknown values are treated the same.
            _ => ColorScheme::NoPreference,
        }
    }
}

/// The `u32` inside however many layers of variant the portal wrapped it in
/// (the older `Read` method double-wraps it, `ReadOne` wraps it once).
fn unwrap_u32(v: &Value<'_>) -> Option<u32> {
    match v {
        Value::Value(inner) => unwrap_u32(inner),
        Value::U32(n) => Some(*n),
        _ => None,
    }
}

/// Ask the settings portal for the desktop's color scheme over `conn`.
pub fn read_color_scheme(conn: &zbus::blocking::Connection) -> zbus::Result<ColorScheme> {
    let args = ("org.freedesktop.appearance", "color-scheme");
    let call = |method: &str| {
        conn.call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            method,
            &args,
        )
    };
    // `ReadOne` is the current method; `Read` is deprecated but the only one
    // older portals have.
    let reply = call("ReadOne").or_else(|_| call("Read"))?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize()?;
    Ok(unwrap_u32(&value)
        .map(ColorScheme::from_portal)
        .unwrap_or(ColorScheme::NoPreference))
}

const UNKNOWN: u8 = 0;
const DARK: u8 = 1;
const LIGHT: u8 = 2;

/// Watches the desktop's color scheme on a background thread, so asking is a
/// plain atomic read - safe to do every frame. If the portal is missing (no
/// session bus, a minimal window manager), it just stays "unknown".
#[derive(Clone)]
pub struct SystemTheme {
    state: Arc<AtomicU8>,
}

impl SystemTheme {
    pub fn spawn() -> Self {
        let state = Arc::new(AtomicU8::new(UNKNOWN));
        let shared = state.clone();
        std::thread::spawn(move || {
            let mut conn = None;
            loop {
                if conn.is_none() {
                    conn = zbus::blocking::Connection::session().ok();
                }
                let now = match conn.as_ref().map(read_color_scheme) {
                    Some(Ok(ColorScheme::Dark)) => DARK,
                    Some(Ok(ColorScheme::Light)) => LIGHT,
                    Some(Ok(ColorScheme::NoPreference)) => UNKNOWN,
                    // The call failed: drop the connection and reconnect next
                    // round, in case the bus itself went away.
                    Some(Err(_)) => {
                        conn = None;
                        UNKNOWN
                    }
                    None => UNKNOWN,
                };
                shared.store(now, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(3));
            }
        });
        SystemTheme { state }
    }

    /// `Some(true)` for dark, `Some(false)` for light, `None` when the desktop
    /// has no stated preference (or couldn't be asked).
    pub fn prefers_dark(&self) -> Option<bool> {
        match self.state.load(Ordering::Relaxed) {
            DARK => Some(true),
            LIGHT => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_values_map_to_schemes() {
        assert_eq!(ColorScheme::from_portal(0), ColorScheme::NoPreference);
        assert_eq!(ColorScheme::from_portal(1), ColorScheme::Dark);
        assert_eq!(ColorScheme::from_portal(2), ColorScheme::Light);
        assert_eq!(ColorScheme::from_portal(99), ColorScheme::NoPreference);
    }

    #[test]
    fn the_value_is_found_through_any_number_of_variant_wrappers() {
        let bare = Value::U32(2);
        let once = Value::Value(Box::new(Value::U32(2)));
        let twice = Value::Value(Box::new(Value::Value(Box::new(Value::U32(2)))));
        assert_eq!(unwrap_u32(&bare), Some(2));
        assert_eq!(unwrap_u32(&once), Some(2));
        assert_eq!(unwrap_u32(&twice), Some(2));
        assert_eq!(unwrap_u32(&Value::Str("dark".into())), None);
    }

    use crate::testbus::PrivateBus;

    /// A settings portal answering `color-scheme` the way the real one does:
    /// `Read` double-wraps the value in variants, `ReadOne` wraps it once.
    struct MockPortal {
        scheme: u32,
        has_read_one: bool,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Settings")]
    impl MockPortal {
        fn read(&self, _ns: String, _key: String) -> zbus::zvariant::OwnedValue {
            zbus::zvariant::OwnedValue::try_from(Value::Value(Box::new(Value::U32(self.scheme))))
                .unwrap()
        }

        fn read_one(
            &self,
            _ns: String,
            _key: String,
        ) -> zbus::fdo::Result<zbus::zvariant::OwnedValue> {
            if !self.has_read_one {
                return Err(zbus::fdo::Error::UnknownMethod("ReadOne".into()));
            }
            Ok(zbus::zvariant::OwnedValue::try_from(Value::U32(self.scheme)).unwrap())
        }
    }

    fn scheme_from(bus: &PrivateBus, scheme: u32, has_read_one: bool) -> ColorScheme {
        let _portal = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                MockPortal {
                    scheme,
                    has_read_one,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        let client = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .unwrap();
        read_color_scheme(&client).expect("the portal answers")
    }

    #[test]
    fn reads_the_scheme_from_a_current_and_from_an_older_portal() {
        // Each portal needs its own bus: only one can own the well-known name.
        for (value, want) in [
            (0, ColorScheme::NoPreference),
            (1, ColorScheme::Dark),
            (2, ColorScheme::Light),
        ] {
            for has_read_one in [true, false] {
                let Some(bus) = PrivateBus::start() else {
                    eprintln!("skipping: no dbus-daemon available");
                    return;
                };
                assert_eq!(
                    scheme_from(&bus, value, has_read_one),
                    want,
                    "scheme {value}, ReadOne available: {has_read_one}"
                );
            }
        }
    }

    #[test]
    fn no_portal_is_an_error_not_a_guess() {
        let Some(bus) = PrivateBus::start() else {
            eprintln!("skipping: no dbus-daemon available");
            return;
        };
        let client = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .build()
            .unwrap();
        assert!(read_color_scheme(&client).is_err());
    }
}
