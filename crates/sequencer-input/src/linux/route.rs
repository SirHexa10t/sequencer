//! Which grabbed input goes on to the session, and which a profile keeps.
//!
//! With the keyboards grabbed, nothing reaches the session unless it is written again to the
//! virtual keyboard, and [`Router`] decides, event by event, which way each goes. A key-down
//! that completes a claimed trigger — its key, with exactly the claimed modifier classes held
//! — is kept: reported to the manager, never written on. So is everything that key does until
//! it comes up, its repeats and its release, whatever the modifiers do meanwhile: the session
//! never sees half a keystroke, and the release is reported with the modifiers the press had,
//! so it reaches the profile the press did. Everything else goes on unchanged, modifiers
//! included — the session sees Ctrl go down and up around a kept Ctrl+J, as an X11 passive
//! grab leaves it.
//!
//! Matching follows the X11 backend's rules, so a profile means the same under both: a trigger
//! is one ordinary key — or, here alone, a mouse button — plus modifier classes, folded as
//! [`Mods`] folds them (either Ctrl is Ctrl; right Alt is its own class), and the held classes
//! must equal the claimed ones.
//!
//! What profiles send is routed too, as the input of one more device, [`OUTPUT`] — as X11
//! routes XTEST input through its grabs. So one profile's output can fire another's trigger
//! (chaining, kept from X11 by the developer's call), and output and hands are counted together
//! on the one virtual keyboard. A chain is cut at [`CHAIN_DEPTH`]: output sent that deep only
//! passes on ([`Router::route_output`]).
//!
//! The router is the policy and only the policy. It touches no device, so every rule here is
//! tested without one; the grab layer moves the events and asks it where each goes, a batch
//! at a time, frames kept whole (`sort_batch`).

use std::collections::{BTreeMap, BTreeSet};

use evdev::{EventType, InputEvent as DeviceEvent, KeyCode, SynchronizationCode};
use sequencer_core::emit::Holdable;
use sequencer_core::input::{EventKind, Key, Mods};

use crate::linux::keymap;

/// A grabbed device, by the number the grab layer gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u32);

/// The device a profile's own output is routed as: never a grabbed one, whose numbers start
/// at one.
pub const OUTPUT: DeviceId = DeviceId(0);

/// How deep one profile's output may fire another's trigger. A hand's press runs its bind at
/// depth 0; output that fires a trigger runs that bind one deeper; output sent at this depth
/// fires nothing. The apply-time circle check refuses the loops it can see, and this stops
/// the ones it can't — a chance block, a set edited by hand. Eight is past any chain a person
/// writes on purpose, and stops a runaway within milliseconds.
pub const CHAIN_DEPTH: u8 = 8;

/// Who claimed a set of triggers — a profile, or the emergency stop — so the claims can go
/// when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimOwner(pub u32);

/// Why a trigger could not be claimed. The X11 backend refuses the same chords for the same
/// reasons (its `GrabError`), so a profile behaves alike under either.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClaimError {
    /// The key or button has no evdev code, so no device can press it.
    #[error("{0} has no Linux key code to grab")]
    Unmappable(Holdable),
    /// A chord names more than one ordinary key or button.
    #[error("`{0}` cannot join a chord trigger: a trigger is one key or button plus modifiers")]
    Unchordable(Holdable),
    /// A chord of nothing but modifiers has no key to fire on.
    #[error("a trigger needs one non-modifier key, not modifiers alone")]
    ModifiersOnly,
    /// Another profile already claimed this trigger.
    #[error("{0} is already claimed by another profile")]
    AlreadyClaimed(Holdable),
}

/// Where one key event goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// On to the virtual keyboard, unchanged.
    Forward,
    /// Kept from the session, and reported: a claimed trigger went down or came up.
    Report(EventKind, Mods),
    /// Kept from the session, and not reported: a kept key's auto-repeat — or a key another
    /// grabbed device already holds down on the virtual keyboard: its press, and every
    /// release but the last, since one virtual keyboard stands in for them all.
    Keep,
}

/// What forgetting a device means ([`Router::forget`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// For the manager: a release for each trigger the device held down, so no profile waits
    /// on a key that will never come up.
    pub reports: Vec<(EventKind, Mods)>,
    /// For the virtual keyboard: the keys the device held down there that no other device
    /// holds, to let up — or they'd stay down for good.
    pub release: Vec<KeyCode>,
}

/// What one batch read from a device comes to, in order ([`sort_batch`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Passed {
    /// A frame to pass on: what the device reported together, short of what a profile keeps
    /// and of the report that closed it — writing a frame closes it anew.
    Frame(Vec<DeviceEvent>),
    /// A kept trigger going down or up, for the manager.
    Report(EventKind, Mods),
}

/// Sorts one batch read from a device into the frames passed on and the triggers reported,
/// asking `route` about each key and button; motion, the wheel and the rest pass as they
/// came. A frame a profile kept all of is no frame at all. `route` answering `None` ends the
/// batch where it stands: the grab is ending, and what is left is the session's already.
///
/// What both readers share — the keyboards', which write to the one virtual keyboard, and
/// the mice's, which write to each mouse's stand-in.
pub(crate) fn sort_batch(
    events: &[DeviceEvent],
    mut route: impl FnMut(KeyCode, i32) -> Option<Route>,
) -> Vec<Passed> {
    let mut sorted = Vec::new();
    let mut frame = Vec::new();
    for event in events {
        if event.event_type() == EventType::SYNCHRONIZATION {
            if event.code() == SynchronizationCode::SYN_REPORT.0 && !frame.is_empty() {
                sorted.push(Passed::Frame(std::mem::take(&mut frame)));
            }
            continue;
        }
        if event.event_type() != EventType::KEY {
            frame.push(*event);
            continue;
        }
        match route(KeyCode(event.code()), event.value()) {
            None => return sorted,
            Some(Route::Forward) => frame.push(*event),
            Some(Route::Report(kind, mods)) => sorted.push(Passed::Report(kind, mods)),
            Some(Route::Keep) => {}
        }
    }
    if !frame.is_empty() {
        sorted.push(Passed::Frame(frame));
    }
    sorted
}

/// What a press, a repeat and a release are on the wire (`input_event.value` for `EV_KEY`).
pub(crate) const RELEASE: i32 = 0;
pub(crate) const PRESS: i32 = 1;
const REPEAT: i32 = 2;

/// The routing state for every grabbed device at once — one keyboard's Ctrl and another's J
/// make a chord as surely as one keyboard's do.
#[derive(Debug, Default)]
pub struct Router {
    /// Every key down on every grabbed device.
    held: BTreeSet<(DeviceId, KeyCode)>,
    /// The claimed triggers: a key with its exact modifier classes, and who claimed it.
    claims: BTreeMap<(KeyCode, Mods), ClaimOwner>,
    /// Triggers that went down and have not come up, with the classes they went down with.
    kept: BTreeMap<(DeviceId, KeyCode), Mods>,
    /// While the screen is locked: nothing is claimed, and every press goes on as typed.
    paused: bool,
    /// Modifiers a lifted tap has let up on the virtual keyboard while the hand still holds
    /// them: out of the held classes until the tap is done, as the session sees them.
    lifted: BTreeSet<KeyCode>,
    /// Devices passed on through stand-ins of their own — the mice — rather than the one
    /// virtual keyboard: never counted with the rest, since the session counts them apart.
    alone: BTreeSet<DeviceId>,
}

impl Router {
    /// A router with nothing held and nothing claimed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims `chords` for `owner`: from now on each one's key-down, with exactly its
    /// modifier classes held, is kept and reported rather than passed on. All or nothing — a
    /// chord that can't be claimed leaves every claim as it was.
    ///
    /// # Errors
    ///
    /// [`ClaimError`]: a chord that is not one key plus modifiers, a key no device has, or a
    /// trigger another owner holds.
    pub fn claim(&mut self, owner: ClaimOwner, chords: &[Vec<Holdable>]) -> Result<(), ClaimError> {
        let mut wanted = Vec::with_capacity(chords.len());
        for chord in chords {
            let (primary, mods) = signature(chord)?;
            let code = keymap::holdable_to_code(primary).ok_or(ClaimError::Unmappable(primary))?;
            if self
                .claims
                .get(&(code, mods))
                .is_some_and(|holder| *holder != owner)
            {
                return Err(ClaimError::AlreadyClaimed(primary));
            }
            wanted.push((code, mods));
        }
        for trigger in wanted {
            self.claims.insert(trigger, owner);
        }
        Ok(())
    }

    /// Pauses claiming, or resumes it. While paused every press goes on to the session —
    /// the screen is locked, and what is typed there is no profile's business — and a
    /// trigger already down stays kept until it comes up.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether claiming is paused.
    #[must_use]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    /// Drops every claim `owner` made. A trigger of its that is down stays kept until it
    /// comes up, so the session still never sees half of that keystroke.
    pub fn release(&mut self, owner: ClaimOwner) {
        self.claims.retain(|_, holder| *holder != owner);
    }

    /// Where a key event from `device` goes: `code` is the key, `value` the press (1),
    /// repeat (2) or release (0) the kernel reported.
    ///
    /// One virtual keyboard passes every grabbed device on, so a key down on two of them is
    /// one key down there: it goes down with the first press and up with the last release,
    /// as the session would count it for two real keyboards. (Its own press passed on twice,
    /// the kernel would drop the second, and the first release would let the key up under
    /// the other device's finger.)
    pub fn route(&mut self, device: DeviceId, code: KeyCode, value: i32) -> Route {
        self.route_as(device, code, value, true)
    }

    /// Where one event of a profile's own output goes, sent by a bind running `depth` deep:
    /// routed as a hand's would be — so another profile's trigger hears it, and the manager
    /// runs that bind one deeper — while `depth` is below [`CHAIN_DEPTH`]; at that depth,
    /// counted and passed on, firing nothing.
    pub fn route_output(&mut self, code: KeyCode, value: i32, depth: u8) -> Route {
        self.route_as(OUTPUT, code, value, depth < CHAIN_DEPTH)
    }

    /// [`Self::route`], with claims heard only when `claiming`.
    fn route_as(&mut self, device: DeviceId, code: KeyCode, value: i32, claiming: bool) -> Route {
        match value {
            PRESS => {
                // The classes held before this key: a modifier's own press is not part of
                // the state it joins, as on X11.
                let mods = self.mods();
                let down_already = self.passed_on_by_another(device, code);
                self.held.insert((device, code));
                let passed = if down_already {
                    Route::Keep
                } else {
                    Route::Forward
                };
                let Some(input) = keymap::code_to_holdable(code) else {
                    return passed;
                };
                if claiming
                    && !self.paused
                    && !is_modifier(input)
                    && self.claims.contains_key(&(code, mods))
                {
                    self.kept.insert((device, code), mods);
                    return Route::Report(pressed(input), mods);
                }
                passed
            }
            REPEAT if self.kept.contains_key(&(device, code)) => Route::Keep,
            RELEASE => {
                self.held.remove(&(device, code));
                match (
                    self.kept.remove(&(device, code)),
                    keymap::code_to_holdable(code),
                ) {
                    (Some(mods), Some(input)) => Route::Report(released(input), mods),
                    _ if self.passed_on_by_another(device, code) => Route::Keep,
                    _ => Route::Forward,
                }
            }
            _ => Route::Forward,
        }
    }

    /// Whether a device other than `device` holds `code` down on the virtual keyboard: down
    /// there, and not a kept trigger, which never reached it. A device passed on alone, through
    /// a stand-in of its own, shares the virtual keyboard with nobody.
    fn passed_on_by_another(&self, device: DeviceId, code: KeyCode) -> bool {
        !self.alone.contains(&device)
            && self.held.iter().any(|&(on, held)| {
                on != device
                    && held == code
                    && !self.alone.contains(&on)
                    && !self.kept.contains_key(&(on, held))
            })
    }

    /// Marks `device` as passed on through a stand-in of its own — a mouse — so its keys
    /// are never counted with the virtual keyboard's. [`Self::forget`] unmarks it.
    pub fn pass_alone(&mut self, device: DeviceId) {
        self.alone.insert(device);
    }

    /// The mouse buttons claimed as triggers right now, by code — whatever modifiers each
    /// claim names: what decides which mice must be held.
    #[must_use]
    pub fn claimed_buttons(&self) -> BTreeSet<KeyCode> {
        self.claims
            .keys()
            .map(|&(code, _)| code)
            .filter(|&code| keymap::code_to_button(code).is_some())
            .collect()
    }

    /// Forgets `device` — unplugged, or let go — answering with what that means: releases
    /// for the triggers it held down, for the manager, and the keys it held on the virtual
    /// keyboard that no other device still holds, to let up there.
    pub fn forget(&mut self, device: DeviceId) -> Forgotten {
        let passed_on: Vec<KeyCode> = self
            .held
            .iter()
            .filter(|&&(on, code)| on == device && !self.kept.contains_key(&(on, code)))
            .map(|&(_, code)| code)
            .collect();
        self.held.retain(|(on, _)| *on != device);
        self.alone.remove(&device);
        let release = passed_on
            .into_iter()
            .filter(|&code| !self.passed_on_by_another(device, code))
            .collect();
        let gone: Vec<((DeviceId, KeyCode), Mods)> = self
            .kept
            .iter()
            .filter(|((on, _), _)| *on == device)
            .map(|(&held, &mods)| (held, mods))
            .collect();
        let reports = gone
            .into_iter()
            .filter_map(|(held, mods)| {
                self.kept.remove(&held);
                keymap::code_to_holdable(held.1).map(|input| (released(input), mods))
            })
            .collect();
        Forgotten { reports, release }
    }

    /// The modifier classes held now, on any grabbed device.
    #[must_use]
    pub fn mods(&self) -> Mods {
        self.held
            .iter()
            .filter(|(_, code)| !self.lifted.contains(code))
            .filter_map(|(_, code)| keymap::code_to_key(*code).and_then(Mods::of_key))
            .fold(Mods::NONE, Mods::and)
    }

    /// Marks `codes` lifted — let up on the virtual keyboard by a lifted tap, though the hand
    /// still holds them — so the tap's own keys are matched as the session sees them; an
    /// empty list ends the lift.
    pub fn set_lifted(&mut self, codes: &[KeyCode]) {
        self.lifted = codes.iter().copied().collect();
    }

    /// Whether `code` is down on any grabbed device.
    #[must_use]
    pub fn holds(&self, code: KeyCode) -> bool {
        self.held.iter().any(|(_, held)| *held == code)
    }

    /// Whether any of `keys` is down on any grabbed device.
    #[must_use]
    pub fn any_down(&self, keys: &[Key]) -> bool {
        keys.iter()
            .filter_map(|&key| keymap::key_to_code(key))
            .any(|code| self.held.iter().any(|(_, held)| *held == code))
    }

    /// The modifiers to lift around a tap of `targets`, the clean tap a trigger's output
    /// needs: every held modifier key of a class in `held` that the targets do not name
    /// themselves. These are down on the virtual keyboard, since modifiers always go on; a
    /// kept trigger never reached it, so it never needs lifting. Each code once, however many
    /// devices hold it.
    #[must_use]
    pub fn lift_plan(&self, targets: &[Key], held: Mods) -> Vec<KeyCode> {
        let named = Mods::of_chord(targets);
        let mut plan: Vec<KeyCode> = self
            .held
            .iter()
            .filter(|(_, code)| {
                keymap::code_to_key(*code)
                    .and_then(Mods::of_key)
                    .is_some_and(|class| held.covers(class) && !named.covers(class))
            })
            .map(|(_, code)| *code)
            .collect();
        plan.sort_unstable_by_key(|code| code.code());
        plan.dedup();
        plan
    }
}

/// A chord as the router matches it: its one ordinary key or button, and the classes of
/// its modifiers.
///
/// # Errors
///
/// [`ClaimError::Unchordable`] for a second ordinary key or button,
/// [`ClaimError::ModifiersOnly`] for a chord with none.
pub fn signature(chord: &[Holdable]) -> Result<(Holdable, Mods), ClaimError> {
    let mut primary = None;
    let mut modifiers = Vec::new();
    for &member in chord {
        match member {
            Holdable::Key(key) if Mods::of_key(key).is_some() => modifiers.push(key),
            _ => {
                if primary.replace(member).is_some() {
                    return Err(ClaimError::Unchordable(member));
                }
            }
        }
    }
    primary
        .map(|primary| (primary, Mods::of_chord(&modifiers)))
        .ok_or(ClaimError::ModifiersOnly)
}

/// Whether `input` is a modifier key — never a trigger's primary.
fn is_modifier(input: Holdable) -> bool {
    matches!(input, Holdable::Key(key) if Mods::of_key(key).is_some())
}

/// The event for `input` going down.
const fn pressed(input: Holdable) -> EventKind {
    match input {
        Holdable::Key(key) => EventKind::KeyDown(key),
        Holdable::Button(button) => EventKind::ButtonDown(button),
    }
}

/// The event for `input` coming up.
const fn released(input: Holdable) -> EventKind {
    match input {
        Holdable::Key(key) => EventKind::KeyUp(key),
        Holdable::Button(button) => EventKind::ButtonUp(button),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYBOARD: DeviceId = DeviceId(1);
    const OTHER: DeviceId = DeviceId(2);
    const PROFILE: ClaimOwner = ClaimOwner(1);
    const SECOND: ClaimOwner = ClaimOwner(2);

    fn code(key: Key) -> KeyCode {
        keymap::key_to_code(key).unwrap()
    }

    /// A chord of keys, as a profile's trigger spells it.
    fn chord(keys: &[Key]) -> Vec<Holdable> {
        keys.iter().copied().map(Holdable::Key).collect()
    }

    fn press(router: &mut Router, device: DeviceId, key: Key) -> Route {
        router.route(device, code(key), PRESS)
    }

    fn release(router: &mut Router, device: DeviceId, key: Key) -> Route {
        router.route(device, code(key), RELEASE)
    }

    fn ctrl_j() -> Router {
        let mut router = Router::new();
        router
            .claim(PROFILE, &[chord(&[Key::LeftCtrl, Key::J])])
            .unwrap();
        router
    }

    /// A kept trigger is the whole keystroke: its press and release reported, its repeats
    /// kept quiet, none of it passed on — while the modifier around it passes as typed.
    #[test]
    fn a_claimed_chord_is_kept_whole_and_its_modifier_passes() {
        let mut router = ctrl_j();
        assert_eq!(press(&mut router, KEYBOARD, Key::LeftCtrl), Route::Forward);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyDown(Key::J), Mods::CTRL)
        );
        assert_eq!(router.route(KEYBOARD, code(Key::J), REPEAT), Route::Keep);
        assert_eq!(
            release(&mut router, KEYBOARD, Key::LeftCtrl),
            Route::Forward
        );
        assert_eq!(
            release(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyUp(Key::J), Mods::CTRL),
            "the release carries the press's classes, though Ctrl came up first"
        );
    }

    /// The held classes must equal the claimed ones: more, fewer or other modifiers pass the
    /// key on, as an X11 grab would.
    #[test]
    fn only_the_exact_classes_fire() {
        let mut router = ctrl_j();
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Forward,
            "no Ctrl"
        );
        assert_eq!(release(&mut router, KEYBOARD, Key::J), Route::Forward);
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::LeftShift);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Forward,
            "Ctrl and Shift"
        );
        assert_eq!(
            router.route(KEYBOARD, code(Key::J), REPEAT),
            Route::Forward,
            "its repeats too"
        );
    }

    /// Classes fold as on X11: either Ctrl is Ctrl, but right Alt is its own class.
    #[test]
    fn classes_fold_as_x11_folds_them() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::RightCtrl);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyDown(Key::J), Mods::CTRL)
        );

        let mut router = Router::new();
        router
            .claim(PROFILE, &[chord(&[Key::LeftAlt, Key::F9])])
            .unwrap();
        press(&mut router, KEYBOARD, Key::RightAlt);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::F9),
            Route::Forward,
            "AltGr is not Alt"
        );
    }

    /// One keyboard's Ctrl and another's J make the chord: the router answers for every
    /// grabbed device at once.
    #[test]
    fn a_chord_may_span_devices() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        assert_eq!(
            press(&mut router, OTHER, Key::J),
            Route::Report(EventKind::KeyDown(Key::J), Mods::CTRL)
        );
        assert!(router.any_down(&[Key::LeftCtrl]) && router.any_down(&[Key::J]));
        assert_eq!(router.mods(), Mods::CTRL);
    }

    /// A trigger belongs to one owner; the same owner may claim it again, and releasing frees
    /// it for another — while a press already kept stays kept until it comes up.
    #[test]
    fn claims_have_one_owner_and_go_with_it() {
        let mut router = ctrl_j();
        assert_eq!(
            router.claim(
                SECOND,
                &[chord(&[Key::F9]), chord(&[Key::RightCtrl, Key::J])]
            ),
            Err(ClaimError::AlreadyClaimed(Holdable::Key(Key::J)))
        );
        press(&mut router, KEYBOARD, Key::F9);
        assert_eq!(
            release(&mut router, KEYBOARD, Key::F9),
            Route::Forward,
            "all or nothing: F9 wasn't claimed"
        );
        router
            .claim(PROFILE, &[chord(&[Key::LeftCtrl, Key::J])])
            .expect("its own claim again");

        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::J);
        router.release(PROFILE);
        assert_eq!(
            release(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyUp(Key::J), Mods::CTRL),
            "the kept press still gets its release"
        );
        router
            .claim(SECOND, &[chord(&[Key::LeftCtrl, Key::J])])
            .expect("free once released");
    }

    /// A chord is one ordinary key and modifiers — what X11 can grab, and so what a profile
    /// may name under either backend.
    #[test]
    fn a_chord_is_one_key_and_modifiers() {
        assert_eq!(
            signature(&chord(&[Key::LeftCtrl, Key::LeftShift, Key::W])),
            Ok((Holdable::Key(Key::W), Mods::CTRL.and(Mods::SHIFT)))
        );
        assert_eq!(
            signature(&chord(&[Key::A, Key::B])),
            Err(ClaimError::Unchordable(Holdable::Key(Key::B)))
        );
        assert_eq!(
            signature(&chord(&[Key::LeftCtrl, Key::LeftShift])),
            Err(ClaimError::ModifiersOnly)
        );
        let mut router = Router::new();
        assert_eq!(
            router.claim(PROFILE, &[chord(&[Key::LeftCtrl])]),
            Err(ClaimError::ModifiersOnly)
        );
    }

    /// A device that goes away takes its held keys with it: a trigger it held down is
    /// released to the manager rather than left pending, and what it held on the virtual
    /// keyboard comes up there — all but what another device still holds.
    #[test]
    fn a_lost_device_releases_what_it_held() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::J);
        press(&mut router, KEYBOARD, Key::A);
        press(&mut router, OTHER, Key::LeftShift);
        press(&mut router, OTHER, Key::A);
        assert_eq!(
            router.forget(KEYBOARD),
            Forgotten {
                reports: vec![(EventKind::KeyUp(Key::J), Mods::CTRL)],
                release: vec![code(Key::LeftCtrl)],
            },
            "the kept J goes to the manager, the passed-on Ctrl up; A stays, held elsewhere"
        );
        assert_eq!(
            router.mods(),
            Mods::SHIFT,
            "the other device's Shift is still down"
        );
        assert!(!router.any_down(&[Key::J, Key::LeftCtrl]));
        assert_eq!(router.forget(KEYBOARD), Forgotten::default());
        assert_eq!(
            release(&mut router, OTHER, Key::A),
            Route::Forward,
            "the last holder's release lets A up"
        );
    }

    /// A key down on two devices is one key down on the virtual keyboard: down with the first
    /// press, up with the last release — so letting go on one keyboard can't release a key
    /// the other still holds.
    #[test]
    fn a_key_held_on_two_devices_goes_up_with_the_last() {
        let mut router = Router::new();
        assert_eq!(press(&mut router, KEYBOARD, Key::LeftShift), Route::Forward);
        assert_eq!(
            press(&mut router, OTHER, Key::LeftShift),
            Route::Keep,
            "down there already"
        );
        assert_eq!(
            release(&mut router, KEYBOARD, Key::LeftShift),
            Route::Keep,
            "the other keyboard still holds it"
        );
        assert_eq!(router.mods(), Mods::SHIFT);
        assert_eq!(
            release(&mut router, OTHER, Key::LeftShift),
            Route::Forward,
            "the last release lets it up"
        );
        assert_eq!(router.mods(), Mods::NONE);
    }

    /// A profile's output is routed as a hand's is: pressing another profile's trigger fires
    /// it — kept from the session, reported — and the rest passes on.
    #[test]
    fn output_fires_a_claimed_trigger_and_otherwise_passes() {
        let mut router = Router::new();
        router.claim(PROFILE, &[chord(&[Key::F9])]).unwrap();
        assert_eq!(
            router.route_output(code(Key::F9), PRESS, 0),
            Route::Report(EventKind::KeyDown(Key::F9), Mods::NONE)
        );
        assert_eq!(
            router.route_output(code(Key::F9), RELEASE, 0),
            Route::Report(EventKind::KeyUp(Key::F9), Mods::NONE)
        );
        assert_eq!(router.route_output(code(Key::X), PRESS, 0), Route::Forward);
        assert_eq!(
            router.route_output(code(Key::X), RELEASE, 0),
            Route::Forward
        );
    }

    /// At the chain's depth output fires nothing — a runaway chain ends there — and only
    /// passes on.
    #[test]
    fn output_at_the_chain_depth_fires_nothing() {
        let mut router = Router::new();
        router.claim(PROFILE, &[chord(&[Key::F9])]).unwrap();
        assert_eq!(
            router.route_output(code(Key::F9), PRESS, CHAIN_DEPTH - 1),
            Route::Report(EventKind::KeyDown(Key::F9), Mods::NONE),
            "one short of the depth still chains"
        );
        router.route_output(code(Key::F9), RELEASE, CHAIN_DEPTH - 1);
        assert_eq!(
            router.route_output(code(Key::F9), PRESS, CHAIN_DEPTH),
            Route::Forward
        );
        assert_eq!(
            router.route_output(code(Key::F9), RELEASE, CHAIN_DEPTH),
            Route::Forward
        );
    }

    /// Output's modifiers make chords as a hand's do — a sequence holding Ctrl and tapping J
    /// fires `ctrl j` — and output and hand share the one virtual keyboard: a modifier both
    /// hold goes up with the last of them.
    #[test]
    fn output_and_hand_are_counted_together() {
        let mut router = ctrl_j();
        assert_eq!(
            router.route_output(code(Key::LeftCtrl), PRESS, 0),
            Route::Forward
        );
        assert_eq!(
            router.route_output(code(Key::J), PRESS, 0),
            Route::Report(EventKind::KeyDown(Key::J), Mods::CTRL)
        );
        router.route_output(code(Key::J), RELEASE, 0);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::LeftCtrl),
            Route::Keep,
            "down already, by the output"
        );
        assert_eq!(
            router.route_output(code(Key::LeftCtrl), RELEASE, 0),
            Route::Keep,
            "the hand still holds it"
        );
        assert_eq!(
            release(&mut router, KEYBOARD, Key::LeftCtrl),
            Route::Forward
        );
    }

    /// While a tap is lifted, its own keys are matched without the lifted modifiers, as the
    /// session sees them: with `ctrl` held and lifted, the tap's x fires a bare `x` trigger.
    #[test]
    fn a_lifted_tap_is_matched_without_its_lifted_modifiers() {
        let mut router = Router::new();
        router.claim(PROFILE, &[chord(&[Key::X])]).unwrap();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        router.set_lifted(&[code(Key::LeftCtrl)]);
        assert_eq!(
            router.route_output(code(Key::X), PRESS, 0),
            Route::Report(EventKind::KeyDown(Key::X), Mods::NONE)
        );
        router.route_output(code(Key::X), RELEASE, 0);
        router.set_lifted(&[]);
        assert_eq!(
            router.mods(),
            Mods::CTRL,
            "the lift over, the hand's Ctrl counts again"
        );
    }

    /// Forgetting the output — a stop — lets up what it holds on the virtual keyboard, all
    /// but what a hand holds too.
    #[test]
    fn forgetting_the_output_spares_what_a_hand_holds() {
        let mut router = Router::new();
        router.route_output(code(Key::LeftShift), PRESS, 0);
        router.route_output(code(Key::X), PRESS, 0);
        press(&mut router, KEYBOARD, Key::LeftShift);
        assert_eq!(
            router.forget(OUTPUT),
            Forgotten {
                reports: Vec::new(),
                release: vec![code(Key::X)],
            }
        );
        assert_eq!(router.mods(), Mods::SHIFT, "the hand's Shift stays down");
    }

    /// A trigger kept on one device never reached the virtual keyboard, so it can't count as
    /// holding the key there: the same key pressed on another device, unclaimed, passes on.
    #[test]
    fn a_kept_trigger_holds_nothing_on_the_virtual_keyboard() {
        let mut router = Router::new();
        router.claim(PROFILE, &[chord(&[Key::J])]).unwrap();
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyDown(Key::J), Mods::NONE)
        );
        router.release(PROFILE);
        assert_eq!(
            press(&mut router, OTHER, Key::J),
            Route::Forward,
            "the kept J is not down on the virtual keyboard"
        );
        assert_eq!(
            release(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyUp(Key::J), Mods::NONE)
        );
        assert_eq!(release(&mut router, OTHER, Key::J), Route::Forward);
    }

    /// The lift clears what the tap must not wear: held modifiers of the trigger's classes the
    /// targets don't name — each code once, whichever devices hold it.
    #[test]
    fn the_lift_clears_held_modifiers_the_targets_do_not_name() {
        let mut router = Router::new();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, OTHER, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::RightShift);
        let ctrl_shift = Mods::CTRL.and(Mods::SHIFT);
        assert_eq!(
            router.lift_plan(&[Key::X], ctrl_shift),
            [code(Key::LeftCtrl), code(Key::RightShift)]
        );
        assert_eq!(
            router.lift_plan(&[Key::LeftShift, Key::X], ctrl_shift),
            [code(Key::LeftCtrl)],
            "Shift is the target's own"
        );
        assert!(
            router.lift_plan(&[Key::X], Mods::NONE).is_empty(),
            "nothing the trigger held"
        );
    }

    /// Keys with no name here — and mouse buttons no profile claims — pass on untouched.
    #[test]
    fn what_no_profile_can_name_passes_on() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        assert_eq!(
            router.route(KEYBOARD, KeyCode::BTN_LEFT, PRESS),
            Route::Forward
        );
        assert_eq!(
            router.route(KEYBOARD, KeyCode::BTN_LEFT, RELEASE),
            Route::Forward
        );
        assert_eq!(
            router.route(KEYBOARD, KeyCode::KEY_FN, PRESS),
            Route::Forward
        );
    }

    /// A mouse button claimed as a trigger is kept and reported like a key — whole, its
    /// release with the press's classes — and makes chords with a keyboard's modifiers: one
    /// keyboard's Ctrl and a mouse's side button are `ctrl mouse4`.
    #[test]
    fn a_claimed_button_is_a_trigger_like_any_key() {
        use sequencer_core::input::Button;
        const MOUSE: DeviceId = DeviceId(3);
        let mut router = Router::new();
        router
            .claim(
                PROFILE,
                &[
                    vec![Holdable::Button(Button::Back)],
                    vec![
                        Holdable::Key(Key::LeftCtrl),
                        Holdable::Button(Button::Forward),
                    ],
                ],
            )
            .unwrap();
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_SIDE, PRESS),
            Route::Report(EventKind::ButtonDown(Button::Back), Mods::NONE)
        );
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_SIDE, RELEASE),
            Route::Report(EventKind::ButtonUp(Button::Back), Mods::NONE)
        );
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_EXTRA, PRESS),
            Route::Report(EventKind::ButtonDown(Button::Forward), Mods::CTRL)
        );
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_LEFT, PRESS),
            Route::Forward,
            "an unclaimed button passes on"
        );
    }

    /// A mouse passed on through its own stand-in shares nothing with the virtual keyboard: its
    /// button and the same button sent by a profile are two buttons to the session, each down
    /// and up as itself.
    #[test]
    fn a_device_passed_on_alone_is_counted_apart() {
        const MOUSE: DeviceId = DeviceId(3);
        let mut router = Router::new();
        router.pass_alone(MOUSE);
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_LEFT, PRESS),
            Route::Forward
        );
        assert_eq!(
            router.route_output(KeyCode::BTN_LEFT, PRESS, 0),
            Route::Forward,
            "the profile's click is the virtual keyboard's own"
        );
        assert_eq!(
            router.route(MOUSE, KeyCode::BTN_LEFT, RELEASE),
            Route::Forward
        );
        assert_eq!(
            router.route_output(KeyCode::BTN_LEFT, RELEASE, 0),
            Route::Forward
        );
    }

    /// The buttons claimed, whatever their modifiers, are what decide which mice are held;
    /// keys never count.
    #[test]
    fn the_claimed_buttons_are_what_mice_are_held_for() {
        use sequencer_core::input::Button;
        let mut router = ctrl_j();
        assert!(router.claimed_buttons().is_empty());
        router
            .claim(
                SECOND,
                &[vec![
                    Holdable::Key(Key::LeftCtrl),
                    Holdable::Button(Button::Back),
                ]],
            )
            .unwrap();
        assert_eq!(
            router.claimed_buttons(),
            BTreeSet::from([KeyCode::BTN_SIDE])
        );
        router.release(SECOND);
        assert!(router.claimed_buttons().is_empty());
    }

    /// Paused — the screen locked — every press goes on as typed, a claimed chord too; a
    /// trigger already down when the pause began still gets its release kept and reported,
    /// so the session never sees half of it. Resumed, claims hold again.
    #[test]
    fn paused_every_press_goes_on_and_a_kept_one_still_ends_kept() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::J);
        router.set_paused(true);
        assert!(router.paused());
        assert_eq!(
            release(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyUp(Key::J), Mods::CTRL),
            "kept before the pause, kept to the end"
        );
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Forward,
            "the password gets its j"
        );
        assert_eq!(release(&mut router, KEYBOARD, Key::J), Route::Forward);
        router.set_paused(false);
        assert_eq!(
            press(&mut router, KEYBOARD, Key::J),
            Route::Report(EventKind::KeyDown(Key::J), Mods::CTRL)
        );
    }

    fn event(kind: EventType, code: u16, value: i32) -> DeviceEvent {
        DeviceEvent::new(kind.0, code, value)
    }

    fn report_end() -> DeviceEvent {
        event(
            EventType::SYNCHRONIZATION,
            SynchronizationCode::SYN_REPORT.0,
            0,
        )
    }

    fn moved(axis: evdev::RelativeAxisCode, by: i32) -> DeviceEvent {
        event(EventType::RELATIVE, axis.0, by)
    }

    fn button(code: KeyCode, value: i32) -> DeviceEvent {
        event(EventType::KEY, code.0, value)
    }

    /// A batch passes on frame by frame, short of what a profile keeps: a kept press is
    /// reported where it came, and a frame that held nothing else is no frame at all.
    #[test]
    fn a_batch_passes_on_frame_by_frame_without_what_is_kept() {
        use sequencer_core::input::Button;
        let side_down = (EventKind::ButtonDown(Button::Back), Mods::NONE);
        let batch = [
            moved(evdev::RelativeAxisCode::REL_X, 3),
            report_end(),
            button(KeyCode::BTN_SIDE, PRESS),
            report_end(),
            moved(evdev::RelativeAxisCode::REL_Y, -2),
            button(KeyCode::BTN_LEFT, PRESS),
            report_end(),
        ];
        let sorted = sort_batch(&batch, |code, _| {
            Some(if code == KeyCode::BTN_SIDE {
                Route::Report(side_down.0, side_down.1)
            } else {
                Route::Forward
            })
        });
        assert_eq!(
            sorted,
            [
                Passed::Frame(vec![moved(evdev::RelativeAxisCode::REL_X, 3)]),
                Passed::Report(side_down.0, side_down.1),
                Passed::Frame(vec![
                    moved(evdev::RelativeAxisCode::REL_Y, -2),
                    button(KeyCode::BTN_LEFT, PRESS),
                ]),
            ]
        );
    }

    /// Only a device's report closes a frame, and none is passed on: writing a frame adds its
    /// own. Other synchronisation — a dropped-events notice — closes nothing. A batch read
    /// before its report came still passes on what it has.
    #[test]
    fn only_a_report_closes_a_frame() {
        let dropped = event(
            EventType::SYNCHRONIZATION,
            SynchronizationCode::SYN_DROPPED.0,
            0,
        );
        let batch = [
            moved(evdev::RelativeAxisCode::REL_X, 1),
            dropped,
            moved(evdev::RelativeAxisCode::REL_Y, 1),
            report_end(),
            moved(evdev::RelativeAxisCode::REL_WHEEL, -1),
        ];
        assert_eq!(
            sort_batch(&batch, |_, _| Some(Route::Forward)),
            [
                Passed::Frame(vec![
                    moved(evdev::RelativeAxisCode::REL_X, 1),
                    moved(evdev::RelativeAxisCode::REL_Y, 1),
                ]),
                Passed::Frame(vec![moved(evdev::RelativeAxisCode::REL_WHEEL, -1)]),
            ]
        );
    }

    /// Keys kept quietly — a kept trigger's repeats — pass nothing and report nothing.
    #[test]
    fn what_is_kept_quietly_leaves_no_trace() {
        let batch = [button(KeyCode::KEY_J, REPEAT), report_end()];
        assert!(sort_batch(&batch, |_, _| Some(Route::Keep)).is_empty());
    }

    /// The grab ending mid-batch ends the batch: what was sorted before stands, and the frame
    /// it was in the middle of goes nowhere — the device is the session's again.
    #[test]
    fn a_grab_ending_mid_batch_passes_on_nothing_more() {
        let batch = [
            moved(evdev::RelativeAxisCode::REL_X, 1),
            report_end(),
            moved(evdev::RelativeAxisCode::REL_Y, 1),
            button(KeyCode::BTN_LEFT, PRESS),
            report_end(),
        ];
        assert_eq!(
            sort_batch(&batch, |_, _| None),
            [Passed::Frame(vec![moved(
                evdev::RelativeAxisCode::REL_X,
                1
            )])]
        );
    }
}
