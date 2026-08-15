//! Geometry and selection helpers shared by every view.
//!
//! The discipline here is the one `tab_rects` has always followed: **one
//! function both renders and hit-tests**, so the two cannot drift apart the
//! way an independent approximation can. Rows get the same treatment —
//! [`row_at`] is what a click, a hover and a scroll all ask, so they cannot
//! disagree about which row is under the cursor.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};

pub fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

/// The exact rects a tab bar renders into: one per label, each `" Label "`
/// wide, separated by a 1-column (non-clickable) gap.
///
/// Takes the labels as already rendered — badge included — because a label
/// only rendering knew about would shift every later tab out from under its
/// own hit-box.
pub fn tab_rects(header_area: Rect, labels: &[String]) -> Vec<Rect> {
    if labels.is_empty() {
        return Vec::new();
    }
    let mut constraints = Vec::with_capacity(labels.len() * 2 - 1);
    for (i, label) in labels.iter().enumerate() {
        constraints.push(Constraint::Length(tab_width(label)));
        if i + 1 < labels.len() {
            constraints.push(Constraint::Length(1)); // gap between tabs
        }
    }
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(header_area)
        .iter()
        .step_by(2) // skip the gap chunks, keep only the tab-label chunks
        .copied()
        .collect()
}

/// The rendered width of `" Label "`.
pub fn tab_width(label: &str) -> u16 {
    label.chars().count() as u16 + 2
}

/// Which tab a click/hover at `(x, y)` lands on, using the same rects
/// [`tab_rects`] hands to rendering.
pub fn tab_at(header_area: Rect, labels: &[String], x: u16, y: u16) -> Option<usize> {
    tab_rects(header_area, labels)
        .into_iter()
        .position(|rect| rect_contains(rect, x, y))
}

/// Which row (an index into the underlying `Vec`) a point at `y` inside
/// `content_area` lands on, given how many header rows the widget draws before
/// its data (1 for a table's column header, 0 for a plain list) and how far it
/// is currently scrolled.
pub fn row_at(content_area: Rect, y: u16, header_rows: u16, offset: usize) -> Option<usize> {
    let first_data_row = content_area.y + header_rows;
    if y < first_data_row {
        return None;
    }
    Some(offset + (y - first_data_row) as usize)
}

/// Where the cursor lands after a list has been re-read — **by identity, then
/// by index**.
///
/// Every list in this dashboard clamped the selected *index* into the new
/// `Vec`. So when another agent released a lease during the 1 Hz refresh, every
/// row below it moved up one and the operator's cursor landed on a different
/// lease, silently, with a release key one press away. Re-find what they had
/// selected; fall back to the clamped index only when it is genuinely gone.
pub fn reselect(keys: &[&str], previous: Option<&str>, index: Option<usize>) -> Option<usize> {
    if keys.is_empty() {
        return None;
    }
    if let Some(previous) = previous {
        if let Some(found) = keys.iter().position(|key| *key == previous) {
            return Some(found);
        }
    }
    // Nothing selected yet means the top; a selection whose row is gone falls
    // back to where it was, clamped into the list that exists now.
    Some(index.unwrap_or(0).min(keys.len() - 1))
}

/// Move a selection by `delta`, wrapping at both ends. `None` for an empty
/// list, which is the only case where there is nothing to select.
pub fn step(len: usize, current: Option<usize>, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = current.unwrap_or(0) as isize;
    Some((current + delta).rem_euclid(len as isize) as usize)
}

/// Whether `index` is hovered but not the current selection — selection's own
/// reversed style is already a strong enough indicator on its own.
pub fn is_hovered_not_selected(
    hovered: Option<usize>,
    selected: Option<usize>,
    index: usize,
) -> bool {
    hovered == Some(index) && selected != Some(index)
}

pub fn hover_style() -> Style {
    Style::default().bg(Color::Rgb(50, 50, 65))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_at_accounts_for_header_rows_and_scroll_offset() {
        let content = Rect::new(0, 3, 80, 8);
        // a table: 1 header row, no scroll yet
        assert_eq!(row_at(content, 3, 1, 0), None); // the column header itself
        assert_eq!(row_at(content, 4, 1, 0), Some(0));
        assert_eq!(row_at(content, 5, 1, 0), Some(1));
        // scrolled down by 2: the same click lands on a later row
        assert_eq!(row_at(content, 4, 1, 2), Some(2));
        // a plain list: no header row
        assert_eq!(row_at(content, 3, 0, 0), Some(0));
    }

    #[test]
    fn tab_at_agrees_with_tab_rects_exact_geometry() {
        let header = Rect::new(0, 0, 90, 1);
        let labels: Vec<String> = ["Fleet", "Activity", "Messages (3)", "Health"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rects = tab_rects(header, &labels);
        assert_eq!(rects.len(), labels.len());

        for (i, rect) in rects.iter().enumerate() {
            assert_eq!(tab_at(header, &labels, rect.x, rect.y), Some(i));
            assert_eq!(
                tab_at(header, &labels, rect.x + rect.width - 1, rect.y),
                Some(i)
            );
            assert_eq!(rect.width, tab_width(&labels[i]));
        }

        // The 1-column gap between tabs belongs to neither.
        let gap_x = rects[0].x + rects[0].width;
        assert!(gap_x < rects[1].x, "expected a gap between tab rects");
        assert_eq!(tab_at(header, &labels, gap_x, 0), None);

        // Empty space past the last tab matches nothing, rather than falling
        // back to "closest tab" the way an equal-zone division would.
        let last = rects.last().unwrap();
        assert_eq!(tab_at(header, &labels, last.x + last.width + 10, 0), None);
    }

    #[test]
    fn a_badge_widens_its_own_tab_and_shifts_every_later_one_with_it() {
        let header = Rect::new(0, 0, 90, 1);
        let plain: Vec<String> = ["Fleet", "Activity", "Messages", "Health"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let badged: Vec<String> = ["Fleet", "Activity", "Messages (3)", "Health"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let plain_rects = tab_rects(header, &plain);
        let badged_rects = tab_rects(header, &badged);
        assert_eq!(
            badged_rects[1], plain_rects[1],
            "tabs before it do not move"
        );
        // " (3)" widens the badged label itself...
        assert_eq!(badged_rects[2].width, plain_rects[2].width + 4);
        // ...and the tab AFTER it is the one a naive fix breaks: rendering
        // pushes it right while hit-testing keeps the old rect.
        assert_eq!(badged_rects[3].x, plain_rects[3].x + 4);
        assert_eq!(tab_at(header, &badged, badged_rects[3].x, 0), Some(3));
        assert_eq!(tab_at(header, &badged, plain_rects[3].x, 0), Some(2));
    }

    /// The defect this whole helper exists for: another agent releases a lease
    /// during the 1 Hz refresh, every row below it moves up one, and the
    /// operator's cursor must not move with the rows.
    #[test]
    fn a_release_elsewhere_does_not_move_the_operators_selection() {
        let before = ["a.rs", "b.rs", "c.rs"];
        let selected = 2; // the operator is on c.rs
        assert_eq!(before[selected], "c.rs");

        // b.rs is released by its holder somewhere else.
        let after = ["a.rs", "c.rs"];
        let index = reselect(&after, Some("c.rs"), Some(selected));
        assert_eq!(index, Some(1));
        assert_eq!(after[index.unwrap()], "c.rs", "still the same lease");

        // Index-clamping alone would have landed on a.rs — a different lease,
        // one keypress from a release.
        assert_eq!(reselect(&after, None, Some(selected)), Some(1));
    }

    #[test]
    fn reselect_falls_back_to_the_clamped_index_only_when_the_row_is_gone() {
        let after = ["a.rs", "b.rs"];
        // the selected row itself was released: hold position, clamped
        assert_eq!(reselect(&after, Some("gone.rs"), Some(1)), Some(1));
        assert_eq!(reselect(&after, Some("gone.rs"), Some(9)), Some(1));
        // nothing selected yet: the top
        assert_eq!(reselect(&after, None, None), Some(0));
        // an empty list has nothing to select
        assert_eq!(reselect(&[], Some("a.rs"), Some(0)), None);
    }

    #[test]
    fn step_wraps_both_ways_and_refuses_an_empty_list() {
        assert_eq!(step(3, Some(2), 1), Some(0));
        assert_eq!(step(3, Some(0), -1), Some(2));
        assert_eq!(step(3, None, 1), Some(1));
        assert_eq!(step(0, Some(0), 1), None);
    }
}
