//! The profile set on disk, and the lock that decides who runs it.
//!
//! The design is a lazy daemon. `~/.config/sequencer/active/` holds one symlink per
//! applied profile — **the directory is the set**: apply adds a link, unapply removes
//! one, re-apply replaces the link (the fresh link is the manager's cue to reload, so
//! an edited profile takes effect without an unapply), `ls` is the status command, and
//! nothing needs to remember what was applied
//! earlier. Every `profile-apply` invocation links its file, then checks the lock: if
//! no live manager holds it, this process takes it and manages everything in the
//! directory; if one does, this process just reports and exits — the running manager
//! notices the new link on its next scan. No sockets, no protocol: the manager already
//! wakes several times a second, so watching a directory is one metadata read per tick.
//!
//! Everything here is deliberately pure-ish (paths in, results out) so the lock and
//! set arithmetic are testable without X11 or a real home directory —
//! `SEQUENCER_CONFIG_DIR` overrides the location for tests and the adventurous.

use std::collections::{BTreeMap, BTreeSet};
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
/// Applying the same file again REPLACES its link: the fresh link carries a new
/// timestamp, which is the running manager's cue to reload — edit and re-apply, no
/// unapply needed. A *different* file that would take the same name is still refused —
/// silently replacing someone's gaming profile with an unrelated file of the same name
/// is how keys end up bound to surprises.
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
    let mut reapplied = false;
    match canonicalize(&link) {
        Ok(existing) if existing == canonical => reapplied = true,
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
        // A dangling symlink under this name is leftover state; the place below
        // replaces it like any other occupant.
        Err(_) => {}
    }
    place_link(&canonical, &link)?;
    Ok(if reapplied {
        Applied::Reapplied(link)
    } else {
        Applied::Linked(link)
    })
}

/// Points `link` at `target`, replacing whatever holds that name — without the name
/// ever being absent.
///
/// The absence matters: a manager scanning the set in that gap sees a profile that is
/// not there, and a set that empties is a manager that stops. So the new symlink is
/// built under a temp name *outside* the set (a scan reads every entry it finds, so a
/// temp inside would briefly load as a second profile) and renamed over the
/// destination, which one filesystem does atomically. The fresh inode carries a fresh
/// timestamp either way, which is the manager's reload cue.
fn place_link(target: &Path, link: &Path) -> Result<()> {
    let failed = |path: &Path, source: std::io::Error| Error::ScriptRead {
        path: path.display().to_string(),
        source,
    };
    // Beside the set, not in it. Without a parent (a set at the filesystem root) there
    // is nowhere else to stage, and the direct replace below is the honest fallback.
    let Some(staging_dir) = link.parent().and_then(Path::parent) else {
        return replace_link(target, link);
    };
    let staging = staging_dir.join(format!(
        ".sequencer-relink-{}-{}",
        std::process::id(),
        link.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&staging);
    std::os::unix::fs::symlink(target, &staging).map_err(|err| failed(&staging, err))?;
    match std::fs::rename(&staging, link) {
        Ok(()) => Ok(()),
        // The set and its parent on different filesystems: rename cannot cross that,
        // so fall back to the unavoidable unlink-and-relink.
        Err(err) if err.raw_os_error() == Some(nix::libc::EXDEV) => {
            let _ = std::fs::remove_file(&staging);
            replace_link(target, link)
        }
        Err(err) => {
            let _ = std::fs::remove_file(&staging);
            Err(failed(link, err))
        }
    }
}

/// The non-atomic replace: remove, then link. Only for the cases [`place_link`] cannot
/// stage a rename for.
fn replace_link(target: &Path, link: &Path) -> Result<()> {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).map_err(|source| Error::ScriptRead {
        path: link.display().to_string(),
        source,
    })
}

/// What linking a profile did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// The link was created.
    Linked(PathBuf),
    /// The same file was already in the set; its link was replaced, so the manager
    /// reloads it.
    Reapplied(PathBuf),
}

/// When `name`'s entry in the active set was last (re)created or written — the
/// manager's reload cue: re-applying replaces the symlink, so even a quick
/// unlink-and-relink between two scans reads as a change.
pub(super) fn link_stamp(name: &str) -> Option<std::time::SystemTime> {
    stamp_in(&active_dir().ok()?, name)
}

/// [`link_stamp`] with the directory explicit, for tests. The link's OWN metadata,
/// never the target's: editing a profile alone must not reload it mid-run — the
/// re-apply is the user's say-so. (A real file copied into the set has only its own
/// timestamp, so editing that one does reload it, which is the honest reading of
/// "the directory is the set".)
fn stamp_in(dir: &Path, name: &str) -> Option<std::time::SystemTime> {
    std::fs::symlink_metadata(dir.join(name))
        .and_then(|meta| meta.modified())
        .ok()
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

/// Removes the named entries from the active set, returning how many went.
///
/// Named, never "everything": both callers know exactly which links stopped being
/// enforced — a stopping manager knows what it was enforcing, and an apply whose
/// manager never got going knows what it just linked. Clearing the whole directory
/// instead would delete links that arrived meanwhile, whose owner is whoever manages
/// next; that apply would have reported success onto cleanup that wiped its work.
pub(super) fn clear_active_named(names: &BTreeSet<String>) -> Result<usize> {
    Ok(clear(&active_dir()?, names))
}

/// [`clear_active_named`] with the directory explicit, for tests.
///
/// Only symlinks are removed — anything else in there was put there by hand and is not
/// ours to delete. Infallible on purpose: this runs on the way out, where a link that
/// refuses to go is worth a log line, not an error that could mask why the manager was
/// stopping.
fn clear(dir: &Path, names: &BTreeSet<String>) -> usize {
    let mut removed = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        if !names.contains(entry.file_name().to_string_lossy().as_ref()) {
            continue;
        }
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
#[derive(Debug)]
pub(super) enum Custody {
    /// This process took the lock and must manage.
    Ours(LockGuard),
    /// A live manager already runs; its PID, for the report.
    Theirs(u32),
}

/// Holds the manager lock for as long as it lives.
///
/// The lock is an advisory `flock` on the PID file, not the file's existence: the
/// kernel drops it when this process dies, whatever kills it. That is the whole point.
/// Liveness by PID cannot be trusted — a `SIGKILL`ed manager leaves its PID file
/// behind, and once the number is recycled (or while the process lingers as a zombie)
/// `kill(pid, 0)` says "alive", so every later apply would report onto a manager that
/// does not exist and quietly enforce nothing. The file's *contents* are still the PID,
/// but only as a label for the report.
#[derive(Debug)]
pub(super) struct LockGuard {
    // Held for its Drop: closing the file is what releases the flock. (`allow`, not
    // `expect`: without the X11 feature the whole module is already dead-code-allowed,
    // and an expectation nothing fulfils is itself a warning.)
    #[allow(dead_code)]
    lock: nix::fcntl::Flock<std::fs::File>,
}

/// Takes the manager lock, or reports who holds it.
pub(super) fn acquire_lock() -> Result<Custody> {
    acquire_lock_at(&lock_path()?)
}

/// [`acquire_lock`] with the path explicit, for tests.
///
/// The file is never deleted: an empty PID file is harmless, and keeping it means the
/// inode a waiting process locked is always the inode the next process opens — the
/// delete-while-another-holds-it race that a "remove the lock on the way out" protocol
/// invites cannot happen here.
fn acquire_lock_at(path: &Path) -> Result<Custody> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| Error::ScriptRead {
            path: path.display().to_string(),
            source,
        })?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            use std::io::{Seek as _, Write as _};
            // Best-effort label for whoever reports on us later.
            let mut file: &std::fs::File = &lock;
            let _ = file.set_len(0);
            let _ = file.rewind();
            let _ = write!(file, "{}", std::process::id());
            let _ = file.flush();
            Ok(Custody::Ours(LockGuard { lock }))
        }
        Err(_) => Ok(Custody::Theirs(read_pid(path).unwrap_or(0))),
    }
}

/// The PID the lock file names, whether or not anyone still holds the lock.
fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether a live manager currently holds the lock, without competing for it.
///
/// Probes with a *shared* non-blocking lock: it fails exactly when someone holds the
/// exclusive one, and two simultaneous probes do not shut each other out.
pub(super) fn current_manager() -> Result<Option<u32>> {
    Ok(lock_holder(&lock_path()?))
}

/// [`current_manager`] with the path explicit, for tests.
fn lock_holder(path: &Path) -> Option<u32> {
    let file = std::fs::OpenOptions::new().read(true).open(path).ok()?;
    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockSharedNonblock) {
        // Nobody holds it: whatever PID the file names has moved on.
        Ok(_probe) => None,
        Err(_) => read_pid(path),
    }
}

// ----------------------------------------------------------------- stopping in progress

/// Where a stopping manager says so.
fn stopping_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("manager.stopping"))
}

/// Marks the set as being torn down, until the returned guard drops.
///
/// A stopping manager un-enforces what it held, which means removing links. An apply
/// that lands in the middle of that would have its own fresh link swept up by cleanup
/// that had already decided what to remove. So the teardown announces itself, and
/// [`stopping_manager`] lets an apply wait for it to finish instead of racing it.
pub(super) fn mark_stopping() -> Option<Stopping> {
    mark_stopping_at(stopping_path().ok()?)
}

/// [`mark_stopping`] with the path explicit, for tests.
fn mark_stopping_at(path: PathBuf) -> Option<Stopping> {
    std::fs::write(&path, std::process::id().to_string()).ok()?;
    Some(Stopping { path })
}

/// The marker a stopping manager holds; removed when it is done.
#[derive(Debug)]
pub(super) struct Stopping {
    path: PathBuf,
}

impl Drop for Stopping {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The PID of a manager currently tearing down, if one says it is.
pub(super) fn stopping_manager() -> Result<Option<u32>> {
    Ok(stopping_at(&stopping_path()?))
}

/// [`stopping_manager`] with the path explicit, for tests.
fn stopping_at(path: &Path) -> Option<u32> {
    path.exists().then(|| read_pid(path).unwrap_or(0))
}

/// Drops a marker left behind by a manager that died mid-teardown.
pub(super) fn clear_stopping() -> Result<()> {
    let _ = std::fs::remove_file(stopping_path()?);
    Ok(())
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

    /// The directory is the set: linking adds, relinking the same file REPLACES the
    /// link (the fresh stamp is the manager's reload cue), a different file under the
    /// same name is refused, unlinking removes.
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
        let first_stamp = stamp_in(&active, "gaming.toml");
        assert!(first_stamp.is_some(), "a fresh link has a stamp");
        // A pause so the replacement's timestamp cannot collide on coarse filesystems.
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(matches!(
            link_into(&active, &profile).unwrap(),
            Applied::Reapplied(_)
        ));
        assert_ne!(
            stamp_in(&active, "gaming.toml"),
            first_stamp,
            "re-applying replaces the link; the new stamp is what the manager notices"
        );
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

    /// Stopping unapplies what was enforced: the directory says what a running manager
    /// enforces, so those entries must not outlive it. Hand-placed real files are left
    /// even when named — they were not ours to create and are not ours to delete.
    #[test]
    fn clearing_removes_links_and_spares_real_files() {
        let base = temp_dir("clear");
        let active = base.join("active");
        let profile = base.join("gaming.toml");
        std::fs::write(&profile, "[binds.F6]\nseq = [\"a\"]\n").unwrap();
        link_into(&active, &profile).unwrap();
        let stranger = active.join("notes.txt");
        std::fs::write(&stranger, "hand-placed").unwrap();

        let both: BTreeSet<String> = ["gaming.toml", "notes.txt"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(clear(&active, &both), 1);
        let left = scan(&active).unwrap();
        assert!(!left.contains_key("gaming.toml"), "the link is gone");
        // A hand-placed file survives and still counts as applied — copying a profile in
        // rather than linking it is a legitimate way to use the directory.
        assert!(stranger.exists(), "a real file is not ours to delete");
        assert!(left.contains_key("notes.txt"));
        // Clearing an already-empty (or absent) set is a no-op, not an error.
        assert_eq!(clear(&active, &both), 0);
        assert_eq!(clear(&base.join("never"), &both), 0);
        let _ = std::fs::remove_dir_all(base);
    }

    /// A stopping manager clears only what it was enforcing. A link that arrived while
    /// it was on its way out belongs to whoever comes next — sweeping it up would have
    /// an apply report success onto cleanup that then deleted its work.
    #[test]
    fn a_stopping_manager_clears_only_what_it_enforced() {
        let base = temp_dir("clear-named");
        let active = base.join("active");
        for name in ["mine.toml", "newcomer.toml"] {
            let profile = base.join(name);
            std::fs::write(&profile, "[binds.F6]\nseq = [\"a\"]\n").unwrap();
            link_into(&active, &profile).unwrap();
        }

        let enforced: BTreeSet<String> = ["mine.toml".to_owned()].into_iter().collect();
        assert_eq!(clear(&active, &enforced), 1);
        let left = scan(&active).unwrap();
        assert!(!left.contains_key("mine.toml"));
        assert!(
            left.contains_key("newcomer.toml"),
            "a link this manager never enforced survives its stop"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// Re-applying must never leave the name absent: a manager that looks a profile up
    /// mid-re-apply has to find the old link or the new one, never nothing — otherwise
    /// it drops a live profile, and an only child emptying the set stops the manager.
    /// Hammering a re-apply against a concurrent watcher is the only honest way to test
    /// it; this fails immediately against an unlink-and-relink implementation.
    ///
    /// Note what is *not* claimed: a concurrent directory **listing** may miss an entry
    /// being replaced, because `readdir` has no atomicity guarantee against concurrent
    /// modification. Lookup does, and that is why the manager decides removal by name.
    #[test]
    fn a_relink_is_never_momentarily_absent_by_name() {
        let base = temp_dir("relink-race");
        let active = base.join("active");
        let profile = base.join("gaming.toml");
        std::fs::write(&profile, "[binds.F6]\nseq = [\"a\"]\n").unwrap();
        link_into(&active, &profile).unwrap();

        let scanned = active.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher_stop = std::sync::Arc::clone(&stop);
        let watcher = std::thread::spawn(move || {
            let mut absent = 0_u32;
            let mut unreadable = 0_u32;
            while !watcher_stop.load(std::sync::atomic::Ordering::Relaxed) {
                // The two by-name questions the manager asks: the reload stamp, and
                // what the link resolves to.
                if stamp_in(&scanned, "gaming.toml").is_none() {
                    absent += 1;
                }
                if std::fs::canonicalize(scanned.join("gaming.toml")).is_err() {
                    unreadable += 1;
                }
            }
            (absent, unreadable)
        });

        for _ in 0..2_000 {
            link_into(&active, &profile).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let (absent, unreadable) = watcher.join().unwrap();

        assert_eq!(absent, 0, "the link's stamp vanished mid-re-apply");
        assert_eq!(unreadable, 0, "the link stopped resolving mid-re-apply");
        assert_eq!(scan(&active).unwrap().len(), 1);
        assert!(
            std::fs::read_dir(&base)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains("relink")),
            "staging links must not outlive the rename"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// Scanning an active dir that never existed is an empty set, not an error.
    #[test]
    fn a_missing_active_dir_is_an_empty_set() {
        let base = temp_dir("missing");
        assert!(scan(&base.join("never-created")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    /// Custody follows the held lock, not the file: the first caller wins, a second is
    /// told whose it is, and letting go frees it. (`flock` is per open file
    /// description, so two opens in one process contend exactly as two processes do.)
    #[test]
    fn custody_follows_the_held_lock() {
        let base = temp_dir("lock");
        let lock = base.join("manager.pid");

        let Custody::Ours(guard) = acquire_lock_at(&lock).unwrap() else {
            panic!("first caller should win the lock");
        };
        match acquire_lock_at(&lock).unwrap() {
            Custody::Theirs(pid) => assert_eq!(pid, std::process::id(), "the label is the PID"),
            Custody::Ours(_) => panic!("the held lock must be respected"),
        }
        assert_eq!(lock_holder(&lock), Some(std::process::id()));

        drop(guard);
        assert!(
            matches!(acquire_lock_at(&lock).unwrap(), Custody::Ours(_)),
            "a released lock is free, whatever the file still says"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// The PID in the file is a label, never the liveness test. A `SIGKILL`ed manager
    /// leaves its file behind; once that number is recycled — or while the process
    /// lingers unreaped — `kill(pid, 0)` says "alive", and believing it would have
    /// every later apply report onto a manager that does not exist while nothing
    /// enforced anything.
    #[test]
    fn a_live_pid_in_the_file_is_not_a_live_manager() {
        let base = temp_dir("lock-pid");
        let lock = base.join("manager.pid");

        // A PID that is certainly alive and certainly not a manager: this test process.
        std::fs::write(&lock, std::process::id().to_string()).unwrap();
        assert!(
            matches!(acquire_lock_at(&lock).unwrap(), Custody::Ours(_)),
            "nobody holds the lock, so custody is available"
        );
        assert_eq!(
            lock_holder(&lock),
            None,
            "and nothing reports a manager either"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// The teardown marker: present while a manager stops, gone after, and clearable
    /// when the manager that wrote it never came back to do so.
    #[test]
    fn the_stopping_marker_tells_an_apply_to_wait() {
        let base = temp_dir("stopping");
        let marker_path = base.join("manager.stopping");

        assert_eq!(stopping_at(&marker_path), None);
        let marker = mark_stopping_at(marker_path.clone()).expect("the marker is written");
        assert_eq!(stopping_at(&marker_path), Some(std::process::id()));
        drop(marker);
        assert_eq!(
            stopping_at(&marker_path),
            None,
            "the guard removes it, so a waiting apply is released"
        );

        // A manager that died mid-teardown leaves it behind: leaking the guard is
        // exactly that, and removing the file is how an apply stops waiting forever.
        std::mem::forget(mark_stopping_at(marker_path.clone()).expect("marker"));
        assert!(stopping_at(&marker_path).is_some());
        std::fs::remove_file(&marker_path).unwrap();
        assert_eq!(stopping_at(&marker_path), None);
        let _ = std::fs::remove_dir_all(base);
    }
}
