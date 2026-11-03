//! The focused program, as KWin reports it to a script sequencer loads into it.
//!
//! KWin's D-Bus interface has no call that names the active window without the user
//! clicking one (`queryWindowInfo` is interactive), so this does what every tool asking
//! KWin does: it loads a small script through `org.kde.KWin /Scripting`, and the script
//! reports each focus change by calling back over D-Bus. KWin waits for an answer to each
//! call, so the listener answers every report.
//!
//! What it reports is the window's `resourceClass`: the `WM_CLASS` class for an X11
//! window — the name [`crate::x11::FocusWatcher`] reads — and the application id for a
//! Wayland one (`org.kde.konsole`, `firefox`), kept exactly as the app set it.
//!
//! The script is loaded for one process and unloaded when it is done. A process killed
//! outright leaves its script behind until KWin restarts; it only ever calls a name that
//! no longer exists, which costs nothing.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::lock;
use crate::dbus::{Closer, Connection, Value};

const KWIN: &str = "org.kde.KWin";
const SCRIPTING_PATH: &str = "/Scripting";
const SCRIPTING: &str = "org.kde.kwin.Scripting";
const SCRIPT: &str = "org.kde.kwin.Script";

/// Where the script reports, on this process's own connection.
const REPORT_PATH: &str = "/org/sequencer/Focus";
const REPORT_INTERFACE: &str = "org.sequencer.Focus";
const REPORT_MEMBER: &str = "activated";

/// How long KWin may take over a scripting call — loading, or running a script, which it
/// answers only once the script has been evaluated.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a started script has to make its first report. Its first line reports the
/// focus it finds, so silence beyond this means it is not running.
const FIRST_REPORT: Duration = Duration::from_secs(1);

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
    /// Loads and unloads the script; the reports arrive on another connection.
    control: Connection,
    plugin: String,
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
            .and_then(|mut bus| bus.name_owner(KWIN))
            .is_ok_and(|owner| owner.is_some())
    }

    fn open_on(mut control: Connection, listener: Connection, scripts: &Path) -> Option<Self> {
        // Reports are accepted from KWin alone: a call claiming a focus change from
        // anyone else on the bus is not one.
        let kwin = control.name_owner(KWIN).ok()??;
        let plugin = format!("sequencer-focus-{}", std::process::id());
        let file = scripts.join(format!("{plugin}.js"));
        write_private(&file, &script(listener.unique_name())).ok()?;

        let reports = Arc::new(Reports::default());
        let closer = listener.closer().ok()?;
        let shared = Arc::clone(&reports);
        let listening = std::thread::Builder::new()
            .name("sequencer-kwin-focus".into())
            .spawn(move || listen(listener, &kwin, &shared))
            .ok()?;
        let mut focus = Self {
            reports,
            control,
            plugin,
            closer,
            listener: Some(listening),
        };
        let started = focus.start(&file);
        // KWin has read it by now, or never will.
        let _ = std::fs::remove_file(&file);
        // On failure, dropping `focus` unloads whatever got loaded and stops the thread.
        started.then_some(focus)
    }

    /// Loads and runs the script, and waits for its first report.
    fn start(&mut self, file: &Path) -> bool {
        let Some(id) = self.load(file) else {
            return false;
        };
        let run = self.control.call(
            KWIN,
            &format!("{SCRIPTING_PATH}/Script{id}"),
            SCRIPT,
            "run",
            Vec::new(),
            CALL_TIMEOUT,
        );
        if run.is_err() {
            return false;
        }
        if self.first_report_within(FIRST_REPORT) {
            return true;
        }
        // KWin numbers scripts by their place in its list, so after an earlier script
        // unloads, a new one can be given a live one's number — and `run` above went to
        // that one. `start` runs every loaded script that is not yet running, this one
        // included.
        let restarted = self.control.call(
            KWIN,
            SCRIPTING_PATH,
            SCRIPTING,
            "start",
            Vec::new(),
            CALL_TIMEOUT,
        );
        restarted.is_ok() && self.first_report_within(FIRST_REPORT)
    }

    /// Loads the script under this process's plugin name; its id for `run`.
    fn load(&mut self, file: &Path) -> Option<i32> {
        match self.load_once(file)? {
            // The name is taken: an earlier process with this pid, killed before it could
            // unload. Its script reports to a name that is gone; replace it.
            -1 => {
                self.unload();
                self.load_once(file).filter(|id| *id >= 0)
            }
            id if id >= 0 => Some(id),
            _ => None,
        }
    }

    /// One `loadScript` call: the new script's id, or -1 when the name is taken.
    fn load_once(&mut self, file: &Path) -> Option<i32> {
        let args = vec![
            Value::Str(file.to_string_lossy().into_owned()),
            Value::str(&self.plugin),
        ];
        self.control
            .call(
                KWIN,
                SCRIPTING_PATH,
                SCRIPTING,
                "loadScript",
                args,
                CALL_TIMEOUT,
            )
            .ok()?
            .first()
            .and_then(Value::as_i32)
    }

    fn unload(&mut self) {
        let _ = self.control.call(
            KWIN,
            SCRIPTING_PATH,
            SCRIPTING,
            "unloadScript",
            vec![Value::str(&self.plugin)],
            CALL_TIMEOUT,
        );
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
        self.unload();
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
fn script(owner: &str) -> String {
    format!(
        "// Loaded by sequencer to learn which window has focus: it reports each change to\n\
         // the process that loaded it, and is unloaded when that process is done.\n\
         function report(window) {{\n    \
             callDBus({owner}, {path}, {interface}, {member},\n        \
                 window ? String(window.resourceClass) : \"\",\n        \
                 window ? String(window.internalId) : \"\");\n\
         }}\n\
         workspace.windowActivated.connect(report);\n\
         report(workspace.activeWindow);\n",
        owner = js_string(owner),
        path = js_string(REPORT_PATH),
        interface = js_string(REPORT_INTERFACE),
        member = js_string(REPORT_MEMBER),
    )
}

/// `text` as a JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for character in text.chars() {
        match character {
            '"' | '\\' => {
                literal.push('\\');
                literal.push(character);
            }
            '\n' => literal.push_str("\\n"),
            other => literal.push(other),
        }
    }
    literal.push('"');
    literal
}

/// Writes `text` to `path` readable by this user alone — KWin runs as the same user.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(text.as_bytes())
}

/// Answers the script's reports until the connection closes.
fn listen(mut connection: Connection, kwin: &str, reports: &Reports) {
    loop {
        let message = match connection.next_message(None) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(_) => return,
        };
        let from_kwin = message.sender.as_deref() == Some(kwin);
        if !(from_kwin && message.is_call(REPORT_PATH, REPORT_INTERFACE, REPORT_MEMBER)) {
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
    use crate::dbus::{Kind, Message};

    /// The script reports to the connection that loaded it, through the interface the
    /// listener answers, and covers both the first focus and every change.
    #[test]
    fn the_script_reports_to_its_loader() {
        let text = script(":1.42");
        assert!(
            text.contains(
                r#"callDBus(":1.42", "/org/sequencer/Focus", "org.sequencer.Focus", "activated","#
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

    #[test]
    fn javascript_strings_are_escaped() {
        assert_eq!(js_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(js_string("x\ny"), r#""x\ny""#);
    }

    /// A stand-in for KWin: owns `org.kde.KWin`, loads scripts by reading their file for
    /// the name to report to, and reports focus to it the way the script would.
    struct FakeKwin {
        connection: Connection,
        /// Who the loaded script reports to.
        owner: Option<String>,
        /// Whether a script is running and has made its first report.
        reporting: bool,
        /// Whether `run` really runs the script; `false` plays KWin's id collision,
        /// where only `start` reaches it.
        run_works: bool,
        unloaded: Vec<String>,
    }

    impl FakeKwin {
        fn new(bus: &TestBus, run_works: bool) -> Self {
            let mut connection = bus.connect();
            connection.request_name(KWIN).unwrap();
            Self {
                connection,
                owner: None,
                reporting: false,
                run_works,
                unloaded: Vec::new(),
            }
        }

        /// Answers one scripting call, if one arrives soon.
        fn serve_one(&mut self) {
            let Some(message) = self
                .connection
                .next_message(Some(Duration::from_millis(50)))
                .unwrap()
            else {
                return;
            };
            if message.kind != Kind::MethodCall {
                return;
            }
            match message.member.as_deref() {
                Some("loadScript") => {
                    let file = message.body[0].as_str().unwrap();
                    let text = std::fs::read_to_string(file).unwrap();
                    let owner = text.split('"').nth(1).unwrap().to_owned();
                    self.owner = Some(owner);
                    self.connection
                        .reply(&message, vec![Value::I32(0)])
                        .unwrap();
                }
                Some("run") => {
                    assert_eq!(message.path.as_deref(), Some("/Scripting/Script0"));
                    self.connection.reply(&message, Vec::new()).unwrap();
                    if self.run_works {
                        self.report("org.kde.konsole", "{0001}");
                    }
                }
                Some("start") => {
                    self.connection.reply(&message, Vec::new()).unwrap();
                    self.report("org.kde.konsole", "{0001}");
                }
                Some("unloadScript") => {
                    self.unloaded
                        .push(message.body[0].as_str().unwrap().to_owned());
                    self.connection
                        .reply(&message, vec![Value::Bool(true)])
                        .unwrap();
                }
                _ => self.connection.decline(&message).unwrap(),
            }
        }

        /// Reports a focus change, as the script does, without waiting for the answer.
        fn report(&mut self, class: &str, window: &str) {
            let owner = self.owner.clone().expect("a script was loaded");
            let call = Message::call(
                &owner,
                REPORT_PATH,
                REPORT_INTERFACE,
                REPORT_MEMBER,
                vec![Value::str(class), Value::str(window)],
            );
            self.connection.send(&call).unwrap();
            self.reporting = true;
        }
    }

    /// A directory of its own for each test — tests share a process id, and so a script
    /// file name — removed when the test is done.
    struct ScriptsDir(std::path::PathBuf);

    impl ScriptsDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "sequencer-test-scripts-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for ScriptsDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "never happened: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The whole round: load, run, the first report, a change, and the unload on drop —
    /// with the script file gone once KWin has read it.
    #[test]
    fn focus_follows_kwins_reports_and_the_script_is_unloaded_after() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus, true);
        let dir = ScriptsDir::new();
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| KWinFocus::open_on(bus.connect(), bus.connect(), &dir.0));
            while !opening.is_finished() {
                kwin.serve_one();
            }
            let focus = opening.join().unwrap().expect("KWin runs the script");
            assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
            assert_eq!(focus.focused_window().as_deref(), Some("{0001}"));
            assert!(
                std::fs::read_dir(&dir.0).unwrap().next().is_none(),
                "the script file is removed once read"
            );

            kwin.report("firefox", "{0002}");
            eventually("the change is heard", || {
                focus.focused_class().as_deref() == Some("firefox")
            });

            // Nothing has focus: KWin reports a null window, sent as empty strings.
            kwin.report("", "");
            eventually("no focus reads as unknown", || {
                focus.focused_class().is_none()
            });

            let plugin = focus.plugin.clone();
            let dropping = scope.spawn(move || drop(focus));
            while !dropping.is_finished() {
                kwin.serve_one();
            }
            assert_eq!(kwin.unloaded, vec![plugin]);
        });
    }

    /// KWin's id collision: `run` reaches another script, so the first report never
    /// comes — and `start` gets this one going.
    #[test]
    fn a_script_run_cannot_reach_is_started_instead() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus, false);
        let dir = ScriptsDir::new();
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| KWinFocus::open_on(bus.connect(), bus.connect(), &dir.0));
            while !opening.is_finished() {
                kwin.serve_one();
            }
            let focus = opening.join().unwrap().expect("start runs it");
            assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
            let dropping = scope.spawn(move || drop(focus));
            while !dropping.is_finished() {
                kwin.serve_one();
            }
        });
    }

    /// Anyone on the bus can call the listener; only KWin is believed.
    #[test]
    fn a_report_from_anyone_but_kwin_is_refused() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus, true);
        let dir = ScriptsDir::new();
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| KWinFocus::open_on(bus.connect(), bus.connect(), &dir.0));
            while !opening.is_finished() {
                kwin.serve_one();
            }
            let focus = opening.join().unwrap().expect("KWin runs the script");
            let mut stranger = bus.connect();
            let refused = stranger.call(
                kwin.owner.as_deref().unwrap(),
                REPORT_PATH,
                REPORT_INTERFACE,
                REPORT_MEMBER,
                vec![Value::str("evil"), Value::str("{6666}")],
                Duration::from_secs(5),
            );
            assert!(refused.is_err(), "the stranger is told no");
            assert_eq!(focus.focused_class().as_deref(), Some("org.kde.konsole"));
            let dropping = scope.spawn(move || drop(focus));
            while !dropping.is_finished() {
                kwin.serve_one();
            }
        });
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
