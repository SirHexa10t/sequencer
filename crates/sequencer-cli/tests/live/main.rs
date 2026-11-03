//! The live suite: everything CI cannot prove because it needs a real machine.
//!
//! Split by what each part needs — [`validation`] drives the real binary but no
//! hardware (it runs in every `cargo test`); [`x11`] needs a real X session and
//! injects keys at it; [`sudo`] needs a cached sudo ticket for the device backend.
//! The hardware-touching tests are `#[ignore]`d so a plain `cargo test` stays safe
//! on any machine, and they serialize themselves through [`harness::serial`] — two
//! managers, or a manager and an injector, must never fight over the session.
//!
//! Run the whole thing with `./SUDO-TEST.sh`, or directly:
//!
//! ```text
//! cargo test -p sequencer-cli --test live -- --ignored --test-threads=1 --nocapture
//! ```

// Each hardware tier uses its own share of the harness, so a build that compiles either
// tier out leaves part of it idle; the full build uses all of it.
#[cfg_attr(
    not(all(feature = "xtest", feature = "evdev", target_os = "linux")),
    allow(dead_code, reason = "each hardware tier uses its own share")
)]
mod harness;
mod validation;

#[cfg(all(feature = "xtest", target_os = "linux"))]
mod x11;

#[cfg(all(feature = "evdev", target_os = "linux"))]
mod sudo;
