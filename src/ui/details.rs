//! The collapsible "Details" card: the other windows as compact rows, the
//! full ALL/FABLE burn lines and projected-use advice, the week's usage by
//! surface, the top burners as a ranked list with proportional bars, and
//! the plan, token validity and when the data was polled; one section per
//! account when several have data. Its open state lives in
//! [`Settings::details_open`].

use std::fmt::Display;

use chrono::{DateTime, Local, TimeZone, Utc};
use eframe::egui::{self, vec2, Align, CollapsingHeader, Rect, RichText, Sense, TextStyle, Ui};

use crate::analytics::{self, alerts::fmt_clock_in};
use crate::config::Settings;
use crate::model::WeekBreakdown;
use crate::storage::InstanceUsage;

use super::accounts::AccountView;
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

/// The plan's name: the credential's `subscriptionType` ("max" → "Max"),
/// else `fallback` (the settings' plan).
pub fn plan_name(plan: Option<&str>, fallback: &str) -> String {
    let Some(plan) = plan.map(str::trim).filter(|p| !p.is_empty()) else {
        return fallback.to_owned();
    };
    let mut chars = plan.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// "token valid 3h 12m" or "token expired"; `None` when the expiry is
/// unknown.
pub fn token_validity(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    let at = expires_at?;
    Some(if at <= now {
        "token expired".to_owned()
    } else {
        format!("token valid {}", analytics::humanize_until(Some(at), now))
    })
}

/// The week's usage by surface: "Claude Code 72% · Chats 11% · Cowork 17%
/// · Other 0% · as of 1:35 PM" (whole percents; the time in `tz`, when
/// known). `None` without rows.
pub fn breakdown_text_in<Tz: TimeZone>(b: &WeekBreakdown, tz: &Tz) -> Option<String>
where
    Tz::Offset: Display,
{
    if b.rows.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = b
        .rows
        .iter()
        .map(|r| format!("{} {:.0}%", r.display_name, r.percent))
        .collect();
    if let Some(at) = b.as_of {
        parts.push(format!("as of {}", fmt_clock_in(at, tz)));
    }
    Some(parts.join(" · "))
}

/// The breakdown row's hover text.
const BREAKDOWN_HOVER: &str = "share of this week's usage by surface";

/// The Details card: one account with data as it always was; several each
/// in a collapsible section under its dot and name (open by default).
/// Clicking the card's header flips (and so persists) its open state.
pub fn show_details(
    ui: &mut Ui,
    accounts: &[AccountView],
    settings: &mut Settings,
    now: DateTime<Utc>,
) {
    let shown = super::timeline::with_data(accounts);
    if shown.is_empty() {
        return;
    }
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
            .show(ui, |ui| match shown[..] {
                [v] => contents(ui, v, settings, now),
                ref several => {
                    for v in several {
                        account_section(ui, v, settings, now, palette);
                    }
                }
            });
        if response.header_response.clicked() {
            settings.details_open = !settings.details_open;
        }
    });
}

/// One account's collapsible section: its dot and name ("· stale" after a
/// failed poll), then its [`contents`].
fn account_section(
    ui: &mut Ui,
    v: &AccountView,
    settings: &Settings,
    now: DateTime<Utc>,
    palette: &Palette,
) {
    let mut title = egui::text::LayoutJob::default();
    let small = TextStyle::Small.resolve(ui.style());
    let body = TextStyle::Body.resolve(ui.style());
    title.append("● ", 0.0, egui::TextFormat::simple(small, v.color));
    let name = theme::semibold(body.size);
    title.append(v.label, 0.0, egui::TextFormat::simple(name, palette.text));
    if v.stale {
        let muted = egui::TextFormat::simple(body, palette.text_muted);
        title.append(" · stale", 0.0, muted);
    }
    let key = super::timeline::series_key(v.id);
    let response = CollapsingHeader::new(title)
        .id_salt(("details-account", key))
        .default_open(true)
        .show(ui, |ui| contents(ui, v, settings, now));
    if let Some(e) = v.error {
        response.header_response.on_hover_text(&e.message);
    }
}

fn contents(ui: &mut Ui, v: &AccountView, settings: &Settings, now: DateTime<Utc>) {
    let Some(d) = v.data else { return };
    let key = super::timeline::series_key(v.id);
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
            rows::show_window_row(ui, key, &rows::window_row(w, now));
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
    if let Some(text) = snap
        .breakdown
        .as_ref()
        .and_then(|b| breakdown_text_in(b, &Local))
    {
        ui.colored_label(palette.text_muted, text)
            .on_hover_text(BREAKDOWN_HOVER);
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
        burner_line(ui, key, row, palette);
    }
    ui.add_space(GAP);
    ui.colored_label(
        palette.text_muted,
        format!(
            "{} plan · {}",
            plan_name(d.plan.as_deref(), &settings.plan),
            rows::last_polled(snap.fetched_at, now)
        ),
    );
    if let Some(text) = token_validity(d.token_expires_at, now) {
        ui.colored_label(palette.text_muted, text);
    }
}

/// "1  name ······ ▬▬▬  191k": rank and name on the left (the name is cut
/// short in what the bar and count leave), bar and count on the right, the
/// count in a fixed-width column so the bars line up.
fn burner_line(ui: &mut Ui, key: u64, row: &BurnerRow, palette: &Palette) {
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
            let id = egui::Id::new(("burner-bar", key, &row.name));
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
    fn plan_names() {
        assert_eq!(plan_name(Some("max"), "Pro"), "Max");
        assert_eq!(plan_name(Some("team"), "Max"), "Team");
        assert_eq!(plan_name(None, "Max 5x"), "Max 5x");
        assert_eq!(plan_name(Some("  "), "Max"), "Max");
    }

    #[test]
    fn token_validity_text() {
        let now = DateTime::from_timestamp(1_791_000_000, 0).unwrap();
        let later = now + chrono::Duration::minutes(3 * 60 + 12) + chrono::Duration::seconds(30);
        assert_eq!(
            token_validity(Some(later), now).as_deref(),
            Some("token valid 3h 12m")
        );
        assert_eq!(
            token_validity(Some(now), now).as_deref(),
            Some("token expired")
        );
        let earlier = now - chrono::Duration::minutes(1);
        assert_eq!(
            token_validity(Some(earlier), now).as_deref(),
            Some("token expired")
        );
        assert_eq!(token_validity(None, now), None);
    }

    fn breakdown(as_of: Option<DateTime<Utc>>, rows: &[(&str, f64)]) -> WeekBreakdown {
        WeekBreakdown {
            as_of,
            window_started_at: None,
            rows: rows
                .iter()
                .map(|&(name, percent)| crate::model::BreakdownRow {
                    key: name.to_lowercase(),
                    display_name: name.to_owned(),
                    percent,
                })
                .collect(),
        }
    }

    #[test]
    fn breakdown_text() {
        use chrono::{FixedOffset, TimeZone};
        // 20:35 UTC is 1:35 PM at UTC-7.
        let at = Utc.with_ymd_and_hms(2026, 10, 11, 20, 35, 0).unwrap();
        let pdt = FixedOffset::west_opt(7 * 3600).unwrap();
        let b = breakdown(
            Some(at),
            &[
                ("Claude Code", 72.4),
                ("Chats", 10.6),
                ("Cowork", 17.0),
                ("Other", 0.0),
            ],
        );
        assert_eq!(
            breakdown_text_in(&b, &pdt).as_deref(),
            Some("Claude Code 72% · Chats 11% · Cowork 17% · Other 0% · as of 1:35 PM")
        );
        let utc = FixedOffset::east_opt(0).unwrap();
        assert!(breakdown_text_in(&b, &utc)
            .unwrap()
            .ends_with("as of 8:35 PM"));
        // No time: no "as of"; no rows: no row at all.
        let b = breakdown(None, &[("Chats", 100.0)]);
        assert_eq!(breakdown_text_in(&b, &utc).as_deref(), Some("Chats 100%"));
        assert_eq!(breakdown_text_in(&breakdown(Some(at), &[]), &utc), None);
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
