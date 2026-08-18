//! One module per subcommand: the handler `cli::run` dispatches to, together
//! with the rendering and the helpers only that subcommand uses.
//!
//! Re-exported from here rather than reached for by path, so that `run`'s
//! dispatch table stays the flat list of verbs it is.

mod audit;
mod completion;
mod context;
mod doctor;
mod init;
mod merge;
mod plan;
mod watch;

pub(super) use audit::{run_audit, AuditArgs};
pub(super) use completion::run_completion;
pub(super) use context::run_context_set;
pub(super) use doctor::run_doctor;
pub(super) use init::run_init;
pub(super) use merge::run_merge;
pub(super) use plan::run_plan_lint;
pub(super) use watch::run_watch;
