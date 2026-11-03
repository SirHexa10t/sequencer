//! The focused program, as KWin reports it to a script sequencer loads into it.
//!
//! KWin's D-Bus interface has no call that names the active window without the user
//! clicking one (`queryWindowInfo` is interactive), so this does what every tool asking
//! KWin does: it loads a small script ([`super::script`]), and the script reports each
//! focus change by calling back over D-Bus. KWin waits for an answer to each call, so the
//! listener answers every report.
//!
//! What it reports is the window's `resourceClass`: the `WM_CLASS` class for an X11
//! window — the name [`crate::x11::FocusWatcher`] reads — and the application id for a
//! Wayland one (`org.kde.konsole`, `firefox`), kept exactly as the app set it.
//!
//! The script is loaded for one process and unloaded when it is done. A process killed
//! outright leaves its script behind until KWin restarts; it only ever calls a name that
//! no longer exists, which costs nothing.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::lock;
use super::script::{FIRST_REPORT, Script, is_report, kwin_owner, report_call};
use crate::dbus::{Closer, Connection, Value};

/// The report the script makes on every focus change.
const ACTIVATED: &str = "activated";

/// The focus the script last reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Focused {
    /// The window's `resourceClass`; empty when KWin reports no active window.
    class: String,
    /// KWin's id for the window.
    window: String,
}

/// What the listener shares: the last report, and a signal for the first one.
#[derive(Debug, Default)]
struct Reports {
    last: Mutex<Option<Focused>>,
    arrived: Condvar,
}

/// Reads which program has focus, as KWin reports it.
#[derive(Debug)]
pub struct KWinFocus {
    reports: Arc<Reports>,
    /// Loaded until this is dropped; the reports arrive on another connection.
    script: Script,
    closer: Closer,
    listener: Option<JoinHandle<()>>,
}

impl KWinFocus {
    /// Loads the reporting script into KWin and starts listening, or `None` when there is
    /// no KWin on the session bus or it would not run the script.
    #[must_use]
    pub fn open() -> Option<Self> {
        let scripts = crate::dbus::runtime_dir().ok()?;
        Self::open_on(
            Connection::session().ok()?,
            Connection::session().ok()?,
            &scripts,
        )
    }

    /// Whether KWin is on the session bus right now — what [`open`] would find, asked
    /// without loading anything into it.
    ///
    /// [`open`]: Self::open
    #[must_use]
    pub fn available() -> bool {
        Connection::session()
            .ok()
            .is_some_and(|mut bus| kwin_owner(&mut bus).is_some())
    }

    fn open_on(mut control: Connection, listener: Connection, scripts: &Path) -> Option<Self> {
        let kwin = kwin_owner(&mut control)?;
        let text = script_text(listener.unique_name());
        let script = Script::load(control, scripts, "focus", &text)?;

        let reports = Arc::new(Reports::default());
        let closer = listener.closer().ok()?;
        let shared = Arc::clone(&reports);
        let listening = std::thread::Builder::new()
            .name("sequencer-kwin-focus".into())
            .spawn(move || listen(listener, &kwin, &shared))
            .ok()?;
        let mut focus = Self {
            reports,
            script,
            closer,
            listener: Some(listening),
        };
        // On failure, dropping `focus` unloads the script and stops the thread.
        focus.start().then_some(focus)
    }

    /// Runs the script and waits for its first report — through `start` when `run`
    /// reached another script.
    fn start(&mut self) -> bool {
        if !self.script.run() {
            return false;
        }
        let reported = self.first_report_within(FIRST_REPORT)
            || (self.script.start_all() && self.first_report_within(FIRST_REPORT));
        if reported {
            self.script.read_by_kwin();
        }
        reported
    }

    fn first_report_within(&self, wait: Duration) -> bool {
        let last = lock(&self.reports.last);
        let (last, _) = self
            .reports
            .arrived
            .wait_timeout_while(last, wait, |last| last.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.is_some()
    }

    /// The focused program's `resourceClass`, or `None` while no window has focus.
    #[must_use]
    pub fn focused_class(&self) -> Option<String> {
        lock(&self.reports.last)
            .as_ref()
            .map(|focused| focused.class.clone())
            .filter(|class| !class.is_empty())
    }

    /// KWin's id for the focused window, for telling one window from another.
    #[must_use]
    pub fn focused_window(&self) -> Option<String> {
        lock(&self.reports.last)
            .as_ref()
            .map(|focused| focused.window.clone())
            .filter(|window| !window.is_empty())
    }
}

impl Drop for KWinFocus {
    fn drop(&mut self) {
        // The script unloads as its own field drops; first, stop the thread it reports to.
        self.closer.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

/// The script, reporting to `owner`. Plasma 6's scripting API: `workspace.activeWindow`
/// and the `windowActivated` signal, whose window is `null` when nothing has focus.
/// `callDBus` turns JavaScript strings into D-Bus strings, and refuses `null`, hence
/// the empty strings.
fn script_text(owner: &str) -> String {
    format!(
        "// Loaded by sequencer to learn which window has focus: it reports each change to\n\
         // the process that loaded it, and is unloaded when that process is done.\n\
         function report(window) {{\n    \
             {call},\n        \
                 window ? String(window.resourceClass) : \"\",\n        \
                 window ? String(window.internalId) : \"\");\n\
         }}\n\
         workspace.windowActivated.connect(report);\n\
         report(workspace.activeWindow);\n",
        call = report_call(owner, ACTIVATED),
    )
}

/// Answers the script's reports until the connection closes.
fn listen(mut connection: Connection, kwin: &str, reports: &Reports) {
    loop {
        let message = match connection.next_message(None) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(_) => return,
        };
        if !is_report(&message, kwin, ACTIVATED) {
            let _ = connection.decline(&message);
            continue;
        }
        if let [Value::Str(class), Value::Str(window)] = message.body.as_slice() {
            *lock(&reports.last) = Some(Focused {
                class: class.clone(),
                window: window.clone(),
            });
            reports.arrived.notify_all();
        }
        let _ = connection.reply(&message, Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::dbus::test_bus::TestBus;
    use crate::kde::script::fake::{FakeKwin, ScriptsDir};
    use crate::kde::script::{REPORT_INTERFACE, REPORT_PATH};

    /// The script reports to the connection that loaded it, through the interface the
    /// listener answers, and covers both the first focus and every change.
    #[test]
    fn the_script_reports_to_its_loader() {
        let text = script_text(":1.42");
        assert!(
            text.contains(
                r#"callDBus(":1.42", "/org/sequencer/KWin", "org.sequencer.KWin", "activated","#
            ),
            "{text}"
        );
        assert!(text.contains("workspace.windowActivated.connect(report);"));
        assert!(text.contains("report(workspace.activeWindow);"));
        assert!(
            text.contains("String(window.resourceClass)")
                && text.contains("String(window.internalId)")
        );
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Opens a `KWinFocus` against `kwin`, serving KWin's side meanwhile.
    fn open(bus: &TestBus, kwin: &mut FakeKwin, dir: &ScriptsDir) -> Option<KWinFocus> {
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| KWinFocus::open_on(bus.connect(), bus.connect(), &dir.0));
            while !opening.is_finished() {
                kwin.serve_one();
            }
            opening.join().unwrap()
        })
    }

    /// Drops `focus`, serving KWin's side of the unload.
    fn close(focus: KWinFocus, kwin: &mut FakeKwin) {
        std::thread::scope(|scope| {
            let dropping = scope.spawn(move || drop(focus));
            while !dropping.is_finished() {
                kwin.serve_one();
            }
        });
    }

    /// The whole round: load, run, the first report, a change, and the unload on drop —
    /// with the script file gone once KWin has read it.
    #[test]
    fn focus_follows_kwins_reports_and_the_script_is_unloaded_after() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        let dir = ScriptsDir::new();
        let focus = open(&bus, &mut kwin, &dir).expect("KWin runs the script");
        assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
        assert_eq!(focus.focused_window().as_deref(), Some("{0001}"));
        assert!(
            std::fs::read_dir(&dir.0).unwrap().next().is_none(),
            "the script file is removed once read"
        );

        kwin.report(ACTIVATED, vec![Value::str("firefox"), Value::str("{0002}")]);
        eventually("the change is heard", || {
            focus.focused_class().as_deref() == Some("firefox")
        });

        // Nothing has focus: KWin reports a null window, sent as empty strings.
        kwin.report(ACTIVATED, vec![Value::str(""), Value::str("")]);
        eventually("no focus reads as unknown", || {
            focus.focused_class().is_none()
        });

        let plugin = focus.script.plugin().to_owned();
        close(focus, &mut kwin);
        assert_eq!(kwin.unloaded, vec![plugin]);
    }

    /// KWin's id collision: `run` reaches another script, so the first report never
    /// comes — and `start` gets this one going.
    #[test]
    fn a_script_run_cannot_reach_is_started_instead() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.run_works = false;
        let dir = ScriptsDir::new();
        let focus = open(&bus, &mut kwin, &dir).expect("start runs it");
        assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
        close(focus, &mut kwin);
    }

    /// Anyone on the bus can call the listener; only KWin is believed.
    #[test]
    fn a_report_from_anyone_but_kwin_is_refused() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        let dir = ScriptsDir::new();
        let focus = open(&bus, &mut kwin, &dir).expect("KWin runs the script");
        let mut stranger = bus.connect();
        let refused = stranger.call(
            kwin.owner.as_deref().unwrap(),
            REPORT_PATH,
            REPORT_INTERFACE,
            ACTIVATED,
            vec![Value::str("evil"), Value::str("{6666}")],
            Duration::from_secs(5),
        );
        assert!(refused.is_err(), "the stranger is told no");
        assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
        close(focus, &mut kwin);
    }

    /// A script that never reports, even after `start`, is a failure to open — and what
    /// was loaded is unloaded again.
    #[test]
    fn a_script_that_never_reports_is_not_a_reader() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.reports = false;
        let dir = ScriptsDir::new();
        assert!(open(&bus, &mut kwin, &dir).is_none());
        assert_eq!(kwin.unloaded.len(), 1, "the silent script is unloaded");
    }

    #[test]
    fn without_kwin_there_is_nothing_to_open() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let dir = ScriptsDir::new();
        assert!(KWinFocus::open_on(bus.connect(), bus.connect(), &dir.0).is_none());
    }
}
