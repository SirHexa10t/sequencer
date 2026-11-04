//! The test's side of `sequencer-live-rig`, the root helper for the device tiers: started on
//! the cached sudo ticket, told what to type on its test keyboard, and asked what
//! sequencer's virtual keyboard sent back. Every judgement stays here, unprivileged.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use sequencer_cli::core::input::Key;

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
    /// What the virtual keyboard sent once watched, in order.
    sent: Receiver<(Key, i32)>,
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
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                let delivered = match line.strip_prefix("out ") {
                    Some(event) => parse_sent(event).is_none_or(|event| sends.send(event).is_ok()),
                    None => answered.send(line).is_ok(),
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

    /// Whether a grab holds the test keyboard.
    pub(crate) fn held(&mut self) -> bool {
        match self.command("held").as_str() {
            "held" => true,
            "free" => false,
            other => panic!("the rig can't tell whether its keyboard is held: {other}"),
        }
    }

    /// What the virtual keyboard sent since last asked: once it begins, until it falls
    /// quiet — or nothing, if it doesn't begin within [`FIRST_SENT_WITHIN`].
    pub(crate) fn sent(&self) -> Vec<(Key, i32)> {
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
