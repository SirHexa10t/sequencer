//! The binds-file format: what a profile file may say, and what it means.
//!
//! `example_profile.toml` at the repository root is the reference; this file is its
//! implementation, and everything here is pure — the same text is accepted or refused
//! identically on any machine, which is what lets the whole grammar be tested headless.
//!
//! Validation is strict on purpose: a binds file is user input that will be *acted on*,
//! and a file that silently means something other than what it says (a PRESS nobody
//! releases, a `suppress = false` nothing honours, two spellings of one trigger) is
//! refused with the reason rather than reinterpreted.

use std::collections::BTreeMap;

use sequencer_core::emit::Holdable;
use sequencer_core::input::{Key, Mods};
use sequencer_core::time::Duration;

/// A parsed, validated binds file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    /// Every binding, in trigger-name order.
    pub(crate) binds: Vec<Bind>,
    /// Apply only while the focused program matches one of these patterns (`*`
    /// wildcards, case-insensitive). `None` means the profile always applies.
    pub(crate) program: Option<Vec<String>>,
    /// Chords that stop the whole run gracefully, releasing everything held — any
    /// one of them fires it. Empty when the file names none.
    pub(crate) emergency_stop: Vec<Vec<Key>>,
}

/// One `[binds.<trigger>]` section.
#[derive(Debug, Clone, PartialEq)]
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
    /// How many times a press runs the sequence. Pressing the trigger again stops it
    /// early, releasing whatever the sequence still held.
    pub(crate) loops: Loops,
}

/// How often one press repeats a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Loops {
    /// Once — the unwritten default; `loop = 1` spells it out.
    #[default]
    Once,
    /// A fixed number of runs.
    Times(u32),
    /// Until the trigger is pressed again (`loop = "inf"`).
    Infinite,
}

/// What a binding does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    /// Each press of the trigger taps the target — one key or a chord, spelled like any
    /// step: down in listed order, up in reverse.
    Mirror(Vec<Holdable>),
    /// A sequence, fired once per press of the trigger.
    Seq(Vec<Step>),
}

/// One entry of a `seq` list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Step {
    /// Down, wait `tap`, up in reverse order. Several members are a chord.
    Tap(Vec<Holdable>),
    /// Press and keep down until a later [`Step::Release`].
    Hold(Vec<Holdable>),
    /// Release something a [`Step::Hold`] pressed.
    Release(Vec<Holdable>),
    /// Pause, replacing the default gap at this seam.
    Wait(Duration),
    /// Roll the dice: with this chance the block runs, otherwise skip to the matching
    /// [`Step::RngEnd`]. Blocks nest.
    Rng(f64),
    /// Closes an [`Step::Rng`] block, `fi` to its `if`.
    RngEnd,
    /// Run the block this many times, then continue past the matching
    /// [`Step::LoopEnd`]. Nests with itself and with RNG blocks.
    Loop(u32),
    /// Closes a [`Step::Loop`] block, `POOL` to its `LOOP`.
    LoopEnd,
}

/// The built-in timing, used wherever the file does not say otherwise. These are the
/// same values `example_profile.toml` documents as the defaults.
const DEFAULT_TAP: Duration = Duration::from_millis(8);
const DEFAULT_GAP: Duration = Duration::from_millis(30);

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
    program: Option<OneOrMany>,
    emergency_stop: Option<OneOrMany>,
}

/// A field spelled as one string or a list of alternatives — `program`'s patterns
/// and `emergency_stop`'s chords both read this way.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(one) => vec![one],
            Self::Many(many) => many,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBind {
    bind: Option<String>,
    seq: Option<Vec<String>>,
    /// Extra trigger spellings for the same action; each becomes its own bind.
    also: Option<Vec<String>>,
    tap: Option<String>,
    gap: Option<String>,
    /// `loop` in the file; TOML has no bare keys, so infinity is spelled `"inf"`.
    #[serde(rename = "loop")]
    loops: Option<RawLoops>,
}

/// `loop = 4` or `loop = "inf"`.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum RawLoops {
    Times(u32),
    Word(String),
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
pub(crate) fn parse(text: &str) -> Result<Profile, String> {
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
    let program = parse_program(defaults.program)?;
    let emergency_stop = parse_emergency(defaults.emergency_stop)?;

    let mut binds = Vec::with_capacity(raw.binds.len());
    let mut seen: BTreeMap<Vec<Key>, String> = BTreeMap::new();
    for (trigger_text, raw_bind) in raw.binds {
        let context = format!("[binds.\"{trigger_text}\"]");
        let bind = lower_bind(&trigger_text, &raw_bind, default_tap, default_gap)
            .map_err(|detail| format!("{context}: {detail}"))?;
        // `also` spellings are full binds of their own, sharing the section's action
        // and timing; every check below sees them individually.
        let mut lowered = vec![bind];
        if let Some(spellings) = &raw_bind.also {
            if spellings.is_empty() {
                return Err(format!("{context}: `also` is empty; drop the field"));
            }
            for spelling in spellings {
                let trigger = parse_trigger(spelling)
                    .map_err(|detail| format!("{context}: also \"{spelling}\": {detail}"))?;
                let mut alias = lowered[0].clone();
                alias.trigger_text.clone_from(spelling);
                alias.trigger = trigger;
                lowered.push(alias);
            }
        }
        // Two spellings of one trigger ({ and [, A and a) are one key twice, which TOML
        // itself cannot see. Later sections would silently shadow earlier ones.
        for bind in lowered {
            if let Some(previous) = seen.insert(bind.trigger.clone(), bind.trigger_text.clone()) {
                return Err(format!(
                    "{context}: this is the same trigger as \"{previous}\" — two \
                     spellings of one key"
                ));
            }
            binds.push(bind);
        }
    }
    for stop in &emergency_stop {
        if let Some(bind) = binds.iter().find(|bind| &bind.trigger == stop) {
            return Err(format!(
                "emergency_stop is also the trigger of [binds.\"{}\"] — one key cannot both \
                 run a binding and stop the run",
                bind.trigger_text
            ));
        }
    }
    // A grab fires on one keycode; binding both `i` and `ctrl i` would leave which one
    // the server delivers up to modifier luck. Refuse rather than roll dice.
    if let Some(clash) = overlapping_trigger(&binds) {
        return Err(clash);
    }
    // Left and right shift/ctrl/meta share one X modifier bit, so `rshift >` and
    // `shift >` are the SAME grab — the server cannot tell them apart, and the second
    // would fail (or worse, silently shadow). Alt is the exception: right alt is AltGr,
    // its own bit.
    let mut grabs: BTreeMap<(Option<Key>, Mods), String> = BTreeMap::new();
    for bind in &binds {
        let signature = (primary_key(&bind.trigger), chord_mods(&bind.trigger));
        let label = format!("[binds.\"{}\"]", bind.trigger_text);
        if let Some(previous) = grabs.insert(signature, label.clone()) {
            return Err(format!(
                "{previous} and {label} are the same grab: X cannot tell left from \
                 right shift/ctrl/meta, so these triggers are one key combination twice"
            ));
        }
    }
    // The stop chords join the same grab space — the manager grabs them alongside
    // the triggers, and X cannot hold two grabs on one combination.
    for stop in &emergency_stop {
        let signature = (primary_key(stop), chord_mods(stop));
        let label = format!("emergency_stop \"{}\"", chord_text(stop));
        if let Some(previous) = grabs.insert(signature, label.clone()) {
            return Err(format!(
                "{previous} and {label} are the same grab: X cannot tell left from \
                 right shift/ctrl/meta, so these are one key combination twice"
            ));
        }
    }
    Ok(Profile {
        binds,
        program,
        emergency_stop,
    })
}

/// A chord the way a user would write it: the keys' display names, space-joined.
pub(super) fn chord_text(chord: &[Key]) -> String {
    chord
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one non-modifier key of a chord — what an X grab actually fires on.
///
/// Mirrors the split the grab layer performs, so the manager can route an arriving
/// keycode back to the bind that asked for it. `None` for an all-modifier chord, which
/// validation refuses anyway.
pub(crate) fn primary_key(chord: &[Key]) -> Option<Key> {
    chord.iter().copied().find(|key| !is_modifier(*key))
}

/// The modifier classes a chord names, folded the way the grab layer folds them —
/// [`Mods`] is the one table for it, shared with the live event path so a trigger
/// and the event it fires can never disagree about what "its modifiers" means.
pub(crate) fn chord_mods(chord: &[Key]) -> Mods {
    Mods::of_chord(chord)
}

/// The modifier classes a bind's *target* names — the keys it will synthesize.
pub(crate) fn target_mods(targets: &[Holdable]) -> Mods {
    Mods::of_chord(
        &targets
            .iter()
            .filter_map(|target| match target {
                Holdable::Key(key) => Some(*key),
                Holdable::Button(_) => None,
            })
            .collect::<Vec<_>>(),
    )
}

/// Advisory notes for a profile that parses and validates fine but will surprise:
/// a mirror whose trigger holds modifiers its target does not name cannot fire
/// while the hand is still down — the held modifiers would recolour the injected
/// keys (`]` under a held shift arrives as `}`) — so its tap is deferred until the
/// trigger's modifiers are released.
pub(crate) fn warnings(profile: &Profile) -> Vec<String> {
    let mut notes = Vec::new();
    for bind in &profile.binds {
        let Action::Mirror(targets) = &bind.action else {
            continue;
        };
        let held = chord_mods(&bind.trigger);
        let wanted = target_mods(targets);
        if !wanted.covers(held) {
            notes.push(format!(
                "[binds.\"{}\"]: the held {held} would recolour the target, so this \
                 tap fires only once {held} is released; a trigger without modifiers \
                 is the straightforward alternative",
                bind.trigger_text
            ));
        } else if wanted == held
            && primary_key(&bind.trigger).is_some_and(|primary| {
                targets
                    .iter()
                    .any(|target| matches!(target, Holdable::Key(key) if *key == primary))
            })
        {
            // Grabs hear injections like physical input, so a target that lands as
            // exactly this trigger fires the grab again — and again.
            notes.push(format!(
                "[binds.\"{}\"]: the target lands as exactly this trigger, so the \
                 mirror re-triggers itself and loops until an emergency stop; \
                 retarget it or change the trigger",
                bind.trigger_text
            ));
        }
    }
    notes
}

/// Whether a key only decorates a chord rather than anchoring it.
pub(super) fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::LeftShift
            | Key::RightShift
            | Key::LeftCtrl
            | Key::RightCtrl
            | Key::LeftAlt
            | Key::RightAlt
            | Key::LeftMeta
            | Key::RightMeta
    )
}

/// The first pair of triggers where one is a bare key another chord decorates.
///
/// `i` and `ctrl i` grab the same keycode, so the X server's choice between them would
/// depend on modifier state it does not promise to arbitrate. One profile may hold both
/// `ctrl i` and `alt i` (distinct masks) — only the *bare* key clashes.
fn overlapping_trigger(binds: &[Bind]) -> Option<String> {
    let bare: Vec<&Bind> = binds
        .iter()
        .filter(|bind| bind.trigger.len() == 1)
        .collect();
    for chord in binds.iter().filter(|bind| bind.trigger.len() > 1) {
        for plain in &bare {
            let key = plain.trigger[0];
            if chord.trigger.contains(&key) {
                return Some(format!(
                    "[binds.\"{}\"] and [binds.\"{}\"] both grab the same key; a chord and \
                     the bare key it contains cannot both be triggers",
                    plain.trigger_text, chord.trigger_text
                ));
            }
        }
    }
    None
}

/// Checks the `program` patterns: optional, but never empty — an empty pattern (or an
/// empty list) matches nothing forever, which is a disabled profile pretending to be a
/// working one. One pattern or several: the profile applies while ANY matches.
fn parse_program(program: Option<OneOrMany>) -> Result<Option<Vec<String>>, String> {
    let Some(patterns) = program.map(OneOrMany::into_vec) else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Err("defaults.program is an empty list; drop the field to always apply".to_owned());
    }
    if patterns.iter().any(|pattern| pattern.trim().is_empty()) {
        return Err("defaults.program has an empty pattern; drop it to always apply".to_owned());
    }
    Ok(Some(patterns))
}

/// Parses `emergency_stop`: one key or chord spelled like any trigger, or a list of
/// alternatives — any one of them stops the run.
fn parse_emergency(raw: Option<OneOrMany>) -> Result<Vec<Vec<Key>>, String> {
    let Some(spellings) = raw.map(OneOrMany::into_vec) else {
        return Ok(Vec::new());
    };
    if spellings.is_empty() {
        return Err(
            "emergency_stop is an empty list; drop the field to run without one".to_owned(),
        );
    }
    spellings
        .iter()
        .map(|spelling| {
            parse_trigger(spelling).map_err(|detail| format!("emergency_stop: {detail}"))
        })
        .collect()
}

/// Whether the focused program's name matches a `program` pattern.
///
/// Case-insensitive, `*` matches any run of characters (including none). That is the
/// whole language: enough for launcher-decorated names like `steam_app_*`, small enough
/// to hold no surprises.
// Only the X11 manager loop consults focus today, but the matcher itself is pure and
// tested unconditionally.
#[cfg_attr(
    not(all(feature = "xtest", target_os = "linux")),
    allow(dead_code, reason = "used by the X11 manager loop and by tests")
)]
pub(crate) fn program_matches(pattern: &str, class: &str) -> bool {
    fn glob(pattern: &[u8], text: &[u8]) -> bool {
        // Classic two-pointer wildcard match with backtracking to the last `*`.
        let (mut p, mut t) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while t < text.len() {
            if p < pattern.len() && (pattern[p] == text[t]) {
                p += 1;
                t += 1;
            } else if p < pattern.len() && pattern[p] == b'*' {
                star = Some((p, t));
                p += 1;
            } else if let Some((star_p, star_t)) = star {
                p = star_p + 1;
                t = star_t + 1;
                star = Some((star_p, star_t + 1));
            } else {
                return false;
            }
        }
        pattern[p..].iter().all(|&c| c == b'*')
    }
    glob(
        pattern.to_ascii_lowercase().as_bytes(),
        class.to_ascii_lowercase().as_bytes(),
    )
}

/// Whether the focused program matches any of a profile's `program` patterns.
///
/// The list is alternatives, nothing more: `["*mpv*", "*celluloid*"]` covers both
/// spellings of one player, and a single pattern behaves exactly as it did alone.
#[cfg_attr(
    not(all(feature = "xtest", target_os = "linux")),
    allow(dead_code, reason = "used by the X11 manager loop and by tests")
)]
pub(crate) fn program_applies(patterns: &[String], class: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| program_matches(pattern, class))
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
) -> Result<Bind, String> {
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
        (Some(target), None) => Action::Mirror(parse_target(target)?),
        (None, Some(steps)) => Action::Seq(parse_seq(steps)?),
    };
    let loops = parse_loops(raw.loops.as_ref())?;
    if loops != Loops::Once && matches!(action, Action::Mirror(_)) {
        return Err(
            "`loop` needs a `seq`: a mirror follows the trigger's own edges and has \
             nothing to repeat"
                .to_owned(),
        );
    }
    Ok(Bind {
        trigger_text: trigger_text.to_owned(),
        trigger,
        action,
        tap,
        gap,
        loops,
    })
}

/// Parses `loop`: a run count, or `"inf"` for until-stopped.
fn parse_loops(raw: Option<&RawLoops>) -> Result<Loops, String> {
    match raw {
        None | Some(RawLoops::Times(1)) => Ok(Loops::Once),
        Some(RawLoops::Times(0)) => {
            Err("loop = 0 would never run; drop the field or use 1".to_owned())
        }
        Some(RawLoops::Times(times)) => Ok(Loops::Times(*times)),
        Some(RawLoops::Word(word)) if word.eq_ignore_ascii_case("inf") => Ok(Loops::Infinite),
        Some(RawLoops::Word(word)) => Err(format!(
            "loop = \"{word}\" is not a count; use a number or \"inf\""
        )),
    }
}

/// Parses a `bind` target: one pressable, or a space-separated chord of them.
///
/// The same spelling as everywhere else in the format — `bind = "shift ]"` taps
/// shift+] together, exactly as the step `"shift ]"` would inside a `seq`.
fn parse_target(text: &str) -> Result<Vec<Holdable>, String> {
    let targets: Vec<Holdable> = text
        .split_whitespace()
        .map(parse_pressable)
        .collect::<Result<_, _>>()?;
    if targets.is_empty() {
        return Err("`bind` is empty; name a key or a chord".to_owned());
    }
    Ok(targets)
}

/// Parses a trigger: one key, or a space-separated chord of keys.
///
/// Triggers are keyboard-only: they are heard by key grab, and no grab can name a mouse
/// button.
fn parse_trigger(text: &str) -> Result<Vec<Key>, String> {
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
    // The grab layer's shape, checked here so a bad chord is a parse error with the
    // file's own words rather than a runtime refusal.
    let ordinary = keys.iter().filter(|key| !is_modifier(**key)).count();
    if ordinary == 0 {
        return Err("a trigger needs one non-modifier key, not modifiers alone".to_owned());
    }
    if ordinary > 1 {
        return Err("a chord may decorate only one ordinary key (modifiers + one key)".to_owned());
    }
    Ok(keys)
}

/// One block a `seq` still has open while it is checked: what opened it, and (for
/// LOOP) how much was held at entry — a repeated block must let go of what it pressed.
enum OpenBlock {
    Rng,
    Loop { held_at_entry: usize },
}

/// Parses a `seq` list and proves its PRESSes/RELEASEs pair up and its RNG/GNR and
/// LOOP/POOL blocks close properly — inside each other, never across.
fn parse_seq(lines: &[String]) -> Result<Vec<Step>, String> {
    if lines.is_empty() {
        return Err("`seq` is empty".to_owned());
    }
    let mut steps = Vec::with_capacity(lines.len());
    let mut held: Vec<Holdable> = Vec::new();
    let mut blocks: Vec<OpenBlock> = Vec::new();
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
                            "step {number} `{line}`: RELEASE {} — nothing is pressing it",
                            sequencer_core::input::INPUT_MAP.display_name(*key)
                        ));
                    };
                    held.remove(position);
                }
            }
            Step::Rng(_) => blocks.push(OpenBlock::Rng),
            Step::RngEnd => match blocks.pop() {
                Some(OpenBlock::Rng) => {}
                Some(OpenBlock::Loop { .. }) => {
                    return Err(format!(
                        "step {number} `{line}`: GNR without a matching RNG — the open \
                         block is a LOOP; close it with POOL first"
                    ));
                }
                None => {
                    return Err(format!(
                        "step {number} `{line}`: GNR without a matching RNG"
                    ));
                }
            },
            Step::Loop(_) => blocks.push(OpenBlock::Loop {
                held_at_entry: held.len(),
            }),
            Step::LoopEnd => match blocks.pop() {
                Some(OpenBlock::Loop { held_at_entry }) => {
                    // A hold that leaks across POOL would stack up an extra press
                    // every iteration; a release of an outer hold would find nothing
                    // left after the first.
                    if held.len() > held_at_entry {
                        return Err(format!(
                            "step {number} `{line}`: PRESS {} is never RELEASEd before \
                             its POOL — each iteration must let go of what it pressed",
                            sequencer_core::input::INPUT_MAP.display_name(held[held_at_entry])
                        ));
                    }
                    if held.len() < held_at_entry {
                        return Err(format!(
                            "step {number} `{line}`: this block RELEASEs a key pressed \
                             before it — after the first iteration there is nothing \
                             left to release"
                        ));
                    }
                }
                Some(OpenBlock::Rng) => {
                    return Err(format!(
                        "step {number} `{line}`: POOL without a matching LOOP — the open \
                         block is an RNG; close it with GNR first"
                    ));
                }
                None => {
                    return Err(format!(
                        "step {number} `{line}`: POOL without a matching LOOP"
                    ));
                }
            },
            Step::Tap(_) | Step::Wait(_) => {}
        }
        steps.push(step);
    }
    if let Some(leftover) = held.first() {
        return Err(format!(
            "PRESS {} is never RELEASEd — a sequence must let go of what it pressed",
            sequencer_core::input::INPUT_MAP.display_name(*leftover)
        ));
    }
    match blocks.last() {
        Some(OpenBlock::Rng) => {
            Err("RNG without a matching GNR — every chance block must close".to_owned())
        }
        Some(OpenBlock::Loop { .. }) => {
            Err("LOOP without a matching POOL — every loop block must close".to_owned())
        }
        None => Ok(steps),
    }
}

/// Parses one step line: keys (a tap), or a keyword (PRESS/RELEASE/WAIT/RNG/GNR/
/// LOOP/POOL) with its operands.
fn parse_step(line: &str) -> Result<Step, String> {
    let mut tokens = line.split_whitespace();
    let Some(first) = tokens.next() else {
        return Err("the step is empty".to_owned());
    };
    // The keywords cannot collide with keys: no keyboard has a press, release, wait,
    // rng, loop or pool key, which is exactly why these words were chosen over
    // `down`/`up` — both of which ARE keys.
    match first.to_ascii_lowercase().as_str() {
        "press" => Ok(Step::Hold(parse_pressables(tokens, "PRESS")?)),
        "release" => Ok(Step::Release(parse_pressables(tokens, "RELEASE")?)),
        "rng" => {
            let Some(spec) = tokens.next() else {
                return Err("RNG needs a chance, like `RNG 30%`".to_owned());
            };
            if let Some(extra) = tokens.next() {
                return Err(format!("unexpected `{extra}` after the chance"));
            }
            Ok(Step::Rng(parse_chance(spec)?))
        }
        "gnr" => {
            if let Some(extra) = tokens.next() {
                return Err(format!(
                    "GNR closes a block and takes nothing, got `{extra}`"
                ));
            }
            Ok(Step::RngEnd)
        }
        "loop" => {
            let Some(spec) = tokens.next() else {
                return Err("LOOP needs a count, like `LOOP 5`".to_owned());
            };
            if let Some(extra) = tokens.next() {
                return Err(format!("unexpected `{extra}` after the count"));
            }
            Ok(Step::Loop(parse_count(spec)?))
        }
        "pool" => {
            if let Some(extra) = tokens.next() {
                return Err(format!(
                    "POOL closes a block and takes nothing, got `{extra}`"
                ));
            }
            Ok(Step::LoopEnd)
        }
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

/// Collects the operand keys of a PRESS or RELEASE.
fn parse_pressables<'a>(
    tokens: impl Iterator<Item = &'a str>,
    keyword: &str,
) -> Result<Vec<Holdable>, String> {
    let keys: Vec<Holdable> = tokens.map(parse_pressable).collect::<Result<_, _>>()?;
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
fn parse_pressable(token: &str) -> Result<Holdable, String> {
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

/// Parses a LOOP count: a whole number of runs, at least one. (For a block that runs
/// until stopped, the bind-level `loop = "inf"` is the tool — an endless block inside
/// a sequence could never reach the steps after it.)
fn parse_count(text: &str) -> Result<u32, String> {
    let count: u32 = text
        .parse()
        .map_err(|_| format!("`{text}` is not a count; use a number, like `LOOP 5`"))?;
    if count == 0 {
        return Err("LOOP 0 would never run; drop the block or use 1".to_owned());
    }
    Ok(count)
}

/// Parses an RNG chance: `0.30`, `30%` or `3/10` — three spellings, one probability.
fn parse_chance(text: &str) -> Result<f64, String> {
    let value = if let Some(percent) = text.strip_suffix('%') {
        percent
            .parse::<f64>()
            .map_err(|_| format!("`{percent}` is not a number"))?
            / 100.0
    } else if let Some((num, den)) = text.split_once('/') {
        let num: f64 = num
            .parse()
            .map_err(|_| format!("`{num}` is not a number"))?;
        let den: f64 = den
            .parse()
            .map_err(|_| format!("`{den}` is not a number"))?;
        if den <= 0.0 {
            return Err(format!("`{text}` divides by {den}"));
        }
        num / den
    } else {
        text.parse::<f64>()
            .map_err(|_| format!("`{text}` is not a chance; use 0.30, 30% or 3/10"))?
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("`{text}` is not between never (0) and always (1)"));
    }
    Ok(value)
}

/// A present-or-absent duration field, with its name for the error.
fn optional_duration(text: Option<&str>, field: &str) -> Result<Option<Duration>, String> {
    text.map(|value| parse_duration(value).map_err(|detail| format!("{field}: {detail}")))
        .transpose()
}

/// Parses `<num><unit>` with unit ms/s/m/h, decimals allowed. Bare `0` is allowed —
/// zero of anything is zero — and only zero: any other number must say its unit.
fn parse_duration(text: &str) -> Result<Duration, String> {
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

    /// The advisory list: a mirror whose trigger holds modifiers its target does not
    /// name gets one (its tap is deferred), everything else stays quiet — a target
    /// that names the held classes, a bare trigger, and a sequence (whose steps are
    /// the author's own to time).
    #[test]
    fn only_contaminated_mirrors_draw_a_warning() {
        let warned = parse("[binds.\"shift b\"]\nbind = \"p\"").expect("parses");
        let notes = warnings(&warned);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains("shift") && notes[0].contains("without modifiers"),
            "the note names the held class and the straightforward fix: {}",
            notes[0]
        );

        for quiet in [
            "[binds.\"shift n\"]\nbind = \"shift >\"",
            "[binds.b]\nbind = \"p\"",
            "[binds.\"ctrl b\"]\nseq = [\"p\"]",
            "[binds.\"ctrl w\"]\nbind = \"ctrl shift w\"",
        ] {
            let profile = parse(quiet).expect("parses");
            assert_eq!(warnings(&profile), Vec::<String>::new(), "for {quiet}");
        }
    }

    /// Injections pass through grabs like physical input, so a target that lands as
    /// exactly its own trigger is an infinite loop — worth a red flag, in both the
    /// bare spelling and the chord spelling.
    #[test]
    fn a_self_retriggering_mirror_is_warned_about() {
        for looping in [
            "[binds.a]\nbind = \"a\"",
            "[binds.\"ctrl a\"]\nbind = \"ctrl a\"",
        ] {
            let profile = parse(looping).expect("parses");
            let notes = warnings(&profile);
            assert_eq!(notes.len(), 1, "for {looping}: {notes:?}");
            assert!(notes[0].contains("re-triggers itself"), "{}", notes[0]);
        }
    }
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
        let text = include_str!("../../../../example_profile.toml");
        let profile = parse_ok(text);
        assert_eq!(
            profile.binds.len(),
            6,
            "PgUp, PgDn, F6, F7, the chord and its `also` alias"
        );
        assert_eq!(profile.program, Some(vec!["*".to_owned()]));
        assert_eq!(profile.emergency_stop, vec![vec![Key::F8]]);
        assert!(
            profile
                .binds
                .iter()
                .any(|bind| bind.loops == Loops::Infinite),
            "the template shows an infinite loop"
        );
        assert!(
            profile
                .binds
                .iter()
                .any(|b| b.action == Action::Mirror(vec![Holdable::Key(Key::VolumeUp)])),
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
            Action::Mirror(vec![Holdable::Key(Key::VolumeUp)])
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
            "[binds.F6]\nseq = [\"shift\", \"PRESS ctrl\", \"space d\", \"wait 200ms\", \
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

    /// A PRESS nobody releases is a stuck key written down; refuse it with its name.
    #[test]
    fn unbalanced_presses_are_refused() {
        let err = parse_err("[binds.F6]\nseq = [\"PRESS ctrl\"]");
        assert!(err.contains("never RELEASEd"), "{err}");
        let err = parse_err("[binds.F6]\nseq = [\"RELEASE ctrl\"]");
        assert!(err.contains("nothing is pressing it"), "{err}");
    }

    /// Chords are grabbable now, within the shape X can express: modifiers plus one
    /// ordinary key. The bad shapes are refused at parse time, in the file's words.
    #[test]
    fn chords_take_modifiers_plus_exactly_one_key() {
        let profile = parse_ok("[binds.\"ctrl shift i\"]\nseq = [\"a\"]");
        assert_eq!(
            profile.binds[0].trigger,
            vec![Key::LeftCtrl, Key::LeftShift, Key::I]
        );
        assert_eq!(primary_key(&profile.binds[0].trigger), Some(Key::I));

        let err = parse_err("[binds.\"a b\"]\nseq = [\"x\"]");
        assert!(err.contains("only one ordinary key"), "{err}");
        let err = parse_err("[binds.\"ctrl shift\"]\nseq = [\"x\"]");
        assert!(err.contains("not modifiers alone"), "{err}");
    }

    /// `i` and `ctrl i` grab the same keycode, so one profile may not hold both — but
    /// two different chords over that key are fine, since their masks differ.
    #[test]
    fn a_bare_key_and_a_chord_over_it_cannot_coexist() {
        let err = parse_err("[binds.i]\nseq = [\"a\"]\n\n[binds.\"ctrl i\"]\nseq = [\"b\"]");
        assert!(err.contains("same key"), "{err}");
        parse_ok("[binds.\"ctrl i\"]\nseq = [\"a\"]\n\n[binds.\"alt i\"]\nseq = [\"b\"]");
    }

    /// An emergency stop may be a chord now; it still cannot double as a trigger.
    #[test]
    fn emergency_stop_accepts_a_chord() {
        let profile =
            parse_ok("[defaults]\nemergency_stop = \"ctrl alt q\"\n[binds.F6]\nseq = [\"a\"]");
        assert_eq!(
            profile.emergency_stop,
            vec![vec![Key::LeftCtrl, Key::LeftAlt, Key::Q]]
        );
        let err =
            parse_err("[defaults]\nemergency_stop = \"ctrl q\"\n[binds.\"ctrl q\"]\nseq = [\"a\"]");
        assert!(err.contains("cannot both"), "{err}");
    }

    /// `bind` takes a chord spelled like any step — the exact shape that failed in the
    /// field: `bind = "shift ]"` on a `rshift >` trigger.
    #[test]
    fn bind_targets_take_chords() {
        let profile = parse_ok("[binds.\"rshift >\"]\nbind = \"shift ]\"");
        assert_eq!(profile.binds[0].trigger, vec![Key::RightShift, Key::Period]);
        assert_eq!(
            profile.binds[0].action,
            Action::Mirror(vec![
                Holdable::Key(Key::LeftShift),
                Holdable::Key(Key::RightBracket)
            ])
        );
        assert!(parse_err("[binds.F6]\nbind = \" \"").contains("empty"));
    }

    /// X folds left and right shift/ctrl/meta into one modifier bit, so two triggers
    /// that differ only by side are one grab — refused with both names. Alt is the
    /// exception: right alt is AltGr, its own bit.
    #[test]
    fn side_blind_duplicate_grabs_are_refused() {
        let err =
            parse_err("[binds.\"rshift >\"]\nbind = \"a\"\n\n[binds.\"shift >\"]\nbind = \"b\"");
        assert!(err.contains("same grab"), "{err}");
        parse_ok("[binds.\"alt i\"]\nbind = \"a\"\n\n[binds.\"ralt i\"]\nbind = \"b\"");
    }

    /// The chance spellings are one probability three ways, and nonsense is refused.
    #[test]
    fn chances_parse_in_all_three_spellings() {
        assert!((parse_chance("0.25").unwrap() - 0.25).abs() < 1e-9);
        assert!((parse_chance("25%").unwrap() - 0.25).abs() < 1e-9);
        assert!((parse_chance("1/4").unwrap() - 0.25).abs() < 1e-9);
        assert!(parse_chance("1.5").is_err(), "over 1 is not a chance");
        assert!(parse_chance("-1%").is_err());
        assert!(parse_chance("1/0").is_err());
        assert!(parse_chance("maybe").is_err());
    }

    /// RNG/GNR pair like PRESS/RELEASE: unbalanced blocks are refused with direction.
    #[test]
    fn unbalanced_rng_blocks_are_refused() {
        let err = parse_err("[binds.F6]\nseq = [\"RNG 50%\", \"a\"]");
        assert!(err.contains("without a matching GNR"), "{err}");
        let err = parse_err("[binds.F6]\nseq = [\"a\", \"GNR\"]");
        assert!(err.contains("GNR without a matching RNG"), "{err}");
    }

    /// `loop` counts, spells infinity as "inf", and refuses the meaningless.
    #[test]
    fn loops_parse_and_guard_their_edges() {
        let profile = parse_ok("[binds.F6]\nloop = 4\nseq = [\"a\"]");
        assert_eq!(profile.binds[0].loops, Loops::Times(4));
        let profile = parse_ok("[binds.F6]\nloop = \"INF\"\nseq = [\"a\"]");
        assert_eq!(profile.binds[0].loops, Loops::Infinite);
        assert!(parse_err("[binds.F6]\nloop = 0\nseq = [\"a\"]").contains("never run"));
        assert!(parse_err("[binds.F6]\nloop = \"forever\"\nseq = [\"a\"]").contains("inf"));
        let err = parse_err("[binds.PgUp]\nloop = 2\nbind = \"volume-up\"");
        assert!(err.contains("needs a `seq`"), "{err}");
    }

    /// The emergency key cannot moonlight as a trigger, and program must say something.
    #[test]
    fn emergency_and_program_guard_their_edges() {
        let err = parse_err("[defaults]\nemergency_stop = \"F6\"\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("cannot both"), "{err}");
        let err = parse_err("[defaults]\nprogram = \" \"\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("empty"), "{err}");
    }

    /// The wildcard language: `*` spans anything, matching ignores case.
    #[test]
    fn program_patterns_match_like_globs() {
        assert!(program_matches("firefox", "Firefox"));
        assert!(program_matches("steam_app_*", "steam_app_12345"));
        assert!(program_matches("*fox", "firefox"));
        assert!(program_matches("*", "anything"));
        assert!(!program_matches("firefox", "chromium"));
        assert!(!program_matches("steam_app_*", "steam"));
    }

    /// `program` takes one pattern or a list of alternatives, applied while ANY
    /// matches — one file can name every spelling a player hides behind.
    #[test]
    fn program_accepts_a_list_of_alternatives() {
        let profile = parse_ok(
            "[defaults]\nprogram = [\"*mpv*\", \"*celluloid*\"]\n[binds.F6]\nseq = [\"a\"]",
        );
        let patterns = profile.program.expect("patterns");
        assert!(program_applies(&patterns, "Celluloid"));
        assert!(program_applies(&patterns, "mpv"));
        assert!(!program_applies(&patterns, "firefox"));

        let single = parse_ok("[defaults]\nprogram = \"*terminal*\"\n[binds.F6]\nseq = [\"a\"]");
        assert_eq!(single.program, Some(vec!["*terminal*".to_owned()]));
    }

    /// An empty list, like an empty pattern, is a profile that can never apply —
    /// refused with the way out rather than silently disabled.
    #[test]
    fn empty_program_lists_and_patterns_are_refused() {
        let err = parse_err("[defaults]\nprogram = []\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("empty list"), "{err}");
        let err = parse_err("[defaults]\nprogram = [\"*mpv*\", \" \"]\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("empty pattern"), "{err}");
    }

    /// `emergency_stop` may list alternatives — any one of them is the stop — and an
    /// empty list is refused like an empty pattern.
    #[test]
    fn emergency_stop_accepts_a_list() {
        let profile = parse_ok(
            "[defaults]\nemergency_stop = [\"F8\", \"ctrl shift e\"]\n[binds.F6]\nseq = [\"a\"]",
        );
        assert_eq!(
            profile.emergency_stop,
            vec![vec![Key::F8], vec![Key::LeftCtrl, Key::LeftShift, Key::E]]
        );
        let err = parse_err("[defaults]\nemergency_stop = []\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("empty list"), "{err}");
    }

    /// A stop chord that lands on an existing grab — its own twin, or a bind's
    /// side-blind fold — is refused: X cannot hold two grabs on one combination.
    #[test]
    fn a_stop_chord_clashing_with_a_grab_is_refused() {
        let err =
            parse_err("[defaults]\nemergency_stop = [\"F8\", \"F8\"]\n[binds.F6]\nseq = [\"a\"]");
        assert!(err.contains("same grab"), "{err}");
        let err = parse_err(
            "[defaults]\nemergency_stop = \"rshift F9\"\n[binds.\"shift F9\"]\nseq = [\"a\"]",
        );
        assert!(err.contains("same grab"), "{err}");
    }

    /// `also` spellings expand into full binds sharing the action — and the usual
    /// duplicate checks see each one individually.
    #[test]
    fn also_spellings_expand_into_their_own_binds() {
        let profile = parse_ok("[binds.F6]\nalso = [\"F7\", \"ctrl i\"]\nbind = \"p\"");
        assert_eq!(profile.binds.len(), 3);
        assert!(
            profile
                .binds
                .iter()
                .all(|bind| bind.action == Action::Mirror(vec![Holdable::Key(Key::P)])),
            "every spelling carries the section's action"
        );
        assert_eq!(profile.binds[0].trigger_text, "F6");
        assert_eq!(profile.binds[1].trigger, vec![Key::F7]);
        assert_eq!(profile.binds[2].trigger, vec![Key::LeftCtrl, Key::I]);

        let err = parse_err("[binds.F6]\nalso = [\"F6\"]\nbind = \"p\"");
        assert!(err.contains("same trigger"), "{err}");
        let err = parse_err("[binds.F6]\nalso = []\nbind = \"p\"");
        assert!(err.contains("empty"), "{err}");
    }

    /// LOOP lowers with its count and POOL closes it — the RNG pair's repeat twin,
    /// guarding its edges the same way.
    #[test]
    fn loop_blocks_lower_and_guard_their_edges() {
        let profile = parse_ok("[binds.F6]\nseq = [\"LOOP 5\", \"a\", \"POOL\"]");
        let Action::Seq(steps) = &profile.binds[0].action else {
            panic!("a seq bind");
        };
        assert_eq!(
            steps,
            &[
                Step::Loop(5),
                Step::Tap(vec![Holdable::Key(Key::A)]),
                Step::LoopEnd,
            ]
        );

        assert!(parse_err("[binds.F6]\nseq = [\"LOOP\"]").contains("needs a count"));
        assert!(parse_err("[binds.F6]\nseq = [\"LOOP 0\", \"a\", \"POOL\"]").contains("never run"));
        assert!(
            parse_err("[binds.F6]\nseq = [\"LOOP inf\", \"a\", \"POOL\"]").contains("not a count")
        );
        assert!(parse_err("[binds.F6]\nseq = [\"POOL now\"]").contains("takes nothing"));
        let err = parse_err("[binds.F6]\nseq = [\"LOOP 2\", \"a\"]");
        assert!(err.contains("LOOP without a matching POOL"), "{err}");
        let err = parse_err("[binds.F6]\nseq = [\"a\", \"POOL\"]");
        assert!(err.contains("POOL without a matching LOOP"), "{err}");
    }

    /// The two block kinds must nest inside each other, never across — the runtime
    /// skip and wrap arithmetic relies on it.
    #[test]
    fn crossed_blocks_are_refused() {
        let err = parse_err("[binds.F6]\nseq = [\"RNG 50%\", \"LOOP 2\", \"GNR\", \"POOL\"]");
        assert!(err.contains("the open block is a LOOP"), "{err}");
        let err = parse_err("[binds.F6]\nseq = [\"LOOP 2\", \"RNG 50%\", \"POOL\", \"GNR\"]");
        assert!(err.contains("the open block is an RNG"), "{err}");
    }

    /// A hold that leaks across POOL would stack an extra press every iteration, and
    /// a release of an outer hold would find nothing left after the first — both are
    /// refused. A hold spanning the whole block from outside is fine.
    #[test]
    fn loop_blocks_must_balance_their_presses() {
        let err =
            parse_err("[binds.F6]\nseq = [\"LOOP 2\", \"PRESS ctrl\", \"POOL\", \"RELEASE ctrl\"]");
        assert!(err.contains("before its POOL"), "{err}");
        let err =
            parse_err("[binds.F6]\nseq = [\"PRESS ctrl\", \"LOOP 2\", \"RELEASE ctrl\", \"POOL\"]");
        assert!(err.contains("pressed before it"), "{err}");
        parse_ok(
            "[binds.F6]\nseq = [\"PRESS ctrl\", \"LOOP 2\", \"a\", \"POOL\", \"RELEASE ctrl\"]",
        );
        parse_ok(
            "[binds.F6]\nseq = [\"LOOP 2\", \"PRESS ctrl\", \"a\", \"RELEASE ctrl\", \"POOL\"]",
        );
    }

    /// The pre-rename keyword is gone on purpose: `hold` is no keyword, so it parses as
    /// a key name and fails as one.
    #[test]
    fn the_old_hold_keyword_is_not_a_keyword() {
        let err = parse_err("[binds.F6]\nseq = [\"hold ctrl\"]");
        assert!(err.contains("hold"), "{err}");
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
            Action::Mirror(vec![Holdable::Button(Button::Left)])
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
