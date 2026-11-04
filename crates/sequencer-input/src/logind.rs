//! What logind says of the user's session: whether its screen is locked.
//!
//! The device grab needs exactly this. While the screen is locked, what is typed is a
//! password, and a profile must neither change it nor type into it; so the grab passes every
//! key on untouched until the session unlocks ([`crate::linux::grab`]). logind's `LockedHint`
//! is what the screen lockers set — KDE's, GNOME's — and asking logind rather than a
//! desktop keeps the answer the same under any of them.
//!
//! The session asked about is the user's display session, the one their screen belongs to:
//! a process started through sudo may sit in a session of its own, depending on how the
//! distribution sets sudo up, and that one never locks.

use std::time::Duration;

use crate::dbus::{Connection, Value};

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";
const MANAGER_INTERFACE: &str = "org.freedesktop.login1.Manager";
const USER_INTERFACE: &str = "org.freedesktop.login1.User";
const SESSION_INTERFACE: &str = "org.freedesktop.login1.Session";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// logind's name for "the session of whoever asks", when nothing better is known.
const ASKERS_SESSION: &str = "/org/freedesktop/login1/session/auto";
/// How long logind gets to answer one question.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// The user's display session as logind names it: the user's own `Display`, else the
/// session this process is in, else logind's "whoever asks".
pub(crate) fn display_session(bus: &mut Connection, uid: u32) -> String {
    let user = bus
        .call(
            LOGIND,
            MANAGER,
            MANAGER_INTERFACE,
            "GetUser",
            vec![Value::U32(uid)],
            CALL_TIMEOUT,
        )
        .ok()
        .and_then(|reply| object_path(reply.first()));
    let display = user.and_then(|user| {
        let value = property(bus, &user, USER_INTERFACE, "Display")?;
        match value.as_struct()? {
            [_, Value::ObjectPath(path)] if path != "/" => Some(path.clone()),
            _ => None,
        }
    });
    display
        .or_else(|| {
            bus.call(
                LOGIND,
                MANAGER,
                MANAGER_INTERFACE,
                "GetSessionByPID",
                vec![Value::U32(std::process::id())],
                CALL_TIMEOUT,
            )
            .ok()
            .and_then(|reply| object_path(reply.first()))
        })
        .unwrap_or_else(|| ASKERS_SESSION.to_owned())
}

/// Whether `session`'s screen is locked, or `None` when logind won't say.
pub(crate) fn locked(bus: &mut Connection, session: &str) -> Option<bool> {
    match property(bus, session, SESSION_INTERFACE, "LockedHint")? {
        Value::Bool(locked) => Some(locked),
        _ => None,
    }
}

/// One property of one logind object, unwrapped from its variant.
fn property(bus: &mut Connection, path: &str, interface: &str, name: &str) -> Option<Value> {
    let reply = bus
        .call(
            LOGIND,
            path,
            PROPERTIES,
            "Get",
            vec![Value::str(interface), Value::str(name)],
            CALL_TIMEOUT,
        )
        .ok()?;
    match reply.into_iter().next()? {
        Value::Variant(value) => Some(*value),
        _ => None,
    }
}

fn object_path(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::ObjectPath(path) => Some(path.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::Kind;
    use crate::dbus::test_bus::TestBus;

    const USER: &str = "/org/freedesktop/login1/user/_1000";
    const SESSION: &str = "/org/freedesktop/login1/session/_32";

    /// Plays logind while `ask` runs: the user's display session is [`SESSION`], whose
    /// `LockedHint` is `locked`; a user without a display has `"/"` there, as logind says.
    fn serving<T: Send>(
        bus: &TestBus,
        display: bool,
        locked: bool,
        ask: impl FnOnce() -> T + Send,
    ) -> T {
        let mut logind = bus.connect();
        logind.request_name(LOGIND).unwrap();
        std::thread::scope(|scope| {
            let asking = scope.spawn(ask);
            while !asking.is_finished() {
                let Ok(Some(call)) = logind.next_message(Some(Duration::from_millis(50))) else {
                    continue;
                };
                if call.kind != Kind::MethodCall {
                    continue;
                }
                let answer = match (
                    call.path.as_deref(),
                    call.member.as_deref(),
                    call.body.get(1).and_then(Value::as_str),
                ) {
                    (Some(MANAGER), Some("GetUser"), _) => {
                        Some(vec![Value::ObjectPath(USER.into())])
                    }
                    (Some(MANAGER), Some("GetSessionByPID"), _) => {
                        Some(vec![Value::ObjectPath(SESSION.into())])
                    }
                    (Some(USER), Some("Get"), Some("Display")) => {
                        let path = if display { SESSION } else { "/" };
                        Some(vec![Value::Variant(Box::new(Value::Struct(vec![
                            Value::str("32"),
                            Value::ObjectPath(path.into()),
                        ])))])
                    }
                    (Some(SESSION), Some("Get"), Some("LockedHint")) => {
                        Some(vec![Value::Variant(Box::new(Value::Bool(locked)))])
                    }
                    _ => None,
                };
                let _ = match answer {
                    Some(body) => logind.reply(&call, body),
                    None => logind.decline(&call),
                };
            }
            asking.join().unwrap()
        })
    }

    /// The session asked about is the user's display session, and its lock is read as logind
    /// says it, locked or not.
    #[test]
    fn the_display_sessions_lock_is_what_logind_says() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        for locked_now in [true, false] {
            let read = serving(&bus, true, locked_now, || {
                let mut asking = bus.connect();
                let session = display_session(&mut asking, 1000);
                (session.clone(), locked(&mut asking, &session))
            });
            assert_eq!(read, (SESSION.to_owned(), Some(locked_now)));
        }
    }

    /// A user with no display session falls back to the session this process is in.
    #[test]
    fn without_a_display_the_askers_own_session_is_asked() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let session = serving(&bus, false, false, || {
            display_session(&mut bus.connect(), 1000)
        });
        assert_eq!(session, SESSION);
    }

    /// No logind on the bus: no lock can be read, and the session is logind's own word for
    /// "whoever asks".
    #[test]
    fn without_logind_nothing_is_known() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut asking = bus.connect();
        assert_eq!(display_session(&mut asking, 1000), ASKERS_SESSION);
        assert_eq!(locked(&mut asking, ASKERS_SESSION), None);
    }
}
