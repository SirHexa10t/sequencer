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
//! is one ordinary key plus modifier classes, folded as [`Mods`] folds them (either Ctrl is
//! Ctrl; right Alt is its own class), and the held classes must equal the claimed ones.
//!
//! The router is the policy and only the policy. It touches no device, so every rule here is
//! tested without one; the grab layer moves the events and asks it where each goes.

use std::collections::{BTreeMap, BTreeSet};

use evdev::KeyCode;
use sequencer_core::input::{EventKind, Key, Mods};

use crate::linux::keymap;

/// A grabbed device, by the number the grab layer gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(pub u32);

/// Who claimed a set of triggers — a profile, or the emergency stop — so the claims can go
/// when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaimOwner(pub u32);

/// Why a trigger could not be claimed. The X11 backend refuses the same chords for the same
/// reasons (its `GrabError`), so a profile behaves alike under either.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ClaimError {
    /// The key has no evdev code, so no device can press it.
    #[error("{0} has no Linux key code to grab")]
    Unmappable(Key),
    /// A chord names more than one ordinary key.
    #[error("`{0}` cannot join a chord trigger: a trigger is one key plus modifiers")]
    Unchordable(Key),
    /// A chord of nothing but modifiers has no key to fire on.
    #[error("a trigger needs one non-modifier key, not modifiers alone")]
    ModifiersOnly,
    /// Another profile already claimed this trigger.
    #[error("{0} is already claimed by another profile")]
    AlreadyClaimed(Key),
}

/// Where one key event goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// On to the virtual keyboard, unchanged.
    Forward,
    /// Kept from the session, and reported: a claimed trigger went down or came up.
    Report(EventKind, Mods),
    /// Kept from the session, and not reported: a kept key's auto-repeat.
    Keep,
}

/// What a press, a repeat and a release are on the wire (`input_event.value` for `EV_KEY`).
const RELEASE: i32 = 0;
const PRESS: i32 = 1;
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
    pub fn claim(&mut self, owner: ClaimOwner, chords: &[Vec<Key>]) -> Result<(), ClaimError> {
        let mut wanted = Vec::with_capacity(chords.len());
        for chord in chords {
            let (key, mods) = signature(chord)?;
            let code = keymap::key_to_code(key).ok_or(ClaimError::Unmappable(key))?;
            if self
                .claims
                .get(&(code, mods))
                .is_some_and(|holder| *holder != owner)
            {
                return Err(ClaimError::AlreadyClaimed(key));
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
    pub fn route(&mut self, device: DeviceId, code: KeyCode, value: i32) -> Route {
        match value {
            PRESS => {
                // The classes held before this key: a modifier's own press is not part of
                // the state it joins, as on X11.
                let mods = self.mods();
                self.held.insert((device, code));
                let Some(key) = keymap::code_to_key(code) else {
                    return Route::Forward;
                };
                if !self.paused
                    && Mods::of_key(key).is_none()
                    && self.claims.contains_key(&(code, mods))
                {
                    self.kept.insert((device, code), mods);
                    return Route::Report(EventKind::KeyDown(key), mods);
                }
                Route::Forward
            }
            REPEAT if self.kept.contains_key(&(device, code)) => Route::Keep,
            RELEASE => {
                self.held.remove(&(device, code));
                match (self.kept.remove(&(device, code)), keymap::code_to_key(code)) {
                    (Some(mods), Some(key)) => Route::Report(EventKind::KeyUp(key), mods),
                    _ => Route::Forward,
                }
            }
            _ => Route::Forward,
        }
    }

    /// Forgets `device` — unplugged, or let go — answering with what that means for the
    /// manager: a release for each trigger it held down, so no profile waits on a key that
    /// will never come up.
    pub fn forget(&mut self, device: DeviceId) -> Vec<(EventKind, Mods)> {
        self.held.retain(|(on, _)| *on != device);
        let gone: Vec<((DeviceId, KeyCode), Mods)> = self
            .kept
            .iter()
            .filter(|((on, _), _)| *on == device)
            .map(|(&held, &mods)| (held, mods))
            .collect();
        gone.into_iter()
            .filter_map(|(held, mods)| {
                self.kept.remove(&held);
                keymap::code_to_key(held.1).map(|key| (EventKind::KeyUp(key), mods))
            })
            .collect()
    }

    /// The modifier classes held now, on any grabbed device.
    #[must_use]
    pub fn mods(&self) -> Mods {
        self.held
            .iter()
            .filter_map(|(_, code)| keymap::code_to_key(*code).and_then(Mods::of_key))
            .fold(Mods::NONE, Mods::and)
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

/// A chord as the router matches it: its one ordinary key, and the classes of its
/// modifiers.
///
/// # Errors
///
/// [`ClaimError::Unchordable`] for a second ordinary key, [`ClaimError::ModifiersOnly`] for a
/// chord with none.
pub fn signature(chord: &[Key]) -> Result<(Key, Mods), ClaimError> {
    let mut primary = None;
    for &key in chord {
        if Mods::of_key(key).is_none() && primary.replace(key).is_some() {
            return Err(ClaimError::Unchordable(key));
        }
    }
    primary
        .map(|key| (key, Mods::of_chord(chord)))
        .ok_or(ClaimError::ModifiersOnly)
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

    fn press(router: &mut Router, device: DeviceId, key: Key) -> Route {
        router.route(device, code(key), PRESS)
    }

    fn release(router: &mut Router, device: DeviceId, key: Key) -> Route {
        router.route(device, code(key), RELEASE)
    }

    fn ctrl_j() -> Router {
        let mut router = Router::new();
        router
            .claim(PROFILE, &[vec![Key::LeftCtrl, Key::J]])
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
            .claim(PROFILE, &[vec![Key::LeftAlt, Key::F9]])
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
            router.claim(SECOND, &[vec![Key::F9], vec![Key::RightCtrl, Key::J]]),
            Err(ClaimError::AlreadyClaimed(Key::J))
        );
        press(&mut router, KEYBOARD, Key::F9);
        assert_eq!(
            release(&mut router, KEYBOARD, Key::F9),
            Route::Forward,
            "all or nothing: F9 wasn't claimed"
        );
        router
            .claim(PROFILE, &[vec![Key::LeftCtrl, Key::J]])
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
            .claim(SECOND, &[vec![Key::LeftCtrl, Key::J]])
            .expect("free once released");
    }

    /// A chord is one ordinary key and modifiers — what X11 can grab, and so what a profile
    /// may name under either backend.
    #[test]
    fn a_chord_is_one_key_and_modifiers() {
        assert_eq!(
            signature(&[Key::LeftCtrl, Key::LeftShift, Key::W]),
            Ok((Key::W, Mods::CTRL.and(Mods::SHIFT)))
        );
        assert_eq!(
            signature(&[Key::A, Key::B]),
            Err(ClaimError::Unchordable(Key::B))
        );
        assert_eq!(
            signature(&[Key::LeftCtrl, Key::LeftShift]),
            Err(ClaimError::ModifiersOnly)
        );
        let mut router = Router::new();
        assert_eq!(
            router.claim(PROFILE, &[vec![Key::LeftCtrl]]),
            Err(ClaimError::ModifiersOnly)
        );
    }

    /// A device that goes away takes its held keys with it, and a trigger it held down is
    /// released to the manager rather than left pending.
    #[test]
    fn a_lost_device_releases_what_it_held() {
        let mut router = ctrl_j();
        press(&mut router, KEYBOARD, Key::LeftCtrl);
        press(&mut router, KEYBOARD, Key::J);
        press(&mut router, OTHER, Key::LeftShift);
        assert_eq!(
            router.forget(KEYBOARD),
            [(EventKind::KeyUp(Key::J), Mods::CTRL)]
        );
        assert_eq!(
            router.mods(),
            Mods::SHIFT,
            "the other device's Shift is still down"
        );
        assert!(!router.any_down(&[Key::J, Key::LeftCtrl]));
        assert!(router.forget(KEYBOARD).is_empty());
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

    /// Keys with no name here — and mouse buttons, until they can be triggers — pass on
    /// untouched.
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
}
