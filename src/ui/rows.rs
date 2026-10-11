//! Dashboard rows: testable formatters from analytics data to [`Run`]s, and
//! the small egui functions that lay them out.
//!
//! Copy and ordering follow ui.py's `render_dashboard` and its helpers.

use chrono::{DateTime, Utc};

use crate::analytics::{self, BurnLine, Eta, Headroom, BURN_WINDOW_MINUTES};
use crate::model::{ExtraUsage, UsageWindow};
use crate::poller::FetchErrorInfo;
use crate::storage::InstanceUsage;

use eframe::egui::{vec2, Align, Color32, Layout, Rect, RichText, Sense, Ui};

use super::{show_runs, tone_color, Run, Tone};

/// One window's summary row, ready to lay out.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowRow {
    pub label: String,
    /// "{:5.1}%" colored by level, or dim "inactive".
    pub pct: Run,
    /// Filled share of the bar, 0..=1.
    pub fraction: f32,
    /// Bar fill tone.
    pub fill: Tone,
    /// Countdown to the reset, or dim "—".
    pub reset: Run,
}

/// The summary row for one window (ui.py `render_window_row`).
pub fn window_row(w: &UsageWindow, now: DateTime<Utc>) -> WindowRow {
    match w.utilization {
        None => WindowRow {
            label: w.label.clone(),
            pct: Run::new("inactive", Tone::Dim),
            fraction: 0.0,
            fill: Tone::Dim,
            reset: Run::new("—", Tone::Dim),
        },
        Some(u) => {
            let level = Tone::Level(analytics::color_for(Some(u)));
            WindowRow {
                label: w.label.clone(),
                pct: Run::new(format!("{u:5.1}%"), level),
                fraction: (u / 100.0).clamp(0.0, 1.0) as f32,
                fill: level,
                reset: Run::new(analytics::humanize_until(w.resets_at, now), Tone::Normal),
            }
        }
    }
}

/// Which burn line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnTag {
    /// The 5-hour window.
    All,
    Fable,
}

/// The burn line's tag: cyan "ALL:" or coral "FABLE:".
pub fn burn_tag(tag: BurnTag) -> Run {
    match tag {
        BurnTag::All => Run::new("ALL:", Tone::Accent),
        BurnTag::Fable => Run::new("FABLE:", Tone::Fable),
    }
}

/// The burn line after its tag (ui.py `render_burn_line`): "15m burn
/// {rate}, → 100% in {eta} (over last {span}m, {n} samples)", or the
/// "need 2 recent samples" form when `line` is `None`.
pub fn burn_body(line: Option<&BurnLine>) -> Vec<Run> {
    let burn = Run::new(format!("{BURN_WINDOW_MINUTES}m burn "), Tone::Strong);
    let to_cap = Run::new("→ 100% in ", Tone::Strong);
    let Some(line) = line else {
        return vec![
            burn,
            Run::new("—", Tone::Dim),
            Run::new(", ", Tone::Normal),
            to_cap,
            Run::new("— (need 2 recent samples)", Tone::Dim),
        ];
    };
    let eta = match line.eta {
        Eta::Steady => "steady".to_owned(),
        Eta::Minutes { minutes, .. } => analytics::fmt_min(minutes),
    };
    vec![
        burn,
        Run::new(analytics::fmt_rate(line.rate), Tone::Level(line.rate_level)),
        Run::new(", ", Tone::Normal),
        to_cap,
        Run::new(eta, Tone::Level(line.eta.level())),
        Run::new(" ", Tone::Normal),
        Run::new(
            format!(
                "(over last {:.0}m, {} samples)",
                line.span_minutes, line.samples
            ),
            Tone::Dim,
        ),
    ]
}

/// "Extra usage: ${used} / ${limit} {currency}  ({pct}%)", spend and percent
/// colored by level, plus a dim "  (disabled: {reason})" when not enabled.
pub fn extra_runs(e: &ExtraUsage) -> Vec<Run> {
    let level = Tone::Level(analytics::color_for(Some(e.utilization)));
    let mut runs = vec![
        Run::new("Extra usage: ", Tone::Strong),
        Run::new(format!("${:.2}", e.used_credits), level),
        Run::new(
            format!(" / ${:.0} {}  (", e.monthly_limit, e.currency),
            Tone::Normal,
        ),
        Run::new(format!("{:.1}%", e.utilization), level),
        Run::new(")", Tone::Normal),
    ];
    if let Some(reason) = analytics::extra_disabled_reason(e) {
        runs.push(Run::new(format!("  (disabled: {reason})"), Tone::Dim));
    }
    runs
}

/// How many burners the dashboard lists.
const TOP_BURNERS: usize = 5;
/// Burner names are cut to this many characters.
const BURNER_NAME_CHARS: usize = 20;

/// "Top burners (5h):  {name} {k}k  ·  …" for the top five (already sorted
/// by tokens, highest first), or a dim "no local activity yet".
pub fn burners_runs(burners: &[InstanceUsage]) -> Vec<Run> {
    let mut runs = vec![Run::new("Top burners (5h):  ", Tone::Strong)];
    if burners.is_empty() {
        runs.push(Run::new("no local activity yet", Tone::Dim));
        return runs;
    }
    for (i, b) in burners.iter().take(TOP_BURNERS).enumerate() {
        if i > 0 {
            runs.push(Run::new("  ·  ", Tone::Normal));
        }
        let name: String = b.name.chars().take(BURNER_NAME_CHARS).collect();
        runs.push(Run::new(format!("{name} "), Tone::Normal));
        runs.push(Run::new(
            format!("{:.0}k", b.tokens as f64 / 1000.0),
            Tone::Strong,
        ));
    }
    runs
}

/// "Projected use: {projected}  ·  {advice}" with the analytics colors.
pub fn headroom_runs(h: &Headroom) -> Vec<Run> {
    vec![
        Run::new("Projected use: ", Tone::Strong),
        Run::new(h.projected.clone(), Tone::Level(h.level)),
        Run::new("  ·  ", Tone::Normal),
        Run::new(
            h.advice.clone(),
            h.advice_level.map_or(Tone::Normal, Tone::Level),
        ),
    ]
}

/// The header title: "claude.ai usage  ·  {plan}".
pub fn header_runs(plan: &str) -> Vec<Run> {
    vec![
        Run::new("claude.ai usage", Tone::Accent),
        Run::new("  ·  ", Tone::Dim),
        Run::new(plan, Tone::Strong),
    ]
}

/// "last polled {secs}s ago", never negative.
pub fn last_polled(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - fetched_at).num_seconds().max(0);
    format!("last polled {secs}s ago")
}

/// Whether an error should carry the "client SHA may be stale" hint: the
/// poller flagged it, or it mentions a 403 or a permission_error.
pub fn shows_sha_hint(err: &FetchErrorInfo) -> bool {
    err.stale_sha_hint || err.message.contains("permission_error") || err.message.contains("403")
}

// --- Layout -------------------------------------------------------------------

/// Label column: fits the longest window label ("Omelette promo").
const LABEL_WIDTH: f32 = 104.0;
/// Percentage column ("100.0%" in monospace).
const PCT_WIDTH: f32 = 56.0;
/// Reset countdown column ("23h 59m").
const RESET_WIDTH: f32 = 64.0;
/// Burn-line tag column: fits "FABLE:".
const TAG_WIDTH: f32 = 52.0;
/// Progress bar thickness.
const BAR_HEIGHT: f32 = 6.0;

/// A fixed-width, row-height cell with the given layout.
fn cell<R>(ui: &mut Ui, width: f32, layout: Layout, add: impl FnOnce(&mut Ui) -> R) -> R {
    let size = vec2(width, ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(size, layout, |ui| {
        ui.set_min_size(size);
        add(ui)
    })
    .inner
}

fn left() -> Layout {
    Layout::left_to_right(Align::Center)
}

fn right() -> Layout {
    Layout::right_to_left(Align::Center)
}

/// One window row: label, percentage, bar, countdown, aligned as a grid.
pub fn show_window_row(ui: &mut Ui, row: &WindowRow) {
    ui.horizontal(|ui| {
        cell(ui, LABEL_WIDTH, left(), |ui| {
            ui.label(RichText::new(&row.label).color(tone_color(Tone::Strong, ui.visuals())));
        });
        cell(ui, PCT_WIDTH, right(), |ui| {
            let color = tone_color(row.pct.tone, ui.visuals());
            ui.label(RichText::new(&row.pct.text).monospace().color(color));
        });
        let gap = ui.spacing().item_spacing.x;
        let bar_width = (ui.available_width() - RESET_WIDTH - gap).max(0.0);
        paint_bar(
            ui,
            bar_width,
            row.fraction,
            tone_color(row.fill, ui.visuals()),
        );
        cell(ui, RESET_WIDTH, right(), |ui| {
            let color = tone_color(row.reset.tone, ui.visuals());
            ui.label(RichText::new(&row.reset.text).monospace().color(color));
        });
    });
}

/// A rounded track with the filled share painted over it.
fn paint_bar(ui: &mut Ui, width: f32, fraction: f32, fill: Color32) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(width, ui.spacing().interact_size.y), Sense::hover());
    let track = Rect::from_center_size(rect.center(), vec2(rect.width(), BAR_HEIGHT));
    let radius = BAR_HEIGHT / 2.0;
    let painter = ui.painter();
    painter.rect_filled(track, radius, ui.visuals().extreme_bg_color);
    if fraction > 0.0 {
        let mut filled = track;
        filled.set_width((track.width() * fraction).max(BAR_HEIGHT));
        painter.rect_filled(filled, radius, fill);
    }
}

/// A burn line: the tag in a fixed column so "ALL:" and "FABLE:" bodies line
/// up, then the body.
pub fn show_burn_row(ui: &mut Ui, tag: BurnTag, body: &[Run]) {
    ui.horizontal(|ui| {
        cell(ui, TAG_WIDTH, left(), |ui| {
            let tag = burn_tag(tag);
            ui.label(
                RichText::new(tag.text)
                    .strong()
                    .color(tone_color(tag.tone, ui.visuals())),
            );
        });
        show_runs(ui, body);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::Level;
    use crate::ui::plain_text;
    use chrono::Duration;

    const NOW: DateTime<Utc> = DateTime::from_timestamp_nanos(1_791_000_000_000_000_000);

    fn window(label: &str, utilization: Option<f64>, resets_in_min: Option<i64>) -> UsageWindow {
        UsageWindow {
            label: label.to_owned(),
            utilization,
            resets_at: resets_in_min.map(|m| NOW + Duration::minutes(m)),
        }
    }

    fn burner(name: &str, tokens: u64) -> InstanceUsage {
        InstanceUsage {
            name: name.to_owned(),
            source: "claude-code",
            tokens,
            last_active: NOW,
        }
    }

    fn extra(enabled: bool, reason: Option<&str>) -> ExtraUsage {
        ExtraUsage {
            enabled,
            monthly_limit: 35.0,
            used_credits: 23.70,
            utilization: 67.7,
            currency: "USD".to_owned(),
            disabled_reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn window_row_active() {
        let row = window_row(&window("5-hour", Some(46.0), Some(119)), NOW);
        assert_eq!(row.label, "5-hour");
        assert_eq!(row.pct, Run::new(" 46.0%", Tone::Level(Level::Green)));
        assert!((row.fraction - 0.46).abs() < 1e-6);
        assert_eq!(row.fill, Tone::Level(Level::Green));
        assert_eq!(row.reset, Run::new("1h 59m", Tone::Normal));
    }

    #[test]
    fn window_row_levels_and_clamp() {
        let row = window_row(&window("7-day all", Some(104.0), None), NOW);
        assert_eq!(row.pct, Run::new("104.0%", Tone::Level(Level::Red)));
        assert_eq!(row.fraction, 1.0);
        assert_eq!(row.reset, Run::new("—", Tone::Normal));
        let row = window_row(&window("Fable", Some(76.0), Some(60 * 30)), NOW);
        assert_eq!(row.fill, Tone::Level(Level::Orange));
        assert_eq!(row.reset.text, "1d 6h");
    }

    #[test]
    fn window_row_inactive() {
        let row = window_row(&window("Opus 7d", None, Some(10)), NOW);
        assert_eq!(row.pct, Run::new("inactive", Tone::Dim));
        assert_eq!(row.fraction, 0.0);
        assert_eq!(row.fill, Tone::Dim);
        assert_eq!(row.reset, Run::new("—", Tone::Dim));
    }

    #[test]
    fn burn_tags() {
        assert_eq!(burn_tag(BurnTag::All), Run::new("ALL:", Tone::Accent));
        assert_eq!(burn_tag(BurnTag::Fable), Run::new("FABLE:", Tone::Fable));
    }

    #[test]
    fn burn_body_known() {
        let line = BurnLine {
            rate: 0.123,
            rate_level: Level::Yellow,
            eta: Eta::Minutes {
                minutes: 185.4,
                level: Level::Yellow,
            },
            span_minutes: 12.4,
            samples: 9,
        };
        let runs = burn_body(Some(&line));
        assert_eq!(
            plain_text(&runs),
            "15m burn +0.12%/min, → 100% in 3h 05m (over last 12m, 9 samples)"
        );
        assert!(runs.contains(&Run::new("+0.12%/min", Tone::Level(Level::Yellow))));
        assert!(runs.contains(&Run::new("3h 05m", Tone::Level(Level::Yellow))));
        assert!(runs.contains(&Run::new("(over last 12m, 9 samples)", Tone::Dim)));
    }

    #[test]
    fn burn_body_steady() {
        let line = BurnLine {
            rate: 0.0,
            rate_level: Level::Green,
            eta: Eta::Steady,
            span_minutes: 7.0,
            samples: 11,
        };
        let runs = burn_body(Some(&line));
        assert_eq!(
            plain_text(&runs),
            "15m burn +0.00%/min, → 100% in steady (over last 7m, 11 samples)"
        );
        assert!(runs.contains(&Run::new("steady", Tone::Level(Level::Green))));
    }

    #[test]
    fn burn_body_unknown() {
        let runs = burn_body(None);
        assert_eq!(
            plain_text(&runs),
            "15m burn —, → 100% in — (need 2 recent samples)"
        );
        assert!(runs.contains(&Run::new("—", Tone::Dim)));
        assert!(runs.contains(&Run::new("— (need 2 recent samples)", Tone::Dim)));
    }

    #[test]
    fn extra_enabled() {
        let runs = extra_runs(&extra(true, None));
        assert_eq!(plain_text(&runs), "Extra usage: $23.70 / $35 USD  (67.7%)");
        assert!(runs.contains(&Run::new("$23.70", Tone::Level(Level::Yellow))));
        assert!(runs.contains(&Run::new("67.7%", Tone::Level(Level::Yellow))));
    }

    #[test]
    fn extra_disabled_reason() {
        let runs = extra_runs(&extra(false, Some("out_of_credits")));
        assert_eq!(
            plain_text(&runs),
            "Extra usage: $23.70 / $35 USD  (67.7%)  (disabled: out of credits)"
        );
        assert_eq!(
            runs.last(),
            Some(&Run::new("  (disabled: out of credits)", Tone::Dim))
        );
        let runs = extra_runs(&extra(false, None));
        assert!(plain_text(&runs).ends_with("  (disabled: off)"));
    }

    #[test]
    fn burners_empty() {
        let runs = burners_runs(&[]);
        assert_eq!(
            plain_text(&runs),
            "Top burners (5h):  no local activity yet"
        );
        assert_eq!(
            runs.last(),
            Some(&Run::new("no local activity yet", Tone::Dim))
        );
    }

    #[test]
    fn burners_top_five_truncated() {
        let burners: Vec<InstanceUsage> = [
            ("C--Users-someone-very-long-project-name", 123_456),
            ("b", 98_500),
            ("c", 1_400),
            ("d", 999),
            ("é-unicode-name-long-enough-to-cut", 50_000),
            ("f", 10),
        ]
        .into_iter()
        .map(|(n, t)| burner(n, t))
        .collect();
        let runs = burners_runs(&burners);
        assert_eq!(
            plain_text(&runs),
            "Top burners (5h):  C--Users-someone-ver 123k  ·  b 98k  ·  c 1k  ·  d 1k  ·  \
             é-unicode-name-long- 50k"
        );
        assert!(runs.contains(&Run::new("123k", Tone::Strong)));
    }

    #[test]
    fn headroom_colors() {
        let h = Headroom {
            projected: "118%".to_owned(),
            level: Level::Orange,
            advice: "throttle if possible — ETA cap in 1h 00m".to_owned(),
            advice_level: Some(Level::Orange),
        };
        let runs = headroom_runs(&h);
        assert_eq!(
            plain_text(&runs),
            "Projected use: 118%  ·  throttle if possible — ETA cap in 1h 00m"
        );
        assert!(runs.contains(&Run::new("118%", Tone::Level(Level::Orange))));
        assert_eq!(
            runs.last().map(|r| r.tone),
            Some(Tone::Level(Level::Orange))
        );

        let h = Headroom {
            advice_level: None,
            ..h
        };
        assert_eq!(headroom_runs(&h).last().map(|r| r.tone), Some(Tone::Normal));
    }

    #[test]
    fn header() {
        let runs = header_runs("Max 5x");
        assert_eq!(plain_text(&runs), "claude.ai usage  ·  Max 5x");
        assert_eq!(runs[0].tone, Tone::Accent);
        assert_eq!(runs[1].tone, Tone::Dim);
        assert_eq!(runs[2], Run::new("Max 5x", Tone::Strong));
    }

    #[test]
    fn last_polled_seconds() {
        assert_eq!(
            last_polled(NOW - Duration::seconds(24), NOW),
            "last polled 24s ago"
        );
        // Clock skew never shows a negative age.
        assert_eq!(
            last_polled(NOW + Duration::seconds(3), NOW),
            "last polled 0s ago"
        );
    }

    #[test]
    fn sha_hint_detection() {
        let info = |message: &str, stale_sha_hint| FetchErrorInfo {
            message: message.to_owned(),
            stale_sha_hint,
        };
        assert!(shows_sha_hint(&info("anything", true)));
        assert!(shows_sha_hint(&info(
            "403 — auth or Cloudflare: <html>",
            false
        )));
        assert!(shows_sha_hint(&info("HTTP 400: permission_error", false)));
        assert!(!shows_sha_hint(&info(
            "401 — session expired. Sign in to claude.ai.",
            false
        )));
        assert!(!shows_sha_hint(&info("request failed: timed out", false)));
    }
}
