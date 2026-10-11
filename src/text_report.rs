//! Plain-text dashboard for `claude-usage-cli once`.
//!
//! Reuses the GUI's row formatters ([`crate::ui::rows`]) and drops their
//! tones, so the console copy matches the dashboard exactly.

use chrono::{DateTime, Utc};

use crate::analytics;
use crate::model::UsageWindow;
use crate::poller::Dashboard;
use crate::ui::rows::{self, BurnTag};
use crate::ui::Run;

/// Label column: fits the longest window label ("Omelette promo").
const LABEL_WIDTH: usize = 14;
/// Burn tag column: fits "FABLE:" plus a space.
const TAG_WIDTH: usize = 7;

/// Concatenate the runs' text, dropping styling.
fn plain(runs: &[Run]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

/// "5-hour          46.0%  resets in 1h 59m", or "Opus            inactive".
pub fn window_line(w: &UsageWindow, now: DateTime<Utc>) -> String {
    let row = rows::window_row(w, now);
    let label = format!("{:<LABEL_WIDTH$}", row.label);
    match w.utilization {
        None => format!("{label}  {}", row.pct.text),
        Some(_) => format!("{label}  {}  resets in {}", row.pct.text, row.reset.text),
    }
}

fn burn_line(tag: BurnTag, line: Option<&analytics::BurnLine>) -> String {
    let tag = rows::burn_tag(tag).text;
    format!("{tag:<TAG_WIDTH$}{}", plain(&rows::burn_body(line)))
}

/// The whole dashboard as plain text, one line per row, ending in a newline.
pub fn render(d: &Dashboard, plan: &str, show_all: bool, now: DateTime<Utc>) -> String {
    let snap = &d.snapshot;
    let order = analytics::ordered_windows(snap);
    let visible = |ws: &[&UsageWindow]| -> Vec<String> {
        ws.iter()
            .filter(|w| analytics::window_visible(w, show_all))
            .map(|w| window_line(w, now))
            .collect()
    };

    let mut out = vec![plain(&rows::header_runs(plan)), String::new()];
    out.extend(visible(&order.primary));
    out.push(String::new());

    let all = analytics::burn_line(
        &d.history,
        snap.five_hour_pct(),
        snap.five_hour_resets_at(),
        now,
    );
    out.push(burn_line(BurnTag::All, all.as_ref()));
    if let Some(fable) = snap.window("Fable") {
        let line = analytics::burn_line(&d.fable_history, fable.utilization, fable.resets_at, now);
        out.push(burn_line(BurnTag::Fable, line.as_ref()));
    }

    let rest = visible(&order.rest);
    if !rest.is_empty() {
        out.push(String::new());
        out.extend(rest);
    }
    if let Some(extra) = analytics::visible_extra(snap) {
        out.push(String::new());
        out.push(plain(&rows::extra_runs(extra)));
    }

    out.push(String::new());
    out.push(plain(&rows::burners_runs(&d.burners)));
    let rate = analytics::compute_rate(&d.history, now);
    if let Some(h) = analytics::headroom(snap, rate, now) {
        out.push(plain(&rows::headroom_runs(&h)));
    }

    let mut text = out.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExtraUsage, Snapshot};
    use crate::storage::InstanceUsage;
    use chrono::{Duration, TimeZone};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap()
    }

    fn window(label: &str, pct: Option<f64>, resets_in_min: Option<i64>) -> UsageWindow {
        UsageWindow {
            label: label.into(),
            utilization: pct,
            resets_at: resets_in_min.map(|m| now() + Duration::minutes(m)),
        }
    }

    fn dashboard(windows: Vec<UsageWindow>) -> Dashboard {
        Dashboard {
            snapshot: Snapshot {
                fetched_at: now(),
                windows,
                extra: None,
            },
            history: Vec::new(),
            fable_history: Vec::new(),
            daily_history: Vec::new(),
            fable_daily: Vec::new(),
            burners: Vec::new(),
            window_start: now() - Duration::hours(5),
            window_end: now(),
            window_samples: Vec::new(),
            persist_error: None,
        }
    }

    #[test]
    fn window_line_active_and_inactive() {
        assert_eq!(
            window_line(&window("5-hour", Some(46.0), Some(119)), now()),
            "5-hour           46.0%  resets in 1h 59m"
        );
        assert_eq!(
            window_line(&window("7-day all", Some(5.0), None), now()),
            "7-day all         5.0%  resets in —"
        );
        assert_eq!(
            window_line(&window("Opus", None, None), now()),
            "Opus            inactive"
        );
    }

    #[test]
    fn render_minimal_dashboard() {
        let d = dashboard(vec![
            window("5-hour", Some(46.0), Some(119)),
            window("7-day all", Some(20.0), Some(3 * 24 * 60 + 60)),
            window("Fable", Some(10.0), Some(3 * 24 * 60 + 60)),
            window("Opus", Some(0.0), None),
        ]);
        let expected = "\
claude.ai usage  ·  Max

5-hour           46.0%  resets in 1h 59m
7-day all        20.0%  resets in 3d 1h
Fable            10.0%  resets in 3d 1h

ALL:   15m burn —, → 100% in — (need 2 recent samples)
FABLE: 15m burn —, → 100% in — (need 2 recent samples)

Top burners (5h):  no local activity yet
Projected use: 0%  ·  idle, safe to spin up agents
";
        assert_eq!(render(&d, "Max", false, now()), expected);
    }

    #[test]
    fn render_full_dashboard_with_show_all() {
        let mut d = dashboard(vec![
            window("5-hour", Some(50.0), Some(60)),
            window("Opus", Some(0.0), None),
        ]);
        d.history = vec![
            (now() - Duration::minutes(10), Some(45.0)),
            (now(), Some(50.0)),
        ];
        d.snapshot.extra = Some(ExtraUsage {
            enabled: false,
            monthly_limit: 50.0,
            used_credits: 12.5,
            utilization: 25.0,
            currency: "USD".into(),
            disabled_reason: Some("out_of_credits".into()),
        });
        d.burners = vec![InstanceUsage {
            name: "proj".into(),
            display_name: "proj".into(),
            source: "claude-code",
            tokens: 12_345,
            last_active: now(),
        }];
        let text = render(&d, "Max 5x", true, now());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "claude.ai usage  ·  Max 5x");
        assert_eq!(lines[2], "5-hour           50.0%  resets in 1h 00m");
        assert_eq!(
            lines[4],
            "ALL:   15m burn +0.50%/min, → 100% in 1h 40m (over last 10m, 2 samples)"
        );
        // No Fable window: no FABLE line. Opus shows only with show_all.
        assert_eq!(lines[6], "Opus              0.0%  resets in —");
        assert_eq!(
            lines[8],
            "Extra usage: $12.50 / $50 USD  (25.0%)  (disabled: out of credits)"
        );
        assert_eq!(lines[10], "Top burners (5h):  proj 12k");
        assert_eq!(
            lines[11],
            "Projected use: 80%  ·  safe to spin up another agent"
        );
        assert_eq!(lines.len(), 12);
        assert!(!render(&d, "Max 5x", false, now()).contains("Opus"));
    }
}
