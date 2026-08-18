//! The live multi-profile manager: one loop owning the sink, the focus watcher, the
//! emergency grab and every profile in the active set, rescanned live.
//!
//! X11-only for now, like the rest of the run path. The on-disk set it manages lives
//! in [`super::state`]; the per-profile execution it dispatches lives in
//! [`super::run`]. Hotkeys never wait on the maintenance cadence: every grab feeds one
//! shared queue and the loop *blocks* on it, waking the moment a key arrives.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sequencer_core::emit::Holdable;
use sequencer_core::input::{Key, Mods};
use sequencer_core::rng::Rng;
use sequencer_core::time::Clock as _;

use super::format::{chord_mods, primary_key, program_applies};
use super::state::{LockGuard, scan_active};
use super::{Profile, parse, run};
use crate::runtime::{CapturePump, EventPump, Wake};
use crate::{Error, Result, exit};
use sequencer_input::{Epoch, FocusWatcher, GrabCapture, SystemClock, XTestSink};

/// Ctrl+C (and SIGTERM) set this; the loop notices within one heartbeat and stops
/// through the normal teardown.
///
/// A signal that killed the process outright would skip the release ledger, and an
/// injected key-down with no matching up is a key stuck on the real keyboard — the
/// exact failure this whole path exists to prevent.
static INTERRUPTED: std::sync::LazyLock<std::sync::Arc<std::sync::atomic::AtomicUsize>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)));

/// The signal that ended a managed run, if one did.
///
/// The binary asks after the run and terminates *by* that signal (cleanup is already
/// done by then) — dying of the signal rather than exiting 0 is how the parent shell
/// learns the user's Ctrl+C landed, so it redraws its prompt instead of leaving the
/// terminal looking stuck. The library itself never re-raises; killing the process is
/// the process owner's call.
pub(crate) fn caught_interrupt() -> Option<i32> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        signal => i32::try_from(signal).ok(),
    }
}

/// Asks for SIGINT/SIGTERM to set [`INTERRUPTED`] instead of ending the process.
///
/// Best effort: if registration fails the signal keeps its default behaviour, which is
/// the old (worse) story, not a new one.
fn catch_interrupts() {
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if let Ok(value) = usize::try_from(signal) {
            let _ = signal_hook::flag::register_usize(
                signal,
                std::sync::Arc::clone(&INTERRUPTED),
                value,
            );
        }
    }
}

/// How often maintenance runs: focus is re-read and activations reconciled.
/// Events do not wait for this — the loop blocks on the shared stream and wakes
/// the moment a grabbed key arrives.
const MAINTENANCE_NANOS: u64 = 50_000_000;
/// The directory is rescanned every this many maintenance passes (~200ms) —
/// apply/unapply is a human-speed act.
const SCAN_EVERY: u32 = 4;

/// One applied profile and its runtime state.
struct Slot {
    profile: Profile,
    held: Vec<Holdable>,
    capture: Option<GrabCapture>,
    /// The profile wants to be active (its program matches, or it has none).
    want: bool,
    /// Activation failed on a grab conflict; warned once, retried on changes.
    blocked: bool,
}

impl Slot {
    /// Every bind's trigger, chords included — the grab layer takes them whole.
    fn triggers(&self) -> Vec<Vec<Key>> {
        self.profile
            .binds
            .iter()
            .map(|bind| bind.trigger.clone())
            .collect()
    }

    /// This profile's emergency chords the way their events arrive: primary + held
    /// modifier classes, one per alternative spelling.
    fn stop_chords(&self) -> Vec<(Key, Mods)> {
        run::stop_chords(&self.profile)
    }

    /// Whether an arriving event (primary + held modifier classes) is exactly one
    /// of this profile's triggers.
    fn owns_trigger(&self, key: Key, mods: Mods) -> bool {
        self.profile.binds.iter().any(|bind| {
            primary_key(&bind.trigger) == Some(key) && chord_mods(&bind.trigger) == mods
        })
    }
}

/// Runs the active set until it empties out, Ctrl+C (or SIGTERM), or a fatal error.
///
/// Everything user-visible goes through `out`, line per event: profiles appearing,
/// leaving, activating, going dormant, being refused. The lock lives exactly as
/// long as this frame.
pub(super) fn manage(out: &mut dyn std::io::Write, _lock: LockGuard) -> Result<u8> {
    if !sequencer_input::x11::is_usable() {
        return Err(Error::NotImplemented(
            "the profile manager needs an X11 session for now: it hears triggers by \
             key grab and injects through XTEST."
                .to_owned(),
        ));
    }
    let epoch = Epoch::start();
    let clock = SystemClock::from_epoch(epoch.instant());
    let mut sink = XTestSink::open()?;
    let mut rng = Rng::new(wall_seed());
    // Every grab — each profile's, and the emergency union — feeds this one queue,
    // so the loop below can block on all of them at once.
    let (queue, stream) = sequencer_input::CaptureStream::channel(256);
    let mut pump = CapturePump::new(stream, &clock);
    let mut slots: BTreeMap<String, Slot> = BTreeMap::new();
    let mut failed: BTreeSet<String> = BTreeSet::new();
    let mut emergency: Option<(BTreeSet<Vec<Key>>, GrabCapture)> = None;
    let mut focus: Option<FocusWatcher> = None;
    // One live connection for "is that key still down?" — the deferred-tap question.
    let probe = sequencer_input::KeyProbe::open();
    let mut passes: u32 = 0;

    writeln!(
        out,
        "managing {}; apply/unapply from any shell, Ctrl+C stops everything",
        super::state::active_dir()?.display()
    )?;
    out.flush()?;

    catch_interrupts();
    let mut next_maintenance = clock.now();
    let outcome: Result<&str> = loop {
        if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            break Ok("interrupted");
        }
        if clock.now() >= next_maintenance {
            let scanned_dir = passes.is_multiple_of(SCAN_EVERY);
            if let Err(err) = rescan(
                out,
                &epoch,
                &queue,
                &mut sink,
                &clock,
                &mut slots,
                &mut failed,
                &mut emergency,
                &mut focus,
                scanned_dir,
            ) {
                break Err(err);
            }
            // The first pass always scans, and apply links its files before taking the
            // lock — so an empty set here is a set that emptied: everything was
            // unapplied or emergency-stopped, and a manager with nothing to enforce
            // (refused links included: they never reload) has no reason to linger.
            if scanned_dir && slots.is_empty() {
                break Ok("no profiles left");
            }
            passes = passes.wrapping_add(1);
            next_maintenance = clock.now().saturating_add_nanos(MAINTENANCE_NANOS);
        }

        match pump.wait_until(Some(next_maintenance)) {
            Wake::Event(event) => {
                if let Err(err) = dispatch(
                    out,
                    event,
                    &mut sink,
                    &clock,
                    &mut rng,
                    &mut pump,
                    &mut slots,
                    probe.as_ref(),
                    focus.as_ref(),
                ) {
                    break Err(err);
                }
            }
            Wake::Deadline => {}
            Wake::Interrupted => break Ok("event sources closed"),
        }
    };

    teardown(&mut sink, &clock, &mut slots, &mut emergency);
    // Everything this manager was enforcing stops being enforced, so the directory that
    // *says* what is enforced is emptied with it. Otherwise `ls active/` would keep
    // claiming profiles are live with nothing running them.
    let cleared = super::state::clear_active().unwrap_or(0);
    let reason = outcome?;
    if !lost_terminal() {
        // A caught Ctrl+C already echoed `^C` and left the cursor mid-line; start clean.
        let lead = if reason == "interrupted" { "\n" } else { "" };
        writeln!(
            out,
            "{lead}stopped ({reason}); {cleared} profile(s) unapplied — nothing is enforced now"
        )?;
        out.flush()?;
    }
    Ok(exit::OK)
}

/// Routes one event: an emergency press stops exactly the slots that named that
/// chord, and anything else runs the slot whose trigger owns the key. Unknown
/// keys — a race with an ungrab — are ignored.
///
/// Stops are per-profile — nothing is global among scripts, so the manager keeps
/// going; Ctrl+C on the manager is the stop-everything. Events carry the modifier
/// classes held when they fired, so routing matches full chords: `ctrl w` is not
/// `ctrl shift w`, and only an exact stop-chord press stops its profiles.
#[allow(
    clippy::too_many_arguments,
    reason = "the manager's whole state, one event"
)]
fn dispatch(
    out: &mut dyn std::io::Write,
    event: sequencer_core::input::InputEvent,
    sink: &mut XTestSink,
    clock: &SystemClock,
    rng: &mut Rng,
    pump: &mut CapturePump<'_>,
    slots: &mut BTreeMap<String, Slot>,
    probe: Option<&sequencer_input::KeyProbe>,
    focus: Option<&FocusWatcher>,
) -> Result<()> {
    let (sequencer_core::input::EventKind::KeyDown(key)
    | sequencer_core::input::EventKind::KeyUp(key)) = event.kind
    else {
        return Ok(());
    };
    if matches!(event.kind, sequencer_core::input::EventKind::KeyDown(_)) {
        let stopping: Vec<String> = slots
            .iter()
            .filter(|(_, slot)| slot.stop_chords().contains(&(key, event.mods)))
            .map(|(name, _)| name.clone())
            .collect();
        if !stopping.is_empty() {
            for name in &stopping {
                stop_slot(out, sink, clock, slots, name)?;
            }
            return Ok(());
        }
    }
    let Some((name, slot)) = slots
        .iter_mut()
        .find(|(_, slot)| slot.capture.is_some() && slot.owns_trigger(key, event.mods))
    else {
        return Ok(());
    };
    let name = name.clone();
    // The gate re-asks focus live, so a running sequence stops the moment its
    // program loses focus rather than typing into whatever has it now. A profile
    // with no `program` has no gate — it is always licensed.
    let program = slot.profile.program.clone();
    let gate_fn = move || match &program {
        None => true,
        Some(patterns) => focus
            .and_then(FocusWatcher::focused_class)
            .is_none_or(|class| program_applies(patterns, &class)),
    };
    // A deferred tap asks the server whether the trigger's modifiers are still
    // physically down; with no probe the answer is "no" and taps fire immediately.
    let mods_down_fn =
        move |mods: Mods| probe.is_some_and(|probe| probe.any_down(&mods.watch_keys()));
    // Same server, single key: the wait a tap owes when its target re-presses the
    // trigger's own key.
    let key_down_fn = move |key: Key| probe.is_some_and(|probe| probe.any_down(&[key]));
    let stops = slot.stop_chords();
    let mut pumps = run::Pumps {
        triggers: pump,
        // Only this slot's own stop chords may end its in-flight sequence; another
        // profile's stop is not its business.
        stop: &stops,
        gate: slot
            .profile
            .program
            .as_ref()
            .map(|_| &gate_fn as &dyn Fn() -> bool),
        mods_down: Some(&mods_down_fn as &dyn Fn(Mods) -> bool),
        key_down: Some(&key_down_fn as &dyn Fn(Key) -> bool),
    };
    let mut executor = run::Executor::new(&slot.profile, sink, clock, rng, &mut slot.held);
    let outcome = executor.handle(event, &mut pumps)?;
    match outcome {
        None | Some(run::Outcome::SourceClosed) => Ok(()),
        Some(run::Outcome::EmergencyStop) => stop_slot(out, sink, clock, slots, &name),
    }
}

/// Ends one slot the way its emergency chord promises: capture stopped, held keys
/// released, its link removed from the set. The manager itself keeps going.
fn stop_slot(
    out: &mut dyn std::io::Write,
    sink: &mut dyn sequencer_core::emit::InputSink,
    clock: &SystemClock,
    slots: &mut BTreeMap<String, Slot>,
    name: &str,
) -> Result<()> {
    let Some(mut slot) = slots.remove(name) else {
        return Ok(());
    };
    if let Some(mut capture) = slot.capture.take() {
        capture.stop();
    }
    run::drain_held(sink, clock, &mut slot.held);
    // The link goes too: the directory says what is enforced, and this profile just
    // stopped being enforced — a rescan would otherwise bring it straight back.
    super::state::unlink_from_active(name)?;
    writeln!(out, "emergency stop: {name} unapplied")?;
    out.flush()?;
    Ok(())
}

/// Whether stdout is a terminal whose foreground has already moved on from us.
///
/// It happens when Ctrl+C kills an intermediary (a wrapper script, `cargo run` on old
/// versions) faster than this process finishes its teardown: the shell reaps the
/// intermediary and draws its prompt while we are still cleaning up. Writing the
/// farewell then would land *below* that prompt — a terminal that looks hung until
/// the user presses Enter — and under `stty tostop` the write would suspend us
/// outright. Cleanup itself already happened; only the goodbye is skipped.
fn lost_terminal() -> bool {
    use std::io::IsTerminal as _;
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return false;
    }
    let fd = std::os::fd::AsFd::as_fd(&stdout);
    nix::unistd::tcgetpgrp(fd).is_ok_and(|owner| owner != nix::unistd::getpgrp())
}

/// Lets every slot go of what it held, whatever ended the run.
fn teardown(
    sink: &mut XTestSink,
    clock: &SystemClock,
    slots: &mut BTreeMap<String, Slot>,
    emergency: &mut Option<(BTreeSet<Vec<Key>>, GrabCapture)>,
) {
    for (_, mut slot) in std::mem::take(slots) {
        if let Some(mut capture) = slot.capture.take() {
            capture.stop();
        }
        run::drain_held(sink, clock, &mut slot.held);
    }
    if let Some((_, mut capture)) = emergency.take() {
        capture.stop();
    }
    sequencer_core::emit::InputSink::release_all(sink);
}

/// One maintenance pass: reconcile the directory, the focus, the emergency grab
/// and each slot's activation with reality.
#[allow(
    clippy::too_many_arguments,
    reason = "the manager's whole state, one pass"
)]
fn rescan(
    out: &mut dyn std::io::Write,
    epoch: &Epoch,
    queue: &sequencer_input::EventQueue,
    sink: &mut dyn sequencer_core::emit::InputSink,
    clock: &SystemClock,
    slots: &mut BTreeMap<String, Slot>,
    failed: &mut BTreeSet<String>,
    emergency: &mut Option<(BTreeSet<Vec<Key>>, GrabCapture)>,
    focus: &mut Option<FocusWatcher>,
    rescan_dir: bool,
) -> Result<()> {
    if rescan_dir {
        reconcile_set(out, clock, sink, slots, failed)?;
    }
    reconcile_emergency(out, epoch, queue, slots, emergency);
    ensure_focus(out, slots, focus)?;
    reconcile_activation(out, epoch, queue, sink, clock, slots, focus.as_ref())?;
    out.flush()?;
    Ok(())
}

/// Keeps the emergency grab equal to the union of every slot's stop chord, alive
/// even while profiles are dormant — stopping a script must not depend on focus.
/// All grabs feed the shared `queue`; dispatch routes each stop to the slots that
/// named it.
fn reconcile_emergency(
    out: &mut dyn std::io::Write,
    epoch: &Epoch,
    queue: &sequencer_input::EventQueue,
    slots: &BTreeMap<String, Slot>,
    emergency: &mut Option<(BTreeSet<Vec<Key>>, GrabCapture)>,
) {
    let wanted: BTreeSet<Vec<Key>> = slots
        .values()
        .flat_map(|slot| slot.profile.emergency_stop.iter().cloned())
        .collect();
    let current = emergency
        .as_ref()
        .map(|(keys, _)| keys.clone())
        .unwrap_or_default();
    if wanted == current {
        return;
    }
    if let Some((_, mut capture)) = emergency.take() {
        capture.stop();
    }
    if !wanted.is_empty() {
        let chords: Vec<Vec<Key>> = wanted.iter().cloned().collect();
        match GrabCapture::start_into(epoch, &chords, queue.clone()) {
            Ok(capture) => *emergency = Some((wanted, capture)),
            Err(err) => {
                let _ = writeln!(out, "emergency-stop grab failed: {err}");
            }
        }
    }
}

/// Opens the focus watcher the first time a program-gated profile appears.
fn ensure_focus(
    out: &mut dyn std::io::Write,
    slots: &BTreeMap<String, Slot>,
    focus: &mut Option<FocusWatcher>,
) -> Result<()> {
    if focus.is_none() && slots.values().any(|slot| slot.profile.program.is_some()) {
        *focus = FocusWatcher::open();
        if focus.is_none() {
            writeln!(
                out,
                "focus is unreadable (no EWMH?): program-gated profiles stay dormant"
            )?;
        }
    }
    Ok(())
}

/// Grabs or releases each slot's triggers so its capture matches whether its program
/// has focus. Grabs feed the shared `queue`; a deactivating slot drains its ledger.
fn reconcile_activation(
    out: &mut dyn std::io::Write,
    epoch: &Epoch,
    queue: &sequencer_input::EventQueue,
    sink: &mut dyn sequencer_core::emit::InputSink,
    clock: &SystemClock,
    slots: &mut BTreeMap<String, Slot>,
    focus: Option<&FocusWatcher>,
) -> Result<()> {
    let class = focus.and_then(FocusWatcher::focused_class);
    for (name, slot) in slots.iter_mut() {
        slot.want = match &slot.profile.program {
            None => true,
            // Unreadable focus keeps the last decision rather than flapping.
            Some(patterns) => class
                .as_deref()
                .map_or(slot.want, |class| program_applies(patterns, class)),
        };
        match (slot.want, slot.capture.is_some()) {
            (true, false) => {
                match GrabCapture::start_into(epoch, &slot.triggers(), queue.clone()) {
                    Ok(capture) => {
                        slot.capture = Some(capture);
                        slot.blocked = false;
                        writeln!(out, "active: {name}")?;
                    }
                    Err(err) => {
                        if !slot.blocked {
                            writeln!(
                                out,
                                "blocked: {name}: {err} (another profile or program \
                                 owns a trigger; retrying as things change)"
                            )?;
                            slot.blocked = true;
                        }
                    }
                }
            }
            (false, true) => {
                if let Some(mut capture) = slot.capture.take() {
                    capture.stop();
                }
                run::drain_held(sink, clock, &mut slot.held);
                writeln!(out, "dormant: {name}")?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn reconcile_set(
    out: &mut dyn std::io::Write,
    clock: &SystemClock,
    sink: &mut dyn sequencer_core::emit::InputSink,
    slots: &mut BTreeMap<String, Slot>,
    failed: &mut BTreeSet<String>,
) -> Result<()> {
    let set = scan_active()?;
    let gone: Vec<String> = slots
        .keys()
        .filter(|name| !set.contains_key(*name))
        .cloned()
        .collect();
    for name in gone {
        if let Some(mut slot) = slots.remove(&name) {
            if let Some(mut capture) = slot.capture.take() {
                capture.stop();
            }
            run::drain_held(sink, clock, &mut slot.held);
            writeln!(out, "profile removed: {name}")?;
        }
    }
    failed.retain(|name| set.contains_key(name));

    for (name, path) in &set {
        if slots.contains_key(name) || failed.contains(name) {
            continue;
        }
        match load(path) {
            Ok(profile) => {
                writeln!(
                    out,
                    "profile applied: {name} ({} binds)",
                    profile.binds.len()
                )?;
                super::write_stop_hint(out, &profile)?;
                slots.insert(
                    name.clone(),
                    Slot {
                        profile,
                        held: Vec::new(),
                        capture: None,
                        want: false,
                        blocked: false,
                    },
                );
            }
            Err(detail) => {
                writeln!(out, "profile refused: {name}: {detail}")?;
                failed.insert(name.clone());
            }
        }
    }
    Ok(())
}

/// Reads and validates one profile for the manager.
fn load(path: &Path) -> std::result::Result<Profile, String> {
    let text = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    parse(&text)
}

/// A fresh seed per manager: RNG steps should differ between runs, and nothing
/// here needs more ceremony than the clock and the pid.
fn wall_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        });
    nanos ^ u64::from(std::process::id())
}
