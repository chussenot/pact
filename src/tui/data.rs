//! The read model: each store under `.pact/` parsed at most once per refresh
//! tick, and the projections every view derives from it.
//!
//! Owned by pact-pyt.11. `mod.rs` calls [`Store::refresh`] once per tick,
//! before any view renders, and views read projections off `App::data` — so a
//! screen is a filter over this, never a second parse of the same file.
//!
//! Two rules this module inherits from the TUI and cannot break: it must not
//! print (a stderr write smears the alternate screen, and `agents::list()`
//! warns on partial failure), and it must not mutate (`lease::peek`, never
//! `lease::list`).

use std::path::Path;

/// Parsed stores plus their derived projections, cached by (mtime, len).
#[derive(Default)]
pub struct Store;

impl Store {
    /// Re-read whatever changed on disk. Called once per refresh tick from the
    /// event loop, never from a view.
    pub fn refresh(&mut self, _repo_root: &Path) {}
}
