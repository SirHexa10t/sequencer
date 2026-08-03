//! The state machine: bindings in, actions out.
//!
//! # Two rules that shape everything here
//!
//! **[`Engine::tick`] is the only thing that emits.** [`Engine::handle_input`] updates
//! state and answers with any runner-level command the event triggered, but produces no
//! output of its own. A cancellation schedules a drain and the very next tick performs it
//! — at no added latency, since the runner always ticks after draining its input queue.
//! One emitting function is dramatically easier to reason about than two.
//!
//! **Whether a binding *should* be running is derived, not commanded.**
//! [`Engine::handle_input`] only records that a trigger is held or a latch flipped; tick
//! reconciles that against the live activations. Modelling it as a level rather than as
//! start/stop edges is what removes the whole family of races where a rapid press-release
//! pair arrives between two ticks.

use alloc::vec::Vec;

use crate::emit::EmitBuf;
use crate::input::{EventKind, EventOrigin, InputEvent};
use crate::ir::{
    Binding, CatchUp, Control, Edge, Epilogue, OtherKey, RepeatMode, Trigger, TriggerInput,
    TriggerMode,
};
use crate::rng::Rng;
use crate::seq::{SeqExec, StepOutcome, wait_duration};
use crate::time::{Period, Timestamp};
use crate::validate::CompiledProfile;

/// Ceiling on steps run per activation per tick.
///
/// This budget exists only because loops do. Without `LoopStart`/`LoopEnd` a program is
/// straight-line and always terminates; with them, a `Forever` loop whose body contains
/// no `Wait` would spin inside [`Engine::tick`] and the runner would never get to see the
/// quit key.
///
/// It keeps the *runner* responsive. It cannot make such a program terminate — nothing
/// can — so [`TickStats::budget_exhausted`] is worth surfacing rather than swallowing.
pub const MAX_STEPS_PER_TICK: u32 = 1024;

/// How a running activation is progressing towards stopping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ActState {
    /// Running normally.
    Running,
    /// Cancelled; finish the current iteration, then retire.
    Draining,
    /// Cancelled; stop at once, releasing whatever is held.
    Aborting,
}

/// Where the next iteration of a repeat starts.
#[derive(Clone, Copy, Debug)]
struct RepeatState {
    /// Iterations begun so far.
    iters: u32,
    /// Absolute time the next iteration should start.
    ///
    /// Advanced by `+= period`, never by `= now + period`. That one distinction is the
    /// entire anti-drift design: relative scheduling accumulates every iteration's
    /// overshoot, so a nominal 20/s becomes 19.6/s within a minute.
    next_start: Timestamp,
    /// Consecutive catch-up iterations already granted under [`CatchUp::Burst`].
    burst_run: u8,
}

impl RepeatState {
    /// Moves the schedule on by one period, deciding what to do about missed slots.
    fn advance_paced(
        &mut self,
        now: Timestamp,
        period: Period,
        catch_up: CatchUp,
        stats: &mut TickStats,
    ) {
        let step = period.nanos();
        self.next_start = self.next_start.saturating_add_nanos(step);
        if self.next_start >= now {
            self.burst_run = 0;
            return;
        }

        // Behind schedule. Exact integer slot arithmetic, no floating point, no drift.
        let behind = now.nanos() - self.next_start.nanos();
        let missed = behind / step + 1;

        match catch_up {
            CatchUp::Burst { max } if u64::from(self.burst_run) < u64::from(max) => {
                self.burst_run += 1;
            }
            _ => {
                self.next_start = self
                    .next_start
                    .saturating_add_nanos(missed.saturating_mul(step));
                stats.slots_skipped = stats
                    .slots_skipped
                    .saturating_add(u32::try_from(missed).unwrap_or(u32::MAX));
                self.burst_run = 0;
            }
        }
    }
}

/// One live run of a binding's program.
///
/// Which binding it belongs to is its index in [`Engine::acts`], which runs parallel to
/// the profile's bindings.
#[derive(Clone, Debug)]
struct Activation {
    started_at: Timestamp,
    state: ActState,
    repeat: RepeatState,
    exec: SeqExec,
}

/// Per-binding state that survives between activations.
#[derive(Clone, Copy, Debug, Default)]
struct BindingState {
    /// When this binding became armed, if it is.
    ///
    /// Held-down for [`TriggerMode::WhileHeld`], latched-on for [`TriggerMode::Toggle`].
    armed_at: Option<Timestamp>,
    /// A [`TriggerMode::Once`] edge waiting to become an activation.
    pending_once: Option<Timestamp>,
}

/// What a tick did, and when to come back.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TickStats {
    /// Steps executed.
    pub steps_run: u32,
    /// Iterations begun.
    pub iterations_started: u32,
    /// Iterations that ran to the end of their program.
    pub iterations_completed: u32,
    /// Scheduled iterations dropped because the process fell behind.
    ///
    /// The quantified replacement for the Python prototype's negative "waiting time left"
    /// print: if this is climbing, the machine cannot keep up with the requested rate.
    pub slots_skipped: u32,
    /// Whether [`MAX_STEPS_PER_TICK`] cut a tick short.
    pub budget_exhausted: bool,
    /// Activations still live at the end of the tick.
    pub active: u32,
}

/// The result of [`Engine::tick`].
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TickOutcome {
    /// When the engine next has work to do.
    ///
    /// `None` means idle: park until an input event arrives. A value in the past, or
    /// equal to `now`, means tick again immediately.
    pub next_deadline: Option<Timestamp>,
    /// What this tick did.
    pub stats: TickStats,
}

/// Runs a [`CompiledProfile`].
#[derive(Clone, Debug)]
pub struct Engine {
    profile: CompiledProfile,
    /// Physical inputs currently held, for edge detection and `on_other_key`.
    down: Vec<TriggerInput>,
    /// Live activations, indexed in parallel with the profile's bindings.
    acts: Vec<Option<Activation>>,
    /// Per-binding arming state, indexed in parallel with the profile's bindings.
    states: Vec<BindingState>,
    /// Guards against a clock that goes backwards.
    last_now: Timestamp,
    rng: Rng,
}

impl Engine {
    /// Builds an engine for `profile`, seeding the jitter generator with `seed`.
    ///
    /// The same seed replays the same jitter, so a bug report that includes it is
    /// reproducible.
    #[must_use]
    pub fn new(profile: CompiledProfile, seed: u64) -> Self {
        let count = profile.bindings.len();
        Self {
            profile,
            down: Vec::new(),
            acts: (0..count).map(|_| None).collect(),
            states: alloc::vec![BindingState::default(); count],
            last_now: Timestamp::ZERO,
            rng: Rng::new(seed),
        }
    }

    /// The profile being run.
    #[must_use]
    pub const fn profile(&self) -> &CompiledProfile {
        &self.profile
    }

    /// Whether the engine is holding nothing down.
    ///
    /// The invariant every cancellation path has to restore, and the one the property
    /// tests chase: a stuck modifier key is the failure mode that has embarrassed every
    /// tool in this space.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.acts.iter().flatten().all(|act| !act.exec.is_holding())
    }

    /// Whether any binding is currently running.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.acts.iter().any(Option::is_some)
    }

    /// Feeds one input event in.
    ///
    /// Updates arming state and reports any runner-level command the event triggered.
    /// Emits nothing — [`Engine::tick`] does that.
    pub fn handle_input(&mut self, event: InputEvent) -> Option<Control> {
        let (input, edge) = classify(event.kind)?;

        // Auto-repeat from the OS resends KeyDown while a key is held. Deriving edges
        // from a transition rather than from the raw event is what stops that from
        // flapping a toggle latch dozens of times a second.
        let was_down = self.down.contains(&input);
        let is_real_edge = match edge {
            Edge::Press => !was_down,
            Edge::Release => was_down,
        };
        match edge {
            Edge::Press => {
                if !was_down {
                    self.down.push(input);
                }
            }
            Edge::Release => self.down.retain(|held| *held != input),
        }

        if !is_real_edge {
            return None;
        }

        let control = self.control_for(input, edge);
        self.apply_trigger(input, edge, event);
        if edge == Edge::Press {
            self.cancel_on_other_key(input);
        }
        control
    }

    fn control_for(&self, input: TriggerInput, edge: Edge) -> Option<Control> {
        // Controls fire on press: waiting for the release of a quit key would leave the
        // engine running through whatever the user did in between.
        if edge != Edge::Press {
            return None;
        }
        self.profile
            .controls
            .iter()
            .find(|(trigger, _)| trigger_matches(*trigger, input))
            .map(|(_, control)| *control)
    }

    /// Arms, disarms or latches every binding this input drives.
    fn apply_trigger(&mut self, input: TriggerInput, edge: Edge, event: InputEvent) {
        for index in 0..self.profile.bindings.len() {
            let binding = &self.profile.bindings[index];
            if !trigger_matches(binding.trigger, input) || !level_admits(binding, event.origin) {
                continue;
            }

            match binding.mode {
                TriggerMode::Once { on } if on == edge => {
                    self.states[index].pending_once = Some(event.at);
                }
                TriggerMode::WhileHeld { .. } => match edge {
                    // Only arm if not already armed, so OS auto-repeat cannot shift the
                    // cadence phase out from under a run in progress.
                    Edge::Press => {
                        if self.states[index].armed_at.is_none() {
                            self.states[index].armed_at = Some(event.at);
                        }
                    }
                    Edge::Release => {
                        if binding.cancel.on_trigger_release {
                            self.states[index].armed_at = None;
                        }
                    }
                },
                TriggerMode::Toggle { on, .. } if on == edge => {
                    self.states[index].armed_at = match self.states[index].armed_at {
                        Some(_) => None,
                        None => Some(event.at),
                    };
                }
                TriggerMode::Once { .. } | TriggerMode::Toggle { .. } => {}
            }
        }
    }

    /// Applies `on_other_key` policies for a press of some unrelated input.
    fn cancel_on_other_key(&mut self, pressed: TriggerInput) {
        for index in 0..self.acts.len() {
            let binding = &self.profile.bindings[index];
            if trigger_matches(binding.trigger, pressed) {
                continue;
            }
            let cancels = match &binding.cancel.on_other_key {
                OtherKey::Ignore => false,
                OtherKey::AnyKey => true,
                OtherKey::Only(list) => list.contains(&pressed),
            };
            if !cancels {
                continue;
            }
            // Disarm as well as cancel. Without this, tick would reconcile a still-held
            // trigger straight back into a fresh activation and the cancellation would
            // look like it did nothing.
            self.states[index].armed_at = None;
            let epilogue = binding.cancel.epilogue;
            if let Some(act) = self.acts[index].as_mut() {
                request_cancel(act, epilogue);
            }
        }
    }

    /// Advances every live activation up to `now`, appending output to `out`.
    ///
    /// Never blocks, never sleeps, and always returns. Infallible: a validated profile
    /// makes every failure mode unreachable, and the one that is not — a bug in this
    /// module — fails safe by releasing what is held and retiring the activation, because
    /// a dropped macro is recoverable and a stuck key is not.
    pub fn tick(&mut self, now: Timestamp, out: &mut EmitBuf) -> TickOutcome {
        // A clock that jumps backwards freezes time rather than misbehaving. One line,
        // and it removes an entire class of bug from everything downstream.
        let now = now.max(self.last_now);
        self.last_now = now;

        let mut stats = TickStats::default();
        let mut next: Option<Timestamp> = None;

        for index in 0..self.acts.len() {
            self.reconcile(index, now);
            self.run_activation(index, now, &mut stats, out);
            if let Some(deadline) = self.next_wake(index) {
                next = Some(next.map_or(deadline, |soonest: Timestamp| soonest.min(deadline)));
            }
        }

        // A tick cut short by the step budget must be re-entered at once, not slept off.
        if stats.budget_exhausted {
            next = Some(now);
        }
        stats.active = u32::try_from(self.acts.iter().flatten().count()).unwrap_or(u32::MAX);
        TickOutcome {
            next_deadline: next,
            stats,
        }
    }

    /// Brings binding `index`'s activation into line with whether it should be running.
    fn reconcile(&mut self, index: usize, now: Timestamp) {
        let binding = &self.profile.bindings[index];
        let epilogue = binding.cancel.epilogue;
        let timeout = binding.cancel.on_timeout;
        let state = self.states[index];

        let start_at = state.pending_once.or(match binding.mode {
            TriggerMode::Once { .. } => None,
            TriggerMode::WhileHeld { .. } | TriggerMode::Toggle { .. } => state.armed_at,
        });

        match self.acts[index].as_mut() {
            None => {
                if let Some(at) = start_at {
                    self.states[index].pending_once = None;
                    self.acts[index] = Some(Activation {
                        started_at: at,
                        state: ActState::Running,
                        repeat: RepeatState {
                            iters: 0,
                            next_start: at,
                            burst_run: 0,
                        },
                        exec: SeqExec::idle(binding.program),
                    });
                }
            }
            Some(act) => {
                if start_at.is_none() && act.state == ActState::Running {
                    request_cancel(act, epilogue);
                } else if let Some(limit) = timeout
                    && act.state == ActState::Running
                    && now.saturating_sub(act.started_at) >= limit
                {
                    request_cancel(act, epilogue);
                    self.states[index].armed_at = None;
                }
            }
        }
    }

    /// Runs binding `index`'s activation as far as `now` allows.
    fn run_activation(
        &mut self,
        index: usize,
        now: Timestamp,
        stats: &mut TickStats,
        out: &mut EmitBuf,
    ) {
        let Some(mut act) = self.acts[index].take() else {
            return;
        };
        let binding = &self.profile.bindings[index];
        let steps = &self.profile.program_for(binding).steps;
        let level = binding.input_level;
        let max_iters = repeat_of(binding).and_then(|spec| spec.max_iters);

        if act.state == ActState::Aborting {
            act.exec.abort();
            act.exec.drain_held(now, out);
            self.states[index].armed_at = None;
            return; // `act` drops here, ledger already empty.
        }

        let mut budget = MAX_STEPS_PER_TICK;
        let retire = loop {
            if act.exec.is_done() {
                if act.state == ActState::Draining {
                    break true;
                }
                if max_iters.is_some_and(|limit| act.repeat.iters >= limit) {
                    break true;
                }
                if !can_start_iteration(&act, binding, now) {
                    break false;
                }
                begin_iteration(&mut act, binding, now, stats);
            }

            if !act.exec.is_runnable_at(now) {
                break false;
            }
            if budget == 0 {
                stats.budget_exhausted = true;
                break false;
            }
            budget -= 1;

            match act.exec.step(now, steps, level, &mut self.rng, out) {
                StepOutcome::Advanced => stats.steps_run = stats.steps_run.saturating_add(1),
                StepOutcome::Yielded => break false,
                StepOutcome::Finished => {
                    // The iteration boundary drain. It is what makes a program
                    // self-contained: anything it pressed is released before the next
                    // pass, so a repeat cannot stack presses on an already-held key.
                    act.exec.drain_held(now, out);
                    stats.iterations_completed = stats.iterations_completed.saturating_add(1);
                    if let Some(spec) = repeat_of(binding)
                        && let RepeatMode::AfterGap { gap } = spec.mode
                    {
                        act.repeat.next_start =
                            now.saturating_add(wait_duration(gap, &mut self.rng));
                    }
                }
            }
        };

        if retire {
            act.exec.drain_held(now, out);
            self.states[index].armed_at = None;
        } else {
            self.acts[index] = Some(act);
        }
    }

    /// When binding `index` next needs attention.
    fn next_wake(&self, index: usize) -> Option<Timestamp> {
        let act = self.acts[index].as_ref()?;
        let binding = &self.profile.bindings[index];

        // Only while it could still fire. Once the activation is draining or aborting the
        // timeout has already been acted on, and continuing to report it would hand the
        // runner a deadline in the past on every tick -- a busy-loop that never ends.
        let by_timeout = binding
            .cancel
            .on_timeout
            .filter(|_| act.state == ActState::Running)
            .map(|limit| act.started_at.saturating_add(limit));

        let by_work = if act.exec.is_done() {
            match repeat_of(binding).map(|spec| spec.mode) {
                None | Some(RepeatMode::Once) => None,
                Some(RepeatMode::AfterGap { .. } | RepeatMode::Paced { .. }) => {
                    Some(act.repeat.next_start)
                }
            }
        } else {
            // Not done and not waiting means it is runnable right now and only the step
            // budget stopped it; either way the runner should come straight back.
            Some(act.exec.resume_at().unwrap_or(self.last_now))
        };

        match (by_work, by_timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }

    /// Cancels everything and releases everything held. Idempotent.
    pub fn shutdown(&mut self, now: Timestamp, out: &mut EmitBuf) {
        let now = now.max(self.last_now);
        self.last_now = now;
        for slot in &mut self.acts {
            if let Some(act) = slot.as_mut() {
                act.exec.abort();
                act.exec.drain_held(now, out);
            }
            *slot = None;
        }
        for state in &mut self.states {
            *state = BindingState::default();
        }
        self.down.clear();
    }
}

/// Whether the schedule says another iteration may begin.
///
/// Free functions rather than methods: they touch only the activation and its binding, so
/// taking `&mut Engine` would borrow the whole engine and collide with the borrow of the
/// program's steps that the caller is already holding.
fn can_start_iteration(act: &Activation, binding: &Binding, now: Timestamp) -> bool {
    match repeat_of(binding).map(|spec| spec.mode) {
        None | Some(RepeatMode::Once) => act.repeat.iters == 0,
        Some(RepeatMode::AfterGap { .. } | RepeatMode::Paced { .. }) => {
            act.repeat.next_start <= now
        }
    }
}

/// Rewinds the executor and moves the repeat schedule on.
fn begin_iteration(act: &mut Activation, binding: &Binding, now: Timestamp, stats: &mut TickStats) {
    act.exec.restart();
    act.repeat.iters = act.repeat.iters.saturating_add(1);
    stats.iterations_started = stats.iterations_started.saturating_add(1);
    if let Some(RepeatMode::Paced { period, catch_up }) = repeat_of(binding).map(|s| s.mode) {
        act.repeat.advance_paced(now, period, catch_up, stats);
    }
}

/// Marks an activation as cancelled, honouring the binding's epilogue.
fn request_cancel(act: &mut Activation, epilogue: Epilogue) {
    if act.state != ActState::Running {
        return;
    }
    match epilogue {
        Epilogue::Abort => act.state = ActState::Aborting,
        Epilogue::FinishIteration => act.state = ActState::Draining,
        Epilogue::RunTail { from } => {
            act.exec.run_tail_from(from);
            act.state = ActState::Draining;
        }
    }
}

/// The repeat settings of a binding, if its mode has any.
const fn repeat_of(binding: &Binding) -> Option<&crate::ir::RepeatSpec> {
    match &binding.mode {
        TriggerMode::Once { .. } => None,
        TriggerMode::WhileHeld { repeat } | TriggerMode::Toggle { repeat, .. } => Some(repeat),
    }
}

/// Splits an event into the input it concerns and which way it went.
const fn classify(kind: EventKind) -> Option<(TriggerInput, Edge)> {
    match kind {
        EventKind::KeyDown(key) => Some((TriggerInput::Key(key), Edge::Press)),
        EventKind::KeyUp(key) => Some((TriggerInput::Key(key), Edge::Release)),
        EventKind::ButtonDown(button) => Some((TriggerInput::Button(button), Edge::Press)),
        EventKind::ButtonUp(button) => Some((TriggerInput::Button(button), Edge::Release)),
        EventKind::Scroll { .. } | EventKind::Motion { .. } => None,
    }
}

fn trigger_matches(trigger: Trigger, input: TriggerInput) -> bool {
    trigger.input == input
}

/// Whether an event is allowed to fire a binding, given where it came from.
///
/// Physical input always is. Synthetic input has to out-rank the binding, which with
/// everything defaulting to zero means the engine cannot trigger itself — the safe
/// behaviour falls out of the comparison instead of needing a special case.
const fn level_admits(binding: &Binding, origin: EventOrigin) -> bool {
    match origin {
        EventOrigin::Physical => true,
        EventOrigin::Synthetic { level } => level > binding.input_level,
    }
}
