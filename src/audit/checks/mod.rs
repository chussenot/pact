//! One file per `--check`, each owning its detection, its report rendering and
//! its fixture tests.
//!
//! Adding a check is a file here plus its arms in the parent: a `Check` variant,
//! its name, its `run_check` arm and its three calls in `render_check`. The
//! registry stays there rather than becoming a table of trait objects here,
//! because `Check::NAMES` is what clap renders `--check`'s help from and the
//! exhaustive `match` on the enum is what makes forgetting an arm a compile
//! error.

pub(in crate::audit) mod chain_integrity;
pub(in crate::audit) mod claim_lease_divergence;
pub(in crate::audit) mod commit_correlation;
pub(in crate::audit) mod double_win;
pub(in crate::audit) mod merge_divergence;
pub(in crate::audit) mod retry_storm;
pub(in crate::audit) mod silent_contention;
pub(in crate::audit) mod stale_holds;
pub(in crate::audit) mod topology;
