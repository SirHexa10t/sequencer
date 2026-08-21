//! The apply/unapply commands: validate, link into the active set, manage or report.
//!
//! The pieces live beside this file, one concern each: [`mod@format`] says what a binds
//! file may mean, [`check`] reports on files without applying them, [`run`] executes one
//! profile's events, [`state`] owns the on-disk active set, and the manager module runs
//! the live multi-profile loop. This file is the command surface stitching them together
//! — plus the picker that makes unapplying humane.

pub(crate) mod run;

// The state layer is symlinks and PID files — Unix machinery; without the X11 feature
// only unapply's half of it runs, so the apply half is allowed to sit idle.
#[cfg(target_os = "linux")]
#[cfg_attr(
    not(feature = "xtest"),
    allow(dead_code, reason = "the apply side waits for the X11 feature")
)]
mod state;

#[cfg(all(feature = "xtest", target_os = "linux"))]
mod manager;

use crate::args::{ProfileApplyArgs, ProfileUnapplyArgs};
use crate::{Deps, Error, Result, exit};

mod check;
mod format;

#[cfg(all(feature = "xtest", target_os = "linux"))]
pub(crate) use manager::caught_interrupt;

pub(crate) use check::profile_check;
pub(crate) use format::{Action, Bind, Loops, Profile, Step, parse};

/// `sequencer profile-apply`.
///
/// Validates every file, links each into the active set — re-linking an applied one,
/// which is how an edited profile takes effect without an unapply: the manager reloads
/// on the fresh link — then either becomes the manager (no live one holds the lock) or
/// reports the one that will pick the links up.
///
/// # Errors
///
/// If a file cannot be read, does not parse, or fails validation; or if there is no
/// backend that can run the manager here.
pub(crate) fn profile_apply(args: &ProfileApplyArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let files = expand_profile_args(&args.files)?;
    let mut parsed = Vec::with_capacity(files.len());
    for file in &files {
        let path = file.display().to_string();
        let text = std::fs::read_to_string(file).map_err(|source| Error::ScriptRead {
            path: path.clone(),
            source,
        })?;
        let profile = parse(&text).map_err(|detail| Error::Profile { path, detail })?;
        parsed.push(profile);
    }

    // The injected pair is the test seam: run the first profile directly, touching no
    // on-disk state. The fixed seed keeps injected runs replayable.
    if let (Some(sink), Some(pump)) = (deps.sink.as_deref_mut(), deps.pump.as_deref_mut()) {
        run::run(&parsed[0], sink, deps.clock, pump, 0)?;
        return Ok(exit::OK);
    }

    #[cfg(all(feature = "xtest", target_os = "linux"))]
    {
        refuse_circles_across_the_set(&files, &parsed)?;

        // A manager on its way out is about to remove the links it was enforcing, and
        // it decided which those are before we got here. Linking into that would be
        // reporting success onto cleanup — so wait for it to finish first.
        await_stopping_manager(deps)?;

        let mut placed = Vec::with_capacity(parsed.len());
        for (file, profile) in files.iter().zip(&parsed) {
            placed.push((state::link_into_active(file)?, profile));
        }
        // Custody is decided before announcing because it decides the announcement:
        // attaching to a live manager means ITS announcements land in another window,
        // so this window repeats each stop hint; becoming the manager means they land
        // right below in this same window, where repeating them is noise. The links
        // are on disk first either way — the manager's opening scan must find them.
        match state::acquire_lock()? {
            state::Custody::Theirs(pid) => {
                for (applied, profile) in &placed {
                    announce(deps.out, applied, profile, true)?;
                }
                let reapplied = placed
                    .iter()
                    .any(|(applied, _)| matches!(applied, state::Applied::Reapplied(_)));
                if reapplied {
                    writeln!(
                        deps.out,
                        "{} (re-applying) onto an {} manager (PID {pid})",
                        crate::style::key("updating"),
                        crate::style::key("existing")
                    )?;
                } else {
                    writeln!(
                        deps.out,
                        "{} to an {} manager (PID {pid})",
                        crate::style::key("adding"),
                        crate::style::key("existing")
                    )?;
                }
                Ok(exit::OK)
            }
            state::Custody::Ours(lock) => {
                for (applied, profile) in &placed {
                    announce(deps.out, applied, profile, false)?;
                }
                deps.out.flush()?;
                let ours: std::collections::BTreeSet<String> = placed
                    .iter()
                    .filter_map(|(applied, _)| match applied {
                        state::Applied::Linked(link) | state::Applied::Reapplied(link) => link
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned()),
                    })
                    .collect();
                let outcome = manager::manage(deps.out, lock);
                if outcome.is_err() {
                    // The manager never got going (or died on an error), so nothing is
                    // enforcing the links this call just made. Only those: anything
                    // that arrived meanwhile belongs to whoever manages next, and
                    // sweeping it up would delete an apply's work behind its back.
                    let _ = state::clear_active_named(&ours);
                }
                outcome
            }
        }
    }
    #[cfg(not(all(feature = "xtest", target_os = "linux")))]
    {
        Err(Error::NotImplemented(
            "profile-apply runs on X11 only for now, and this build has no X11 backend.".to_owned(),
        ))
    }
}

/// Refuses the whole apply if the set it would create contains a feedback circle.
///
/// Injections pass through grabs like real presses, and EVERY profile's grabs hear
/// them — so a circle can span files. The whole future set is checked before anything
/// links: this batch, plus what is already applied (minus the versions this batch
/// replaces). The manager re-checks at load, for links made by hand.
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn refuse_circles_across_the_set(files: &[std::path::PathBuf], parsed: &[Profile]) -> Result<()> {
    let batch_names: Vec<String> = files.iter().map(|file| linked_name(file)).collect();
    let mut already_applied: Vec<(String, Profile)> = Vec::new();
    for (name, path) in state::scan_active()? {
        if batch_names.contains(&name) {
            continue;
        }
        // An unreadable or unparsable link is not running (the manager refused it
        // too), so it is no part of the live graph.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(profile) = parse(&text) else {
            continue;
        };
        already_applied.push((name, profile));
    }
    let mut graph: Vec<(&str, &Profile)> = batch_names
        .iter()
        .map(String::as_str)
        .zip(parsed.iter())
        .collect();
    graph.extend(
        already_applied
            .iter()
            .map(|(name, profile)| (name.as_str(), profile)),
    );
    if let Some(circle) = format::trigger_cycle(&graph) {
        return Err(Error::Profile {
            path: "profile-apply".to_owned(),
            detail: format!(
                "refused: these binds would trigger each other in a circle: {}",
                circle.join(" -> ")
            ),
        });
    }
    Ok(())
}

/// How long an apply waits for a stopping manager before calling the marker stale.
/// Generous: a teardown is a key-release ledger and a handful of unlinks, so anything
/// approaching this means the manager died mid-stop and will never clear it.
#[cfg(all(feature = "xtest", target_os = "linux"))]
const STOPPING_WAIT_NANOS: u64 = 10_000_000_000;
/// How often to look while waiting.
#[cfg(all(feature = "xtest", target_os = "linux"))]
const STOPPING_POLL_NANOS: u64 = 50_000_000;

/// Waits for a manager that is tearing down to finish, so this apply's links are made
/// into a set nobody is still cleaning up — and so the lock is free by the time we ask
/// for it, which is what lets this process become the next manager instead of reporting
/// onto one that has gone.
///
/// A marker nobody clears is a manager that died mid-teardown; after
/// [`STOPPING_WAIT_NANOS`] we say so, drop it, and carry on rather than refusing to
/// work because of leftover state.
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn await_stopping_manager(deps: &mut Deps<'_>) -> Result<()> {
    let Some(pid) = state::stopping_manager()? else {
        return Ok(());
    };
    writeln!(
        deps.out,
        "waiting for the stopping manager (PID {pid}) to finish unapplying"
    )?;
    deps.out.flush()?;
    let deadline = deps.clock.now().saturating_add_nanos(STOPPING_WAIT_NANOS);
    while deps.clock.now() < deadline {
        deps.clock
            .sleep_until(deps.clock.now().saturating_add_nanos(STOPPING_POLL_NANOS));
        if state::stopping_manager()?.is_none() {
            return Ok(());
        }
    }
    writeln!(
        deps.out,
        "that manager never finished stopping; clearing its marker and carrying on"
    )?;
    state::clear_stopping()
}

/// `sequencer profile-unapply`.
///
/// With names, removes those from the active set. Without, offers a numbered picker on
/// the terminal. Either way a running manager notices within a moment; with no manager,
/// editing the set is still the honest operation — the directory is the state.
///
/// # Errors
///
/// If a given name matches nothing, or the picker is asked for without a terminal.
#[cfg(target_os = "linux")]
pub(crate) fn unprofile_apply(args: &ProfileUnapplyArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let set = state::scan_active()?;
    if set.is_empty() {
        writeln!(deps.out, "nothing is applied.")?;
        return Ok(exit::OK);
    }
    let names: Vec<String> = set.keys().cloned().collect();

    let chosen = if args.names.is_empty() {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            return Err(Error::Profile {
                path: "profile-unapply".to_owned(),
                detail: format!(
                    "no names given and no terminal for the picker; applied: {}",
                    names.join(", ")
                ),
            });
        }
        for (index, name) in names.iter().enumerate() {
            let target = set
                .get(name)
                .map_or_else(String::new, |path| format!("  ->  {}", path.display()));
            writeln!(deps.out, "  {}) {name}{target}", index + 1)?;
        }
        writeln!(deps.out, "unapply which? (numbers or names, 'all'):")?;
        deps.out.flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(Error::from)?;
        parse_selection(&line, &names).map_err(|detail| Error::Profile {
            path: "profile-unapply".to_owned(),
            detail,
        })?
    } else {
        resolve_names(&args.names, &names).map_err(|detail| Error::Profile {
            path: "profile-unapply".to_owned(),
            detail,
        })?
    };

    for name in &chosen {
        if state::unlink_from_active(name)? {
            writeln!(deps.out, "unapplied: {name}")?;
        }
    }
    if let Some(pid) = state::current_manager()? {
        writeln!(
            deps.out,
            "the manager (PID {pid}) picks up removals within a moment."
        )?;
    }
    Ok(exit::OK)
}

/// `sequencer profile-unapply`, where no profile can ever have been applied.
///
/// # Errors
///
/// Always: the state layer is Linux-only, like everything else here.
#[cfg(not(target_os = "linux"))]
pub(crate) fn unprofile_apply(_args: &ProfileUnapplyArgs, deps: &mut Deps<'_>) -> Result<u8> {
    writeln!(deps.out, "nothing is applied.")?;
    Ok(exit::OK)
}

/// Expands directory arguments into the `.toml` files directly inside them, in name
/// order; plain files pass through untouched, so directories and files mix freely.
/// Non-recursive on purpose: a nested directory is organization, not a request.
fn expand_profile_args(given: &[std::path::PathBuf]) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::with_capacity(given.len());
    for path in given {
        if !path.is_dir() {
            files.push(path.clone());
            continue;
        }
        let listing = |source| Error::ScriptRead {
            path: path.display().to_string(),
            source,
        };
        let mut found = Vec::new();
        for entry in std::fs::read_dir(path).map_err(listing)? {
            let candidate = entry.map_err(listing)?.path();
            let is_toml = candidate
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"));
            if is_toml && candidate.is_file() {
                found.push(candidate);
            }
        }
        // An empty expansion would silently apply nothing — refuse with the reason,
        // like every other input that does not mean what it appears to.
        if found.is_empty() {
            return Err(Error::Profile {
                path: path.display().to_string(),
                detail: "this directory holds no .toml profiles".to_owned(),
            });
        }
        found.sort();
        files.append(&mut found);
    }
    Ok(files)
}

/// The name a file takes in the active set — the canonical file name, the same rule
/// [`state`]'s linking uses, so batch entries can be matched against applied ones.
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn linked_name(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|canonical| {
            canonical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// Turns picker input into set names: numbers, names, or `all`.
fn parse_selection(input: &str, names: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut chosen = Vec::new();
    for token in input.split_whitespace() {
        if token.eq_ignore_ascii_case("all") {
            return Ok(names.to_vec());
        }
        if let Ok(number) = token.parse::<usize>() {
            let name = number
                .checked_sub(1)
                .and_then(|index| names.get(index))
                .ok_or_else(|| format!("{number} is not on the list (1..={})", names.len()))?;
            chosen.push(name.clone());
            continue;
        }
        chosen.push(resolve_one(token, names)?);
    }
    if chosen.is_empty() {
        return Err("nothing chosen".to_owned());
    }
    chosen.dedup();
    Ok(chosen)
}

/// Resolves user-given names (with or without `.toml`, or full paths) to set names.
fn resolve_names(given: &[String], names: &[String]) -> std::result::Result<Vec<String>, String> {
    given
        .iter()
        .map(|token| resolve_one(token, names))
        .collect()
}

/// One profile's apply-time line; `with_hint` adds its stop chord (the attach case,
/// where the manager's own hint-bearing announcement lands in another window).
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn announce(
    out: &mut dyn std::io::Write,
    applied: &state::Applied,
    profile: &Profile,
    with_hint: bool,
) -> Result<()> {
    match applied {
        state::Applied::Linked(link) => {
            writeln!(
                out,
                "applied: {} ({} binds)",
                link.display(),
                profile.binds.len()
            )?;
            if with_hint {
                write_stop_hint(out, profile)?;
            }
        }
        state::Applied::Reapplied(link) => {
            // Red on purpose: the profile was already in the set, so this apply
            // REPLACED it — worth a glance, not just a shrug.
            writeln!(
                out,
                "{}: {}",
                crate::style::alarm("already applied"),
                link.display()
            )?;
            if with_hint {
                write_stop_hint(out, profile)?;
            }
        }
    }
    Ok(())
}

/// The one line worth knowing after a profile lands: how to make it stop.
#[cfg(all(feature = "xtest", target_os = "linux"))]
fn write_stop_hint(out: &mut dyn std::io::Write, profile: &Profile) -> Result<()> {
    if profile.emergency_stop.is_empty() {
        return Ok(());
    }
    let chords = profile
        .emergency_stop
        .iter()
        .map(|chord| crate::style::stopper(&format::chord_text(chord)))
        .collect::<Vec<_>>()
        .join(" or ");
    writeln!(
        out,
        "to {} this script, press: {chords}",
        crate::style::stopper("stop")
    )?;
    Ok(())
}

fn resolve_one(token: &str, names: &[String]) -> std::result::Result<String, String> {
    let base = std::path::Path::new(token)
        .file_name()
        .map_or(token, |name| name.to_str().unwrap_or(token));
    for candidate in [base.to_owned(), format!("{base}.toml")] {
        if names.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!(
        "`{token}` is (already) {}; list of applied: {}",
        crate::style::alarm("not applied"),
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The picker takes numbers, names (with or without .toml), paths, or `all` — and
    /// says what it cannot place instead of guessing.
    #[test]
    fn the_unapply_picker_understands_people() {
        let names = vec!["browser.toml".to_owned(), "gaming.toml".to_owned()];
        assert_eq!(parse_selection("2", &names).unwrap(), vec!["gaming.toml"]);
        assert_eq!(
            parse_selection("1 2", &names).unwrap(),
            vec!["browser.toml", "gaming.toml"]
        );
        assert_eq!(parse_selection("all", &names).unwrap(), names);
        assert_eq!(
            parse_selection("gaming", &names).unwrap(),
            vec!["gaming.toml"]
        );
        assert_eq!(
            parse_selection("/somewhere/else/gaming.toml", &names).unwrap(),
            vec!["gaming.toml"]
        );
        assert!(
            parse_selection("3", &names)
                .unwrap_err()
                .contains("not on the list")
        );
        assert!(
            parse_selection("music", &names)
                .unwrap_err()
                .contains("not applied")
        );
        assert!(
            parse_selection("", &names)
                .unwrap_err()
                .contains("nothing chosen")
        );
    }
}
