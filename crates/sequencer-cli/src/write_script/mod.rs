//! `write-script`: run a scripted sequence of input events. **Not implemented.**
//!
//! A placeholder with a real command behind it, kept for two reasons. It marks the shape the
//! next product takes — a sibling of [`crate::clicker`], reusing the engine, the step IR, the
//! run loop and both input backends unchanged — so the split between "one product" and "the
//! shared machinery" has a second occupant proving it rather than one occupant asserting it.
//! And it means the layout question is already answered when the work starts.
//!
//! The engine can very nearly do this already: [`crate::clicker`] lowers its settings into a
//! [`Profile`](sequencer_core::ir::Profile) of emit-and-wait steps, and a script is the same
//! IR with the steps read from a file instead of generated from a rate. What is missing is a
//! script format, its parser, and the validation to reject a sequence that would leave a key
//! held. [`crate::cmd::simulate`] already replays a scripted list of *input* events for
//! testing; this is the other direction — scripted *output*.

pub mod args;

pub use args::WriteScriptArgs;

use crate::{Deps, Result, exit};

/// Says what it is and stops.
///
/// Exits zero: asked to do nothing, it did nothing, and that is not a failure. A non-zero
/// code here would break any `write-script && next` a user writes while waiting for it.
///
/// # Errors
///
/// Only if writing to the output stream fails.
pub fn write_script(_args: &WriteScriptArgs, deps: &mut Deps<'_>) -> Result<u8> {
    writeln!(
        deps.out,
        "TODO: write-script is not implemented yet — it will run a scripted sequence of \
         input events. For now, `clicker` repeats one action and `simulate` replays a \
         scripted list of input events through the engine."
    )?;
    Ok(exit::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sequencer_core::testutil::VirtualClock;

    /// Says what it is, and succeeds. Exiting non-zero would break a `write-script && next`
    /// somebody writes while waiting for the feature, and saying nothing would look broken.
    #[test]
    fn the_stub_reports_itself_and_succeeds() {
        let clock = VirtualClock::default();
        let mut out: Vec<u8> = Vec::new();
        let mut deps = Deps::new(&mut out, &clock);
        let code = write_script(&WriteScriptArgs::new(), &mut deps).expect("writes to a Vec");
        assert_eq!(code, exit::OK);
        let said = String::from_utf8(out).expect("text");
        assert!(said.contains("not implemented"), "{said}");
        assert!(said.contains("write-script"), "it should name itself: {said}");
    }
}
