//! The sequencer engine: bindings, action sequences, and the state machine that runs them.
//!
//! This crate does no I/O. It never reads a clock, never sleeps, never touches a device
//! and never spawns a thread. The runner hands it a timestamp and a list of input events;
//! it hands back a list of actions to perform and a deadline for when to come back.
//!
//! That is not architectural decoration — it is what makes the interesting behaviour
//! testable. Timing bugs in an autoclicker are the whole ballgame, and a test suite that
//! has to sleep in real time to exercise them is a test suite nobody runs. Here,
//! "iteration 10,000 starts at exactly 500 seconds" is an exact assertion that completes
//! in microseconds.
//!
//! `#![no_std]` is the mechanical guarantee. Reaching for `Instant::now()` or
//! `thread::sleep()` from this crate is a compile error rather than a code-review catch.
//!
//! # Shape
//!
//! ```text
//!            input events                       actions to perform
//!                  |                                    ^
//!                  v                                    |
//!   [runner] -> Engine::handle_input          Engine::tick -> EmitBuf -> [runner] -> OS
//!                  |                                    ^
//!                  +--- state: activations, latches ----+
//! ```
//!
//! [`Engine::tick`] is the only thing that emits. [`Engine::handle_input`] only updates
//! state, answering with any runner-level command the event triggered.
//!
//! # Example
//!
//! Hold a key to click at a fixed rate, which is the whole of v1:
//!
//! ```
//! use sequencer_core::{
//!     emit::{EmitAction, EmitBuf},
//!     input::{Button, EventKind, InputEvent, Key},
//!     ir::*,
//!     time::{Period, Timestamp},
//!     Engine, CompiledProfile,
//! };
//!
//! let profile = Profile {
//!     name: "hold-to-click".into(),
//!     programs: vec![Program {
//!         name: "click".into(),
//!         steps: vec![
//!             Step::Emit(EmitAction::ButtonDown(Button::Left)),
//!             Step::Emit(EmitAction::ButtonUp(Button::Left)),
//!         ],
//!     }],
//!     bindings: vec![Binding {
//!         id: BindingId(0),
//!         trigger: Trigger::key(Key::F9),
//!         mode: TriggerMode::WhileHeld {
//!             repeat: RepeatSpec::paced(Period::from_cps(20.0)?),
//!         },
//!         program: ProgramId(0),
//!         cancel: CancelPolicy::default(),
//!         input_level: 0,
//!     }],
//!     controls: vec![(Trigger::key(Key::F8), Control::Quit)],
//! };
//!
//! let mut engine = Engine::new(CompiledProfile::validate(profile)?, 0);
//! let mut out = EmitBuf::new();
//!
//! engine.handle_input(InputEvent::physical(
//!     Timestamp::ZERO,
//!     EventKind::KeyDown(Key::F9),
//! ));
//!
//! // One second of virtual time at 20 clicks per second: iterations at 0, 50, .. 950 ms,
//! // so 20 press/release pairs.
//! for ms in 0..1000 {
//!     engine.tick(Timestamp::from_millis(ms), &mut out);
//! }
//! assert_eq!(out.len(), 40);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![no_std]
// Tests unwrap freely: an `unwrap()` in a test reports a failure rather than
// hiding one. Library code keeps the workspace-level `warn`.
#![cfg_attr(test, allow(clippy::unwrap_used))]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod clicker;
pub mod emit;
pub mod engine;
pub mod input;
pub mod ir;
mod rng;
mod seq;
pub mod time;
pub mod validate;

pub mod testutil;

pub use crate::clicker::{ActivationMode, ClickAction, ClickConfig};
pub use crate::emit::{Emit, EmitAction, EmitBuf, Holdable, InputSink, SinkError};
pub use crate::engine::{Engine, TickOutcome, TickStats};
pub use crate::input::{Button, EventKind, EventOrigin, InputEvent, Key};
pub use crate::ir::{Binding, Control, Profile, Program, Step, Trigger};
pub use crate::time::{Clock, Duration, Period, Timestamp};
pub use crate::validate::{CompiledProfile, ConfigError};
