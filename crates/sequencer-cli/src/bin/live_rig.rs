//! `sequencer-live-rig`: the live suite's hands and eyes on the device backend, run as root
//! through the user's cached sudo ticket. Never shipped — it exists only with the
//! `live-tests` feature, and `tests/live/rig.rs` is its one user.
//!
//! It makes a keyboard of its own (`linux::rig::TestKeyboard`) — and a mouse, when asked —
//! and answers one command per line on stdin, so that every assertion stays in the
//! unprivileged test:
//!
//! - `press KEY`, `release KEY`, `repeat KEY`: the test keyboard sends it; answers `ok`.
//! - `watch`: grabs sequencer's virtual keyboard and from then on says `out KEY VALUE` for
//!   every key it sends; answers `watching`, or `none` while no manager holds the devices.
//! - `held`: answers `held` while a grab holds the test keyboard, `free` once none does.
//! - `mouse`: plugs in a test mouse (`linux::rig::TestMouse`); answers `plugged`.
//! - `mpress BUTTON`, `mrelease BUTTON` (`left`, `right`, `middle`, `back`, `forward`), and
//!   `move ACROSS DOWN`: the test mouse sends it; answers `ok`.
//! - `mouse-held`: as `held`, for the test mouse.
//! - `watch-stand-in`: grabs the test mouse's stand-in once it is there, checked to carry the
//!   mouse's identity, and from then on says `in button BUTTON VALUE` and `in move ACROSS
//!   DOWN` for what it passes on; answers `watching`, or `none` when no stand-in came.
//! - `supervise`: plays a manager that stops answering — starts a real supervisor, has it
//!   take the grab on the test keyboard, and never beats; answers `supervised`.
//! - `replug`: a new test keyboard in the old one's place, as a keyboard back from a suspend
//!   comes back a new device; answers `replugged` once the new one has its node.
//!
//! It says `ready` once the keyboard has its device node, and `error WHY` for a command it
//! couldn't carry out. It ends at the end of its input — its test gone — and its devices,
//! the watches' grabs and any supervisor end with it.

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    // Its supervisor is this program run again, so the marker is routed first, as every
    // program that starts one must.
    if let Some(code) = sequencer_cli::supervise_if_asked(std::env::args_os()) {
        return ExitCode::from(code);
    }
    rig::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("sequencer-live-rig drives the Linux device backend, which this platform has not");
    ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
mod rig {
    use std::io::{self, BufRead as _, Write as _};
    use std::os::fd::AsFd as _;
    use std::process::{Command, ExitCode};

    use sequencer_cli::core::input::{Button, Key};
    use sequencer_cli::input::linux::rig::{
        PassedOn, StandInWatch, TEST_MOUSE_BUTTONS, TestKeyboard, TestMouse, Watch,
    };
    use sequencer_cli::input::linux::supervise::Supervisor;

    pub(super) fn main() -> ExitCode {
        let keyboard = match TestKeyboard::open() {
            Ok(keyboard) => keyboard,
            Err(err) => {
                say(&format!(
                    "error the test keyboard could not be made through /dev/uinput: {err}"
                ));
                return ExitCode::FAILURE;
            }
        };
        let mut rig = Devices {
            keyboard,
            mouse: None,
            supervised: None,
        };
        say("ready");
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            say(&rig.answer(line.trim()));
        }
        ExitCode::SUCCESS
    }

    /// What the rig has made, kept to its end.
    struct Devices {
        keyboard: TestKeyboard,
        /// The test mouse, once `mouse` plugs it in.
        mouse: Option<TestMouse>,
        /// A supervisor `supervise` started, never beaten.
        supervised: Option<Supervisor>,
    }

    impl Devices {
        /// What one command does, and the line that answers it.
        fn answer(&mut self, command: &str) -> String {
            let (verb, rest) = command.split_once(' ').unwrap_or((command, ""));
            let answered = match verb {
                "press" => send(&mut self.keyboard, rest, 1),
                "release" => send(&mut self.keyboard, rest, 0),
                "repeat" => send(&mut self.keyboard, rest, 2),
                "watch" => watch(),
                "held" => self.keyboard.is_held().map(held_or_free),
                "supervise" => supervise(&self.keyboard, &mut self.supervised),
                "replug" => replug(&mut self.keyboard),
                "mouse" => self.plug_mouse(),
                "mpress" => self.mouse().and_then(|mouse| click(mouse, rest, 1)),
                "mrelease" => self.mouse().and_then(|mouse| click(mouse, rest, 0)),
                "move" => self.mouse().and_then(|mouse| move_by(mouse, rest)),
                "mouse-held" => self
                    .mouse()
                    .and_then(|mouse| mouse.is_held())
                    .map(held_or_free),
                "watch-stand-in" => self.mouse().and_then(|mouse| watch_stand_in(mouse)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("no command {verb:?}"),
                )),
            };
            answered.unwrap_or_else(|err| format!("error {err}"))
        }

        /// Plugs the test mouse in.
        fn plug_mouse(&mut self) -> io::Result<String> {
            if self.mouse.is_some() {
                return Err(io::Error::other("the test mouse is plugged in already"));
            }
            self.mouse = Some(TestMouse::open()?);
            Ok("plugged".to_owned())
        }

        /// The test mouse, once plugged in.
        fn mouse(&mut self) -> io::Result<&mut TestMouse> {
            self.mouse
                .as_mut()
                .ok_or_else(|| io::Error::other("no test mouse: `mouse` plugs one in"))
        }
    }

    fn held_or_free(held: bool) -> String {
        if held { "held" } else { "free" }.to_owned()
    }

    /// Plays a manager that stops answering: starts a real supervisor — this program run
    /// again, as a manager starts one — has it take the grab on the test keyboard, and then,
    /// being no manager, never beats.
    fn supervise(
        keyboard: &TestKeyboard,
        supervised: &mut Option<Supervisor>,
    ) -> io::Result<String> {
        let mut program = Command::new(std::env::current_exe()?);
        program.arg(sequencer_cli::SUPERVISOR_MARKER);
        let supervisor = Supervisor::spawn(program)?;
        // The supervisor keeps its own copy of the open device, and the grab with it.
        let device = std::fs::File::open(keyboard.node())?;
        supervisor.grab(device.as_fd()).map_err(io::Error::other)?;
        *supervised = Some(supervisor);
        Ok("supervised".to_owned())
    }

    /// Puts a new test keyboard in the old one's place: the new one made first, then the old
    /// one gone — a keyboard back from a suspend, under a node of its own.
    fn replug(keyboard: &mut TestKeyboard) -> io::Result<String> {
        drop(std::mem::replace(keyboard, TestKeyboard::open()?));
        Ok("replugged".to_owned())
    }

    fn send(keyboard: &mut TestKeyboard, name: &str, value: i32) -> io::Result<String> {
        let key: Key = name.parse().map_err(|err| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?}: {err}"))
        })?;
        keyboard.send(key, value).map(|()| "ok".to_owned())
    }

    /// Grabs the virtual keyboard and, on a thread of its own, says what it sends for as
    /// long as it is there.
    fn watch() -> io::Result<String> {
        let Some(mut watch) = Watch::open()? else {
            return Ok("none".to_owned());
        };
        std::thread::Builder::new()
            .name("live-rig-watch".into())
            .spawn(move || {
                while let Ok(keys) = watch.next_keys() {
                    for (key, value) in keys {
                        say(&format!("out {key} {value}"));
                    }
                }
            })?;
        Ok("watching".to_owned())
    }

    /// The test mouse sends `button` (by its name: `back`) going down (`value` 1) or up (0).
    fn click(mouse: &mut TestMouse, name: &str, value: i32) -> io::Result<String> {
        let button: Button = TEST_MOUSE_BUTTONS
            .into_iter()
            .find(|button| button.to_string() == name)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("no button {name:?}"))
            })?;
        mouse.send(button, value).map(|()| "ok".to_owned())
    }

    /// The test mouse moves by `ACROSS DOWN`.
    fn move_by(mouse: &mut TestMouse, steps: &str) -> io::Result<String> {
        let parsed = steps
            .split_once(' ')
            .and_then(|(across, down)| Some((across.parse().ok()?, down.parse().ok()?)));
        let Some((across, down)) = parsed else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not ACROSS DOWN: {steps:?}"),
            ));
        };
        mouse.move_by(across, down).map(|()| "ok".to_owned())
    }

    /// Grabs the test mouse's stand-in and, on a thread of its own, says what it passes on for
    /// as long as it is there.
    fn watch_stand_in(mouse: &TestMouse) -> io::Result<String> {
        let Some(mut watch) = StandInWatch::open(mouse)? else {
            return Ok("none".to_owned());
        };
        std::thread::Builder::new()
            .name("live-rig-stand-in".into())
            .spawn(move || {
                while let Ok(passed) = watch.next_passed() {
                    for passed in passed {
                        say(&match passed {
                            PassedOn::Button(button, value) => {
                                format!("in button {button} {value}")
                            }
                            PassedOn::Moved(across, down) => format!("in move {across} {down}"),
                        });
                    }
                }
            })?;
        Ok("watching".to_owned())
    }

    /// One line on stdout, at once: the test reads each as it comes, from every thread.
    fn say(line: &str) {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
