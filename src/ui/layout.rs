//! Layout rules shared by the dashboard sections: outer padding, gaps, the
//! capped and centered content column, the charts' side-by-side split,
//! greedy row packing for things that wrap (stat chips, footer items), and
//! the right-then-left row helper.

use eframe::egui::{Align, Layout, Ui};

/// Padding between the window edge and the content, in points.
pub const OUTER_PAD: i8 = 12;
/// Vertical gap between sections and horizontal gap between packed items.
pub const GAP: f32 = 8.0;
/// Content never grows wider than this; extra width becomes side margins.
/// Wide enough for the two charts side by side.
pub const MAX_CONTENT_WIDTH: f32 = 1000.0;
/// From this content width up, the timeline and the 7-day chart sit side
/// by side; narrower, they stack (timeline first).
pub const CHARTS_SIDE_BY_SIDE_MIN: f32 = 640.0;
/// The timeline's share of the row when the charts sit side by side.
const TIMELINE_SHARE: f32 = 0.62;

/// Width of the content column for `available` width.
pub fn content_width(available: f32) -> f32 {
    available.clamp(0.0, MAX_CONTENT_WIDTH)
}

/// Left margin that centers the content column in `available` width.
pub fn centered_inset(available: f32) -> f32 {
    ((available - content_width(available)) / 2.0).max(0.0)
}

/// How a chart card is sized: the chart's height, and the least height of
/// the card's content, so cards side by side end level (0 when stacked).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardSize {
    pub chart: f32,
    pub min_content: f32,
}

/// The content height the next frame gives both side-by-side cards: the
/// taller of their natural heights this frame.
pub fn level_height(natural: &[f32]) -> f32 {
    natural.iter().copied().fold(0.0, f32::max)
}

/// The widths of the timeline and the 7-day chart side by side in a row
/// `width` wide with a [`GAP`] between them, or `None` when they stack.
pub fn chart_widths(width: f32) -> Option<(f32, f32)> {
    if width < CHARTS_SIDE_BY_SIDE_MIN {
        return None;
    }
    let timeline = ((width - GAP) * TIMELINE_SHARE).round();
    Some((timeline, width - GAP - timeline))
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
        assert_eq!(content_width(1400.0), MAX_CONTENT_WIDTH);
        assert_eq!(centered_inset(1400.0), 200.0);
        assert_eq!(content_width(-5.0), 0.0);
    }

    #[test]
    fn cards_level_to_the_taller() {
        assert_eq!(level_height(&[230.0, 262.0]), 262.0);
        assert_eq!(level_height(&[262.0, 230.0]), 262.0);
        assert_eq!(level_height(&[]), 0.0);
    }

    #[test]
    fn charts_stack_when_narrow() {
        assert_eq!(chart_widths(396.0), None);
        assert_eq!(chart_widths(CHARTS_SIDE_BY_SIDE_MIN - 0.5), None);
        let (a, b) = chart_widths(CHARTS_SIDE_BY_SIDE_MIN).unwrap();
        assert_eq!(a + GAP + b, CHARTS_SIDE_BY_SIDE_MIN);
        // The timeline takes about 62%.
        let (a, b) = chart_widths(1000.0).unwrap();
        assert_eq!((a, b), (615.0, 377.0));
        // The column cap leaves room for both.
        assert!(chart_widths(MAX_CONTENT_WIDTH).is_some());
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
