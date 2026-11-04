//! `sequencer-live-rig`: the live suite's hands and eyes on the device backend, run as root
//! through the user's cached sudo ticket. Never shipped — it exists only with the
//! `live-tests` feature, and `tests/live/rig.rs` is its one user.
//!
//! It makes a keyboard of its own (`linux::rig::TestKeyboard`) and answers one command per
//! line on stdin, so that every assertion stays in the unprivileged test:
//!
//! - `press KEY`, `release KEY`, `repeat KEY`: the test keyboard sends it; answers `ok`.
//! - `watch`: grabs sequencer's virtual keyboard and from then on says `out KEY VALUE` for
//!   every key it sends; answers `watching`, or `none` while no manager holds the devices.
//! - `held`: answers `held` while a grab holds the test keyboard, `free` once none does.
//! - `supervise`: plays a manager that stops answering — starts a real supervisor, has it
//!   take the grab on the test keyboard, and never beats; answers `supervised`.
//!
//! It says `ready` once the keyboard has its device node, and `error WHY` for a command it
//! couldn't carry out. It ends at the end of its input — its test gone — and the keyboard,
//! the watch's grab and any supervisor end with it.

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

    use sequencer_cli::core::input::Key;
    use sequencer_cli::input::linux::rig::{TestKeyboard, Watch};
    use sequencer_cli::input::linux::supervise::Supervisor;

    pub(super) fn main() -> ExitCode {
        let mut keyboard = match TestKeyboard::open() {
            Ok(keyboard) => keyboard,
            Err(err) => {
                say(&format!(
                    "error the test keyboard could not be made through /dev/uinput: {err}"
                ));
                return ExitCode::FAILURE;
            }
        };
        // A supervisor `supervise` started, kept to the end and never beaten.
        let mut supervised = None;
        say("ready");
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            say(&answer(&mut keyboard, &mut supervised, line.trim()));
        }
        ExitCode::SUCCESS
    }

    /// What one command does, and the line that answers it.
    fn answer(
        keyboard: &mut TestKeyboard,
        supervised: &mut Option<Supervisor>,
        command: &str,
    ) -> String {
        let (verb, key) = command.split_once(' ').unwrap_or((command, ""));
        let answered = match verb {
            "press" => send(keyboard, key, 1),
            "release" => send(keyboard, key, 0),
            "repeat" => send(keyboard, key, 2),
            "watch" => watch(),
            "held" => keyboard
                .is_held()
                .map(|held| if held { "held" } else { "free" }.to_owned()),
            "supervise" => supervise(keyboard, supervised),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no command {verb:?}"),
            )),
        };
        answered.unwrap_or_else(|err| format!("error {err}"))
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

    /// One line on stdout, at once: the test reads each as it comes, from both threads.
    fn say(line: &str) {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
