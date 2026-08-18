//! One module per subcommand: the handler `cli::run` dispatches to, together
//! with the rendering and the helpers only that subcommand uses.
//!
//! Re-exported from here rather than reached for by path, so that `run`'s
//! dispatch table stays the flat list of verbs it is.

mod agents;
mod audit;
mod completion;
mod context;
mod doctor;
mod init;
mod lease;
mod log;
mod merge;
mod msg;
mod plan;
mod watch;
mod whoami;

pub(super) use agents::run_agents;
pub(super) use audit::{run_audit, AuditArgs};
pub(super) use completion::run_completion;
pub(super) use context::run_context_set;
pub(super) use doctor::run_doctor;
pub(super) use init::run_init;
pub(super) use lease::run_lease;
pub(super) use log::run_log;
pub(super) use merge::run_merge;
pub(super) use msg::run_msg;
pub(super) use plan::run_plan_lint;
pub(super) use watch::run_watch;
pub(super) use whoami::run_whoami;

/// One stored message, the shape it has on disk. Shared because `lease
/// acquire`'s pending-message check and every `msg` renderer are tested against
/// the same record, and a second copy of it is a second thing to keep in step.
#[cfg(test)]
pub(super) fn message(id: &str, from: &str, body: &str, read: bool) -> crate::msg::Message {
    crate::msg::Message {
        id: id.to_string(),
        thread: id.to_string(),
        from: from.to_string(),
        to: "cli-wire".to_string(),
        subject: Some("a subject".to_string()),
        body: body.to_string(),
        created_at: "2026-07-31T09:00:00Z".to_string(),
        read,
        read_by: if read {
            vec!["cli-wire".to_string()]
        } else {
            Vec::new()
        },
        notice: false,
    }
}
