//! One module per subcommand: the handler `cli::run` dispatches to, together
//! with the rendering and the helpers only that subcommand uses.
//!
//! Re-exported from here rather than reached for by path, so that `run`'s
//! dispatch table stays the flat list of verbs it is.

mod completion;
mod context;

pub(super) use completion::run_completion;
pub(super) use context::run_context_set;
