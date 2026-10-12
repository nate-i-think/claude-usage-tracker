//! The collapsible "Details" card: the other windows as compact rows, the
//! full ALL/FABLE burn lines and projected-use advice, the top burners as a
//! ranked list with proportional bars, and when the data was polled. Its
//! open state lives in [`Settings::details_open`].

use chrono::{DateTime, Utc};
use eframe::egui::{self, vec2, Align, CollapsingHeader, Rect, RichText, Sense, TextStyle, Ui};

use crate::analytics;
use crate::config::Settings;
use crate::poller::Dashboard;
use crate::storage::InstanceUsage;

use super::layout::{right_then_left, GAP};
use super::rows::{self, BurnTag};
use super::theme::{self, Palette};
use super::{digits, motion, show_runs};

/// How many burners the list shows.
pub const TOP_BURNERS: usize = 5;
/// Width of a burner's proportional bar.
const BURNER_BAR_W: f32 = 56.0;
/// Thickness of a burner's bar.
const BURNER_BAR_H: f32 = 4.0;

/// One ranked burner.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnerRow {
    /// 1-based.
    pub rank: usize,
    pub name: String,
    /// See [`fmt_tokens`].
    pub tokens: String,
    /// Tokens relative to the top burner, 0..=1.
    pub fraction: f32,
}

/// The top [`TOP_BURNERS`] (already sorted by tokens, highest first), each
/// with its share of the top burner's tokens.
pub fn burner_rows(burners: &[InstanceUsage]) -> Vec<BurnerRow> {
    let top = burners.first().map_or(0, |b| b.tokens);
    burners
        .iter()
        .take(TOP_BURNERS)
        .enumerate()
        .map(|(i, b)| BurnerRow {
            rank: i + 1,
            name: b.display_name.clone(),
            tokens: fmt_tokens(b.tokens),
            fraction: if top == 0 {
                0.0
            } else {
                (b.tokens as f64 / top as f64).clamp(0.0, 1.0) as f32
            },
        })
        .collect()
}

/// A token count, short: "950", "51.8k", "191k", "1.2M".
pub fn fmt_tokens(n: u64) -> String {
    let n = n as f64;
    if n < 1e3 {
        format!("{n:.0}")
    } else if n < 99_950.0 {
        format!("{:.1}k", n / 1e3)
    } else if n < 999_500.0 {
        format!("{:.0}k", n / 1e3)
    } else {
        format!("{:.1}M", n / 1e6)
    }
}

/// The Details card. Clicking the header flips (and so persists) the open
/// state.
pub fn show_details(ui: &mut Ui, d: &Dashboard, settings: &mut Settings, now: DateTime<Utc>) {
    let palette = Palette::for_visuals(ui.visuals());
    theme::card(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        let body = TextStyle::Body.resolve(ui.style());
        let title = RichText::new("Details")
            .font(theme::semibold(body.size))
            .color(palette.text);
        let response = CollapsingHeader::new(title)
            .id_salt("details")
            .open(Some(settings.details_open))
            .show(ui, |ui| contents(ui, d, settings, now));
        if response.header_response.clicked() {
            settings.details_open = !settings.details_open;
        }
    });
}

fn contents(ui: &mut Ui, d: &Dashboard, settings: &Settings, now: DateTime<Utc>) {
    let palette = Palette::for_visuals(ui.visuals());
    let snap = &d.snapshot;
    let order = analytics::ordered_windows(snap);
    let others: Vec<_> = order
        .rest
        .iter()
        .filter(|w| analytics::window_visible(w, settings.show_all))
        .collect();
    if !others.is_empty() {
        for w in others {
            rows::show_window_row(ui, &rows::window_row(w, now));
        }
        ui.add_space(GAP);
    }

    // As of now: with no active window (after a reset, or a rollover since
    // the poll) the ALL line says idle and there is no projected use.
    let five = d.five_hour_now(now);
    let idle = five.state == analytics::FiveHourState::Idle;
    let all_body = if idle {
        vec![super::Run::new(super::footer::IDLE, super::Tone::Dim)]
    } else {
        let all = analytics::burn_line(&d.history, five.pct, five.resets_at, now);
        rows::burn_body(all.as_ref())
    };
    rows::show_burn_row(ui, BurnTag::All, &all_body);
    if let Some(fable) = snap.window("Fable") {
        let line = analytics::burn_line(&d.fable_history, fable.utilization, fable.resets_at, now);
        rows::show_burn_row(ui, BurnTag::Fable, &rows::burn_body(line.as_ref()));
    }
    let rate = analytics::compute_rate(&d.history, now);
    let headroom = analytics::headroom(five.pct, five.resets_at, rate, now).filter(|_| !idle);
    if let Some(h) = headroom {
        show_runs(ui, &rows::headroom_runs(&h));
    }
    ui.add_space(GAP);

    let body = TextStyle::Body.resolve(ui.style());
    ui.label(
        RichText::new("Top burners (5h)")
            .font(theme::semibold(body.size))
            .color(palette.text),
    );
    let burners = burner_rows(&d.burners);
    if burners.is_empty() {
        ui.weak("no local activity yet");
    }
    for row in &burners {
        burner_line(ui, row, palette);
    }
    ui.add_space(GAP);
    ui.colored_label(
        palette.text_muted,
        format!(
            "{} plan · {}",
            settings.plan,
            rows::last_polled(snap.fetched_at, now)
        ),
    );
}

/// "1  name ······ ▬▬▬  191k": rank and name on the left (the name is cut
/// short in what the bar and count leave), bar and count on the right, the
/// count in a fixed-width column so the bars line up.
fn burner_line(ui: &mut Ui, row: &BurnerRow, palette: &Palette) {
    right_then_left(
        ui,
        Align::Center,
        |ui| {
            let font = theme::semibold(TextStyle::Body.resolve(ui.style()).size);
            let (_, column) =
                digits::number_galley(ui.painter(), "000.0M", font.clone(), palette.text);
            let size = vec2(column, ui.spacing().interact_size.y);
            ui.allocate_ui_with_layout(size, egui::Layout::right_to_left(Align::Center), |ui| {
                ui.set_min_size(size);
                digits::number_label(ui, &row.tokens, font, palette.text);
            });
            let (rect, _) = ui.allocate_exact_size(
                vec2(BURNER_BAR_W, ui.spacing().interact_size.y),
                Sense::hover(),
            );
            let track = Rect::from_center_size(rect.center(), vec2(BURNER_BAR_W, BURNER_BAR_H));
            ui.painter()
                .rect_filled(track, BURNER_BAR_H / 2.0, palette.track);
            let id = egui::Id::new(("burner-bar", &row.name));
            let fraction = motion::eased(ui.ctx(), id, row.fraction);
            if fraction > 0.0 {
                let mut fill = track;
                fill.set_width((BURNER_BAR_W * fraction).max(BURNER_BAR_H));
                ui.painter()
                    .rect_filled(fill, BURNER_BAR_H / 2.0, palette.bar);
            }
        },
        |ui| {
            ui.colored_label(palette.text_muted, format!("{}", row.rank));
            ui.add(egui::Label::new(RichText::new(&row.name).color(palette.text)).truncate());
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn burner(name: &str, tokens: u64) -> InstanceUsage {
        InstanceUsage {
            name: format!("C--Users-x-{name}"),
            display_name: name.to_owned(),
            source: "claude-code",
            tokens,
            last_active: DateTime::from_timestamp(1_791_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn token_counts() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(51_822), "51.8k");
        assert_eq!(fmt_tokens(99_949), "99.9k");
        assert_eq!(fmt_tokens(190_775), "191k");
        assert_eq!(fmt_tokens(999_499), "999k");
        assert_eq!(fmt_tokens(1_234_567), "1.2M");
    }

    #[test]
    fn burners_ranked_with_shares() {
        let burners: Vec<_> = [("a", 200_000), ("b", 50_000), ("c", 0)]
            .into_iter()
            .map(|(n, t)| burner(n, t))
            .collect();
        let rows = burner_rows(&burners);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            BurnerRow {
                rank: 1,
                name: "a".to_owned(),
                tokens: "200k".to_owned(),
                fraction: 1.0
            }
        );
        assert_eq!(rows[1].rank, 2);
        assert_eq!(rows[1].fraction, 0.25);
        assert_eq!(rows[2].fraction, 0.0);
    }

    #[test]
    fn burners_top_five_and_empty() {
        let burners: Vec<_> = (0..8).map(|i| burner("x", 1000 - i)).collect();
        assert_eq!(burner_rows(&burners).len(), TOP_BURNERS);
        assert!(burner_rows(&[]).is_empty());
        // All zero: no division by zero.
        assert_eq!(burner_rows(&[burner("z", 0)])[0].fraction, 0.0);
    }
}
