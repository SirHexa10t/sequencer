//! The action IR: what a binding does, as data.
//!
//! Two shape decisions drive everything else here.
//!
//! **Steps are a flat, index-addressed list, not a tree.** The program counter is one
//! integer and control flow is index arithmetic, so the entire executor state is a small
//! struct that can be dropped to cancel. A nested AST would need a live Rust call stack,
//! which is precisely what makes mid-sequence cancellation hard — AutoHotkey grew an
//! interrupting pseudo-thread model to work around exactly this, and it is still the part
//! of AutoHotkey nobody can explain.
//!
//! **Repeat behaviour lives on the binding, not in the sequence.** Logitech Gaming
//! Software shipped three repeat modes to millions of users without ever putting them in
//! the macro body, and keeping them out means a sequence stays a plain list of things to
//! do.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::emit::EmitAction;
use crate::input::{Button, Key};
use crate::time::{Duration, Period};

/// Index of a step within a [`Program`].
pub type StepIx = u32;

/// Index of a [`Program`] within a [`Profile`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProgramId(pub u32);

/// Index of a [`Binding`] within a [`Profile`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BindingId(pub u32);

/// One instruction.
///
/// Reserved for later, deliberately absent rather than stubbed: `MoveCursor` with
/// easing, `Text` for unicode typing, `Jump`/`JumpIf` for conditionals, and
/// `Call(ProgramId)` for sub-sequences. `Call` is the expensive one — it reintroduces a
/// call stack with per-frame release ledgers, a defined cross-frame drain order,
/// recursion limits and cycle detection — so it stays out until something needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Step {
    /// Do something to the outside world.
    Emit(EmitAction),
    /// Pause. Does not sleep: the executor records a deadline and yields.
    Wait(WaitSpec),
    /// Begin a loop body.
    LoopStart {
        /// How many times to run the body.
        count: LoopCount,
        /// Index just past the matching [`Step::LoopEnd`], where the loop exits to.
        end: StepIx,
    },
    /// End a loop body.
    LoopEnd {
        /// Index of the matching [`Step::LoopStart`].
        start: StepIx,
    },
}

/// How long to wait, and by how much to vary it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitSpec {
    /// The nominal wait.
    pub base: Duration,
    /// Variation applied to `base`.
    pub jitter: Jitter,
}

impl WaitSpec {
    /// A wait of exactly `base`.
    #[must_use]
    pub const fn fixed(base: Duration) -> Self {
        Self {
            base,
            jitter: Jitter::None,
        }
    }
}

/// Variation applied to a wait.
///
/// First-class rather than a later addition because perfectly uniform timing is both the
/// most detectable property an autoclicker has and the thing "humanlike" modes in
/// comparable tools exist to break up — and because a randomised click *hold* duration is
/// just another jittered wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Jitter {
    /// Wait exactly the base duration.
    #[default]
    None,
    /// Wait a uniformly random duration in `base ± plus_minus`, clamped at zero.
    Uniform {
        /// Maximum deviation in either direction.
        plus_minus: Duration,
    },
}

/// How many times a loop body runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopCount {
    /// A fixed number of iterations. Zero means the body is skipped.
    Times(u32),
    /// Until cancelled.
    Forever,
}

/// The physical input that fires a binding.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TriggerInput {
    /// A keyboard key.
    Key(Key),
    /// A mouse button.
    Button(Button),
}

/// What must be held alongside the trigger input.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum ModMatch {
    /// Fire regardless of which modifiers are held.
    #[default]
    Ignore,
}

/// A trigger: an input, plus a condition on the modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Trigger {
    /// The key or button.
    pub input: TriggerInput,
    /// The modifier condition.
    pub mods: ModMatch,
}

impl Trigger {
    /// A trigger on `key`, ignoring modifiers.
    #[must_use]
    pub const fn key(key: Key) -> Self {
        Self {
            input: TriggerInput::Key(key),
            mods: ModMatch::Ignore,
        }
    }

    /// A trigger on `button`, ignoring modifiers.
    #[must_use]
    pub const fn button(button: Button) -> Self {
        Self {
            input: TriggerInput::Button(button),
            mods: ModMatch::Ignore,
        }
    }
}

/// Which edge of a press a rule reacts to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// The moment the input goes down.
    Press,
    /// The moment the input comes up.
    Release,
}

/// When and how often a binding's program runs.
///
/// These are Logitech Gaming Software's three repeat modes. [`Edge`] is broken out
/// because the Python prototype toggles on *release*, and that distinction is real: it is
/// what stops a held key from flapping the latch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerMode {
    /// Run the program once per matching edge.
    Once {
        /// Which edge fires it.
        on: Edge,
    },
    /// Run repeatedly while the input is held.
    WhileHeld {
        /// How the repetition is paced.
        repeat: RepeatSpec,
    },
    /// Flip a latch on each matching edge; run repeatedly while latched.
    Toggle {
        /// Which edge flips the latch.
        on: Edge,
        /// How the repetition is paced.
        repeat: RepeatSpec,
    },
}

/// How a repetition is paced, and when it gives up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RepeatSpec {
    /// The pacing discipline.
    pub mode: RepeatMode,
    /// Stop after this many iterations. `None` means until cancelled.
    pub max_iters: Option<u32>,
}

impl RepeatSpec {
    /// Repeat forever at a fixed rate, skipping missed slots.
    #[must_use]
    pub const fn paced(period: Period) -> Self {
        Self {
            mode: RepeatMode::Paced {
                period,
                catch_up: CatchUp::Skip,
            },
            max_iters: None,
        }
    }
}

/// The pacing discipline for a repeat.
///
/// The two repeating variants are genuinely different promises, which is why both exist:
/// [`RepeatMode::Paced`] guarantees a *rate* and [`RepeatMode::AfterGap`] guarantees
/// *no overlap*. A rate cannot be honoured if an iteration outlasts its period, and a gap
/// cannot be honoured while also hitting a fixed rate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepeatMode {
    /// Run the program once, then stop.
    Once,
    /// Start the next iteration `gap` after the previous one *finishes*.
    ///
    /// The resulting rate is emergent. This is what a macro repeat wants; Logitech's
    /// default gap was 25 ms.
    AfterGap {
        /// Idle time between iterations.
        gap: WaitSpec,
    },
    /// Start iterations on a fixed cadence, scheduled against absolute deadlines.
    ///
    /// The rate is guaranteed as long as iterations fit inside the period. This is what
    /// `--cps` means.
    Paced {
        /// Time between iteration *starts*.
        period: Period,
        /// What to do about slots missed while descheduled.
        catch_up: CatchUp,
    },
}

/// What to do when the process was descheduled and now owes iterations.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CatchUp {
    /// Drop the missed slots and keep the original phase.
    ///
    /// The default, and it should stay the default: after a 500 ms stall at 20 cps you
    /// owe ten clicks, and delivering ten back to back is worse in every way than
    /// delivering none — it is a burst no application expects and the single most
    /// recognisable signature an autoclicker can produce.
    #[default]
    Skip,
    /// Fire up to `max` iterations back to back, then skip the rest.
    Burst {
        /// Hard cap on consecutive catch-up iterations.
        max: u8,
    },
}

/// Which other inputs cancel a running program.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum OtherKey {
    /// Nothing else cancels it.
    #[default]
    Ignore,
    /// Any other key or button press cancels it.
    AnyKey,
    /// Only these inputs cancel it.
    Only(Vec<TriggerInput>),
}

/// Which steps still run once a cancellation has been requested.
///
/// This never controls whether held inputs get released. The release ledger drains on
/// every one of these paths without exception.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Epilogue {
    /// Stop at the current step.
    Abort,
    /// Finish the current iteration, then stop.
    ///
    /// The default, because it is what makes a half-executed click come out as a proper
    /// press-and-release rather than a press that the ledger has to clean up after.
    #[default]
    FinishIteration,
    /// Jump to a cleanup section and run to the end of the program.
    RunTail {
        /// Index of the first cleanup step.
        from: StepIx,
    },
}

/// When a running program gets cancelled.
///
/// Orthogonal axes rather than a fixed set of named modes, which is what lets one type
/// cover kanata's four separate macro variants.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CancelPolicy {
    /// Cancel when the trigger input is released.
    pub on_trigger_release: bool,
    /// Cancel when some other input is pressed.
    pub on_other_key: OtherKey,
    /// Cancel this long after the program started.
    pub on_timeout: Option<Duration>,
    /// What still runs after a cancellation is requested.
    pub epilogue: Epilogue,
}

impl Default for CancelPolicy {
    fn default() -> Self {
        Self {
            on_trigger_release: true,
            on_other_key: OtherKey::Ignore,
            on_timeout: None,
            epilogue: Epilogue::FinishIteration,
        }
    }
}

/// A trigger bound to a program.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Binding {
    /// This binding's index in the profile.
    pub id: BindingId,
    /// What fires it.
    pub trigger: Trigger,
    /// When and how often it runs.
    pub mode: TriggerMode,
    /// What it runs.
    pub program: ProgramId,
    /// When it stops.
    pub cancel: CancelPolicy,
    /// Synthetic events fire this binding only when their send level exceeds this.
    ///
    /// Zero on both sides means the engine cannot retrigger itself. See
    /// [`crate::input::EventOrigin`].
    pub input_level: u8,
}

/// A named sequence of steps.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Program {
    /// Human-readable name, used in diagnostics.
    pub name: Box<str>,
    /// The steps, addressed by index.
    pub steps: Vec<Step>,
}

/// A runner-level command, as opposed to something done to another application.
///
/// Deliberately not a [`Step`] variant. Quitting has to tear down the capture thread and
/// the display-server connection, it has to work even when the profile itself is broken,
/// and putting it in the step IR would mean any profile a user downloads can kill their
/// process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Control {
    /// Shut down cleanly.
    Quit,
}

/// A complete set of bindings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Profile {
    /// Human-readable name, used in diagnostics.
    pub name: Box<str>,
    /// Programs, indexed by [`ProgramId`].
    pub programs: Vec<Program>,
    /// Bindings, indexed by [`BindingId`].
    pub bindings: Vec<Binding>,
    /// Triggers wired to runner-level commands.
    pub controls: Vec<(Trigger, Control)>,
}
