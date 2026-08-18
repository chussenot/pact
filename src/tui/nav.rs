//! The view stack: what the dashboard is looking at, and how it got there.
//!
//! There used to be a 3-value `Tab` plus `App::thread: Option<Vec<Message>>` —
//! a hand-rolled one-level stack for the single drill-in that existed. Adding a
//! second drill-in that way means a second ad-hoc `Option` field, and a third
//! meaning for `Esc` (it already meant "cancel a release" on Leases and "close
//! the thread" on Messages). One stack instead, with two rules that hold on
//! every screen:
//!
//! - **Enter drills in.** It pushes. It never mutates — see `on_enter` in each
//!   view module, which takes `&App` precisely so that is a compile error
//!   rather than a code review.
//! - **Esc goes back.** It pops. Nothing else, anywhere.
//!
//! The roots are the operator's *questions* — who is doing what (Fleet), what
//! just happened (Activity), what is being said (Messages), is the setup sound
//! (Health) — so switching root replaces the stack rather than growing it. The
//! drill-ins are the *answers* about one entity, and those nest.

/// Where the dashboard is.
///
/// Drill-ins carry an identity (a path, an agent name, a message id) and not
/// an index into some list, so the thing you opened stays the thing you are
/// looking at across a refresh that reordered the list behind it.
#[derive(Clone, PartialEq, Debug)]
pub enum View {
    // Roots. Tab/BackTab cycle, 1..4 jump. They replace the root, never push.
    Fleet,
    Activity,
    Messages,
    Health,

    // Drill-ins. Pushed by Enter, popped by Esc.
    Path(String),
    /// Every agent name in the UI is a link to this (pact-pyt.5).
    Agent(String),
    Thread(String),
}

/// Which module renders and handles a view.
///
/// `View` is by identity and so cannot be `Copy`; this can, which is what lets
/// dispatch read the current view, end the borrow, and then hand the whole
/// `&mut App` to the view module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Fleet,
    Activity,
    Messages,
    Detail,
    Health,
}

impl View {
    /// The four roots, in tab-bar order.
    pub fn roots() -> [View; 4] {
        [View::Fleet, View::Activity, View::Messages, View::Health]
    }

    pub fn is_root(&self) -> bool {
        matches!(
            self,
            View::Fleet | View::Activity | View::Messages | View::Health
        )
    }

    /// Which module owns this view. `Path` and `Agent` share `detail`, and a
    /// `Thread` is rendered by the module that owns messages — a thread reached
    /// from a path detail is the same thread reached from the message list.
    pub fn screen(&self) -> Screen {
        match self {
            View::Fleet => Screen::Fleet,
            View::Activity => Screen::Activity,
            View::Messages | View::Thread(_) => Screen::Messages,
            View::Health => Screen::Health,
            View::Path(_) | View::Agent(_) => Screen::Detail,
        }
    }

    /// This view's own segment of the breadcrumb.
    pub fn label(&self) -> String {
        match self {
            View::Fleet => "Fleet".to_string(),
            View::Activity => "Activity".to_string(),
            View::Messages => "Messages".to_string(),
            View::Health => "Health".to_string(),
            View::Path(path) => path.clone(),
            View::Agent(agent) => agent.clone(),
            View::Thread(id) => id.clone(),
        }
    }
}

/// The stack. Never empty, and `stack[0]` is always a root.
#[derive(Clone, Debug)]
pub struct Nav {
    stack: Vec<View>,
}

impl Default for Nav {
    fn default() -> Self {
        Nav {
            stack: vec![View::Fleet],
        }
    }
}

impl Nav {
    pub fn current(&self) -> &View {
        // The invariant: `pop` refuses to empty the stack and `set_root`
        // rebuilds it, so there is always a bottom.
        self.stack.last().unwrap_or(&View::Fleet)
    }

    pub fn root(&self) -> &View {
        self.stack.first().unwrap_or(&View::Fleet)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Drill in. Re-opening the view you are already on is a no-op rather than
    /// a duplicate segment — Enter on a row that is already open should not
    /// need two Escs to undo.
    pub fn push(&mut self, view: View) {
        if self.current() != &view {
            self.stack.push(view);
        }
    }

    /// Go back one level. `false` at a root, where there is nothing to pop —
    /// the caller uses that to clear transient state instead.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }

    /// Switch the question being asked. Drill-ins belong to the root they were
    /// opened from, so they go with it.
    pub fn set_root(&mut self, root: View) {
        debug_assert!(root.is_root(), "set_root called with a drill-in");
        self.stack = vec![root];
    }

    /// The root `delta` steps away, for Tab/BackTab.
    pub fn cycle_root(&self, delta: isize) -> View {
        let roots = View::roots();
        let len = roots.len() as isize;
        let current = self.root_index() as isize;
        roots[(current + delta).rem_euclid(len) as usize].clone()
    }

    /// Which root the stack sits under, as an index into [`View::roots`] —
    /// also what the tab bar highlights and what hit-testing returns.
    pub fn root_index(&self) -> usize {
        View::roots()
            .iter()
            .position(|r| r == self.root())
            .unwrap_or(0)
    }

    /// The stack rendered for the header, so an operator three levels down can
    /// see where they are and what Esc will take them back to.
    pub fn breadcrumb(&self) -> String {
        self.stack
            .iter()
            .map(View::label)
            .collect::<Vec<_>>()
            .join(" > ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_pushes_and_esc_pops_back_to_the_root() {
        let mut nav = Nav::default();
        assert_eq!(nav.current(), &View::Fleet);

        nav.push(View::Path("src/lease.rs".into()));
        nav.push(View::Agent("docs-story".into()));
        assert_eq!(nav.depth(), 3);
        assert_eq!(nav.current(), &View::Agent("docs-story".into()));

        assert!(nav.pop());
        assert_eq!(nav.current(), &View::Path("src/lease.rs".into()));
        assert!(nav.pop());
        assert_eq!(nav.current(), &View::Fleet);

        // At the root there is nothing to go back to, and the stack must not
        // empty out from under `current()`.
        assert!(!nav.pop());
        assert_eq!(nav.current(), &View::Fleet);
        assert_eq!(nav.depth(), 1);
    }

    #[test]
    fn pushing_the_view_you_are_already_on_is_a_no_op() {
        let mut nav = Nav::default();
        nav.push(View::Path("src/lease.rs".into()));
        nav.push(View::Path("src/lease.rs".into()));
        assert_eq!(nav.depth(), 2, "Enter twice must not need Esc twice");
    }

    #[test]
    fn switching_root_replaces_the_stack_rather_than_growing_it() {
        let mut nav = Nav::default();
        nav.push(View::Path("src/lease.rs".into()));
        nav.set_root(View::Activity);
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.current(), &View::Activity);
        assert_eq!(nav.root_index(), 1);
    }

    #[test]
    fn roots_cycle_both_ways_and_wrap() {
        let mut nav = Nav::default();
        for expected in [View::Activity, View::Messages, View::Health, View::Fleet] {
            let next = nav.cycle_root(1);
            nav.set_root(next);
            assert_eq!(nav.current(), &expected);
        }
        let back = nav.cycle_root(-1);
        assert_eq!(back, View::Health);
    }

    #[test]
    fn a_drill_in_is_dispatched_to_the_module_that_owns_its_entity() {
        assert_eq!(View::Path("x".into()).screen(), Screen::Detail);
        assert_eq!(View::Agent("x".into()).screen(), Screen::Detail);
        // A thread is a thread wherever it was opened from.
        assert_eq!(View::Thread("m1".into()).screen(), Screen::Messages);
        assert_eq!(View::Messages.screen(), Screen::Messages);
    }

    #[test]
    fn the_breadcrumb_names_every_level_by_identity() {
        let mut nav = Nav::default();
        nav.push(View::Path("src/lease.rs".into()));
        nav.push(View::Thread("pact-msg-1".into()));
        assert_eq!(nav.breadcrumb(), "Fleet > src/lease.rs > pact-msg-1");
    }

    #[test]
    fn every_root_is_a_root_and_no_drill_in_is() {
        assert!(View::roots().iter().all(View::is_root));
        assert!(!View::Path("x".into()).is_root());
        assert!(!View::Thread("x".into()).is_root());
    }
}
