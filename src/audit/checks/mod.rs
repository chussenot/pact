//! One file per `--check`, each owning its detection, its report rendering and
//! its fixture tests.
//!
//! The registry that dispatches to them is `Check` in the parent module, which
//! is where the names live because clap renders `--check`'s help from
//! `Check::NAMES`. Adding a check is a file here plus its arms there.

pub(in crate::audit) mod claim_lease_divergence;
pub(in crate::audit) mod merge_divergence;
pub(in crate::audit) mod retry_storm;
