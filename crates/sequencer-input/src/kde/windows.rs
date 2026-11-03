//! The windows KWin manages, and asking them to close.
//!
//! In a Wayland session Xwayland lists only X11 windows; KWin manages them all. Like focus
//! ([`super::KWinFocus`]) this goes through a script loaded into KWin ([`super::script`]),
//! but one that runs once: it reports each window, or closes the ones named, says it is
//! done, and is unloaded.
//!
//! Special windows — the desktop, panels, splashes, notifications, on-screen displays —
//! are left out: they are KWin's and the shell's, not windows a user manages.

use std::path::Path;
use std::time::Instant;

use super::script::{FIRST_REPORT, Script, is_report, js_string, kwin_owner, report_call};
use crate::dbus::{Connection, Message, Value};

/// The report a listing script makes for each window.
const WINDOW: &str = "window";
/// The report every one-shot script makes last.
const DONE: &str = "done";

/// One window KWin manages.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct KWinWindow {
    /// KWin's id for the window — what [`close`] takes.
    pub id: String,
    /// The owning process, when KWin knows it.
    pub pid: Option<u32>,
    /// The title as the app set it (KWin's `captionNormal`, without the host name or
    /// shortcut KWin may add for display).
    pub title: String,
    /// `resourceClass`: the app id of a Wayland window (`org.kde.konsole`), the
    /// `WM_CLASS` class of an X11 one.
    pub class: String,
}

/// Why KWin could not be asked.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KWinError {
    /// There is no session bus to reach KWin on.
    #[error("no session bus: {0}")]
    Bus(String),
    /// KWin is not running in this session.
    #[error("KWin is not running in this session")]
    NotRunning,
    /// KWin would not load or run sequencer's script, or it never reported back.
    #[error("KWin did not run sequencer's script: {0}")]
    Script(&'static str),
}

impl From<crate::dbus::Error> for KWinError {
    fn from(err: crate::dbus::Error) -> Self {
        Self::Bus(err.to_string())
    }
}

/// Every window KWin manages but its special ones, in KWin's order.
///
/// # Errors
///
/// [`KWinError`]: no session bus, no KWin on it, or a script that would not run.
pub fn windows() -> Result<Vec<KWinWindow>, KWinError> {
    let scripts = crate::dbus::runtime_dir()?;
    windows_on(Connection::session()?, Connection::session()?, &scripts)
}

/// Asks each window `ids` names to close, as its titlebar's close button does. The app
/// decides what that means — it may ask about unsaved work first — so call [`windows`]
/// afterwards to see what is still open. An id KWin no longer knows is passed over.
///
/// # Errors
///
/// As [`windows`].
pub fn close(ids: &[String]) -> Result<(), KWinError> {
    let scripts = crate::dbus::runtime_dir()?;
    close_on(
        Connection::session()?,
        Connection::session()?,
        &scripts,
        ids,
    )
}

fn windows_on(
    control: Connection,
    listener: Connection,
    scripts: &Path,
) -> Result<Vec<KWinWindow>, KWinError> {
    let reports = run_once(control, listener, scripts, "windows", listing_script)?;
    Ok(reports.iter().filter_map(window_from).collect())
}

fn close_on(
    control: Connection,
    listener: Connection,
    scripts: &Path,
    ids: &[String],
) -> Result<(), KWinError> {
    if ids.is_empty() {
        return Ok(());
    }
    run_once(control, listener, scripts, "close", |owner| {
        closing_script(owner, ids)
    })
    .map(drop)
}

/// The listing script, reporting to `owner`: every window but the special ones (Plasma
/// 6's `workspace.windowList()` and `specialWindow`), then `done`. A pid KWin doesn't
/// know is -1.
fn listing_script(owner: &str) -> String {
    format!(
        "// Loaded by sequencer to list KWin's windows for the process that loaded it.\n\
         const owner = {owner};\n\
         const list = workspace.windowList();\n\
         for (let i = 0; i < list.length; i++) {{\n    \
             const window = list[i];\n    \
             if (window.specialWindow) {{\n        \
                 continue;\n    \
             }}\n    \
             {call}, String(window.internalId), window.pid,\n        \
                 String(window.captionNormal), String(window.resourceClass));\n\
         }}\n\
         {done});\n",
        owner = js_string(owner),
        call = report_call(owner, WINDOW),
        done = report_call(owner, DONE),
    )
}

/// The closing script, reporting to `owner`: `closeWindow()` on each window whose id is in
/// `ids`, then `done`.
fn closing_script(owner: &str, ids: &[String]) -> String {
    let wanted = ids
        .iter()
        .map(|id| js_string(id))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "// Loaded by sequencer to close the windows the process that loaded it named.\n\
         const owner = {owner};\n\
         const wanted = [{wanted}];\n\
         const list = workspace.windowList();\n\
         for (let i = 0; i < list.length; i++) {{\n    \
             if (wanted.indexOf(String(list[i].internalId)) >= 0) {{\n        \
                 list[i].closeWindow();\n    \
             }}\n\
         }}\n\
         {done});\n",
        owner = js_string(owner),
        done = report_call(owner, DONE),
    )
}

/// Runs a one-shot script, written by `text` for the name it reports to, and gathers its
/// reports until it says it is done.
fn run_once(
    mut control: Connection,
    mut listener: Connection,
    scripts: &Path,
    purpose: &str,
    text: impl FnOnce(&str) -> String,
) -> Result<Vec<Message>, KWinError> {
    let kwin = kwin_owner(&mut control).ok_or(KWinError::NotRunning)?;
    let text = text(listener.unique_name());
    let mut script = Script::load(control, scripts, purpose, &text)
        .ok_or(KWinError::Script("KWin would not load it"))?;
    if !script.run() {
        return Err(KWinError::Script("KWin would not run it"));
    }
    let mut reports = Vec::new();
    // `start` reaches the script when `run` went to another one (see `super::script`).
    let done = gather(&mut listener, &kwin, &mut reports)
        || (script.start_all() && gather(&mut listener, &kwin, &mut reports));
    if !done {
        return Err(KWinError::Script("it never reported back"));
    }
    script.read_by_kwin();
    Ok(reports)
}

/// Answers and keeps the script's reports until it says it is done; `false` when it falls
/// silent first.
fn gather(listener: &mut Connection, kwin: &str, reports: &mut Vec<Message>) -> bool {
    let deadline = Instant::now() + FIRST_REPORT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let Ok(Some(message)) = listener.next_message(Some(left)) else {
            return false;
        };
        if is_report(&message, kwin, DONE) {
            let _ = listener.reply(&message, Vec::new());
            return true;
        }
        if is_report(&message, kwin, WINDOW) {
            let _ = listener.reply(&message, Vec::new());
            reports.push(message);
        } else {
            let _ = listener.decline(&message);
        }
    }
}

/// A window report: id, pid (-1 when KWin does not know it), title, class.
fn window_from(report: &Message) -> Option<KWinWindow> {
    let [Value::Str(id), pid, Value::Str(title), Value::Str(class)] = report.body.as_slice() else {
        return None;
    };
    Some(KWinWindow {
        id: id.clone(),
        pid: pid.as_i32().and_then(|pid| u32::try_from(pid).ok()),
        title: title.clone(),
        class: class.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::test_bus::TestBus;
    use crate::kde::script::fake::{FakeKwin, ScriptsDir};
    use crate::kde::script::{REPORT_INTERFACE, REPORT_PATH};

    /// Runs `ask` against `kwin`, serving KWin's side meanwhile.
    fn asking<T: Send>(kwin: &mut FakeKwin, ask: impl FnOnce() -> T + Send) -> T {
        std::thread::scope(|scope| {
            let asking = scope.spawn(ask);
            while !asking.is_finished() {
                kwin.serve_one();
            }
            asking.join().unwrap()
        })
    }

    /// What KWin reports comes back as windows, a pid KWin doesn't know as `None` — and
    /// the script is unloaded, its file gone.
    #[test]
    fn the_list_is_what_kwin_reports() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.windows = vec![
            ("{0001}", 4242, "~ : bash — Konsole", "org.kde.konsole"),
            ("{0002}", -1, "Untitled", "kdialog"),
        ];
        let dir = ScriptsDir::new();
        let listed = asking(&mut kwin, || {
            windows_on(bus.connect(), bus.connect(), &dir.0)
        })
        .expect("KWin answers");
        assert_eq!(
            listed,
            vec![
                KWinWindow {
                    id: "{0001}".into(),
                    pid: Some(4242),
                    title: "~ : bash — Konsole".into(),
                    class: "org.kde.konsole".into(),
                },
                KWinWindow {
                    id: "{0002}".into(),
                    pid: None,
                    title: "Untitled".into(),
                    class: "kdialog".into(),
                },
            ]
        );
        assert_eq!(kwin.unloaded.len(), 1, "the one-shot script is unloaded");
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
    }

    /// The close script names exactly the windows asked for.
    #[test]
    fn close_names_its_windows_to_kwin() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        let dir = ScriptsDir::new();
        let ids = vec!["{0001}".to_owned(), "{0003}".to_owned()];
        asking(&mut kwin, || {
            close_on(bus.connect(), bus.connect(), &dir.0, &ids)
        })
        .expect("KWin answers");
        assert_eq!(kwin.closed, ids);
        assert_eq!(kwin.unloaded.len(), 1);
    }

    /// Nothing to close asks nothing of KWin.
    #[test]
    fn closing_nothing_loads_nothing() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let dir = ScriptsDir::new();
        // No KWin on this bus at all: asking it anything would fail.
        close_on(bus.connect(), bus.connect(), &dir.0, &[]).expect("nothing to do");
    }

    /// KWin's id collision: `run` reached another script, and `start` gets this one going.
    #[test]
    fn a_script_run_cannot_reach_is_started_instead() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.run_works = false;
        kwin.windows = vec![("{0001}", 7, "x", "y")];
        let dir = ScriptsDir::new();
        let listed = asking(&mut kwin, || {
            windows_on(bus.connect(), bus.connect(), &dir.0)
        })
        .expect("start runs it");
        assert_eq!(listed.len(), 1);
    }

    /// Only KWin's reports count: a window reported by anyone else is not listed.
    #[test]
    fn a_window_reported_by_anyone_but_kwin_is_not_listed() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.windows = vec![("{0001}", 7, "real", "app")];
        let dir = ScriptsDir::new();
        let mut stranger = bus.connect();
        let listed = asking(&mut kwin, || {
            let listener = bus.connect();
            // Planted before the script runs, so it waits ahead of KWin's own reports.
            let planted = Message::call(
                listener.unique_name(),
                REPORT_PATH,
                REPORT_INTERFACE,
                WINDOW,
                vec![
                    Value::str("{6666}"),
                    Value::I32(1),
                    Value::str("fake"),
                    Value::str("evil"),
                ],
            );
            stranger.send(&planted).unwrap();
            windows_on(bus.connect(), listener, &dir.0)
        })
        .expect("KWin answers");
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].title, "real");
    }

    #[test]
    fn a_script_that_never_reports_is_an_error() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let mut kwin = FakeKwin::new(&bus);
        kwin.reports = false;
        let dir = ScriptsDir::new();
        let err = asking(&mut kwin, || {
            windows_on(bus.connect(), bus.connect(), &dir.0)
        })
        .unwrap_err();
        assert!(matches!(err, KWinError::Script(_)), "{err:?}");
        assert_eq!(kwin.unloaded.len(), 1, "the silent script is unloaded");
    }

    #[test]
    fn without_kwin_it_is_not_running() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let dir = ScriptsDir::new();
        let err = windows_on(bus.connect(), bus.connect(), &dir.0).unwrap_err();
        assert!(matches!(err, KWinError::NotRunning), "{err:?}");
    }

    /// The listing script names its owner first, skips special windows, reports each
    /// window's id, pid, raw title and class, and ends with `done`.
    #[test]
    fn the_listing_script_reports_every_managed_window_then_done() {
        let text = listing_script(":1.42");
        assert!(
            text.starts_with("// ") && text.contains(r#"const owner = ":1.42";"#),
            "{text}"
        );
        assert!(text.contains("workspace.windowList()"), "{text}");
        assert!(text.contains("if (window.specialWindow)"), "{text}");
        assert!(
            text.contains("String(window.internalId), window.pid")
                && text.contains("String(window.captionNormal), String(window.resourceClass)"),
            "{text}"
        );
        assert!(text.trim_end().ends_with(r#""done");"#), "{text}");
    }

    /// The closing script names exactly the ids given, escaped, and ends with `done`.
    #[test]
    fn the_closing_script_closes_exactly_the_ids_given() {
        let text = closing_script(":1.42", &["{0001}".to_owned(), r#"odd"id"#.to_owned()]);
        assert!(
            text.contains(r#"const wanted = ["{0001}", "odd\"id"];"#),
            "{text}"
        );
        assert!(text.contains("list[i].closeWindow();"), "{text}");
        assert!(text.trim_end().ends_with(r#""done");"#), "{text}");
    }
}
