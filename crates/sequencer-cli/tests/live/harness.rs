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
