//! Shared plumbing for the live suite: the serial lock, an isolated config dir,
//! and ways to run the real binary against it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// The compiled binary under test — cargo builds it for us and says where it is.
pub(crate) fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sequencer")
}

/// One test at a time: the tests share the X session, the real keyboard's modifier
/// state, and the sudo ticket. `--test-threads=1` is belt; this is braces.
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An isolated `SEQUENCER_CONFIG_DIR`, removed on drop. The real
/// `~/.config/sequencer` is never touched by any live test.
pub(crate) struct TempConfig {
    dir: PathBuf,
}

impl TempConfig {
    pub(crate) fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sequencer-live-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp config dir");
        Self { dir }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn active(&self) -> PathBuf {
        self.dir.join("active")
    }

    pub(crate) fn lock_file(&self) -> PathBuf {
        self.dir.join("manager.pid")
    }

    /// Writes a profile beside the config and returns its path.
    pub(crate) fn profile(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, text).expect("write profile");
        path
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Runs the binary to completion against `config`, stdin closed, streams merged.
pub(crate) fn run(config: &TempConfig, args: &[&str]) -> (std::process::ExitStatus, String) {
    let output = Command::new(bin())
        .args(args)
        .env("SEQUENCER_CONFIG_DIR", config.dir())
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status, text)
}

/// Whether nobody holds the manager's lock under `config`. The lock file itself stays: the
/// lock is an advisory `flock`, and keeping one stable inode is what makes taking it
/// race-free. What must be true once a manager is gone is that nobody holds it — which the
/// kernel guarantees, however the manager died.
#[cfg(target_os = "linux")]
pub(crate) fn lock_is_free(config: &TempConfig) -> bool {
    let Ok(file) = std::fs::File::open(config.lock_file()) else {
        return true;
    };
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).is_ok()
}

/// The file's content right now (empty if it does not exist yet).
pub(crate) fn read(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// Polls until `needle` appears in the file, up to `secs`.
pub(crate) fn await_line(log: &Path, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if read(log).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// How many injections the manager has logged (`-vv` debug lines).
pub(crate) fn inject_count(log: &Path) -> usize {
    read(log).matches("XTEST inject").count()
}

/// The PID in the *real* config's lock file, if that process is alive.
pub(crate) fn live_real_manager() -> Option<u32> {
    let config = std::env::var_os("SEQUENCER_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(|base| PathBuf::from(base).join("sequencer"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/sequencer"))
        })?;
    let pid: u32 = std::fs::read_to_string(config.join("manager.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
}

/// A manager child on the temp config, `-vv` logs and stdout merged into one file.
/// Dropping it kills anything still running — a leaked manager holds real grabs. Only
/// where a manager can run: Linux, with the X11 backend every profile manager is built on.
#[cfg(all(feature = "xtest", target_os = "linux"))]
pub(crate) struct Manager {
    child: std::process::Child,
    log: PathBuf,
    /// The manager process itself, which every signal goes to: the child — or, on the
    /// devices, the elevated run the child only waits on through sudo, which a signal to
    /// the child would never reach.
    pid: i32,
}

#[cfg(all(feature = "xtest", target_os = "linux"))]
impl Manager {
    /// `profile-apply` on `profiles`, considered started once it says it is managing. Its
    /// stdin is closed, so it never elevates.
    pub(crate) fn start(config: &TempConfig, profiles: &[&Path]) -> Self {
        let (child, log) = spawn_manager(config, profiles, |command| {
            command.stdin(Stdio::null());
        });
        let spawned = Self {
            pid: i32::try_from(child.id()).expect("pid fits"),
            child,
            log,
        };
        assert!(
            await_line(&spawned.log, "managing", 10),
            "child never said \"managing\": {}",
            read(&spawned.log)
        );
        spawned
    }

    /// `profile-apply` on `profiles` on the device backend, holding the rig's devices and
    /// nothing of the user's: stdin is the terminal, so session mode elevates on the cached
    /// sudo ticket. Started once it says it holds the rig's keyboard alone — the rig's mouse
    /// it holds only while a profile names its buttons.
    #[cfg(all(feature = "live-tests", target_os = "linux"))]
    pub(crate) fn start_on_devices(config: &TempConfig, profiles: &[&Path]) -> Self {
        use sequencer_cli::input::linux::grab::GRAB_ONLY_VARIABLE;
        use sequencer_cli::input::linux::rig::{TEST_DEVICES, TEST_KEYBOARD_NAME};

        let (child, log) = spawn_manager(config, profiles, |command| {
            command
                .stdin(Stdio::inherit())
                .env(GRAB_ONLY_VARIABLE, TEST_DEVICES);
        });
        let mut spawned = Self {
            pid: i32::try_from(child.id()).expect("pid fits"),
            child,
            log,
        };
        assert!(
            await_line(&spawned.log, &format!("holding: {TEST_KEYBOARD_NAME};"), 30),
            "the manager never held the rig's keyboard, and it alone: {}",
            read(&spawned.log)
        );
        spawned.pid = read(&config.lock_file())
            .trim()
            .parse()
            .expect("the lock names the manager");
        spawned
    }

    pub(crate) fn log(&self) -> &Path {
        &self.log
    }

    /// The manager process itself: on the devices, the elevated run, not the child waiting on
    /// it through sudo.
    #[cfg(all(feature = "live-tests", target_os = "linux"))]
    pub(crate) const fn pid(&self) -> i32 {
        self.pid
    }

    pub(crate) fn alive(&mut self) -> bool {
        self.child.try_wait().expect("child state").is_none()
    }

    pub(crate) fn interrupt(&self) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.pid),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("SIGINT lands");
    }

    /// Sends the manager `signal` — `SIGKILL`, to end it without its goodbye. Never a stop
    /// signal to a manager on the devices: it runs under sudo, which copies a stop — in its
    /// default pty mode onto its parent's whole process group, which is this test run, as
    /// one run found out by freezing whole (2026-11-03).
    #[cfg(all(feature = "live-tests", target_os = "linux"))]
    pub(crate) fn signal(&self, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.pid), signal)
            .expect("the signal lands");
    }

    /// Waits up to `secs` for the child to end on its own.
    pub(crate) fn ended_within(&mut self, secs: u64) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("child state") {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

#[cfg(all(feature = "xtest", target_os = "linux"))]
impl Drop for Manager {
    fn drop(&mut self) {
        // This runs on assertion failures too, so: graceful SIGINT first — the
        // manager's own teardown releases held keys, drops every grab and clears
        // its state, leaving nothing listening behind a red test — then SIGKILL
        // only if that is ignored. And never panic: this Drop runs during panics.
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let manager = nix::unistd::Pid::from_raw(self.pid);
        let _ = nix::sys::signal::kill(manager, nix::sys::signal::Signal::SIGINT);
        for _ in 0..40 {
            if !matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = nix::sys::signal::kill(manager, nix::sys::signal::Signal::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `profile-apply -vv` on `profiles` against `config`, as `configure` finishes it, and
/// where its log goes.
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn spawn_manager(
    config: &TempConfig,
    profiles: &[&Path],
    configure: impl FnOnce(&mut Command),
) -> (std::process::Child, PathBuf) {
    let log = config.dir().join("child.log");
    let out = std::fs::File::create(&log).expect("log file");
    let err = out.try_clone().expect("log file handle");
    let mut command = Command::new(bin());
    command
        .args(["profile-apply", "-vv"])
        .args(profiles)
        .env("SEQUENCER_CONFIG_DIR", config.dir())
        .env("NO_COLOR", "1")
        .stdout(out)
        .stderr(err);
    configure(&mut command);
    (command.spawn().expect("child spawns"), log)
}
