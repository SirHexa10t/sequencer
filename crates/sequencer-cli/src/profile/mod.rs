//! The apply-profile command: read a binds file, remap keys until stopped.
//!
//! `binds.example.toml` at the repository root is the format reference; this module is
//! its implementation. A profile is a set of bindings, each either an **edge mirror**
//! (`bind = "volume-up"`: the trigger's own down/up drive the target's) or a **sequence**
//! (`seq = [...]`: steps fired once per press). Parsing and validation live here and are
//! pure — the same file is accepted or refused identically on any machine — while
//! [`run`] holds the executor and the platform wiring stays in [`apply_profile`].
//!
//! Validation is strict on purpose: a binds file is user input that will be *acted on*,
//! and a file that silently means something other than what it says (a HOLD nobody
//! releases, a `suppress = false` nothing honours, two spellings of one trigger) is
//! refused with the reason rather than reinterpreted.

pub(crate) mod run;

use std::collections::BTreeMap;

use sequencer_core::emit::Holdable;
use sequencer_core::input::Key;
use sequencer_core::time::Duration;

use crate::args::ApplyProfileArgs;
use crate::{Deps, Error, Result, exit};

/// A parsed, validated binds file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    /// Every binding, in trigger-name order.
    pub(crate) binds: Vec<Bind>,
}

/// One `[binds.<trigger>]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bind {
    /// The trigger, as written — for error and banner text.
    pub(crate) trigger_text: String,
    /// The trigger keys. One for a plain key; several for a chord trigger.
    pub(crate) trigger: Vec<Key>,
    /// What the binding does.
    pub(crate) action: Action,
    /// How long a tap in this binding holds the key down.
    pub(crate) tap: Duration,
    /// The pause between consecutive steps, where no WAIT replaces it.
    pub(crate) gap: Duration,
}

/// What a binding does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// The trigger's edges drive the target's: down on down, up on up.
    Mirror(Holdable),
    /// A sequence, fired once per press of the trigger.
    Seq(Vec<Step>),
}

/// One entry of a `seq` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Down, wait `tap`, up in reverse order. Several members are a chord.
    Tap(Vec<Holdable>),
    /// Press and keep down until a later [`Step::Release`].
    Hold(Vec<Holdable>),
    /// Release something a [`Step::Hold`] pressed.
    Release(Vec<Holdable>),
    /// Pause, replacing the default gap at this seam.
    Wait(Duration),
}

/// The built-in timing, used wherever the file does not say otherwise. These are the
/// same values `binds.example.toml` documents as the defaults.
const DEFAULT_TAP: Duration = Duration::from_millis(8);
const DEFAULT_GAP: Duration = Duration::from_millis(30);

/// `sequencer apply-profile`.
///
/// # Errors
///
/// If the file cannot be read, does not parse, or fails validation; or if there is no
/// backend that can run it here.
pub(crate) fn apply_profile(args: &ApplyProfileArgs, deps: &mut Deps<'_>) -> Result<u8> {
    let path = args.file.display().to_string();
    let text = std::fs::read_to_string(&args.file).map_err(|source| Error::ScriptRead {
        path: path.clone(),
        source,
    })?;
    let profile = parse(&text).map_err(|detail| Error::Profile { path, detail })?;

    if !args.global.quiet {
        for bind in &profile.binds {
            writeln!(deps.out, "{}", describe(bind))?;
        }
        writeln!(
            deps.out,
            "Applying {} binds. Ctrl+C stops.",
            profile.binds.len()
        )?;
        deps.out.flush()?;
    }

    // The injected pair is the test seam; a real run grabs the triggers on X11.
    if let (Some(sink), Some(pump)) = (deps.sink.as_deref_mut(), deps.pump.as_deref_mut()) {
        run::run(&profile, sink, deps.clock, pump)?;
        return Ok(exit::OK);
    }
    platform::apply(&profile)?;
    Ok(exit::OK)
}

/// One line per binding, in the words of the file.
fn describe(bind: &Bind) -> String {
    match &bind.action {
        Action::Mirror(target) => format!(
            "  {} -> {}",
            bind.trigger_text,
            sequencer_core::input::INPUT_MAP.display_name(*target)
        ),
        Action::Seq(steps) => format!(
            "  {} -> sequence of {} steps",
            bind.trigger_text,
            steps.len()
        ),
    }
}

/// The X11 side: grab the triggers, inject through XTEST.
#[cfg(all(feature = "xtest", target_os = "linux"))]
mod platform {
    use super::{Error, Profile, Result, run};
    use sequencer_input::{Epoch, SystemClock};

    pub(super) fn apply(profile: &Profile) -> Result<()> {
        if !sequencer_input::x11::is_usable() {
            return Err(Error::NotImplemented(
                "apply-profile needs an X11 session for now: it hears its triggers by \
                 key grab and injects through XTEST. Wayland and the console come with \
                 the device backend later."
                    .to_owned(),
            ));
        }
        // Chord triggers parse — the format owns them — but no grab can express one
        // yet: a grab names one key, and matching a chord needs modifier state this
        // backend does not track. Refuse rather than fire on the bare key.
        let mut triggers = Vec::with_capacity(profile.binds.len());
        for bind in &profile.binds {
            match bind.trigger.as_slice() {
                [one] => triggers.push(*one),
                _ => {
                    return Err(Error::NotImplemented(format!(
                        "chord triggers are not runnable yet: [binds.\"{}\"] would need \
                         a modifier-aware grab. Bind a single key for now.",
                        bind.trigger_text
                    )));
                }
            }
        }

        let epoch = Epoch::start();
        let clock = SystemClock::from_epoch(epoch.instant());
        let mut sink = sequencer_input::XTestSink::open()?;
        let (mut capture, stream) = sequencer_input::GrabCapture::start(&epoch, &triggers)?;
        tracing::info!(
            binds = profile.binds.len(),
            "X11: triggers grabbed, injecting through XTEST"
        );
        let mut pump = crate::runtime::CapturePump::new(stream, &clock);
        let outcome = run::run(profile, &mut sink, &clock, &mut pump);
        capture.stop();
        outcome
    }
}

#[cfg(not(all(feature = "xtest", target_os = "linux")))]
mod platform {
    use super::{Error, Profile, Result};

    pub(super) fn apply(_profile: &Profile) -> Result<()> {
        Err(Error::NotImplemented(
            "apply-profile runs on X11 only for now, and this build has no X11 backend.".to_owned(),
        ))
    }
}

// ------------------------------------------------------------------------- parsing

/// The file as TOML sees it, before any meaning is checked.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    defaults: Option<RawDefaults>,
    binds: BTreeMap<String, RawBind>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefaults {
    tap: Option<String>,
    gap: Option<String>,
    suppress: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBind {
    bind: Option<String>,
    seq: Option<Vec<String>>,
    tap: Option<String>,
    gap: Option<String>,
}

/// Parses and validates a binds file.
///
/// # Errors
///
/// A human-readable reason, positioned as well as the format allows: TOML errors carry
/// their own line numbers, and everything else names its binding.
#[allow(
    clippy::similar_names,
    reason = "tap and gap are the format's own field names; the pairing is the point"
)]
pub(crate) fn parse(text: &str) -> std::result::Result<Profile, String> {
    let raw: RawFile = toml::from_str(text).map_err(|err| err.to_string())?;
    let defaults = raw.defaults.unwrap_or_default();

    if defaults.suppress == Some(false) {
        return Err(
            "suppress = false is not supported yet: an X11 grab consumes the key by \
             nature. Remove the setting (true is the default and the only behaviour)."
                .to_owned(),
        );
    }
    let default_tap =
        optional_duration(defaults.tap.as_deref(), "defaults.tap")?.unwrap_or(DEFAULT_TAP);
    let default_gap =
        optional_duration(defaults.gap.as_deref(), "defaults.gap")?.unwrap_or(DEFAULT_GAP);

    let mut binds = Vec::with_capacity(raw.binds.len());
    let mut seen: BTreeMap<Vec<Key>, String> = BTreeMap::new();
    for (trigger_text, raw_bind) in raw.binds {
        let context = format!("[binds.\"{trigger_text}\"]");
        let bind = lower_bind(&trigger_text, &raw_bind, default_tap, default_gap)
            .map_err(|detail| format!("{context}: {detail}"))?;
        // Two spellings of one trigger ({ and [, A and a) are one key twice, which TOML
        // itself cannot see. Later sections would silently shadow earlier ones.
        if let Some(previous) = seen.insert(bind.trigger.clone(), trigger_text.clone()) {
            return Err(format!(
                "{context}: this is the same trigger as [binds.\"{previous}\"] — two \
                 spellings of one key"
            ));
        }
        binds.push(bind);
    }
    Ok(Profile { binds })
}

/// Checks one binding and lowers it into its runnable form.
#[allow(
    clippy::similar_names,
    reason = "tap and gap are the format's own field names; the pairing is the point"
)]
fn lower_bind(
    trigger_text: &str,
    raw: &RawBind,
    default_tap: Duration,
    default_gap: Duration,
) -> std::result::Result<Bind, String> {
    let trigger = parse_trigger(trigger_text)?;
    let tap = optional_duration(raw.tap.as_deref(), "tap")?.unwrap_or(default_tap);
    let gap = optional_duration(raw.gap.as_deref(), "gap")?.unwrap_or(default_gap);

    let action = match (&raw.bind, &raw.seq) {
        (Some(_), Some(_)) => {
            return Err("has both `bind` and `seq`; a binding is one or the other".to_owned());
        }
        (None, None) => {
            return Err("has neither `bind` nor `seq`; a binding needs exactly one".to_owned());
        }
        (Some(target), None) => Action::Mirror(parse_pressable(target)?),
        (None, Some(steps)) => Action::Seq(parse_seq(steps)?),
    };
    Ok(Bind {
        trigger_text: trigger_text.to_owned(),
        trigger,
        action,
        tap,
        gap,
    })
}

/// Parses a trigger: one key, or a space-separated chord of keys.
///
/// Triggers are keyboard-only: they are heard by key grab, and no grab can name a mouse
/// button.
fn parse_trigger(text: &str) -> std::result::Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    for token in text.split_whitespace() {
        if let Some(Holdable::Button(_)) = sequencer_core::input::INPUT_MAP.input_of(token) {
            return Err(format!(
                "`{token}` cannot trigger: mouse buttons are not hearable yet, only sendable"
            ));
        }
        keys.push(
            token
                .parse::<Key>()
                .map_err(|err| format!("in the trigger: {err}"))?,
        );
    }
    if keys.is_empty() {
        return Err("the trigger is empty".to_owned());
    }
    Ok(keys)
}

/// Parses a `seq` list and proves its HOLDs and RELEASEs pair up.
fn parse_seq(lines: &[String]) -> std::result::Result<Vec<Step>, String> {
    if lines.is_empty() {
        return Err("`seq` is empty".to_owned());
    }
    let mut steps = Vec::with_capacity(lines.len());
    let mut held: Vec<Holdable> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let step =
            parse_step(line).map_err(|detail| format!("step {number} `{line}`: {detail}"))?;
        match &step {
            Step::Hold(keys) => held.extend(keys.iter().copied()),
            Step::Release(keys) => {
                for key in keys {
                    let Some(position) = held.iter().rposition(|h| h == key) else {
                        return Err(format!(
                            "step {number} `{line}`: RELEASE {} — nothing is holding it",
                            sequencer_core::input::INPUT_MAP.display_name(*key)
                        ));
                    };
                    held.remove(position);
                }
            }
            Step::Tap(_) | Step::Wait(_) => {}
        }
        steps.push(step);
    }
    if let Some(leftover) = held.first() {
        return Err(format!(
            "HOLD {} is never RELEASEd — a sequence must let go of what it grabbed",
            sequencer_core::input::INPUT_MAP.display_name(*leftover)
        ));
    }
    Ok(steps)
}

/// Parses one step line: keys (a tap), or HOLD/RELEASE/WAIT with their operands.
fn parse_step(line: &str) -> std::result::Result<Step, String> {
    let mut tokens = line.split_whitespace();
    let Some(first) = tokens.next() else {
        return Err("the step is empty".to_owned());
    };
    // The keywords cannot collide with keys: no keyboard has a hold, release or wait
    // key, which is exactly why those words were chosen over `down`/`up` — both of
    // which ARE keys.
    match first.to_ascii_lowercase().as_str() {
        "hold" => Ok(Step::Hold(parse_pressables(tokens, "HOLD")?)),
        "release" => Ok(Step::Release(parse_pressables(tokens, "RELEASE")?)),
        "wait" => {
            let Some(spec) = tokens.next() else {
                return Err("WAIT needs a duration, like `WAIT 200ms`".to_owned());
            };
            if let Some(extra) = tokens.next() {
                return Err(format!("unexpected `{extra}` after the duration"));
            }
            Ok(Step::Wait(parse_duration(spec)?))
        }
        _ => {
            let mut keys = vec![parse_pressable(first)?];
            for token in tokens {
                keys.push(parse_pressable(token)?);
            }
            Ok(Step::Tap(keys))
        }
    }
}

/// Collects the operand keys of a HOLD or RELEASE.
fn parse_pressables<'a>(
    tokens: impl Iterator<Item = &'a str>,
    keyword: &str,
) -> std::result::Result<Vec<Holdable>, String> {
    let keys: Vec<Holdable> = tokens
        .map(parse_pressable)
        .collect::<std::result::Result<_, _>>()?;
    if keys.is_empty() {
        return Err(format!("{keyword} needs at least one key"));
    }
    Ok(keys)
}

/// Parses something that can be pressed, through the shared name↔input map.
///
/// A reserved spelling (wheel notches, pad buttons) gets its own refusal: the name is
/// right, the capability is what's missing, and "unknown key" would send the user
/// hunting for a typo that isn't there.
fn parse_pressable(token: &str) -> std::result::Result<Holdable, String> {
    if let Some(input) = sequencer_core::input::INPUT_MAP.input_of(token) {
        return Ok(input);
    }
    if sequencer_core::input::INPUT_MAP.is_reserved(token) {
        return Err(format!(
            "`{token}` is reserved for the device backend; nothing can send it yet"
        ));
    }
    Err(token
        .parse::<Key>()
        .expect_err("input_of would have found a parseable key")
        .to_string())
}

/// A present-or-absent duration field, with its name for the error.
fn optional_duration(
    text: Option<&str>,
    field: &str,
) -> std::result::Result<Option<Duration>, String> {
    text.map(|value| parse_duration(value).map_err(|detail| format!("{field}: {detail}")))
        .transpose()
}

/// Parses `<num><unit>` with unit ms/s/m/h, decimals allowed. Bare `0` is allowed —
/// zero of anything is zero — and only zero: any other number must say its unit.
fn parse_duration(text: &str) -> std::result::Result<Duration, String> {
    let trimmed = text.trim();
    if trimmed.chars().all(|c| c == '0' || c == '.') && trimmed.contains('0') {
        return Ok(Duration::ZERO);
    }
    let unit_at = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| {
            format!("`{trimmed}` has no unit; write ms, s, m or h (only zero may omit it)")
        })?;
    let (number_text, unit) = trimmed.split_at(unit_at);
    let number: f64 = number_text
        .parse()
        .map_err(|_| format!("`{number_text}` is not a number"))?;
    if !number.is_finite() || number < 0.0 {
        return Err(format!("`{number_text}` is not a usable amount of time"));
    }
    let scale: f64 = match unit.to_ascii_lowercase().as_str() {
        "ms" => 1e6,
        "s" => 1e9,
        "m" => 60.0 * 1e9,
        "h" => 3600.0 * 1e9,
        other => return Err(format!("`{other}` is not a unit; use ms, s, m or h")),
    };
    let nanos = number * scale;
    #[allow(
        clippy::cast_precision_loss,
        reason = "an upper-bound comparison; off-by-a-few nanoseconds at u64::MAX is moot"
    )]
    if nanos > u64::MAX as f64 {
        return Err(format!("`{trimmed}` is longer than this program will live"));
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked non-negative and in range just above"
    )]
    Ok(Duration::from_nanos(nanos as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::input::Button;

    fn parse_ok(text: &str) -> Profile {
        parse(text).expect("should parse")
    }

    fn parse_err(text: &str) -> String {
        parse(text).expect_err("should be refused")
    }

    /// The repository's own template is the format reference, so it must parse — every
    /// feature it documents goes through this one assertion.
    #[test]
    fn the_shipped_template_parses_and_validates() {
        let text = include_str!("../../../../binds.example.toml");
        let profile = parse_ok(text);
        assert_eq!(profile.binds.len(), 4, "PgUp, PgDn, F6 and the chord");
        assert!(
            profile
                .binds
                .iter()
                .any(|b| matches!(&b.action, Action::Mirror(Holdable::Key(Key::VolumeUp)))),
            "PgUp mirrors volume-up"
        );
        let chord = profile
            .binds
            .iter()
            .find(|b| b.trigger.len() == 2)
            .expect("the ctrl i trigger");
        assert_eq!(chord.trigger, vec![Key::LeftCtrl, Key::I]);
    }

    #[test]
    fn a_minimal_mirror_gets_the_built_in_timing() {
        let profile = parse_ok("[binds.PgUp]\nbind = \"volume-up\"");
        assert_eq!(profile.binds[0].tap, DEFAULT_TAP);
        assert_eq!(profile.binds[0].gap, DEFAULT_GAP);
        assert_eq!(
            profile.binds[0].action,
            Action::Mirror(Holdable::Key(Key::VolumeUp))
        );
    }

    #[test]
    fn per_bind_timing_overrides_the_defaults() {
        let profile = parse_ok(
            "[defaults]\ntap = \"20ms\"\n\n[binds.F6]\ntap = \"25ms\"\nseq = [\"a\"]\n\n\
             [binds.F7]\nseq = [\"b\"]",
        );
        assert_eq!(profile.binds[0].tap, Duration::from_millis(25));
        assert_eq!(profile.binds[1].tap, Duration::from_millis(20));
    }

    /// The steps land as written: keywords any case, chords in order, WAIT parsed.
    #[test]
    fn a_sequence_lowers_step_by_step() {
        let profile = parse_ok(
            "[binds.F6]\nseq = [\"shift\", \"HOLD ctrl\", \"space d\", \"wait 200ms\", \
             \"RELEASE ctrl\"]",
        );
        let Action::Seq(steps) = &profile.binds[0].action else {
            panic!("a seq bind");
        };
        assert_eq!(
            steps,
            &[
                Step::Tap(vec![Holdable::Key(Key::LeftShift)]),
                Step::Hold(vec![Holdable::Key(Key::LeftCtrl)]),
                Step::Tap(vec![Holdable::Key(Key::Space), Holdable::Key(Key::D)]),
                Step::Wait(Duration::from_millis(200)),
                Step::Release(vec![Holdable::Key(Key::LeftCtrl)]),
            ]
        );
    }

    /// The rules the template promises about durations: units required, zero exempt.
    #[test]
    fn durations_demand_units_except_for_zero() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_duration("5").is_err(), "nonzero needs a unit");
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("-1ms").is_err());
    }

    #[test]
    fn a_bind_must_be_exactly_one_of_bind_or_seq() {
        assert!(parse_err("[binds.F6]\nbind = \"a\"\nseq = [\"b\"]").contains("one or the other"));
        assert!(parse_err("[binds.F6]").contains("needs exactly one"));
    }

    #[test]
    fn suppress_false_is_refused_not_ignored() {
        let err = parse_err("[defaults]\nsuppress = false\n[binds.F6]\nbind = \"a\"");
        assert!(err.contains("not supported yet"), "{err}");
    }

    /// A HOLD nobody releases is a stuck key written down; refuse it with its name.
    #[test]
    fn unbalanced_holds_are_refused() {
        let err = parse_err("[binds.F6]\nseq = [\"HOLD ctrl\"]");
        assert!(err.contains("never RELEASEd"), "{err}");
        let err = parse_err("[binds.F6]\nseq = [\"RELEASE ctrl\"]");
        assert!(err.contains("nothing is holding it"), "{err}");
    }

    /// `{` and `[` are one key, so two sections triggered by them are one trigger twice
    /// — TOML cannot see that, so validation must.
    #[test]
    fn two_spellings_of_one_trigger_are_refused() {
        let err = parse_err("[binds.\"[\"]\nbind = \"a\"\n\n[binds.\"{\"]\nbind = \"b\"");
        assert!(err.contains("same trigger"), "{err}");
    }

    #[test]
    fn mouse_buttons_send_but_do_not_trigger() {
        let profile = parse_ok("[binds.F6]\nbind = \"mouse1\"");
        assert_eq!(
            profile.binds[0].action,
            Action::Mirror(Holdable::Button(Button::Left))
        );
        let err = parse_err("[binds.mouse1]\nbind = \"a\"");
        assert!(err.contains("cannot trigger"), "{err}");
    }

    #[test]
    fn unknown_fields_and_unknown_keys_name_themselves() {
        assert!(parse_err("[binds.F6]\nbind = \"a\"\nspeed = \"9\"").contains("speed"));
        let err = parse_err("[binds.F6]\nseq = [\"nosuchkey\"]");
        assert!(err.contains("nosuchkey"), "{err}");
        assert!(err.contains("step 1"), "{err}");
    }
}
