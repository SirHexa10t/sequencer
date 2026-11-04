//! A run of this program as the supervisor of a device grab.
//!
//! The device backend grabs keyboards only while a supervisor watches over them
//! ([`sequencer_input::linux::supervise`]), and the supervisor is the program itself run
//! again — whichever program embeds this crate. So every binary that runs profiles routes
//! [`SUPERVISOR_MARKER`] to [`supervise_if_asked`] before anything else: `sequencer`'s own
//! `main` does, and an embedder's must too. One that doesn't gets no supervisor, and then
//! no grab — never a grab nobody watches.

use std::ffi::{OsStr, OsString};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;

use sequencer_input::linux::supervise;

/// The first argument that makes a run the supervisor: a flag, so no command line a person
/// types for a command can be mistaken for it.
pub const SUPERVISOR_MARKER: &str = "--sequencer-supervisor";

/// Serves as the supervisor when `args` — the process's own, from `std::env::args_os()` —
/// make this run one: `Some(exit code)` once the watch is over, `None` for any other run,
/// which goes on as usual.
#[must_use]
pub fn supervise_if_asked(args: impl IntoIterator<Item = OsString>) -> Option<u8> {
    if args.into_iter().nth(1).as_deref() != Some(OsStr::new(SUPERVISOR_MARKER)) {
        return None;
    }
    // Its stdin is the manager's socket. Anything else means a person typed the marker.
    let link = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(UnixStream::from)
        .ok()
        .filter(|link| link.peer_addr().is_ok());
    let Some(link) = link else {
        eprintln!(
            "sequencer: {SUPERVISOR_MARKER} is for sequencer's own use, by a profile manager"
        );
        return Some(2);
    };
    let _ = supervise::serve(&link);
    Some(0)
}

/// Starts this run's supervisor: this very program again, routed by the marker — so an
/// embedder that doesn't route it gets an error here, and the grab never starts.
#[cfg(feature = "xtest")]
pub(crate) fn start() -> std::io::Result<supervise::Supervisor> {
    let mut program = std::process::Command::new(std::env::current_exe()?);
    program.arg(SUPERVISOR_MARKER);
    supervise::Supervisor::spawn(program)
}
