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

use super::format::{chord_mods, primary_key, program_applies, trigger_cycle};
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
    /// The grab signatures this slot's capture holds right now — the routing truth:
    /// an event belongs to whoever GRABBED it, never to whoever declares it, which
    /// is what lets a yielded bind's events arrive at its claimant instead.
    grabbed: BTreeSet<(Key, Mods)>,
    /// Signatures this slot yielded to a higher-precedence claimant, with the
    /// claimant's name — kept so only the transitions get printed.
    yielded: BTreeMap<(Key, Mods), String>,
    /// The link's own timestamp when this slot loaded. A re-apply replaces the link,
    /// and the changed stamp is the cue to reload — editing the FILE alone changes
    /// nothing here, which is what keeps a running profile from mutating under the
    /// user's hands without their say-so.
    stamp: Option<std::time::SystemTime>,
    /// The profile wants to be active (its program matches, or it has none).
    want: bool,
    /// Activation failed on a grab conflict; warned once, retried on changes.
    blocked: bool,
}

impl Slot {
    /// This profile's emergency chords the way their events arrive: primary + held
    /// modifier classes, one per alternative spelling.
    fn stop_chords(&self) -> Vec<(Key, Mods)> {
        run::stop_chords(&self.profile)
    }
}

/// One slot's share of the grab space for this pass.
#[derive(Debug, Default)]
struct Claim {
    /// The trigger chords to grab — one representative per folded signature.
    chords: Vec<Vec<Key>>,
    /// The signatures those chords cover.
    sigs: BTreeSet<(Key, Mods)>,
    /// Signatures someone earlier already claimed, and who.
    yielded: BTreeMap<(Key, Mods), String>,
}

/// Claims every trigger of `profile` whose folded signature is still free, and
/// records a yield for each one already spoken for. Callers invoke this in
/// precedence order — stop chords seeded first (safety is nobody's to shadow), then
/// program-gated slots, then global ones — which IS the precedence system: while a
/// gated profile is focused, its colliding binds eclipse a global's bind by bind,
/// and everything swaps back once the claimant goes dormant. Side-variant spellings
/// within one profile still collapse to a single grab (routing by probe, as ever).
fn claim_triggers(
    profile: &Profile,
    claimant: &str,
    claimed: &mut BTreeMap<(Key, Mods), String>,
) -> Claim {
    let mut claim = Claim::default();
    for bind in &profile.binds {
        let Some(primary) = primary_key(&bind.trigger) else {
            continue;
        };
        let signature = (primary, chord_mods(&bind.trigger));
        if claim.sigs.contains(&signature) {
            continue; // a side-variant spelling of one this slot already grabs
        }
        if let Some(owner) = claimed.get(&signature) {
            claim
                .yielded
                .entry(signature)
                .or_insert_with(|| owner.clone());
        } else {
            claimed.insert(signature, claimant.to_owned());
            claim.sigs.insert(signature);
            claim.chords.push(bind.trigger.clone());
        }
    }
    claim
}

/// The bracketed spelling of the bind owning `signature` in `profile` — the first
/// spelling, when side-variants share it.
fn label_of(profile: &Profile, signature: (Key, Mods)) -> String {
    profile
        .binds
        .iter()
        .find(|bind| {
            primary_key(&bind.trigger) == Some(signature.0)
                && chord_mods(&bind.trigger) == signature.1
        })
        .map_or_else(String::new, |bind| {
            format!("[binds.\"{}\"]", bind.trigger_text)
        })
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
    // Refused links, with the stamp they were refused at: a re-apply replaces the
    // link, and the changed stamp is what earns a retry.
    let mut failed: BTreeMap<String, Option<std::time::SystemTime>> = BTreeMap::new();
    let mut emergency: Option<(BTreeSet<Vec<Key>>, GrabCapture)> = None;
    let mut focus: Option<FocusWatcher> = None;
    // One live connection for "is that key still down?" — the deferred-tap question.
    let probe = sequencer_input::KeyProbe::open();
    // Contaminated mirrors tap between lifted modifiers — this owns that path;
    // without it they fall back to plain (recolourable) injection.
    let lifter = sequencer_input::LiftedTap::open();
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
                    lifter.as_ref(),
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
    lifter: Option<&sequencer_input::LiftedTap>,
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
        .find(|(_, slot)| slot.grabbed.contains(&(key, event.mods)))
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
    // A tap whose trigger holds classes its target does not name fires between
    // lifted modifiers: the standing keys come up, the tap lands clean, and the
    // hand's keys go back down — one backend call on one connection.
    let lift_fn = move |targets: &[Holdable],
                        held: Mods,
                        primary: Option<Key>,
                        hold: sequencer_core::time::Duration| {
        lifter.is_some_and(|lifter| lifter.tap(targets, held, primary, hold))
    };
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
        lift: Some(&lift_fn as run::LiftTap<'_>),
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
    failed: &mut BTreeMap<String, Option<std::time::SystemTime>>,
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
/// has focus — and who outranks it. The grab space is claimed in precedence order
/// each pass: stop chords first, then program-gated slots, then global ones, names
/// alphabetical within a tier. A colliding bind yields individually (its siblings
/// stay live) and resumes the moment its claimant goes dormant. Grabs feed the
/// shared `queue`; a deactivating slot drains its ledger.
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
    for slot in slots.values_mut() {
        slot.want = match &slot.profile.program {
            None => true,
            // Unreadable focus keeps the last decision rather than flapping.
            Some(patterns) => class
                .as_deref()
                .map_or(slot.want, |class| program_applies(patterns, class)),
        };
    }

    // The claim pass. Stops are seeded for every slot, dormant ones included — the
    // emergency grab stays alive regardless of focus, and no bind may shadow it.
    let mut claimed: BTreeMap<(Key, Mods), String> = slots
        .values()
        .flat_map(Slot::stop_chords)
        .map(|stop| (stop, "the emergency grab".to_owned()))
        .collect();
    let in_order: Vec<String> = slots
        .iter()
        .filter(|(_, slot)| slot.want && slot.profile.program.is_some())
        .map(|(name, _)| name.clone())
        .chain(
            slots
                .iter()
                .filter(|(_, slot)| slot.want && slot.profile.program.is_none())
                .map(|(name, _)| name.clone()),
        )
        .collect();
    let mut claims: BTreeMap<String, Claim> = BTreeMap::new();
    for name in &in_order {
        let claim = claim_triggers(&slots[name].profile, name, &mut claimed);
        claims.insert(name.clone(), claim);
    }

    for (name, slot) in slots.iter_mut() {
        if !slot.want {
            if slot.capture.is_some() {
                if let Some(mut capture) = slot.capture.take() {
                    capture.stop();
                }
                slot.grabbed.clear();
                slot.yielded.clear();
                run::drain_held(sink, clock, &mut slot.held);
                writeln!(out, "dormant: {name}")?;
            }
            continue;
        }
        let claim = claims.remove(name).unwrap_or_default();
        // Only the transitions are worth a line, and each names its bind.
        for (signature, owner) in &claim.yielded {
            if !slot.yielded.contains_key(signature) {
                writeln!(
                    out,
                    "bind yielded: {name} {} -> {owner}",
                    label_of(&slot.profile, *signature)
                )?;
            }
        }
        for signature in slot.yielded.keys() {
            if !claim.yielded.contains_key(signature) {
                writeln!(
                    out,
                    "bind resumed: {name} {}",
                    label_of(&slot.profile, *signature)
                )?;
            }
        }
        let was_live = slot.capture.is_some();
        if was_live && slot.grabbed == claim.sigs {
            slot.yielded = claim.yielded;
            continue;
        }
        if let Some(mut capture) = slot.capture.take() {
            capture.stop();
        }
        slot.grabbed.clear();
        if claim.chords.is_empty() {
            // Everything this slot binds is spoken for right now. Its stops stay
            // live through the emergency grab, and the claims free up the moment
            // their owners go dormant — the yield lines above told the story.
            slot.yielded = claim.yielded;
            continue;
        }
        match GrabCapture::start_into(epoch, &claim.chords, queue.clone()) {
            Ok(capture) => {
                slot.capture = Some(capture);
                slot.grabbed = claim.sigs;
                slot.yielded = claim.yielded;
                slot.blocked = false;
                if !was_live {
                    writeln!(out, "active: {name}")?;
                }
            }
            Err(err) => {
                if !slot.blocked {
                    writeln!(
                        out,
                        "blocked: {name}: {err} (another program owns a trigger; \
                         retrying as things change)"
                    )?;
                    slot.blocked = true;
                }
            }
        }
    }
    Ok(())
}

fn reconcile_set(
    out: &mut dyn std::io::Write,
    clock: &SystemClock,
    sink: &mut dyn sequencer_core::emit::InputSink,
    slots: &mut BTreeMap<String, Slot>,
    failed: &mut BTreeMap<String, Option<std::time::SystemTime>>,
) -> Result<()> {
    let set = scan_active()?;
    // Gone, or re-applied: a replaced link carries a fresh stamp, and a reload is a
    // removal the loop below immediately follows with a load. Held keys are drained
    // either way — the new version must not inherit the old one's ledger.
    let mut updated: BTreeSet<String> = BTreeSet::new();
    let leaving: Vec<(String, bool)> = slots
        .iter()
        .filter_map(|(name, slot)| {
            if !set.contains_key(name) {
                Some((name.clone(), false))
            } else if super::state::link_stamp(name) != slot.stamp {
                Some((name.clone(), true))
            } else {
                None
            }
        })
        .collect();
    for (name, reapplied) in leaving {
        if let Some(mut slot) = slots.remove(&name) {
            if let Some(mut capture) = slot.capture.take() {
                capture.stop();
            }
            run::drain_held(sink, clock, &mut slot.held);
            if reapplied {
                updated.insert(name);
            } else {
                writeln!(out, "profile removed: {name}")?;
            }
        }
    }
    // A refused link earns a retry the same way a slot earns a reload: by being
    // replaced. Same name, same stamp — still refused, still quiet.
    failed.retain(|name, stamp| set.contains_key(name) && super::state::link_stamp(name) == *stamp);

    for (name, path) in &set {
        if slots.contains_key(name) || failed.contains_key(name) {
            continue;
        }
        // The stamp is read BEFORE the load: a re-apply racing this very pass makes
        // the stored stamp stale, so the next pass reloads again rather than missing
        // the newer version.
        let stamp = super::state::link_stamp(name);
        match load(path) {
            Ok(profile) => {
                // A newcomer that would close a trigger circle with the live set is
                // refused like a parse failure. The apply command checks this too,
                // but links made by hand never pass through it.
                let circle = {
                    let mut graph: Vec<(&str, &Profile)> = slots
                        .iter()
                        .map(|(slot_name, slot)| (slot_name.as_str(), &slot.profile))
                        .collect();
                    graph.push((name.as_str(), &profile));
                    trigger_cycle(&graph)
                };
                if let Some(circle) = circle {
                    writeln!(
                        out,
                        "profile refused: {name}: these binds would trigger each \
                         other in a circle: {}",
                        circle.join(" -> ")
                    )?;
                    failed.insert(name.clone(), stamp);
                    continue;
                }
                let verb = if updated.contains(name) {
                    "profile updated"
                } else {
                    "profile applied"
                };
                writeln!(out, "{verb}: {name} ({} binds)", profile.binds.len())?;
                super::write_stop_hint(out, &profile)?;
                slots.insert(
                    name.clone(),
                    Slot {
                        profile,
                        held: Vec::new(),
                        capture: None,
                        grabbed: BTreeSet::new(),
                        yielded: BTreeMap::new(),
                        stamp,
                        want: false,
                        blocked: false,
                    },
                );
            }
            Err(detail) => {
                writeln!(out, "profile refused: {name}: {detail}")?;
                failed.insert(name.clone(), stamp);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(text: &str) -> Profile {
        parse(text).expect("test profile parses")
    }

    /// The precedence system in one pass: stops are claimed first, then the
    /// program-gated slot, then the global one — the shared trigger goes to the
    /// gated profile, and the global slot yields it while keeping its own.
    #[test]
    fn the_grab_space_is_claimed_in_precedence_order() {
        let gated = profile("[defaults]\nprogram = \"*term*\"\n[binds.\"ctrl w\"]\nbind = \"q\"");
        let global =
            profile("[binds.\"ctrl w\"]\nbind = \"p\"\n\n[binds.\"ctrl e\"]\nbind = \"p\"");
        let mut claimed: BTreeMap<(Key, Mods), String> = BTreeMap::new();

        let first = claim_triggers(&gated, "gated.toml", &mut claimed);
        let second = claim_triggers(&global, "global.toml", &mut claimed);

        let ctrl_w = (Key::W, Mods::CTRL);
        assert!(first.sigs.contains(&ctrl_w), "the gated profile claims it");
        assert!(first.yielded.is_empty());
        assert!(!second.sigs.contains(&ctrl_w), "the global slot yields it");
        assert_eq!(
            second.yielded.get(&ctrl_w).map(String::as_str),
            Some("gated.toml"),
            "the yield names its claimant"
        );
        assert!(
            second.sigs.contains(&(Key::E, Mods::CTRL)),
            "the global slot's other bind stays live"
        );
        assert_eq!(second.chords.len(), 1, "one grab survives the yield");
    }

    /// A stop chord is claimed before any bind: a bind on another profile's stop
    /// combination yields to the emergency grab instead of blocking its whole slot,
    /// and the rest of the profile is untouched.
    #[test]
    fn stop_chords_outrank_every_bind() {
        let global = profile("[binds.\"ctrl shift F10\"]\nbind = \"p\"\n\n[binds.q]\nbind = \"p\"");
        let stop = (Key::F10, Mods::CTRL.and(Mods::SHIFT));
        let mut claimed: BTreeMap<(Key, Mods), String> = BTreeMap::new();
        claimed.insert(stop, "the emergency grab".to_owned());

        let claim = claim_triggers(&global, "global.toml", &mut claimed);
        assert!(
            !claim.sigs.contains(&stop),
            "the stop's combination is not grabbable"
        );
        assert_eq!(
            claim.yielded.get(&stop).map(String::as_str),
            Some("the emergency grab")
        );
        assert!(claim.sigs.contains(&(Key::Q, Mods::NONE)));
    }

    /// Side-variant spellings collapse to one grab within their own slot — the
    /// second spelling shares the claim, it does not yield it.
    #[test]
    fn side_variants_share_their_slots_own_claim() {
        let both = profile(
            "[binds.\"shift >\"]\nbind = \"shift ]\"\n\n[binds.\"rshift >\"]\nbind = \"q\"",
        );
        let mut claimed: BTreeMap<(Key, Mods), String> = BTreeMap::new();
        let claim = claim_triggers(&both, "p.toml", &mut claimed);
        assert_eq!(claim.chords.len(), 1, "one grab for both spellings");
        assert!(claim.yielded.is_empty(), "sharing is not yielding");
    }

    /// Two global profiles colliding: the alphabetically-first claims the shared
    /// trigger and the second keeps everything else — nobody's whole profile is
    /// blocked over one bind anymore.
    #[test]
    fn colliding_globals_lose_only_the_shared_bind() {
        let first = profile("[binds.F5]\nbind = \"p\"");
        let second = profile("[binds.F5]\nbind = \"q\"\n\n[binds.F6]\nbind = \"q\"");
        let mut claimed: BTreeMap<(Key, Mods), String> = BTreeMap::new();

        let a = claim_triggers(&first, "a.toml", &mut claimed);
        let b = claim_triggers(&second, "b.toml", &mut claimed);
        assert!(a.sigs.contains(&(Key::F5, Mods::NONE)));
        assert_eq!(
            b.yielded.get(&(Key::F5, Mods::NONE)).map(String::as_str),
            Some("a.toml")
        );
        assert!(
            b.sigs.contains(&(Key::F6, Mods::NONE)),
            "the rest of b stays live"
        );
    }

    /// The yield line names the bind by its written spelling.
    #[test]
    fn labels_name_the_written_spelling() {
        let p = profile("[binds.\"ctrl w\"]\nbind = \"q\"");
        assert_eq!(label_of(&p, (Key::W, Mods::CTRL)), "[binds.\"ctrl w\"]");
        assert_eq!(label_of(&p, (Key::Q, Mods::NONE)), "");
    }
}
