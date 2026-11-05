//! The test's side of `sequencer-live-rig`, the root helper for the device tiers: started on
//! the cached sudo ticket, told what to type on its test keyboard and do with its test mouse,
//! and asked what sequencer's virtual keyboard sent back and what a mouse's stand-in passed
//! on. Every judgement stays here, unprivileged.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use sequencer_cli::core::input::{Button, Key};
use sequencer_cli::input::linux::rig::TEST_MOUSE_BUTTONS;

/// How long the rig may take to start: sudo, then a keyboard and its device node.
const START_WITHIN: Duration = Duration::from_secs(10);

/// How long one command may take to be answered.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);

/// How long the virtual keyboard may take to begin answering a key: a profile's whole
/// reaction, sudo and all already behind it.
const FIRST_SENT_WITHIN: Duration = Duration::from_secs(2);

/// How long the virtual keyboard must stay quiet for what it sends to count as over:
/// longer than any WAIT the tests' profiles hold between two steps.
const QUIET: Duration = Duration::from_millis(400);

/// The pause after each key event, as a hand's: each lands in a frame of its own, and the
/// manager has time to answer it before the next.
const BETWEEN_KEYS: Duration = Duration::from_millis(30);

/// The rig, running as root.
pub(crate) struct Rig {
    child: Child,
    /// Its commands; closed, the rig ends.
    input: Option<ChildStdin>,
    /// Its answers, in order.
    answers: Receiver<String>,
    /// What the virtual keyboard sent once watched, in order, each when it came.
    sent: Receiver<(Instant, Key, i32)>,
    /// What the test mouse's stand-in passed on once watched, in order.
    passed: Receiver<Passed>,
}

/// One thing the test mouse's stand-in passed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Passed {
    /// A button going down (1) or coming up (0).
    Button(Button, i32),
    /// One frame's motion: its steps to the right and down.
    Moved(i32, i32),
}

impl Rig {
    /// Starts the rig on the cached sudo ticket, or `None` — said why — when it can't run.
    pub(crate) fn start() -> Option<Self> {
        let mut child = Command::new("sudo")
            .args(["-n", env!("CARGO_BIN_EXE_sequencer-live-rig")])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| eprintln!("skip: sudo would not run: {err}"))
            .ok()?;
        let output = child.stdout.take().expect("the rig's output is piped");
        let (answered, answers) = mpsc::channel();
        let (sends, sent) = mpsc::channel();
        let (stood_in, passed) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                let delivered = if let Some(event) = line.strip_prefix("out ") {
                    parse_sent(event)
                        .is_none_or(|(key, value)| sends.send((Instant::now(), key, value)).is_ok())
                } else if let Some(event) = line.strip_prefix("in ") {
                    parse_passed(event).is_none_or(|event| stood_in.send(event).is_ok())
                } else {
                    answered.send(line).is_ok()
                };
                if !delivered {
                    break;
                }
            }
        });
        let rig = Self {
            input: child.stdin.take(),
            child,
            answers,
            sent,
            passed,
        };
        match rig.answers.recv_timeout(START_WITHIN) {
            Ok(line) if line == "ready" => Some(rig),
            Ok(line) => {
                eprintln!("skip: the rig could not start: {line}");
                None
            }
            Err(_) => {
                eprintln!("skip: the rig never started — no cached sudo ticket? `sudo -v` first");
                None
            }
        }
    }

    /// Presses `key` on the test keyboard and leaves it down.
    pub(crate) fn press(&mut self, key: Key) {
        self.send("press", key);
    }

    /// Lets `key` up on the test keyboard.
    pub(crate) fn release(&mut self, key: Key) {
        self.send("release", key);
    }

    /// `key` repeating, as the kernel reports a key held down.
    pub(crate) fn repeat(&mut self, key: Key) {
        self.send("repeat", key);
    }

    pub(crate) fn tap(&mut self, key: Key) {
        self.press(key);
        self.release(key);
    }

    /// Presses `keys` in order and lets them up in reverse, as a hand plays a chord.
    pub(crate) fn chord(&mut self, keys: &[Key]) {
        for &key in keys {
            self.press(key);
        }
        for &key in keys.iter().rev() {
            self.release(key);
        }
    }

    /// Grabs the running manager's virtual keyboard, so that what it sends comes to
    /// [`Self::sent`] and reaches nothing else.
    pub(crate) fn watch(&mut self) {
        let answer = self.command("watch");
        assert_eq!(
            answer, "watching",
            "the rig found no virtual keyboard to watch: is the manager on the devices?"
        );
    }

    /// Plays a manager that stops answering: has a real supervisor take the grab on the
    /// test keyboard, and then never beats.
    pub(crate) fn supervise(&mut self) {
        let answer = self.command("supervise");
        assert_eq!(
            answer, "supervised",
            "the rig's supervisor would not take the grab"
        );
    }

    /// Puts a new test keyboard in place of the old one, as a keyboard back from a suspend
    /// comes back a new device.
    pub(crate) fn replug(&mut self) {
        let answer = self.command("replug");
        assert_eq!(
            answer, "replugged",
            "the rig could not plug a new keyboard in"
        );
    }

    /// Whether a grab holds the test keyboard.
    pub(crate) fn held(&mut self) -> bool {
        match self.command("held").as_str() {
            "held" => true,
            "free" => false,
            other => panic!("the rig can't tell whether its keyboard is held: {other}"),
        }
    }

    /// Plugs in the test mouse: the buttons a profile names, two axes and a wheel. Nothing
    /// may be sent from it until its stand-in is watched: until then, a click or a move would
    /// reach the user's session.
    pub(crate) fn plug_mouse(&mut self) {
        let answer = self.command("mouse");
        assert_eq!(answer, "plugged", "the rig could not plug a mouse in");
    }

    /// Clicks `button` on the test mouse: down, then up.
    pub(crate) fn click(&mut self, button: Button) {
        for verb in ["mpress", "mrelease"] {
            let answer = self.command(&format!("{verb} {button}"));
            assert_eq!(answer, "ok", "the rig would not {verb} {button}");
            std::thread::sleep(BETWEEN_KEYS);
        }
    }

    /// Moves the test mouse `across` to the right and `down`.
    pub(crate) fn move_by(&mut self, across: i32, down: i32) {
        let answer = self.command(&format!("move {across} {down}"));
        assert_eq!(answer, "ok", "the rig would not move its mouse");
        std::thread::sleep(BETWEEN_KEYS);
    }

    /// Whether a grab holds the test mouse.
    pub(crate) fn mouse_held(&mut self) -> bool {
        match self.command("mouse-held").as_str() {
            "held" => true,
            "free" => false,
            other => panic!("the rig can't tell whether its mouse is held: {other}"),
        }
    }

    /// Grabs the test mouse's stand-in, so that what it passes on comes to [`Self::passed`]
    /// and reaches nothing else — the rig checking first that it carries the mouse's name,
    /// IDs, buttons and axes.
    pub(crate) fn watch_stand_in(&mut self) {
        let answer = self.command("watch-stand-in");
        assert_eq!(
            answer, "watching",
            "the rig found no stand-in for its mouse to watch, or a wrong one"
        );
    }

    /// What the test mouse's stand-in passed on since last asked: once it begins, until it
    /// falls quiet — or nothing, if it doesn't begin within [`FIRST_SENT_WITHIN`].
    pub(crate) fn passed(&self) -> Vec<Passed> {
        let mut events = Vec::new();
        let mut wait = FIRST_SENT_WITHIN;
        while let Ok(event) = self.passed.recv_timeout(wait) {
            events.push(event);
            wait = QUIET;
        }
        events
    }

    /// What the virtual keyboard sent since last asked: once it begins, until it falls
    /// quiet — or nothing, if it doesn't begin within [`FIRST_SENT_WITHIN`].
    pub(crate) fn sent(&self) -> Vec<(Key, i32)> {
        self.sent_timed()
            .into_iter()
            .map(|(_, key, value)| (key, value))
            .collect()
    }

    /// [`Self::sent`], each event with when it came — for what a profile's timing must
    /// keep apart.
    pub(crate) fn sent_timed(&self) -> Vec<(Instant, Key, i32)> {
        let mut events = Vec::new();
        let mut wait = FIRST_SENT_WITHIN;
        while let Ok(event) = self.sent.recv_timeout(wait) {
            events.push(event);
            wait = QUIET;
        }
        events
    }

    fn send(&mut self, verb: &str, key: Key) {
        let answer = self.command(&format!("{verb} {key}"));
        assert_eq!(answer, "ok", "the rig would not {verb} {key}");
        std::thread::sleep(BETWEEN_KEYS);
    }

    fn command(&mut self, line: &str) -> String {
        let input = self
            .input
            .as_mut()
            .expect("the rig's input stays open while it runs");
        writeln!(input, "{line}")
            .and_then(|()| input.flush())
            .expect("the rig takes commands");
        self.answers
            .recv_timeout(ANSWER_WITHIN)
            .unwrap_or_else(|_| panic!("the rig never answered {line:?}"))
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The end of its input ends the rig, and its keyboard and the watch's grab with it.
        // Never panic: this runs during panics too.
        drop(self.input.take());
        if ended_within(&mut self.child, 40) {
            return;
        }
        // sudo passes SIGTERM on to the rig; a SIGKILL would end only sudo.
        if let Ok(pid) = i32::try_from(self.child.id()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if !ended_within(&mut self.child, 40) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Whether `child` ends within `polls` looks, 50 ms apart.
fn ended_within(child: &mut Child, polls: u32) -> bool {
    for _ in 0..polls {
        if !matches!(child.try_wait(), Ok(None)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// `KEY VALUE`, as the rig says what the virtual keyboard sent.
fn parse_sent(text: &str) -> Option<(Key, i32)> {
    let (key, value) = text.rsplit_once(' ')?;
    Some((key.parse().ok()?, value.parse().ok()?))
}

/// `button BUTTON VALUE` or `move ACROSS DOWN`, as the rig says what a stand-in passed on.
fn parse_passed(text: &str) -> Option<Passed> {
    let (what, rest) = text.split_once(' ')?;
    let (first, second) = rest.split_once(' ')?;
    match what {
        "button" => {
            let button = TEST_MOUSE_BUTTONS
                .into_iter()
                .find(|button| button.to_string() == first)?;
            Some(Passed::Button(button, second.parse().ok()?))
        }
        "move" => Some(Passed::Moved(first.parse().ok()?, second.parse().ok()?)),
        _ => None,
    }
}
