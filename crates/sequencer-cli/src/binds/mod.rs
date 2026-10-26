//! Key bindings: the format a set of them is written in, and the executor that runs one.
//!
//! Spine, not product: `profile` applies binds *files* — a directory of them, a lock, a
//! manager loop — and `write_script` is to run the same format's sequences. Products never
//! import each other, so what both need lives here.

pub(crate) mod format;
pub(crate) mod run;
