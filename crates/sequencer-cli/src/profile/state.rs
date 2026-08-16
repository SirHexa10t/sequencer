//! The profile set on disk, and the lock that decides who runs it.
//!
//! The design is a lazy daemon. `~/.config/sequencer/active/` holds one symlink per
//! applied profile — **the directory is the set**: apply adds a link, unapply removes
//! one, `ls` is the status command, and nothing needs to remember what was applied
//! earlier. Every `profile-apply` invocation links its file, then checks the lock: if
//! no live manager holds it, this process takes it and manages everything in the
//! directory; if one does, this process just reports and exits — the running manager
//! notices the new link on its next scan. No sockets, no protocol: the manager already
//! wakes several times a second, so watching a directory is one metadata read per tick.
//!
//! Everything here is deliberately pure-ish (paths in, results out) so the lock and
//! set arithmetic are testable without X11 or a real home directory —
//! `SEQUENCER_CONFIG_DIR` overrides the location for tests and the adventurous.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The configuration directory: `$SEQUENCER_CONFIG_DIR`, else XDG, else `~/.config`.
pub(super) fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("SEQUENCER_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("sequencer"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config").join("sequencer"))
        .ok_or_else(|| {
            Error::NotImplemented(
                "no HOME to keep state under; set SEQUENCER_CONFIG_DIR".to_owned(),
            )
        })
}

/// Where the applied-profile symlinks live.
pub(super) fn active_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("active"))
}

/// The manager's PID file, next to the set it manages.
pub(super) fn lock_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("manager.pid"))
}

/// Links `source` into the active set, after the caller validated it.
pub(super) fn link_into_active(source: &Path) -> Result<Applied> {
    link_into(&active_dir()?, source)
}

/// [`link_into_active`] with the directory explicit, for tests.
///
/// Idempotent by canonical path: applying the same file twice is a no-op with a report,
/// not an error. A *different* file that would take the same name is refused — silently
/// replacing someone's gaming profile with an unrelated file of the same name is how
/// keys end up bound to surprises.
fn link_into(dir: &Path, source: &Path) -> Result<Applied> {
    std::fs::create_dir_all(dir).map_err(|source_err| Error::ScriptRead {
        path: dir.display().to_string(),
        source: source_err,
    })?;
    let canonical = canonicalize(source)?;
    let name = canonical
        .file_name()
        .ok_or_else(|| Error::Profile {
            path: source.display().to_string(),
            detail: "a profile needs a file name".to_owned(),
        })?
        .to_owned();
    let link = dir.join(&name);
    match canonicalize(&link) {
        Ok(existing) if existing == canonical => return Ok(Applied::AlreadyActive(link)),
        Ok(existing) => {
            return Err(Error::Profile {
                path: link.display().to_string(),
                detail: format!(
                    "the name {} is already applied from {}; rename one of the files",
                    name.display(),
                    existing.display()
                ),
            });
        }
        // A dangling symlink under this name is leftover state; replace it.
        Err(_) if link.is_symlink() => {
            let _ = std::fs::remove_file(&link);
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink(&canonical, &link).map_err(|source_err| Error::ScriptRead {
        path: link.display().to_string(),
        source: source_err,
    })?;
    Ok(Applied::Linked(link))
}

/// What linking a profile did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// The link was created.
    Linked(PathBuf),
    /// The same file was already in the set.
    AlreadyActive(PathBuf),
}

/// The active set right now: link name → resolved profile path.
pub(super) fn scan_active() -> Result<BTreeMap<String, PathBuf>> {
    scan(&active_dir()?)
}

/// [`scan_active`] with the directory explicit, for tests. Dangling links are warned
/// about by name so the manager can carry on without them.
fn scan(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut set = BTreeMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // No directory yet simply means nothing was ever applied.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(set),
        Err(err) => {
            return Err(Error::ScriptRead {
                path: dir.display().to_string(),
                source: err,
            });
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match canonicalize(&entry.path()) {
            Ok(target) => {
                set.insert(name, target);
            }
            Err(_) => {
                tracing::warn!(link = %name, "dangling profile link ignored");
            }
        }
    }
    Ok(set)
}

/// Empties the active set, returning how many links went.
///
/// Called when the manager stops: `active/` means "what a running manager is
/// enforcing", so leaving links behind after a quit would have the directory claim
/// profiles are live when nothing is. Only symlinks are removed — anything else in
/// there was put there by hand and is not ours to delete.
pub(super) fn clear_active() -> Result<usize> {
    Ok(clear(&active_dir()?))
}

/// [`clear_active`] with the directory explicit, for tests.
///
/// Infallible on purpose: this runs on the way out, where a link that refuses to go is
/// worth a log line, not an error that could mask why the manager was stopping.
fn clear(dir: &Path) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        } else {
            tracing::warn!(path = %path.display(), "not a link; left in place");
        }
    }
    removed
}

/// Removes `name` from the active set. `true` if there was something to remove.
pub(super) fn unlink_from_active(name: &str) -> Result<bool> {
    unlink_from(&active_dir()?, name)
}

/// [`unlink_from_active`] with the directory explicit, for tests.
fn unlink_from(dir: &Path, name: &str) -> Result<bool> {
    let link = dir.join(name);
    match std::fs::remove_file(&link) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::ScriptRead {
            path: link.display().to_string(),
            source: err,
        }),
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|source| Error::ScriptRead {
        path: path.display().to_string(),
        source,
    })
}

// --------------------------------------------------------------------------- the lock

/// Who manages the active set, if anyone.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Custody {
    /// This process took the lock and must manage.
    Ours(LockGuard),
    /// A live manager already runs; its PID, for the report.
    Theirs(u32),
}

/// Holds the PID file for the lifetime of the manager; best-effort removal on drop.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Takes the manager lock, or reports who holds it.
pub(super) fn acquire_lock() -> Result<Custody> {
    acquire_lock_at(lock_path()?)
}

/// [`acquire_lock`] with the path explicit, for tests.
///
/// A lock whose PID no longer runs is stale — a crashed or killed manager — and is
/// replaced. The create is `O_EXCL`, so two simultaneous applicants cannot both win:
/// the loser re-reads and yields to the winner's PID.
fn acquire_lock_at(path: PathBuf) -> Result<Custody> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    for _ in 0..4 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = write!(file, "{}", std::process::id());
                return Ok(Custody::Ours(LockGuard { path }));
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                match read_live_pid(&path) {
                    Some(pid) => return Ok(Custody::Theirs(pid)),
                    // Stale: remove and try to win the recreate race.
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(err) => {
                return Err(Error::ScriptRead {
                    path: path.display().to_string(),
                    source: err,
                });
            }
        }
    }
    Err(Error::NotImplemented(
        "the manager lock kept changing hands; try again".to_owned(),
    ))
}

/// The PID in the lock file, if it names a process that is still alive.
fn read_live_pid(path: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    pid_alive(pid).then_some(pid)
}

/// Whether `pid` names a running process: signal 0 probes without touching it.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    false
}

/// Whether a live manager currently holds the lock, without competing for it.
pub(super) fn current_manager() -> Result<Option<u32>> {
    Ok(read_live_pid(&lock_path()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sequencer-manager-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The directory is the set: linking adds, relinking the same file is a no-op,
    /// a different file under the same name is refused, unlinking removes.
    #[test]
    fn the_active_directory_is_the_set() {
        let base = temp_dir("set");
        let active = base.join("active");
        let profile = base.join("gaming.toml");
        std::fs::write(&profile, "[binds.F6]\nseq = [\"a\"]\n").unwrap();

        assert!(matches!(
            link_into(&active, &profile).unwrap(),
            Applied::Linked(_)
        ));
        assert!(matches!(
            link_into(&active, &profile).unwrap(),
            Applied::AlreadyActive(_)
        ));
        let set = scan(&active).unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains_key("gaming.toml"));

        // Same name, different file: refused, and the set is unchanged.
        let imposter_dir = base.join("elsewhere");
        std::fs::create_dir_all(&imposter_dir).unwrap();
        let imposter = imposter_dir.join("gaming.toml");
        std::fs::write(&imposter, "[binds.F7]\nseq = [\"b\"]\n").unwrap();
        let err = link_into(&active, &imposter).unwrap_err();
        assert!(err.to_string().contains("rename"), "{err}");
        assert_eq!(scan(&active).unwrap().len(), 1);

        assert!(unlink_from(&active, "gaming.toml").unwrap());
        assert!(!unlink_from(&active, "gaming.toml").unwrap());
        assert!(scan(&active).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    /// Stopping unapplies everything: the directory says what a running manager
    /// enforces, so it must not outlive the manager. Hand-placed real files are left —
    /// they were not ours to create and are not ours to delete.
    #[test]
    fn clearing_removes_links_and_spares_real_files() {
        let base = temp_dir("clear");
        let active = base.join("active");
        let profile = base.join("gaming.toml");
        std::fs::write(&profile, "[binds.F6]\nseq = [\"a\"]\n").unwrap();
        link_into(&active, &profile).unwrap();
        let stranger = active.join("notes.txt");
        std::fs::write(&stranger, "hand-placed").unwrap();

        assert_eq!(clear(&active), 1);
        let left = scan(&active).unwrap();
        assert!(!left.contains_key("gaming.toml"), "the link is gone");
        // A hand-placed file survives and still counts as applied — copying a profile in
        // rather than linking it is a legitimate way to use the directory.
        assert!(stranger.exists(), "a real file is not ours to delete");
        assert!(left.contains_key("notes.txt"));
        // Clearing an already-empty (or absent) set is a no-op, not an error.
        assert_eq!(clear(&active), 0);
        assert_eq!(clear(&base.join("never")), 0);
        let _ = std::fs::remove_dir_all(base);
    }

    /// Scanning an active dir that never existed is an empty set, not an error.
    #[test]
    fn a_missing_active_dir_is_an_empty_set() {
        let base = temp_dir("missing");
        assert!(scan(&base.join("never-created")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    /// First caller becomes the manager; a second yields to the first's live PID; a
    /// dead PID is stale and custody transfers.
    #[test]
    fn the_lock_knows_live_from_stale() {
        let base = temp_dir("lock");
        let lock = base.join("manager.pid");

        let Custody::Ours(guard) = acquire_lock_at(lock.clone()).unwrap() else {
            panic!("first caller should win the lock");
        };
        match acquire_lock_at(lock.clone()).unwrap() {
            Custody::Theirs(pid) => assert_eq!(pid, std::process::id()),
            Custody::Ours(_) => panic!("the live lock must be respected"),
        }
        drop(guard);
        assert!(!lock.exists(), "dropping the guard removes the lock file");

        // A crashed manager: PID of a child that has already exited.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(&lock, dead_pid.to_string()).unwrap();
        assert!(
            matches!(acquire_lock_at(lock).unwrap(), Custody::Ours(_)),
            "a dead manager's lock is stale"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
