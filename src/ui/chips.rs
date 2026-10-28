//! The header's stat chips: one rounded chip per headline window (5-hour,
//! 7-day all, and Fable when reported) with a small label, the big
//! percentage in its state color, and the pace delta against an even burn.
//!
//! The chip model, copy, pace colors and wrapping are pure and tested; the
//! painter at the bottom only draws what they decide.

use std::fmt::Display;

use chrono::{DateTime, Local, TimeZone, Utc};
use eframe::egui::{
    epaint::Shadow, pos2, vec2, Align2, Color32, FontId, Id, Rect, Response, Sense, Stroke,
    TextStyle, Ui,
};

use crate::analytics::{self, alerts::fmt_clock_in, Level};
use crate::model::Snapshot;

use super::accounts::AccountView;
use super::digits;
use super::layout::{self, GAP};
use super::motion;
use super::theme::{self, Palette};

/// Chip corner radius.
pub const CHIP_RADIUS: u8 = 10;
/// Chip inner padding, horizontal and vertical.
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 7.0;
/// Space between the label line and the percentage line.
const LINE_GAP: f32 = 1.0;
/// Space between the percentage and the pace delta.
const DELTA_GAP: f32 = 4.0;
/// A pace delta this far from even (in points, either way) gets a color.
pub const PACE_NOTABLE: f64 = 10.0;

/// One stat chip's data.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    /// Muted caption ("5-hour").
    pub label: &'static str,
    pub pct: Option<f64>,
    /// Utilization minus the elapsed share of the window (see
    /// [`analytics::pace`]); `None` without a reset time.
    pub pace: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    /// Shows the near-cap pulse (the 5-hour chip).
    pub pulses: bool,
    /// No 5-hour window is active ([`analytics::FiveHourState::Idle`]).
    pub idle: bool,
}

/// The 5-hour chip's hover text while no window is active.
pub const IDLE_TIP: &str = "no active window · starts with your next message";

/// The chips for this snapshot: 5-hour and 7-day all always, Fable only
/// when the API reports it. The 5-hour pace runs over the dashboard's
/// window (`window_start..window_end`), only while it is active with a
/// known reset (see [`analytics::five_hour_now`]: a window that rolled over
/// since the poll reads 0% with no pace); the 7-day ones over the 7 days
/// before their reset.
pub fn chips(
    snap: &Snapshot,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<Chip> {
    let five = analytics::five_hour_now(snap.five_hour_resets_at(), snap.five_hour_pct(), now);
    let mut out = vec![Chip {
        label: "5-hour",
        pct: five.pct,
        pace: five
            .resets_at
            .and(five.pct)
            .and_then(|u| analytics::pace(u, window_start, window_end, now)),
        resets_at: five.resets_at,
        pulses: true,
        idle: five.state == analytics::FiveHourState::Idle,
    }];
    let weekly = |label: &'static str| {
        let w = snap.window(label);
        let pct = w.and_then(|w| w.utilization);
        let resets_at = w.and_then(|w| w.resets_at);
        Chip {
            label,
            pct,
            pace: analytics::seven_day_pace(pct, resets_at, now),
            resets_at,
            pulses: false,
            idle: false,
        }
    };
    out.push(weekly("7-day all"));
    if snap.window("Fable").is_some() {
        out.push(weekly("Fable"));
    }
    out
}

/// The chip's percentage: whole percent, rounded down (so 99.6 never reads
/// as the cap), or "—" when unknown.
pub fn pct_text(pct: Option<f64>) -> String {
    match pct {
        Some(p) => format!("{:.0}%", p.floor()),
        None => "—".to_owned(),
    }
}

/// Pace delta in whole points: "▲+6" ahead, "▼-4" behind, "±0" on pace.
pub fn pace_text(delta: f64) -> String {
    let points = delta.round() as i64;
    match points {
        0 => "±0".to_owned(),
        p if p > 0 => format!("▲+{p}"),
        p => format!("▼{p}"),
    }
}

/// How a pace delta is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceTone {
    /// Within [`PACE_NOTABLE`] of even.
    Muted,
    /// Notably ahead: burning faster than an even pace (amber).
    Ahead,
    /// Notably behind: room to spare (green).
    Behind,
}

/// The pace delta's color class, from the delta as shown (whole points).
pub fn pace_tone(delta: f64) -> PaceTone {
    let points = delta.round();
    if points >= PACE_NOTABLE {
        PaceTone::Ahead
    } else if points <= -PACE_NOTABLE {
        PaceTone::Behind
    } else {
        PaceTone::Muted
    }
}

/// The pace in words: "6 points ahead of an even pace", "1 point behind
/// an even pace", "right on an even pace".
pub fn pace_explained(delta: f64) -> String {
    let points = delta.round() as i64;
    if points == 0 {
        return "right on an even pace".to_owned();
    }
    let n = points.abs();
    let unit = if n == 1 { "point" } else { "points" };
    let side = if points > 0 { "ahead of" } else { "behind" };
    format!("{n} {unit} {side} an even pace")
}

/// "resets 3:42 PM · in 1h 59m", with the weekday ("resets Tue 3:42 PM ·
/// in 1d 10h") when the reset is not today in `tz`.
pub fn reset_tip_in<Tz: TimeZone>(resets_at: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    let same_day = resets_at.with_timezone(tz).date_naive() == now.with_timezone(tz).date_naive();
    let day = if same_day {
        String::new()
    } else {
        resets_at.with_timezone(tz).format("%a ").to_string()
    };
    format!(
        "resets {day}{} · in {}",
        fmt_clock_in(resets_at, tz),
        analytics::humanize_until(Some(resets_at), now)
    )
}

/// The chip's hover text: reset line and pace line, each when known, or
/// [`IDLE_TIP`] while no window is active.
pub fn tooltip_in<Tz: TimeZone>(chip: &Chip, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: Display,
{
    if chip.idle {
        return IDLE_TIP.to_owned();
    }
    let lines: Vec<String> = [
        chip.resets_at.map(|r| reset_tip_in(r, now, tz)),
        chip.pace.map(pace_explained),
    ]
    .into_iter()
    .flatten()
    .collect();
    if lines.is_empty() {
        "no reset time reported".to_owned()
    } else {
        lines.join("\n")
    }
}

// --- Painting -----------------------------------------------------------------

/// A chip's measured text, ready to place.
struct Measured {
    label: std::sync::Arc<eframe::egui::Galley>,
    pct: (std::sync::Arc<eframe::egui::Galley>, f32),
    delta: Option<std::sync::Arc<eframe::egui::Galley>>,
    size: eframe::egui::Vec2,
}

/// `chip` measured with its percentage shown as `pct` (eased) in
/// `pct_color` (eased state color).
fn measure(
    ui: &Ui,
    chip: &Chip,
    pct: Option<f64>,
    pct_color: Color32,
    palette: &Palette,
) -> Measured {
    let painter = ui.painter();
    let small = TextStyle::Small.resolve(ui.style());
    let big = TextStyle::Name(theme::CHIP.into()).resolve(ui.style());
    let label = painter.layout_no_wrap(chip.label.to_owned(), small.clone(), palette.text_muted);
    let pct = digits::number_galley(painter, &pct_text(pct), big, pct_color);
    let delta = chip.pace.filter(|_| chip.pct.is_some()).map(|d| {
        let font = FontId::new(small.size, small.family.clone());
        painter.layout_no_wrap(pace_text(d), font, pace_color(pace_tone(d), palette))
    });
    let line2 = pct.1 + delta.as_ref().map_or(0.0, |g| DELTA_GAP + g.size().x);
    let size = vec2(
        2.0 * PAD_X + label.size().x.max(line2),
        2.0 * PAD_Y + label.size().y + LINE_GAP + pct.0.size().y,
    );
    Measured {
        label,
        pct,
        delta,
        size,
    }
}

/// Color of a pace delta.
pub fn pace_color(tone: PaceTone, palette: &Palette) -> Color32 {
    match tone {
        PaceTone::Muted => palette.text_muted,
        PaceTone::Ahead => palette.level(Level::Yellow),
        PaceTone::Behind => palette.level(Level::Green),
    }
}

/// One account's chips in the header.
#[derive(Debug, Clone, PartialEq)]
pub struct ChipGroup {
    /// The account's index in the views.
    pub account: usize,
    /// Its chips; empty for an account with only an error.
    pub chips: Vec<Chip>,
    /// Draw the account's dot and label (and badge) over its chips: only
    /// when two or more accounts are shown.
    pub header: bool,
}

/// The header's chip groups, in account order. With two or more accounts
/// that have data or an error, each gets a group (one with only an error
/// shows its header and badge alone); otherwise only an account with data
/// is shown, headerless, as one account always was. Only the 5-hour chip
/// of the [`accounts::hottest`] account pulses, the one the app's pulse
/// follows.
///
/// [`accounts::hottest`]: super::accounts::hottest
pub fn chip_groups(views: &[AccountView], now: DateTime<Utc>) -> Vec<ChipGroup> {
    let hot = super::accounts::hottest(views, now);
    let shown: Vec<usize> = (0..views.len())
        .filter(|&i| views[i].data.is_some() || views[i].error.is_some())
        .collect();
    let header = shown.len() >= 2;
    shown
        .into_iter()
        .filter(|&i| header || views[i].data.is_some())
        .map(|i| {
            let mut chips = views[i]
                .data
                .map(|d| chips(&d.snapshot, d.window_start, d.window_end, now))
                .unwrap_or_default();
            for c in &mut chips {
                c.pulses &= hot == Some(i);
            }
            ChipGroup {
                account: i,
                chips,
                header,
            }
        })
        .collect()
}

/// The id of one eased value (`what`: "chip-pct", "chip-color") of an
/// account's chip, keyed by [`timeline::series_key`] of its profile id.
///
/// [`timeline::series_key`]: super::timeline::series_key
pub fn chip_id(what: &'static str, key: u64, label: &str) -> Id {
    Id::new((what, key, label))
}

/// The badge after an account's label: "stale" over old data, else the
/// error in brief; with the full error message as its hover text. `None`
/// for a fresh account.
pub fn badge(view: &AccountView) -> Option<(&'static str, String)> {
    let error = view.error?;
    let text = if view.data.is_some() {
        "stale"
    } else {
        super::accounts::short_error(error.kind)
    };
    Some((text, error.message.clone()))
}

/// The accounts' stat chips (see [`chip_groups`]): each account's dot,
/// label and badge over its chips when several are shown, a stale one
/// dimmed; nothing before the first poll.
pub fn show_account_chips(ui: &mut Ui, accounts: &[AccountView], now: DateTime<Utc>, pulse: f32) {
    let groups = chip_groups(accounts, now);
    if groups.is_empty() {
        return;
    }
    let palette = Palette::for_visuals(ui.visuals());
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(GAP, GAP);
        for g in &groups {
            let view = &accounts[g.account];
            let key = super::timeline::series_key(view.id);
            ui.vertical(|ui| {
                if g.header {
                    if view.stale {
                        ui.multiply_opacity(super::timeline::DIM_ALPHA);
                    }
                    ui.spacing_mut().item_spacing.y = 2.0;
                    group_header(ui, palette, view);
                }
                if !g.chips.is_empty() {
                    show_chips(ui, key, &g.chips, now, pulse);
                }
            });
        }
    });
}

/// An account's dot and name over its chips, and its [`badge`].
fn group_header(ui: &mut Ui, palette: &Palette, view: &AccountView) {
    use eframe::egui::{CornerRadius, Frame, Margin, RichText};
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(RichText::new("●").small().color(view.color));
        ui.label(RichText::new(view.label).small().color(palette.text_muted));
        if let Some((text, hover)) = badge(view) {
            Frame::NONE
                .stroke(Stroke::new(1.0, palette.border))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(4, 0))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).small().color(palette.text_muted))
                })
                .response
                .on_hover_text(hover);
        }
    });
}

/// One account's chips (ids keyed by `key`), packed into as many rows as
/// the width needs. Percentages and their state colors ease to each poll's
/// values; the border and glow of the chip that [`Chip::pulses`] show
/// `pulse` (a [`motion::Pulse`] level, 0 = none).
fn show_chips(ui: &mut Ui, key: u64, chips: &[Chip], now: DateTime<Utc>, pulse: f32) {
    let palette = Palette::for_visuals(ui.visuals());
    let ctx = ui.ctx().clone();
    let shown: Vec<(Option<f64>, Color32)> = chips
        .iter()
        .map(|c| {
            let color = palette.level(analytics::color_for(c.pct));
            (
                c.pct
                    .map(|p| motion::eased(&ctx, chip_id("chip-pct", key, c.label), p)),
                motion::eased(&ctx, chip_id("chip-color", key, c.label), color),
            )
        })
        .collect();
    let measured: Vec<Measured> = chips
        .iter()
        .zip(&shown)
        .map(|(c, &(pct, color))| measure(ui, c, pct, color, palette))
        .collect();
    let widths: Vec<f32> = measured.iter().map(|m| m.size.x).collect();
    let rows = layout::pack_rows(&widths, ui.available_width(), GAP);
    let mut items = chips.iter().zip(measured).zip(shown);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing = vec2(GAP, GAP);
        for count in rows {
            ui.horizontal(|ui| {
                for ((chip, m), (_, color)) in items.by_ref().take(count) {
                    let glow = if chip.pulses { pulse } else { 0.0 };
                    paint_chip(ui, m, palette, glow, color)
                        .on_hover_text(tooltip_in(chip, now, &Local));
                }
            });
        }
    });
}

/// The chip's border: the theme's border, drawn toward `color` (the chip's
/// state color) and thicker as `glow` rises to 1.
pub fn glow_stroke(glow: f32, color: Color32, palette: &Palette) -> Stroke {
    let glow = glow.clamp(0.0, 1.0);
    Stroke::new(1.0 + 0.5 * glow, palette.border.lerp_to_gamma(color, glow))
}

fn paint_chip(ui: &mut Ui, m: Measured, palette: &Palette, glow: f32, color: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(m.size, Sense::hover());
    let painter = ui.painter();
    let shadow = Shadow {
        offset: [0, 1],
        blur: 6,
        spread: 0,
        color: palette.shadow,
    };
    painter.add(shadow.as_shape(rect, CHIP_RADIUS));
    if glow > 0.0 {
        let halo = Shadow {
            offset: [0, 0],
            blur: 12,
            spread: 1,
            color: color.gamma_multiply(0.45 * glow.min(1.0)),
        };
        painter.add(halo.as_shape(rect, CHIP_RADIUS));
    }
    painter.rect(
        rect,
        CHIP_RADIUS,
        palette.surface,
        glow_stroke(glow, color, palette),
        eframe::egui::StrokeKind::Inside,
    );
    let inner = rect.shrink2(vec2(PAD_X, PAD_Y));
    painter.galley(inner.min, m.label.clone(), palette.text_muted);
    let pct_top = inner.min.y + m.label.size().y + LINE_GAP;
    let (pct_galley, pct_w) = m.pct;
    let pct_rect = Rect::from_min_size(pos2(inner.min.x, pct_top), pct_galley.size());
    painter.galley(pct_rect.min, pct_galley.clone(), palette.text);
    if let Some(delta) = m.delta {
        // Baseline-ish: bottom-align the small delta with the big number,
        // nudged up by the difference in descender depth.
        let x = inner.min.x + pct_w + DELTA_GAP;
        let descent = (pct_galley.size().y - delta.size().y) * 0.18;
        let pos =
            Align2::LEFT_BOTTOM.anchor_size(pos2(x, pct_rect.bottom() - descent), delta.size());
        painter.galley(pos.min, delta, palette.text_muted);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;
    use chrono::{Duration, FixedOffset};

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, h, m, 0).unwrap()
    }

    fn window(label: &str, pct: Option<f64>, resets_at: Option<DateTime<Utc>>) -> UsageWindow {
        UsageWindow {
            label: label.to_owned(),
            utilization: pct,
            resets_at,
        }
    }

    fn snap(windows: Vec<UsageWindow>) -> Snapshot {
        Snapshot {
            fetched_at: t(12, 0),
            windows,
            extra: None,
            breakdown: None,
        }
    }

    #[test]
    fn chips_for_headline_windows() {
        let now = t(12, 0);
        let s = snap(vec![
            window("Iguana", Some(5.0), None),
            window("7-day all", Some(13.0), Some(now + Duration::days(3))),
            window("5-hour", Some(46.0), Some(t(14, 0))),
        ]);
        // 5-hour window 9:00-14:00: 3 of 5 hours gone = 60% -> 46 - 60.
        let c = chips(&s, t(9, 0), t(14, 0), now);
        assert_eq!(c.len(), 2, "no Fable chip when not reported: {c:?}");
        assert_eq!(c[0].label, "5-hour");
        assert_eq!(c[0].pct, Some(46.0));
        assert!((c[0].pace.unwrap() - -14.0).abs() < 1e-9);
        assert_eq!(c[0].resets_at, Some(t(14, 0)));
        assert!(c[0].pulses, "the 5-hour chip shows the near-cap pulse");
        assert!(!c[1].pulses);
        // 7-day: 4 of 7 days gone -> 13 - 57.14.
        assert_eq!(c[1].label, "7-day all");
        assert!((c[1].pace.unwrap() - (13.0 - 400.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn chips_fable_when_reported_and_no_pace_without_reset() {
        let now = t(12, 0);
        let s = snap(vec![
            window("5-hour", Some(46.0), None),
            window("Fable", None, Some(now + Duration::days(1))),
        ]);
        let c = chips(&s, t(9, 0), t(14, 0), now);
        let labels: Vec<_> = c.iter().map(|c| c.label).collect();
        assert_eq!(labels, ["5-hour", "7-day all", "Fable"]);
        assert_eq!(c[0].pace, None, "5-hour without reset has no pace");
        assert_eq!(c[1].pct, None, "7-day all missing: an empty chip");
        assert_eq!(c[2].pct, None);
        assert_eq!(c[2].pace, None, "no utilization, no pace");
    }

    #[test]
    fn chips_idle_after_a_reset_show_0_without_pace() {
        let now = t(12, 0);
        // After a window ends: 0% and no reset until the next message.
        let s = snap(vec![window("5-hour", Some(0.0), None)]);
        let c = chips(&s, now - Duration::hours(5), now, now);
        assert_eq!(
            (c[0].pct, c[0].pace, c[0].resets_at),
            (Some(0.0), None, None)
        );
        assert!(c[0].idle);
        assert!(!c[1].idle);
    }

    #[test]
    fn chips_after_the_reset_passes_read_idle_until_the_next_poll() {
        // Polled at 85% in the 9:00-14:00 window; now is 14:01.
        let s = snap(vec![window("5-hour", Some(85.0), Some(t(14, 0)))]);
        let c = chips(&s, t(9, 0), t(14, 0), t(14, 1));
        assert_eq!(
            (c[0].pct, c[0].pace, c[0].resets_at),
            (Some(0.0), None, None)
        );
        assert!(c[0].idle);
        // Before the reset: the normal chip.
        let c = chips(&s, t(9, 0), t(14, 0), t(13, 0));
        assert_eq!(c[0].pct, Some(85.0));
        assert!(c[0].pace.is_some() && !c[0].idle);
    }

    #[test]
    fn pct_rounds_down() {
        assert_eq!(pct_text(Some(46.0)), "46%");
        assert_eq!(pct_text(Some(99.6)), "99%");
        assert_eq!(pct_text(Some(104.2)), "104%");
        assert_eq!(pct_text(Some(0.4)), "0%");
        assert_eq!(pct_text(None), "—");
    }

    #[test]
    fn pace_copy() {
        assert_eq!(pace_text(6.2), "▲+6");
        assert_eq!(pace_text(-4.4), "▼-4");
        assert_eq!(pace_text(0.4), "±0");
        assert_eq!(pace_text(-0.5), "▼-1");
        assert_eq!(pace_explained(6.2), "6 points ahead of an even pace");
        assert_eq!(pace_explained(-1.0), "1 point behind an even pace");
        assert_eq!(pace_explained(0.3), "right on an even pace");
    }

    #[test]
    fn pace_colors_from_ten_points() {
        assert_eq!(pace_tone(9.4), PaceTone::Muted);
        assert_eq!(pace_tone(9.5), PaceTone::Ahead, "shows as +10");
        assert_eq!(pace_tone(25.0), PaceTone::Ahead);
        assert_eq!(pace_tone(-9.4), PaceTone::Muted);
        assert_eq!(pace_tone(-10.0), PaceTone::Behind);
        assert_eq!(pace_tone(0.0), PaceTone::Muted);
    }

    #[test]
    fn reset_tips() {
        let tz = FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            reset_tip_in(t(13, 59), t(12, 0), &tz),
            "resets 1:59 PM · in 1h 59m"
        );
        // Another day: weekday, day-hour countdown. 2026-10-14 is a Wednesday.
        assert_eq!(
            reset_tip_in(t(22, 0) + Duration::days(3), t(12, 0), &tz),
            "resets Wed 10:00 PM · in 3d 10h"
        );
        // Same UTC day, next local day in UTC+5:30 (12:00 -> 17:30, 20:00 -> 01:30).
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(
            reset_tip_in(t(20, 0), t(12, 0), &ist),
            "resets Mon 1:30 AM · in 8h 00m"
        );
    }

    #[test]
    fn glow_border_from_theme_to_state_color() {
        let palette = &theme::DARK;
        let red = palette.danger;
        let none = glow_stroke(0.0, red, palette);
        assert_eq!(none, Stroke::new(1.0, palette.border));
        let full = glow_stroke(1.0, red, palette);
        assert_eq!(full, Stroke::new(1.5, red));
        assert_eq!(glow_stroke(3.0, red, palette), full, "clamped");
    }

    fn dashboard(pct: f64) -> crate::poller::Dashboard {
        let now = t(12, 0);
        crate::poller::Dashboard {
            snapshot: snap(vec![
                window("5-hour", Some(pct), Some(t(14, 0))),
                window("7-day all", Some(10.0), Some(now + Duration::days(3))),
            ]),
            history: Vec::new(),
            fable_history: Vec::new(),
            seven_day_recent: Vec::new(),
            fable_recent: Vec::new(),
            burners: Vec::new(),
            window_start: t(9, 0),
            window_end: t(14, 0),
            window_samples: Vec::new(),
            week_start: now,
            week_end: now,
            week_samples: Vec::new(),
            persist_error: None,
            plan: None,
            token_expires_at: None,
        }
    }

    fn error(message: &str) -> crate::poller::FetchErrorInfo {
        crate::poller::FetchErrorInfo {
            kind: crate::fetch::ErrorKind::TokenExpired,
            message: message.to_owned(),
        }
    }

    fn view<'a>(
        id: &'a crate::profiles::ProfileId,
        data: Option<&'a crate::poller::Dashboard>,
        error: Option<&'a crate::poller::FetchErrorInfo>,
    ) -> AccountView<'a> {
        AccountView {
            id,
            label: id.as_str(),
            color: Color32::RED,
            data,
            error,
            stale: data.is_some() && error.is_some(),
        }
    }

    fn summary(groups: &[ChipGroup]) -> Vec<(usize, usize, bool)> {
        groups
            .iter()
            .map(|g| (g.account, g.chips.len(), g.header))
            .collect()
    }

    #[test]
    fn one_account_is_headerless_as_always() {
        use crate::profiles::ProfileId;
        let now = t(12, 0);
        let (a, b) = (ProfileId::new("a"), ProfileId::new("b"));
        let d = dashboard(40.0);
        // B has not polled yet: only A, without a header.
        let views = [view(&a, Some(&d), None), view(&b, None, None)];
        let g = chip_groups(&views, now);
        assert_eq!(summary(&g), [(0, 2, false)]);
        assert!(
            g[0].chips[0].pulses,
            "the only account's 5-hour chip pulses"
        );
        // Nothing yet, or only an error with no data: nothing (the banner
        // says why).
        assert!(chip_groups(&[view(&a, None, None)], now).is_empty());
        let e = error("expired");
        assert!(chip_groups(&[view(&a, None, Some(&e))], now).is_empty());
    }

    #[test]
    fn groups_in_account_order_with_error_only_accounts() {
        use crate::profiles::ProfileId;
        let now = t(12, 0);
        let ids: Vec<ProfileId> = ["a", "b", "c", "d"].map(ProfileId::new).into();
        let (cool, hot) = (dashboard(20.0), dashboard(70.0));
        let e = error("HTTP 401: token expired");
        let views = [
            view(&ids[0], None, Some(&e)),
            view(&ids[1], Some(&cool), None),
            view(&ids[2], None, None),
            view(&ids[3], Some(&hot), None),
        ];
        let g = chip_groups(&views, now);
        // "c" (still polling) is left out; "a" shows its header alone.
        assert_eq!(summary(&g), [(0, 0, true), (1, 2, true), (3, 2, true)]);
        // Only the hottest account's 5-hour chip pulses.
        let pulsing: Vec<_> = g
            .iter()
            .flat_map(|g| g.chips.iter().map(move |c| (g.account, c.label, c.pulses)))
            .filter(|c| c.2)
            .collect();
        assert_eq!(pulsing, [(3, "5-hour", true)]);
    }

    #[test]
    fn a_stale_hot_account_does_not_pulse() {
        use crate::profiles::ProfileId;
        let now = t(12, 0);
        let (a, b) = (ProfileId::new("a"), ProfileId::new("b"));
        let (cool, hot) = (dashboard(20.0), dashboard(90.0));
        let e = error("HTTP 500: oops");
        let views = [view(&a, Some(&cool), None), view(&b, Some(&hot), Some(&e))];
        let g = chip_groups(&views, now);
        assert!(g[0].chips[0].pulses);
        assert!(
            !g[1].chips[0].pulses,
            "stale: its old figure is not followed"
        );
        assert_eq!(
            badge(&views[1]),
            Some(("stale", "HTTP 500: oops".to_owned()))
        );
        assert_eq!(badge(&views[0]), None);
        let expired = view(&a, None, Some(&e));
        assert_eq!(badge(&expired).unwrap().0, "token expired");
    }

    #[test]
    fn chip_ids_unique_across_accounts() {
        use crate::profiles::ProfileId;
        let now = t(12, 0);
        let (a, b) = (ProfileId::new("default"), ProfileId::new("claude-b"));
        let (d1, d2) = (dashboard(20.0), dashboard(70.0));
        let views = [view(&a, Some(&d1), None), view(&b, Some(&d2), None)];
        let mut ids = Vec::new();
        for g in chip_groups(&views, now) {
            let key = crate::ui::timeline::series_key(views[g.account].id);
            for c in &g.chips {
                ids.push(chip_id("chip-pct", key, c.label));
                ids.push(chip_id("chip-color", key, c.label));
            }
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), 8);
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn tooltip_lines() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let chip = Chip {
            label: "5-hour",
            pct: Some(46.0),
            pace: Some(6.0),
            resets_at: Some(t(13, 59)),
            pulses: true,
            idle: false,
        };
        assert_eq!(
            tooltip_in(&chip, t(12, 0), &tz),
            "resets 1:59 PM · in 1h 59m\n6 points ahead of an even pace"
        );
        let bare = Chip {
            pace: None,
            resets_at: None,
            ..chip
        };
        assert_eq!(tooltip_in(&bare, t(12, 0), &tz), "no reset time reported");
        let idle = Chip {
            pct: Some(0.0),
            idle: true,
            ..bare
        };
        assert_eq!(tooltip_in(&idle, t(12, 0), &tz), IDLE_TIP);
    }
}
