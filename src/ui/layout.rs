//! Layout rules shared by the dashboard sections: outer padding, gaps, the
//! capped and centered content column, greedy row packing for things that
//! wrap (stat chips, footer items), and the right-then-left row helper.

use eframe::egui::{Align, Layout, Ui};

/// Padding between the window edge and the content, in points.
pub const OUTER_PAD: i8 = 12;
/// Vertical gap between sections and horizontal gap between packed items.
pub const GAP: f32 = 8.0;
/// Content never grows wider than this; extra width becomes side margins.
pub const MAX_CONTENT_WIDTH: f32 = 720.0;

/// Width of the content column for `available` width.
pub fn content_width(available: f32) -> f32 {
    available.clamp(0.0, MAX_CONTENT_WIDTH)
}

/// Left margin that centers the content column in `available` width.
pub fn centered_inset(available: f32) -> f32 {
    ((available - content_width(available)) / 2.0).max(0.0)
}

/// Greedy row packing: how many of the items (in order, with these widths)
/// go on each row of `available` width, `gap` apart. Every row holds at
/// least one item, so an item wider than the row sits alone instead of
/// vanishing. No items: no rows.
pub fn pack_rows(widths: &[f32], available: f32, gap: f32) -> Vec<usize> {
    let mut rows = Vec::new();
    let mut count = 0;
    let mut used = 0.0;
    for &w in widths {
        let needed = if count == 0 { w } else { used + gap + w };
        if count > 0 && needed > available {
            rows.push(count);
            count = 1;
            used = w;
        } else {
            count += 1;
            used = needed;
        }
    }
    if count > 0 {
        rows.push(count);
    }
    rows
}

/// One row: `right` laid out first from the right edge, then `left` from the
/// left edge in the remaining width, so wrapping content on the left never
/// runs under the right-hand widgets.
pub fn right_then_left(
    ui: &mut Ui,
    align: Align,
    right: impl FnOnce(&mut Ui),
    left: impl FnOnce(&mut Ui),
) {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(align), |ui| {
            right(ui);
            ui.with_layout(Layout::left_to_right(align), left);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_width_caps_and_centers() {
        assert_eq!(content_width(400.0), 400.0);
        assert_eq!(centered_inset(400.0), 0.0);
        assert_eq!(content_width(MAX_CONTENT_WIDTH), MAX_CONTENT_WIDTH);
        assert_eq!(content_width(1000.0), MAX_CONTENT_WIDTH);
        assert_eq!(centered_inset(1000.0), 140.0);
        assert_eq!(content_width(-5.0), 0.0);
    }

    #[test]
    fn pack_rows_fills_greedily() {
        // 3 x 90 with 8 gaps = 286.
        assert_eq!(pack_rows(&[90.0; 3], 286.0, 8.0), vec![3]);
        assert_eq!(pack_rows(&[90.0; 3], 285.9, 8.0), vec![2, 1]);
        assert_eq!(pack_rows(&[90.0; 3], 187.9, 8.0), vec![1, 1, 1]);
        assert_eq!(pack_rows(&[50.0, 200.0, 30.0], 240.0, 8.0), vec![1, 2]);
    }

    #[test]
    fn pack_rows_never_drops_items() {
        assert_eq!(pack_rows(&[500.0, 20.0], 100.0, 8.0), vec![1, 1]);
        assert_eq!(pack_rows(&[], 100.0, 8.0), Vec::<usize>::new());
        assert_eq!(pack_rows(&[10.0], 0.0, 8.0), vec![1]);
    }
}
