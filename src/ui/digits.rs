//! Numbers that don't jitter: every ASCII digit gets a cell as wide as the
//! widest digit in the font, so "46.0%" → "51.0%" keeps its width and its
//! digit positions. Inter's tabular figures (`tnum`) would do this, but egui
//! can't enable OpenType features, so the cells are built from per-section
//! leading space instead. Everything else (`%`, `.`, `:`, `h`, spaces, …)
//! keeps its natural width.
//!
//! [`plan`] is the pure layout math, over a measuring function so tests need
//! no fonts; [`number_job`] turns a plan into an egui [`LayoutJob`].

use std::sync::Arc;

use eframe::egui::{
    text::{ByteIndex, LayoutJob, LayoutSection},
    vec2, Color32, FontId, Galley, Painter, Response, Sense, TextFormat, Ui,
};

/// One piece of a laid-out number: a single digit, or a run of non-digits.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    /// Space before the text, in points (centers a digit in its cell, plus
    /// the previous digit's right-hand padding).
    pub lead: f32,
    /// The text's natural width (what egui advances after drawing it).
    pub advance: f32,
}

/// A number's layout.
#[derive(Debug, Clone, PartialEq)]
pub struct NumberPlan {
    /// Width of one digit cell.
    pub cell: f32,
    pub segments: Vec<Segment>,
    /// Total width, including a trailing digit's right-hand padding.
    pub width: f32,
}

/// Lay out `text`, measuring strings with `measure` (their natural width at
/// the target font). Each digit is centered in a cell of the widest digit's
/// width; consecutive non-digits form one segment, measured as a whole.
pub fn plan(text: &str, mut measure: impl FnMut(&str) -> f32) -> NumberPlan {
    let cell = ('0'..='9')
        .map(|d| measure(d.encode_utf8(&mut [0; 4])))
        .fold(0.0, f32::max);

    let mut segments = Vec::new();
    let mut pending_pad = 0.0; // right half of the previous digit's cell
    let mut rest = text;
    while let Some(first) = rest.chars().next() {
        if first.is_ascii_digit() {
            let (digit, tail) = rest.split_at(1);
            let natural = measure(digit);
            let pad = (cell - natural) / 2.0;
            segments.push(Segment {
                text: digit.to_owned(),
                lead: pending_pad + pad,
                advance: natural,
            });
            pending_pad = pad;
            rest = tail;
        } else {
            let end = rest
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            segments.push(Segment {
                text: run.to_owned(),
                lead: pending_pad,
                advance: measure(run),
            });
            pending_pad = 0.0;
            rest = tail;
        }
    }
    let width = segments.iter().map(|s| s.lead + s.advance).sum::<f32>() + pending_pad;
    NumberPlan {
        cell,
        segments,
        width,
    }
}

/// The plan as one galley's worth of sections, one per segment, so egui
/// never merges (and kerns) across a cell boundary.
pub fn number_job(plan: &NumberPlan, font: FontId, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for seg in &plan.segments {
        let start = job.text.len();
        job.text.push_str(&seg.text);
        job.sections.push(LayoutSection {
            leading_space: seg.lead,
            byte_range: ByteIndex(start)..ByteIndex(job.text.len()),
            format: TextFormat::simple(font.clone(), color),
        });
    }
    job
}

/// Lay out `text` with fixed-width digits; returns the galley and the width
/// to allocate for it (which counts a trailing digit's padding).
pub fn number_galley(
    painter: &Painter,
    text: &str,
    font: FontId,
    color: Color32,
) -> (Arc<Galley>, f32) {
    let plan = plan(text, |s| {
        painter
            .layout_no_wrap(s.to_owned(), font.clone(), color)
            .size()
            .x
    });
    let galley = painter.layout_job(number_job(&plan, font, color));
    (galley, plan.width)
}

/// A non-interactive label with fixed-width digits.
pub fn number_label(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> Response {
    let (galley, width) = number_galley(ui.painter(), text, font, color);
    let (rect, response) = ui.allocate_exact_size(vec2(width, galley.size().y), Sense::hover());
    ui.painter().galley(rect.min, galley, color);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake font: '1' is narrow, '0' and '4' are widest, everything else
    /// 5 per char ('.' 2, '%' 9).
    fn measure(s: &str) -> f32 {
        s.chars()
            .map(|c| match c {
                '1' => 3.0,
                '0' | '4' => 7.0,
                '.' => 2.0,
                '%' => 9.0,
                _ => 5.0,
            })
            .sum()
    }

    fn texts(p: &NumberPlan) -> Vec<&str> {
        p.segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn cell_is_widest_digit() {
        assert_eq!(plan("1", measure).cell, 7.0);
    }

    #[test]
    fn digits_split_and_non_digits_grouped() {
        let p = plan("1h 59m", measure);
        assert_eq!(texts(&p), ["1", "h ", "5", "9", "m"]);
    }

    #[test]
    fn digit_centered_in_cell() {
        let p = plan("1%", measure);
        // '1' is 3 wide in a 7 cell: 2 each side.
        assert_eq!(p.segments[0].lead, 2.0);
        assert_eq!(p.segments[0].advance, 3.0);
        // '%' starts after the cell's right padding.
        assert_eq!(p.segments[1].lead, 2.0);
        assert_eq!(p.segments[1].advance, 9.0);
        assert_eq!(p.width, 7.0 + 9.0);
    }

    #[test]
    fn width_counts_each_cell_once() {
        // Three cells, '.' and '%' natural.
        assert_eq!(plan("46.0%", measure).width, 3.0 * 7.0 + 2.0 + 9.0);
        // Trailing digit keeps its right padding.
        assert_eq!(plan("11", measure).width, 14.0);
    }

    #[test]
    fn width_invariant_across_digit_changes() {
        let widths: Vec<f32> = ["46.0%", "51.0%", "11.1%", "00.0%", "99.9%"]
            .iter()
            .map(|t| plan(t, measure).width)
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert_eq!(plan("1h 11m", measure).width, plan("4h 40m", measure).width);
    }

    #[test]
    fn digit_positions_stable() {
        // Where each glyph starts, accumulating lead + advance.
        let starts = |t: &str| {
            let mut x = 0.0;
            let mut out = Vec::new();
            for s in plan(t, measure).segments {
                x += s.lead;
                out.push(x);
                x += s.advance;
            }
            out
        };
        // Cell boundaries line up: glyph start minus its centering pad.
        let a = starts("46.0%");
        let b = starts("51.0%");
        let pads_a = [0.0, 1.0, 0.0, 0.0, 0.0];
        let pads_b = [1.0, 2.0, 0.0, 0.0, 0.0];
        for i in 0..5 {
            assert_eq!(a[i] - pads_a[i], b[i] - pads_b[i], "segment {i}");
        }
    }

    #[test]
    fn no_digits_is_natural_width() {
        let p = plan("—", measure);
        assert_eq!(texts(&p), ["—"]);
        assert_eq!(p.width, 5.0);
        assert_eq!(plan("", measure).width, 0.0);
    }

    #[test]
    fn job_has_one_section_per_segment() {
        let p = plan("46.0%", measure);
        let job = number_job(&p, FontId::proportional(14.0), Color32::WHITE);
        assert_eq!(job.text, "46.0%");
        assert_eq!(job.sections.len(), p.segments.len());
        let leads: Vec<f32> = job.sections.iter().map(|s| s.leading_space).collect();
        let want: Vec<f32> = p.segments.iter().map(|s| s.lead).collect();
        assert_eq!(leads, want);
    }
}
