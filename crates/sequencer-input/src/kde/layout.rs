//! The active keyboard layout, from KDE's keyboard service.
//!
//! In a Wayland session KWin itself serves `org.kde.keyboard` at `/Layouts`: `getLayout`
//! is the active layout's index into `getLayoutsList`, whose entries are (short name,
//! display name, long name) — the short name being the XKB layout name a `kb_lang`
//! pattern matches (`us`, `il`). It signals `layoutChanged` with the new index on every
//! switch, and `layoutListChanged` when the configured list changes, so this listens
//! rather than polls.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::lock;
use crate::dbus::{Closer, Connection, Value};

const SERVICE: &str = "org.kde.keyboard";
const PATH: &str = "/Layouts";
const INTERFACE: &str = "org.kde.KeyboardLayouts";

/// KWin answers from its own event loop, which is never long busy; beyond this it is
/// stuck, and the layout counts as unknown.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// What the listener last heard.
#[derive(Debug, Default)]
struct Layouts {
    /// Each configured layout's short name, in KDE's order.
    names: Vec<String>,
    /// The active one's position in `names`.
    active: u32,
}

/// Names the active keyboard layout, as KDE's keyboard service reports it.
#[derive(Debug)]
pub struct KdeLayout {
    layouts: Arc<Mutex<Layouts>>,
    closer: Closer,
    listener: Option<JoinHandle<()>>,
}

impl KdeLayout {
    /// Starts listening to KDE's keyboard service on the session bus, or `None` when the
    /// bus or the service is not there.
    #[must_use]
    pub fn open() -> Option<Self> {
        Self::open_on(Connection::session().ok()?)
    }

    /// Whether KDE's keyboard service is on the session bus right now — what [`open`]
    /// would find, asked without subscribing to anything.
    ///
    /// [`open`]: Self::open
    #[must_use]
    pub fn available() -> bool {
        Connection::session()
            .and_then(|mut bus| bus.name_owner(SERVICE))
            .is_ok_and(|owner| owner.is_some())
    }

    fn open_on(mut connection: Connection) -> Option<Self> {
        connection.name_owner(SERVICE).ok()??;
        // Subscribed before the first read, so a switch in between still arrives.
        connection
            .add_match(&format!(
                "type='signal',sender='{SERVICE}',path='{PATH}',interface='{INTERFACE}'"
            ))
            .ok()?;
        let layouts = Arc::new(Mutex::new(fetch(&mut connection).ok()?));
        let closer = connection.closer().ok()?;
        let shared = Arc::clone(&layouts);
        let listener = std::thread::Builder::new()
            .name("sequencer-kde-layout".into())
            .spawn(move || listen(connection, &shared))
            .ok()?;
        Some(Self {
            layouts,
            closer,
            listener: Some(listener),
        })
    }

    /// The active layout's short name (`us`, `il`), or `None` while that is unknown.
    #[must_use]
    pub fn current(&self) -> Option<String> {
        let layouts = lock(&self.layouts);
        let index = usize::try_from(layouts.active).ok()?;
        layouts
            .names
            .get(index)
            .filter(|name| !name.is_empty())
            .cloned()
    }
}

impl Drop for KdeLayout {
    fn drop(&mut self) {
        self.closer.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

/// The configured layouts and the active one, asked afresh.
fn fetch(connection: &mut Connection) -> Result<Layouts, crate::dbus::Error> {
    let list = connection.call(
        SERVICE,
        PATH,
        INTERFACE,
        "getLayoutsList",
        Vec::new(),
        CALL_TIMEOUT,
    )?;
    let names = list
        .first()
        .and_then(Value::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| entry.as_struct()?.first()?.as_str().map(str::to_owned))
        .collect();
    let active = connection.call(
        SERVICE,
        PATH,
        INTERFACE,
        "getLayout",
        Vec::new(),
        CALL_TIMEOUT,
    )?;
    Ok(Layouts {
        names,
        active: active.first().and_then(Value::as_u32).unwrap_or(u32::MAX),
    })
}

/// Keeps `layouts` current until the connection closes.
fn listen(mut connection: Connection, layouts: &Mutex<Layouts>) {
    loop {
        let message = match connection.next_message(None) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(_) => return,
        };
        if message.is_signal(INTERFACE, "layoutChanged") {
            if let Some(active) = message.body.first().and_then(Value::as_u32) {
                lock(layouts).active = active;
            }
        } else if message.is_signal(INTERFACE, "layoutListChanged") {
            // Every position may have moved, the active one included.
            if let Ok(fresh) = fetch(&mut connection) {
                *lock(layouts) = fresh;
            }
        } else {
            let _ = connection.decline(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::dbus::test_bus::TestBus;
    use crate::dbus::{Kind, Message};

    /// A stand-in for KWin's keyboard service: answers the two calls from `state`, and
    /// emits what the test sends it.
    struct FakeKeyboard {
        connection: Connection,
        names: Vec<&'static str>,
        active: u32,
    }

    impl FakeKeyboard {
        fn new(bus: &TestBus, names: Vec<&'static str>, active: u32) -> Self {
            let mut connection = bus.connect();
            connection.request_name(SERVICE).unwrap();
            Self {
                connection,
                names,
                active,
            }
        }

        /// Answers one call, if one arrives soon.
        fn serve_one(&mut self) -> Option<Message> {
            let message = self
                .connection
                .next_message(Some(Duration::from_millis(50)))
                .unwrap()?;
            if message.kind != Kind::MethodCall {
                return None;
            }
            let reply = match message.member.as_deref() {
                Some("getLayout") => vec![Value::U32(self.active)],
                Some("getLayoutsList") => vec![Value::Array(
                    "(sss)".into(),
                    self.names
                        .iter()
                        .map(|name| {
                            Value::Struct(vec![
                                Value::str(name),
                                Value::str(""),
                                Value::str("long name"),
                            ])
                        })
                        .collect(),
                )],
                _ => {
                    self.connection.decline(&message).unwrap();
                    return Some(message);
                }
            };
            self.connection.reply(&message, reply).unwrap();
            Some(message)
        }
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The reported shape of a real session: `us` then `il`, `us` active, then a switch.
    #[test]
    fn the_active_layout_follows_kdes_switches() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut keyboard = FakeKeyboard::new(&bus, vec!["us", "il"], 0);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| KdeLayout::open_on(bus.connect()));
            while !reader.is_finished() {
                keyboard.serve_one();
            }
            let layout = reader.join().unwrap().expect("the service answers");
            assert_eq!(layout.current().as_deref(), Some("us"));

            keyboard
                .connection
                .emit(PATH, INTERFACE, "layoutChanged", vec![Value::U32(1)])
                .unwrap();
            eventually("the switch to il is heard", || {
                layout.current().as_deref() == Some("il")
            });

            // A new list: the listener asks again, and what it asks is answered here.
            keyboard.names = vec!["de", "us"];
            keyboard.active = 0;
            keyboard
                .connection
                .emit(PATH, INTERFACE, "layoutListChanged", Vec::new())
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while layout.current().as_deref() != Some("de") {
                assert!(Instant::now() < deadline, "the new list was never read");
                keyboard.serve_one();
            }
        });
    }

    #[test]
    fn without_the_service_there_is_nothing_to_open() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        assert!(KdeLayout::open_on(bus.connect()).is_none());
    }

    /// An index past the list, or an empty name, is not a layout.
    #[test]
    fn an_unknown_layout_reads_as_none() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut keyboard = FakeKeyboard::new(&bus, vec![""], 3);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| KdeLayout::open_on(bus.connect()));
            while !reader.is_finished() {
                keyboard.serve_one();
            }
            let layout = reader.join().unwrap().expect("the service answers");
            assert_eq!(layout.current(), None);
        });
    }
}
