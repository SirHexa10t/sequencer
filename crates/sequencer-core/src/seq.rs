//! Running one pass over a program's steps.
//!
//! The executor is a program counter, a loop stack and a release ledger. It never sleeps:
//! a [`Step::Wait`] records a deadline and yields, so cancelling an in-flight sequence is
//! just dropping this struct after draining the ledger.

use alloc::vec::Vec;

use crate::emit::{Emit, EmitAction, EmitBuf, Holdable};
use crate::ir::{Jitter, LoopCount, ProgramId, Step, StepIx, WaitSpec};
use crate::rng::Rng;
use crate::time::{Duration, Timestamp, clamp_nanos};

/// What happened when the executor ran one step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StepOutcome {
    /// The step completed; call again.
    Advanced,
    /// The step set a deadline; do not call again until it passes.
    Yielded,
    /// The program ran off the end.
    Finished,
}

/// One entry in the release ledger.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Held {
    what: Holdable,
    level: u8,
}

/// One active loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LoopFrame {
    /// Index of the `LoopStart` that opened this frame.
    start: StepIx,
    /// Iterations still to run *after* the current one. `None` means forever.
    remaining: Option<u32>,
}

/// A single pass over one program.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct SeqExec {
    program: ProgramId,
    pc: StepIx,
    /// Set by [`Step::Wait`]. The executor must not be stepped again until this passes.
    resume_at: Option<Timestamp>,
    loops: Vec<LoopFrame>,
    /// Everything this pass has pressed and not yet released, oldest first.
    held: Vec<Held>,
    done: bool,
}

impl SeqExec {
    /// A pass that has not started.
    ///
    /// Starts in the "done" state so the engine's loop has one uniform entry point: it
    /// always asks whether the next iteration may begin, rather than special-casing the
    /// first one.
    pub(crate) const fn idle(program: ProgramId) -> Self {
        Self {
            program,
            pc: 0,
            resume_at: None,
            loops: Vec::new(),
            held: Vec::new(),
            done: true,
        }
    }

    /// Rewinds for the next iteration of a repeat, keeping the allocations.
    ///
    /// The caller drains the ledger at every iteration boundary, so a program is
    /// self-contained: whatever it presses is released before the next pass. Without
    /// that, a program that presses without releasing would grow the ledger once per
    /// iteration and press an already-held key forever.
    pub(crate) fn restart(&mut self) {
        debug_assert!(
            self.held.is_empty(),
            "restarted while still holding {:?}; the caller must drain first",
            self.held
        );
        self.pc = 0;
        self.resume_at = None;
        self.loops.clear();
        self.done = false;
    }

    pub(crate) const fn is_done(&self) -> bool {
        self.done
    }

    pub(crate) fn is_holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// When this pass may next be stepped, if it is waiting.
    pub(crate) const fn resume_at(&self) -> Option<Timestamp> {
        self.resume_at
    }

    /// Whether `now` has reached the pending deadline.
    pub(crate) fn is_runnable_at(&self, now: Timestamp) -> bool {
        !self.done && self.resume_at.is_none_or(|deadline| deadline <= now)
    }

    /// Abandons the remaining steps without touching the ledger.
    ///
    /// The caller drains separately, which keeps "stop running" and "let go of things"
    /// as two decisions rather than one entangled one.
    pub(crate) fn abort(&mut self) {
        self.done = true;
        self.resume_at = None;
    }

    /// Jumps to a cleanup section and keeps running from there.
    pub(crate) fn run_tail_from(&mut self, from: StepIx) {
        self.pc = from;
        self.resume_at = None;
        self.loops.clear();
        self.done = false;
    }

    /// Runs one step.
    ///
    /// `steps` must be the program named by [`SeqExec::program`], and must have passed
    /// [`crate::CompiledProfile::validate`] — that is what lets every index here be used
    /// without a bounds check producing an error path.
    pub(crate) fn step(
        &mut self,
        now: Timestamp,
        steps: &[Step],
        level: u8,
        rng: &mut Rng,
        out: &mut EmitBuf,
    ) -> StepOutcome {
        if self.done {
            return StepOutcome::Finished;
        }
        let Some(step) = steps.get(self.pc as usize) else {
            self.done = true;
            return StepOutcome::Finished;
        };

        match *step {
            Step::Emit(action) => {
                out.push(Emit {
                    at: now,
                    action,
                    level,
                });
                self.record(action, level);
                self.pc += 1;
                StepOutcome::Advanced
            }
            Step::Wait(spec) => {
                self.pc += 1;
                self.resume_at = Some(now.saturating_add(wait_duration(spec, rng)));
                StepOutcome::Yielded
            }
            Step::LoopStart { count, end } => {
                match count {
                    LoopCount::Times(0) => self.pc = end,
                    LoopCount::Times(n) => {
                        // `n - 1` because falling through starts the first iteration.
                        self.loops.push(LoopFrame {
                            start: self.pc,
                            remaining: Some(n - 1),
                        });
                        self.pc += 1;
                    }
                    LoopCount::Forever => {
                        self.loops.push(LoopFrame {
                            start: self.pc,
                            remaining: None,
                        });
                        self.pc += 1;
                    }
                }
                StepOutcome::Advanced
            }
            Step::LoopEnd { start } => {
                match self.loops.last_mut() {
                    Some(frame) if frame.start == start => match frame.remaining {
                        None => self.pc = start + 1,
                        Some(0) => {
                            self.loops.pop();
                            self.pc += 1;
                        }
                        Some(left) => {
                            frame.remaining = Some(left - 1);
                            self.pc = start + 1;
                        }
                    },
                    // Validation rules this out. If it happens anyway it is a bug in the
                    // engine, and stopping this pass is the failure that cannot leave a
                    // key stuck -- the caller drains the ledger on the way out.
                    _ => {
                        debug_assert!(false, "LoopEnd at {} with no matching frame", self.pc);
                        self.done = true;
                        return StepOutcome::Finished;
                    }
                }
                StepOutcome::Advanced
            }
        }
    }

    /// Notes what an emitted action did to the set of things held down.
    fn record(&mut self, action: EmitAction, level: u8) {
        if let Some(what) = action.holds() {
            self.held.push(Held { what, level });
        } else if let Some(what) = action.releases()
            && let Some(index) = self.held.iter().rposition(|held| held.what == what)
        {
            self.held.remove(index);
        }
    }

    /// Releases everything this pass still holds, most recent first.
    ///
    /// The single drain. Called on normal completion, on every cancellation path, and on
    /// shutdown. Reverse order matters: a sequence that pressed Ctrl, Shift, A releases
    /// A, Shift, Ctrl, which is the order a real user's fingers would leave in and the
    /// only order that never produces a spurious accelerator on the way out.
    pub(crate) fn drain_held(&mut self, at: Timestamp, out: &mut EmitBuf) {
        while let Some(held) = self.held.pop() {
            out.push(Emit {
                at,
                action: held.what.up(),
                level: held.level,
            });
        }
    }
}

// There is deliberately no `Drop` impl asserting the ledger is empty. It is tempting --
// it would catch any path that forgets to drain -- but a panicking destructor that fires
// while the thread is already unwinding aborts the process, so one unrelated failing
// assertion anywhere in a test would replace its message with "panic in a destructor
// during cleanup". The invariant is covered better by `Engine::is_quiescent`, by
// `RecordingSink::is_balanced`, and by the property test that drives both across hundreds
// of random cancellation scenarios.

/// How long a [`Step::Wait`] actually waits.
///
/// Also used by the engine for the gap between iterations of an
/// [`crate::ir::RepeatMode::AfterGap`] repeat, which is the same idea at a different
/// scale.
pub(crate) fn wait_duration(spec: WaitSpec, rng: &mut Rng) -> Duration {
    match spec.jitter {
        Jitter::None => spec.base,
        Jitter::Uniform { plus_minus } if plus_minus.is_zero() => spec.base,
        Jitter::Uniform { plus_minus } => {
            let base = clamp_nanos(spec.base);
            let spread = clamp_nanos(plus_minus);
            let low = base.saturating_sub(spread);
            let high = base.saturating_add(spread);
            Duration::from_nanos(low.saturating_add(rng.at_most(high - low)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Button, Key};
    use alloc::vec;

    const LEVEL: u8 = 0;

    /// Runs to completion or until `limit` steps, returning the emitted actions.
    fn run(steps: &[Step], limit: usize) -> (SeqExec, EmitBuf) {
        let mut exec = SeqExec::idle(ProgramId(0));
        exec.restart();
        let mut out = EmitBuf::new();
        let mut rng = Rng::new(1);
        let now = Timestamp::ZERO;
        for _ in 0..limit {
            match exec.step(now, steps, LEVEL, &mut rng, &mut out) {
                StepOutcome::Advanced => {}
                StepOutcome::Yielded | StepOutcome::Finished => break,
            }
        }
        (exec, out)
    }

    fn actions(out: &EmitBuf) -> Vec<EmitAction> {
        out.as_slice().iter().map(|e| e.action).collect()
    }

    #[test]
    fn a_click_emits_press_then_release_and_holds_nothing() {
        let steps = vec![
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
            Step::Emit(EmitAction::ButtonUp(Button::Left)),
        ];
        let (exec, out) = run(&steps, 10);
        assert_eq!(
            actions(&out),
            vec![
                EmitAction::ButtonDown(Button::Left),
                EmitAction::ButtonUp(Button::Left)
            ]
        );
        assert!(!exec.is_holding());
    }

    #[test]
    fn the_ledger_drains_in_reverse_order() {
        let steps = vec![
            Step::Emit(EmitAction::KeyDown(Key::LeftCtrl)),
            Step::Emit(EmitAction::KeyDown(Key::LeftShift)),
            Step::Emit(EmitAction::KeyDown(Key::A)),
        ];
        let (mut exec, mut out) = run(&steps, 10);
        assert!(exec.is_holding());
        out.clear();
        exec.drain_held(Timestamp::from_millis(5), &mut out);
        assert_eq!(
            actions(&out),
            vec![
                EmitAction::KeyUp(Key::A),
                EmitAction::KeyUp(Key::LeftShift),
                EmitAction::KeyUp(Key::LeftCtrl),
            ]
        );
        assert!(!exec.is_holding());
    }

    #[test]
    fn an_explicit_release_removes_the_ledger_entry() {
        let steps = vec![
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Emit(EmitAction::KeyUp(Key::A)),
        ];
        let (mut exec, mut out) = run(&steps, 10);
        out.clear();
        exec.drain_held(Timestamp::ZERO, &mut out);
        assert!(out.is_empty(), "drain should have nothing left to release");
    }

    #[test]
    fn repeated_presses_of_one_key_release_once_each() {
        let steps = vec![
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Emit(EmitAction::KeyDown(Key::A)),
            Step::Emit(EmitAction::KeyUp(Key::A)),
        ];
        let (mut exec, mut out) = run(&steps, 10);
        out.clear();
        exec.drain_held(Timestamp::ZERO, &mut out);
        assert_eq!(actions(&out), vec![EmitAction::KeyUp(Key::A)]);
    }

    #[test]
    fn a_wait_yields_and_sets_a_deadline() {
        let steps = vec![
            Step::Wait(WaitSpec::fixed(Duration::from_millis(10))),
            Step::Emit(EmitAction::ButtonDown(Button::Left)),
        ];
        let mut exec = SeqExec::idle(ProgramId(0));
        exec.restart();
        let mut out = EmitBuf::new();
        let mut rng = Rng::new(1);

        let outcome = exec.step(Timestamp::ZERO, &steps, LEVEL, &mut rng, &mut out);
        assert_eq!(outcome, StepOutcome::Yielded);
        assert_eq!(exec.resume_at(), Some(Timestamp::from_millis(10)));
        assert!(out.is_empty());
        assert!(!exec.is_runnable_at(Timestamp::from_millis(9)));
        assert!(exec.is_runnable_at(Timestamp::from_millis(10)));
    }

    /// Wheel ticks, so the loop tests exercise counting without also holding anything.
    const TICK: EmitAction = EmitAction::Scroll { dx: 0, dy: 1 };
    const AFTER: EmitAction = EmitAction::Scroll { dx: 1, dy: 0 };

    #[test]
    fn a_counted_loop_runs_its_body_exactly_that_many_times() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(3),
                end: 3,
            },
            Step::Emit(TICK),
            Step::LoopEnd { start: 0 },
            Step::Emit(AFTER),
        ];
        let (_, out) = run(&steps, 100);
        assert_eq!(actions(&out), vec![TICK, TICK, TICK, AFTER]);
    }

    #[test]
    fn a_zero_count_loop_skips_its_body() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(0),
                end: 3,
            },
            Step::Emit(TICK),
            Step::LoopEnd { start: 0 },
            Step::Emit(AFTER),
        ];
        let (_, out) = run(&steps, 100);
        assert_eq!(actions(&out), vec![AFTER]);
    }

    #[test]
    fn nested_loops_multiply() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(2),
                end: 5,
            },
            Step::LoopStart {
                count: LoopCount::Times(3),
                end: 4,
            },
            Step::Emit(EmitAction::Scroll { dx: 0, dy: 1 }),
            Step::LoopEnd { start: 1 },
            Step::LoopEnd { start: 0 },
        ];
        let (_, out) = run(&steps, 200);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn a_forever_loop_keeps_going_until_the_caller_stops_it() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Forever,
                end: 3,
            },
            Step::Emit(EmitAction::Scroll { dx: 0, dy: 1 }),
            Step::LoopEnd { start: 0 },
        ];
        // The opening LoopStart, then each (Emit, LoopEnd) pair costs two steps and
        // produces one action: 1 + 2*20 steps is 20 actions.
        let (mut exec, out) = run(&steps, 41);
        assert_eq!(out.len(), 20);
        assert!(!exec.is_done());
        exec.abort();
        assert!(exec.is_done());
    }

    #[test]
    fn restart_rewinds_the_program_counter_and_the_loop_stack() {
        let steps = vec![
            Step::LoopStart {
                count: LoopCount::Times(2),
                end: 3,
            },
            Step::Emit(EmitAction::Scroll { dx: 0, dy: 1 }),
            Step::LoopEnd { start: 0 },
        ];
        let (mut exec, mut out) = run(&steps, 100);
        assert_eq!(out.len(), 2);
        assert!(exec.is_done());

        out.clear();
        exec.restart();
        let mut rng = Rng::new(1);
        for _ in 0..100 {
            if exec.step(Timestamp::ZERO, &steps, LEVEL, &mut rng, &mut out)
                == StepOutcome::Finished
            {
                break;
            }
        }
        assert_eq!(
            out.len(),
            2,
            "a restarted pass runs the loop again from scratch"
        );
    }

    #[test]
    fn jitter_stays_within_the_requested_band() {
        let mut rng = Rng::new(99);
        let spec = WaitSpec {
            base: Duration::from_millis(50),
            jitter: Jitter::Uniform {
                plus_minus: Duration::from_millis(5),
            },
        };
        for _ in 0..10_000 {
            let d = wait_duration(spec, &mut rng);
            assert!(
                d >= Duration::from_millis(45) && d <= Duration::from_millis(55),
                "{d:?} outside 45..=55ms"
            );
        }
    }

    #[test]
    fn jitter_wider_than_the_base_clamps_at_zero_and_never_underflows() {
        let mut rng = Rng::new(3);
        let spec = WaitSpec {
            base: Duration::from_millis(1),
            jitter: Jitter::Uniform {
                plus_minus: Duration::from_millis(10),
            },
        };
        for _ in 0..10_000 {
            let d = wait_duration(spec, &mut rng);
            assert!(
                d <= Duration::from_millis(11),
                "{d:?} exceeded base + spread"
            );
        }
    }

    #[test]
    fn zero_jitter_is_exactly_the_base() {
        let mut rng = Rng::new(3);
        let spec = WaitSpec {
            base: Duration::from_millis(7),
            jitter: Jitter::Uniform {
                plus_minus: Duration::ZERO,
            },
        };
        assert_eq!(wait_duration(spec, &mut rng), Duration::from_millis(7));
    }
}
