//! One file per `--check`, each owning its detection, its report rendering and
//! its fixture tests.
//!
//! The registry that dispatches to them is `Check` in the parent module, which
//! is where the names live because clap renders `--check`'s help from
//! `Check::NAMES`. Adding a check is a file here plus its arms there.

pub(in crate::audit) mod chain_integrity;
pub(in crate::audit) mod claim_lease_divergence;
pub(in crate::audit) mod commit_correlation;
pub(in crate::audit) mod double_win;
pub(in crate::audit) mod merge_divergence;
pub(in crate::audit) mod retry_storm;
pub(in crate::audit) mod silent_contention;
pub(in crate::audit) mod stale_holds;
pub(in crate::audit) mod topology;
