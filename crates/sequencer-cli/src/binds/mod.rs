//! Key bindings: the format a set of them is written in, and the executor that runs one.
//!
//! Spine, not product: two products consume this. `profile` applies binds *files* — a
//! directory of them, a lock, a manager loop — and `clicker` builds one in memory from
//! its flags and runs it as a plain process. Neither imports the other; both import this.
//! The split is the one the crate's dependency rule demands, and it is also the honest
//! description of what the clicker is: a binds profile with no file behind it.

pub(crate) mod format;
pub(crate) mod run;
