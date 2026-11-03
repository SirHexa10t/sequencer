//! Running a script inside KWin on this process's behalf — the one way KWin answers some
//! questions: which window has focus ([`super::KWinFocus`]), which windows there are
//! ([`super::windows`]).
//!
//! The script is written to the runtime directory, loaded through `org.kde.KWin
//! /Scripting` under a name of this process's own, and run; it reports back by calling
//! methods on one of this process's connections (`callDBus`). KWin answers `run` only once
//! the script has been evaluated, and waits for an answer to every report.
//!
//! Two quirks of KWin's are handled here. `loadScript` returns -1 when the name is taken —
//! a killed process's leftover, since names carry the pid: it is unloaded and loaded again.
//! And script ids are positions in KWin's list, so once an earlier script has unloaded, a
//! new one can be given a live one's id and `run` reaches that one instead; a caller whose
//! first report does not come calls [`Script::start_all`], which runs every loaded script
//! not yet running.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::dbus::{Connection, Message, Value};

pub(super) const KWIN: &str = "org.kde.KWin";
const SCRIPTING_PATH: &str = "/Scripting";
const SCRIPTING: &str = "org.kde.kwin.Scripting";
const SCRIPT: &str = "org.kde.kwin.Script";

/// Where every script reports, on the loading process's own connection.
pub(super) const REPORT_PATH: &str = "/org/sequencer/KWin";
pub(super) const REPORT_INTERFACE: &str = "org.sequencer.KWin";

/// How long KWin may take over a scripting call — loading, or running a script, which it
/// answers only once the script has been evaluated.
pub(super) const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a started script has to make its first report. Each script reports as soon as
/// it runs, so silence beyond this means it is not running.
pub(super) const FIRST_REPORT: Duration = Duration::from_secs(1);

/// A script loaded into KWin for this process; unloaded when dropped.
#[derive(Debug)]
pub(super) struct Script {
    control: Connection,
    plugin: String,
    file: PathBuf,
    id: i32,
}

impl Script {
    /// Writes `text` into `dir` and loads it under a name unique to this process and this
    /// call, or `None` when KWin will not load it.
    pub(super) fn load(control: Connection, dir: &Path, purpose: &str, text: &str) -> Option<Self> {
        let plugin = plugin_name(purpose);
        let file = dir.join(format!("{plugin}.js"));
        write_private(&file, text).ok()?;
        let mut script = Self {
            control,
            plugin,
            file,
            id: -1,
        };
        // On failure, dropping `script` unloads whatever got loaded and removes the file.
        script.id = match script.load_once()? {
            -1 => {
                script.unload();
                script.load_once().filter(|id| *id >= 0)?
            }
            id if id >= 0 => id,
            _ => return None,
        };
        Some(script)
    }

    /// The name it is loaded under.
    #[cfg(test)]
    pub(super) fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Runs it; KWin answers once the script has been evaluated.
    pub(super) fn run(&mut self) -> bool {
        let path = format!("{SCRIPTING_PATH}/Script{}", self.id);
        self.control
            .call(KWIN, &path, SCRIPT, "run", Vec::new(), CALL_TIMEOUT)
            .is_ok()
    }

    /// Runs every loaded script not yet running — how this one is reached when its id
    /// went to another script.
    pub(super) fn start_all(&mut self) -> bool {
        self.control
            .call(
                KWIN,
                SCRIPTING_PATH,
                SCRIPTING,
                "start",
                Vec::new(),
                CALL_TIMEOUT,
            )
            .is_ok()
    }

    /// The script has reported, so KWin has read its file, which can go.
    pub(super) fn read_by_kwin(&self) {
        let _ = std::fs::remove_file(&self.file);
    }

    /// One `loadScript` call: the new script's id, or -1 when the name is taken.
    fn load_once(&mut self) -> Option<i32> {
        let args = vec![
            Value::Str(self.file.to_string_lossy().into_owned()),
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
}

impl Drop for Script {
    fn drop(&mut self) {
        self.unload();
        self.read_by_kwin();
    }
}

/// KWin's unique name on the bus — the only sender whose reports are believed — or
/// `None` when KWin is not running.
pub(super) fn kwin_owner(control: &mut Connection) -> Option<String> {
    control.name_owner(KWIN).ok()?
}

/// Whether `message` is the report `member` from KWin itself. Anyone on the bus can call
/// this process's connection; only KWin's calls are reports.
pub(super) fn is_report(message: &Message, kwin: &str, member: &str) -> bool {
    message.sender.as_deref() == Some(kwin)
        && message.is_call(REPORT_PATH, REPORT_INTERFACE, member)
}

/// The name a script is loaded under: `sequencer-<purpose>-<pid>-<n>`, unique to this
/// process and this call, so one process's scripts never collide — not even with one it
/// unloaded a moment ago, which KWin deletes only later.
fn plugin_name(purpose: &str) -> String {
    static CALLS: AtomicU32 = AtomicU32::new(0);
    let call = CALLS.fetch_add(1, Ordering::Relaxed);
    format!("sequencer-{purpose}-{}-{call}", std::process::id())
}

/// `text` as a JavaScript string literal.
pub(super) fn js_string(text: &str) -> String {
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

/// The `callDBus` that sends report `member` to `owner` — each script's one way out.
pub(super) fn report_call(owner: &str, member: &str) -> String {
    format!(
        "callDBus({}, {}, {}, {}",
        js_string(owner),
        js_string(REPORT_PATH),
        js_string(REPORT_INTERFACE),
        js_string(member)
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_strings_are_escaped() {
        assert_eq!(js_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(js_string("x\ny"), r#""x\ny""#);
    }

    /// One process never loads two scripts under one name, so a name KWin is still
    /// deleting can't refuse the next load.
    #[test]
    fn every_load_gets_a_name_of_its_own() {
        let (first, second) = (plugin_name("windows"), plugin_name("windows"));
        assert_ne!(first, second);
        assert!(first.starts_with(&format!("sequencer-windows-{}-", std::process::id())));
    }

    #[test]
    fn a_report_names_its_owner_first() {
        assert_eq!(
            report_call(":1.42", "done"),
            r#"callDBus(":1.42", "/org/sequencer/KWin", "org.sequencer.KWin", "done""#
        );
    }
}

/// A stand-in for KWin, for the readers' tests: owns `org.kde.KWin` on a private bus,
/// "runs" the scripts it is given the way KWin's would, and records what was unloaded.
#[cfg(test)]
pub(super) mod fake {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{KWIN, REPORT_INTERFACE, REPORT_PATH};
    use crate::dbus::test_bus::TestBus;
    use crate::dbus::{Connection, Kind, Message, Value};

    /// A window the fake reports for a listing script: (id, pid, title, class).
    pub(crate) type FakeWindow = (&'static str, i32, &'static str, &'static str);

    pub(crate) struct FakeKwin {
        pub(crate) connection: Connection,
        /// Who the loaded script reports to.
        pub(crate) owner: Option<String>,
        /// The loaded script's purpose, from its name: `focus`, `windows` or `close`.
        purpose: String,
        /// The ids a close script named.
        wanted: Vec<String>,
        /// Whether `run` really runs the script; `false` plays KWin's id collision, where
        /// only `start` reaches it.
        pub(crate) run_works: bool,
        /// Whether a script reports at all once run.
        pub(crate) reports: bool,
        /// What a listing script finds.
        pub(crate) windows: Vec<FakeWindow>,
        /// What close scripts asked to close.
        pub(crate) closed: Vec<String>,
        pub(crate) unloaded: Vec<String>,
    }

    impl FakeKwin {
        pub(crate) fn new(bus: &TestBus) -> Self {
            let mut connection = bus.connect();
            connection.request_name(KWIN).unwrap();
            Self {
                connection,
                owner: None,
                purpose: String::new(),
                wanted: Vec::new(),
                run_works: true,
                reports: true,
                windows: Vec::new(),
                closed: Vec::new(),
                unloaded: Vec::new(),
            }
        }

        /// Answers one scripting call, if one arrives soon.
        pub(crate) fn serve_one(&mut self) {
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
                    let file = PathBuf::from(message.body[0].as_str().unwrap());
                    let text = std::fs::read_to_string(file).unwrap();
                    // Every script names its owner in its first string literal.
                    self.owner = Some(text.split('"').nth(1).unwrap().to_owned());
                    let plugin = message.body[1].as_str().unwrap();
                    self.purpose = plugin.split('-').nth(1).unwrap().to_owned();
                    self.wanted = text
                        .split_once("const wanted = [")
                        .map(|(_, rest)| rest.split(']').next().unwrap())
                        .map(|list| {
                            list.split('"')
                                .skip(1)
                                .step_by(2)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    self.connection
                        .reply(&message, vec![Value::I32(0)])
                        .unwrap();
                }
                Some("run") => {
                    assert_eq!(message.path.as_deref(), Some("/Scripting/Script0"));
                    self.connection.reply(&message, Vec::new()).unwrap();
                    if self.run_works {
                        self.execute();
                    }
                }
                Some("start") => {
                    self.connection.reply(&message, Vec::new()).unwrap();
                    self.execute();
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

        /// What the loaded script does once running.
        fn execute(&mut self) {
            if !self.reports {
                return;
            }
            match self.purpose.as_str() {
                "focus" => self.report(
                    "activated",
                    vec![Value::str("org.kde.konsole"), Value::str("{0001}")],
                ),
                "windows" => {
                    for (id, pid, title, class) in self.windows.clone() {
                        let body = vec![
                            Value::str(id),
                            Value::I32(pid),
                            Value::str(title),
                            Value::str(class),
                        ];
                        self.report("window", body);
                    }
                    self.report("done", Vec::new());
                }
                "close" => {
                    self.closed.append(&mut self.wanted);
                    self.report("done", Vec::new());
                }
                other => panic!("no such script: {other}"),
            }
        }

        /// Reports to the script's owner, as `callDBus` does: without waiting for the answer.
        pub(crate) fn report(&mut self, member: &str, body: Vec<Value>) {
            let owner = self.owner.clone().expect("a script was loaded");
            let call = Message::call(&owner, REPORT_PATH, REPORT_INTERFACE, member, body);
            self.connection.send(&call).unwrap();
        }
    }

    /// A directory of its own for each test — tests share a process id — removed when the
    /// test is done.
    pub(crate) struct ScriptsDir(pub(crate) PathBuf);

    impl ScriptsDir {
        pub(crate) fn new() -> Self {
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
}
