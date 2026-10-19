//! Pure presentation of the read-only dashboard snapshot.
use super::{AccountCard, Frame as Snapshot, PressureCard, WindowCard};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span, Text},
    widgets::{
        Axis, Block, BorderType, Borders, Cell, Chart, Clear, Dataset, GraphType, LineGauge,
        Padding, Paragraph, Row, Table, Wrap,
    },
    Frame,
};

const BG: Color = Color::Rgb(12, 17, 25);
const PANEL: Color = Color::Rgb(18, 25, 36);
const BORDER: Color = Color::Rgb(48, 62, 81);
const TEXT: Color = Color::Rgb(222, 231, 241);
const MUTED: Color = Color::Rgb(139, 156, 177);
const GREEN: Color = Color::Rgb(105, 214, 171);
const AMBER: Color = Color::Rgb(241, 190, 103);
const RED: Color = Color::Rgb(245, 128, 137);
const ACCENTS: [Color; 3] = [
    Color::Rgb(111, 211, 225),
    Color::Rgb(185, 165, 247),
    Color::Rgb(128, 174, 248),
];

#[derive(Debug, Default)]
pub struct UiState {
    pub selected: usize,
    pub details: bool,
    pub help: bool,
    pub paused: bool,
    pub scroll: u16,
}

pub fn draw(frame: &mut Frame, data: &Snapshot, state: &mut UiState) {
    state.selected = state.selected.min(data.accounts.len().saturating_sub(1));
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    if area.width < 50 || area.height < 14 {
        frame.render_widget(
            Paragraph::new("SUBGOV\nEnlarge terminal to 50 × 14\nq quit · ? help")
                .style(Style::default().fg(MUTED)),
            area,
        );
        return;
    }
    let content = area.inner(Margin {
        horizontal: u16::from(area.width >= 90),
        vertical: 0,
    });
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(content);
    header(frame, rows[0], data, state);
    if data.accounts.is_empty() {
        frame.render_widget(Paragraph::new("No accounts available\n\nAdd a configured account to see cached quota and fleet observations.")
            .block(panel(" WAITING FOR TELEMETRY ", BORDER)).wrap(Wrap { trim: true }).style(Style::default().fg(MUTED)), rows[1]);
    } else if state.details {
        details(frame, rows[1], data, state);
    } else if content.width >= 108 && content.height >= 36 && data.accounts.len() <= 3 {
        overview(frame, rows[1], data, state);
    } else {
        compact(frame, rows[1], data, state);
    }
    footer(frame, rows[2], state, content.width);
    if state.help {
        help(frame, area);
    }
}

fn panel(title: impl Into<Line<'static>>, color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(PANEL).fg(TEXT))
        .title(title)
        .title_style(Style::default().fg(TEXT).add_modifier(Modifier::BOLD))
}

fn bold(value: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        value.into(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn dim(value: impl Into<String>) -> Span<'static> {
    Span::styled(value.into(), Style::default().fg(MUTED))
}

fn primary(account: &AccountCard) -> Option<&WindowCard> {
    account.windows.iter().min_by(|a, b| {
        a.headroom_percentage_points
            .total_cmp(&b.headroom_percentage_points)
    })
}

fn depletion_window(account: &AccountCard) -> Option<&WindowCard> {
    account
        .windows
        .iter()
        .filter(|w| w.fresh && w.projected_empty_at.is_some())
        .min_by_key(|w| {
            (
                w.projected_empty_at.is_some_and(|at| at > w.resets_at),
                w.projected_empty_at,
            )
        })
        .or_else(|| primary(account))
}

fn number(value: Option<u32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
}

fn duration(seconds: i64) -> String {
    if seconds < 0 {
        return "expired".into();
    }
    if seconds >= 86400 {
        format!("{}d {:02}h", seconds / 86400, seconds % 86400 / 3600)
    } else if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn age(now: DateTime<Utc>, observed: Option<DateTime<Utc>>) -> String {
    match observed {
        Some(at) if at > now => "future timestamp".into(),
        Some(at) => format!("{} ago", duration((now - at).num_seconds())),
        None => "unknown age".into(),
    }
}

fn health(account: &AccountCard) -> (&'static str, Color) {
    if account.windows.is_empty() {
        return ("UNKNOWN", MUTED);
    }
    if account.windows.iter().any(|w| !w.fresh) {
        return ("STALE", AMBER);
    }
    let state = account.readiness.to_ascii_lowercase();
    let reason = account.reason.to_ascii_lowercase();
    if ["hold", "block", "pause", "degrad", "exhaust", "error"]
        .iter()
        .any(|s| state.contains(s) || reason.contains(s))
    {
        ("HOLD", AMBER)
    } else if state == "unknown" {
        ("UNKNOWN", MUTED)
    } else {
        ("FRESH", GREEN)
    }
}

fn human(value: &str) -> String {
    value.replace('_', " ").replace("::", " / ")
}

fn provider_name(account: &str) -> &str {
    match account {
        "claude-anthropic" => "Anthropic",
        "claude-zai" => "Z.ai",
        "codex" => "Codex",
        other => other,
    }
}

fn header(frame: &mut Frame, area: Rect, data: &Snapshot, state: &UiState) {
    let columns =
        Layout::horizontal([Constraint::Percentage(47), Constraint::Percentage(53)]).split(area);
    let mut title = vec![bold(" SUBGOV", TEXT), dim("  /  quota intelligence")];
    if state.paused {
        title.push(bold("  PAUSED", AMBER));
    }
    let all_live = data.accounts.iter().all(|a| a.live_workers.is_some());
    let all_target = data.accounts.iter().all(|a| a.target_workers.is_some());
    let live: u32 = data
        .accounts
        .iter()
        .filter_map(|a| a.live_workers.as_ref())
        .map(|a| a.live)
        .sum();
    let active: u32 = data
        .accounts
        .iter()
        .filter_map(|a| a.live_workers.as_ref())
        .map(|a| a.executing)
        .sum();
    let idle: u32 = data
        .accounts
        .iter()
        .filter_map(|a| a.live_workers.as_ref())
        .map(|a| a.idle)
        .sum();
    let target: u32 = data.accounts.iter().filter_map(|a| a.target_workers).sum();
    let partial = if all_live { "" } else { "+?" };
    let target_partial = if all_target { "" } else { "+?" };
    if area.width < 108 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    bold(" SUBGOV", TEXT),
                    dim(format!("  {} UTC", data.observed_at.format("%H:%M:%S"))),
                    bold(if state.paused { "  PAUSED" } else { "" }, AMBER),
                ]),
                Line::from(vec![
                    bold(format!(" {live}{partial}"), TEXT),
                    dim(" live   "),
                    bold(format!("{active}{partial}"), GREEN),
                    dim(" active   "),
                    bold(format!("{idle}{partial}"), TEXT),
                    dim(" idle   "),
                    bold(format!("{target}{target_partial}"), TEXT),
                    dim(" target"),
                ]),
            ]),
            area,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(title),
            Line::from(dim(format!(
                " {} UTC  ·  cached observations",
                data.observed_at.format("%H:%M:%S")
            ))),
        ]),
        columns[0],
    );
    let fleet = Line::from(vec![
        bold(format!("{live}{partial}"), TEXT),
        dim(" LIVE   "),
        bold(format!("{active}{partial}"), GREEN),
        dim(" ACTIVE   "),
        bold(format!("{idle}{partial}"), MUTED),
        dim(" IDLE   "),
        bold(format!("{target}{target_partial}"), TEXT),
        dim(" TARGET "),
    ]);
    frame.render_widget(
        Paragraph::new(vec![
            fleet,
            Line::from(dim("verified local agents · target is controller intent ")),
        ])
        .alignment(Alignment::Right),
        columns[1],
    );
}

fn overview(frame: &mut Frame, area: Rect, data: &Snapshot, state: &UiState) {
    let rows = Layout::vertical([
        Constraint::Length(16),
        Constraint::Min(9),
        Constraint::Length(7),
    ])
    .spacing(1)
    .split(area);
    let count = data.accounts.len();
    let columns = Layout::horizontal(vec![Constraint::Ratio(1, count as u32); count])
        .spacing(1)
        .split(rows[0]);
    for (offset, card_area) in columns.iter().enumerate() {
        if let Some(card) = data.accounts.get(offset) {
            provider_card(
                frame,
                *card_area,
                card,
                offset,
                state.selected == offset,
                data.observed_at,
            );
        }
    }
    let middle = Layout::horizontal(vec![Constraint::Ratio(1, count as u32); count])
        .spacing(1)
        .split(rows[1]);
    for (index, account) in data.accounts.iter().enumerate() {
        if account.pressure.is_some() {
            pressure_panel(frame, middle[index], account, data.observed_at);
        } else {
            history(
                frame,
                middle[index],
                account,
                data.observed_at,
                ACCENTS[index % 3],
            );
        }
    }
    fleet_table(frame, rows[2], data, state.selected);
}

fn provider_card(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    index: usize,
    selected: bool,
    now: DateTime<Utc>,
) {
    if account.pressure.is_some() {
        pressure_card(frame, area, account, index, selected, now);
        return;
    }
    let accent = ACCENTS[index % 3];
    let (status, status_color) = health(account);
    let title = format!(
        " {} {}{} ",
        index + 1,
        account.account.to_uppercase(),
        if selected { "  ◆" } else { "" }
    );
    let block = panel(title, if selected { accent } else { BORDER });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(window) = depletion_window(account) else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(bold(status, status_color)),
                Line::from(""),
                Line::from(bold("—", MUTED)),
                Line::from(dim("No usable quota observation")),
                Line::from(""),
                Line::from(human(&account.cache_status)),
            ])
            .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };
    let lines = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let quota_status = if window.fresh { "FRESH" } else { "STALE" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            bold(
                format!(
                    "● {quota_status}{}",
                    if status == "HOLD" { " · HOLD" } else { "" }
                ),
                status_color,
            ),
            dim(format!("  {}", age(now, Some(window.observed_at)))),
        ])),
        lines[0],
    );
    frame.render_widget(Paragraph::new(dim("QUOTA REMAINING")), lines[1]);
    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    frame.render_widget(
        Paragraph::new(large_percent(
            remaining,
            if window.fresh { accent } else { MUTED },
        )),
        lines[2],
    );
    frame.render_widget(
        LineGauge::default()
            .filled_symbol("━")
            .unfilled_symbol("━")
            .filled_style(
                Style::default().fg(if window.used_percent > window.target_percent {
                    RED
                } else {
                    accent
                }),
            )
            .unfilled_style(Style::default().fg(BORDER))
            .ratio((window.used_percent / 100.0).clamp(0.0, 1.0))
            .label(""),
        lines[4],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            bold(format!("{:.1}%", window.used_percent), TEXT),
            dim(format!(" used  /  {:.0}% target", window.target_percent)),
        ])),
        lines[5],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![dim("Pace   "), bold(rate(window), TEXT)])),
        lines[6],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            dim("Reset  "),
            bold(duration((window.resets_at - now).num_seconds()), TEXT),
        ])),
        lines[7],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            dim("EMPTY  "),
            bold(
                empty_estimate(window, now),
                if window
                    .projected_empty_at
                    .is_some_and(|at| at <= window.resets_at)
                {
                    AMBER
                } else {
                    TEXT
                },
            ),
        ])),
        lines[8],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            bold(
                account
                    .live_workers
                    .as_ref()
                    .map(|l| l.live.to_string())
                    .unwrap_or_else(|| "?".into()),
                TEXT,
            ),
            dim(" live · "),
            bold(
                account
                    .live_workers
                    .as_ref()
                    .map(|l| l.executing.to_string())
                    .unwrap_or_else(|| "?".into()),
                GREEN,
            ),
            dim(format!(" busy · {} target", number(account.target_workers))),
        ])),
        lines[9],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(dim(format!("At reset  {}", forecast(window)))),
            Line::from(dim(format!("↳ {} · forecast window", window.id))),
        ]),
        lines[10],
    );
}

fn rate(window: &WindowCard) -> String {
    if !window.fresh {
        return "unknown".into();
    }
    window
        .consumption_percentage_points_per_hour
        .map(|n| format!("{n:.2} pp/h"))
        .unwrap_or_else(|| "unknown".into())
}

fn forecast(window: &WindowCard) -> String {
    if !window.fresh {
        return "unknown".into();
    }
    window
        .expected_remaining_percent_at_reset
        .map(|n| format!("{n:.1}% left"))
        .unwrap_or_else(|| "unknown".into())
}

fn empty_estimate(window: &WindowCard, now: DateTime<Utc>) -> String {
    if !window.fresh {
        return "unknown".into();
    }
    let Some(at) = window.projected_empty_at else {
        return "unknown".into();
    };
    let until = (at - now).num_seconds();
    let estimate = if until <= 0 {
        "due at measured pace".into()
    } else {
        format!("~{}", duration(until))
    };
    if at > window.resets_at {
        format!("{estimate} · reset first")
    } else {
        estimate
    }
}

fn eligible_pressure(pressure: &PressureCard, attempts: u64, seconds: u64) -> bool {
    pressure.fresh
        && pressure.observed_at.is_some()
        && seconds > 0
        && attempts >= pressure.minimum_attempts
        && seconds >= pressure.minimum_window_seconds
}

fn pressure_fraction(pressure: &PressureCard, concurrency: bool) -> Option<f64> {
    let value = if concurrency {
        pressure.saturation.as_ref().and_then(|s| {
            s.upstream_concurrency_fraction
                .filter(|_| eligible_pressure(pressure, s.attempts, s.window_seconds))
        })
    } else {
        pressure.backpressure.as_ref().and_then(|s| {
            s.upstream_429_fraction
                .filter(|_| eligible_pressure(pressure, s.attempts, s.window_seconds))
        })
    };
    value.filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
}

fn pressure_throughput(pressure: &PressureCard) -> String {
    pressure
        .saturation
        .as_ref()
        .filter(|s| {
            pressure.fresh
                && pressure.observed_at.is_some()
                && s.window_seconds > 0
                && s.completed_requests <= s.attempts
        })
        .map(|s| {
            format!(
                "{:.3} req/s",
                s.completed_requests as f64 / s.window_seconds as f64
            )
        })
        .unwrap_or_else(|| "unknown req/s".into())
}

fn percentage(value: Option<f64>) -> String {
    value
        .map(|n| format!("{:.1}%", n * 100.0))
        .unwrap_or_else(|| "unknown".into())
}

fn pressure_status(pressure: &PressureCard) -> (&'static str, Color) {
    if pressure.observed_at.is_none() {
        ("UNKNOWN", MUTED)
    } else if !pressure.fresh {
        ("STALE", AMBER)
    } else {
        ("FRESH", GREEN)
    }
}

fn pressure_sample(pressure: &PressureCard, concurrency: bool) -> String {
    let sample = if concurrency {
        pressure
            .saturation
            .as_ref()
            .map(|s| (s.attempts, s.window_seconds))
    } else {
        pressure
            .backpressure
            .as_ref()
            .map(|s| (s.attempts, s.window_seconds))
    };
    sample
        .map(|(n, seconds)| format!("{n}/{seconds}s"))
        .unwrap_or_else(|| "unknown".into())
}

fn pressure_unknown_reason(pressure: &PressureCard, concurrency: bool) -> String {
    if pressure.observed_at.is_none() {
        return "source age unknown".into();
    }
    if !pressure.fresh {
        return if pressure.observed_at.is_some() {
            "stale source"
        } else {
            "source age unknown"
        }
        .into();
    }
    let sample = if concurrency {
        pressure
            .saturation
            .as_ref()
            .map(|s| (s.attempts, s.window_seconds))
    } else {
        pressure
            .backpressure
            .as_ref()
            .map(|s| (s.attempts, s.window_seconds))
    };
    let Some((attempts, seconds)) = sample else {
        return "no cached sample".into();
    };
    if attempts < pressure.minimum_attempts {
        return format!("{attempts}/{} minimum attempts", pressure.minimum_attempts);
    }
    if seconds < pressure.minimum_window_seconds || seconds == 0 {
        return format!(
            "{seconds}/{}s minimum interval",
            pressure.minimum_window_seconds
        );
    }
    "fraction not reported".into()
}

fn worker_summary(account: &AccountCard) -> Line<'static> {
    Line::from(vec![
        bold(number(account.live_workers.as_ref().map(|l| l.live)), TEXT),
        dim(" live · "),
        bold(
            number(account.live_workers.as_ref().map(|l| l.executing)),
            GREEN,
        ),
        dim(" busy · "),
        bold(number(account.target_workers), TEXT),
        dim(" target"),
    ])
}

fn pressure_card(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    index: usize,
    selected: bool,
    now: DateTime<Utc>,
) {
    let Some(pressure) = account.pressure.as_ref() else {
        return;
    };
    let accent = ACCENTS[index % 3];
    let title = format!(
        " {} {}{} ",
        index + 1,
        provider_name(&account.account).to_uppercase(),
        if selected { "  ◆" } else { "" }
    );
    let block = panel(title, if selected { accent } else { BORDER });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    let (status, color) = pressure_status(pressure);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            bold(format!("● {status}"), color),
            dim(format!("  {}", age(now, pressure.observed_at))),
        ])),
        rows[0],
    );
    frame.render_widget(Paragraph::new(dim("UPSTREAM 429 RATE")), rows[1]);
    let error_rate = pressure_fraction(pressure, false);
    let large = error_rate
        .map(|n| large_percent(n * 100.0, if n > 0.0 { AMBER } else { accent }))
        .unwrap_or_else(|| {
            Text::from(vec![
                Line::from(""),
                Line::from(bold("UNKNOWN", MUTED)),
                Line::from(""),
            ])
        });
    frame.render_widget(Paragraph::new(large), rows[2]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            dim("1302 concurrency  "),
            bold(percentage(pressure_fraction(pressure, true)), TEXT),
        ])),
        rows[4],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            dim("Completed  "),
            bold(pressure_throughput(pressure), TEXT),
        ])),
        rows[5],
    );
    let note = if error_rate.is_none() {
        format!("429: {}", pressure_unknown_reason(pressure, false))
    } else if pressure_fraction(pressure, true).is_none() {
        format!("1302: {}", pressure_unknown_reason(pressure, true))
    } else {
        "Full-proxy request intervals".into()
    };
    frame.render_widget(Paragraph::new(dim(note)), rows[6]);
    frame.render_widget(
        Paragraph::new(dim(format!(
            "429 attempts / window   {}",
            pressure_sample(pressure, false)
        ))),
        rows[7],
    );
    frame.render_widget(
        Paragraph::new(dim(format!(
            "1302 attempts / window  {}",
            pressure_sample(pressure, true)
        ))),
        rows[8],
    );
    frame.render_widget(Paragraph::new(worker_summary(account)), rows[9]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(dim(format!(
                "Gate ≥{} attempts / ≥{}s",
                pressure.minimum_attempts, pressure.minimum_window_seconds
            ))),
            Line::from(dim("Rolling quota · no empty forecast")),
        ]),
        rows[10],
    );
}

fn pressure_lines(pressure: &PressureCard, now: DateTime<Utc>) -> Vec<Line<'static>> {
    let (status, color) = pressure_status(pressure);
    let concurrency = pressure_fraction(pressure, true);
    let error_rate = pressure_fraction(pressure, false);
    let mut lines = vec![
        Line::from(vec![
            bold(format!("● {status}"), color),
            dim(format!(" · {}", age(now, pressure.observed_at))),
        ]),
        Line::from(""),
        Line::from(vec![
            dim("Upstream 429  "),
            bold(
                percentage(error_rate),
                if error_rate.is_some_and(|r| r > 0.0) {
                    AMBER
                } else {
                    TEXT
                },
            ),
        ]),
        Line::from(dim(if error_rate.is_none() {
            pressure_unknown_reason(pressure, false)
        } else {
            format!("{} attempts / interval", pressure_sample(pressure, false))
        })),
        Line::from(""),
        Line::from(vec![
            dim("1302 concurrency  "),
            bold(
                percentage(concurrency),
                if concurrency.is_some_and(|r| r >= pressure.concurrency_threshold) {
                    AMBER
                } else {
                    TEXT
                },
            ),
        ]),
        Line::from(dim(if concurrency.is_none() {
            pressure_unknown_reason(pressure, true)
        } else {
            format!("{} attempts / interval", pressure_sample(pressure, true))
        })),
        Line::from(dim(format!(
            "Controller threshold {:.1}%",
            pressure.concurrency_threshold * 100.0
        ))),
        Line::from(""),
        Line::from(vec![
            dim("Completed  "),
            bold(pressure_throughput(pressure), TEXT),
        ]),
    ];
    if let Some(sample) = &pressure.saturation {
        lines.push(Line::from(dim(format!(
            "{} completed / {}s",
            sample.completed_requests, sample.window_seconds
        ))));
    }
    lines.extend([
        Line::from(""),
        Line::from(bold("MINIMUM EVIDENCE", TEXT)),
        Line::from(dim(format!(
            "≥{} attempts · ≥{}s interval",
            pressure.minimum_attempts, pressure.minimum_window_seconds
        ))),
        Line::from(dim("Unknown = missing, stale or below gate.")),
        Line::from(""),
        Line::from(dim("1302 = concurrency rejections.")),
        Line::from(dim("429 = upstream rate limiting.")),
        Line::from(dim("Request rates ≠ live agents.")),
    ]);
    lines
}

fn pressure_panel(frame: &mut Frame, area: Rect, account: &AccountCard, now: DateTime<Utc>) {
    if let Some(pressure) = &account.pressure {
        frame.render_widget(
            Paragraph::new(pressure_lines(pressure, now))
                .block(panel(" ZAI / REQUEST PRESSURE ", BORDER))
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

fn fleet_table(frame: &mut Frame, area: Rect, data: &Snapshot, selected: usize) {
    let header = Row::new([
        "GOVERNOR",
        "LIVE",
        "ACTIVE",
        "IDLE",
        "TARGET",
        "CACHED CONTROLLER DECISION",
    ])
    .style(Style::default().fg(MUTED));
    let rows = data.accounts.iter().enumerate().map(|(index, a)| {
        Row::new(vec![
            Cell::from(format!(
                "{}{}",
                if index == selected { "◆ " } else { "  " },
                provider_name(&a.account)
            )),
            Cell::from(number(a.live_workers.as_ref().map(|l| l.live))),
            Cell::from(number(a.live_workers.as_ref().map(|l| l.executing)))
                .style(Style::default().fg(GREEN)),
            Cell::from(number(a.live_workers.as_ref().map(|l| l.idle))),
            Cell::from(number(a.target_workers)),
            Cell::from(human(&a.reason)),
        ])
        .style(Style::default().fg(if index == selected { TEXT } else { MUTED }))
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(21),
                Constraint::Percentage(7),
                Constraint::Percentage(8),
                Constraint::Percentage(7),
                Constraint::Percentage(8),
                Constraint::Percentage(49),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(panel(
            " ALL GOVERNORS / FLEET · Enter policy, holds & quota details ",
            BORDER,
        )),
        area,
    );
}

fn large_percent(value: f64, color: Color) -> Text<'static> {
    // A compact three-row numeral alphabet keeps the primary metric readable at a glance.
    const DIGITS: [[&str; 3]; 10] = [
        ["█▀█", "█ █", "█▄█"],
        [" ▀█", "  █", "  █"],
        ["▀▀█", "▄▀▀", "█▄▄"],
        ["▀▀█", " ▀█", "▄▄█"],
        ["█ █", "▀▀█", "  █"],
        ["█▀▀", "▀▀█", "▄▄█"],
        ["█▀▀", "█▀█", "█▄█"],
        ["▀▀█", "  █", "  █"],
        ["█▀█", "█▀█", "█▄█"],
        ["█▀█", "▀▀█", "▄▄█"],
    ];
    let value = format!("{value:.1}");
    let rows = (0..3)
        .map(|row| {
            let mut output = String::new();
            for digit in value.chars() {
                if let Some(n) = digit.to_digit(10) {
                    output.push_str(DIGITS[n as usize][row]);
                } else {
                    output.push_str(if row == 2 { "▪" } else { " " });
                }
                output.push(' ');
            }
            output.push_str(if row == 2 { "%" } else { " " });
            Line::from(bold(output, color))
        })
        .collect::<Vec<_>>();
    Text::from(rows)
}

/// A saturated forecast reaches 100 at the measured exhaustion time, never at an
/// arbitrary reset endpoint. A flat cap then describes the exhausted quota.
fn projection(window: &WindowCard, origin: DateTime<Utc>) -> Vec<(f64, f64)> {
    let Some(rate) = window
        .consumption_percentage_points_per_hour
        .filter(|r| r.is_finite() && *r > 0.0)
    else {
        return Vec::new();
    };
    if !window.fresh || window.expected_remaining_percent_at_reset.is_none() {
        return Vec::new();
    }
    let start = (window.observed_at - origin).num_seconds() as f64 / 3600.0;
    let end = (window.resets_at - origin).num_seconds() as f64 / 3600.0;
    if end <= start {
        return Vec::new();
    }
    let used = window.used_percent.clamp(0.0, 100.0);
    let exhaustion = start + (100.0 - used) / rate;
    if exhaustion < end {
        vec![(start, used), (exhaustion, 100.0), (end, 100.0)]
    } else {
        vec![
            (start, used),
            (end, (used + rate * (end - start)).min(100.0)),
        ]
    }
}

fn history(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    now: DateTime<Utc>,
    accent: Color,
) {
    let Some(window) = depletion_window(account) else {
        frame.render_widget(
            Paragraph::new("No saved quota history available")
                .block(panel(" QUOTA TRAJECTORY ", BORDER)),
            area,
        );
        return;
    };
    let block = panel(
        format!(" {} / quota trajectory ", provider_name(&account.account)),
        BORDER,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let narrow = inner.width < 55;
    let rows = Layout::vertical([
        Constraint::Length(if narrow { 2 } else { 1 }),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(if narrow {
            vec![
                Line::from(vec![
                    Span::styled("━ saved", Style::default().fg(accent)),
                    dim("  "),
                    Span::styled("━ projected", Style::default().fg(AMBER)),
                ]),
                Line::from(dim(format!(
                    "─ {:.0}% target · sample = 0h",
                    window.target_percent
                ))),
            ]
        } else {
            vec![Line::from(vec![
                Span::styled("━ saved", Style::default().fg(accent)),
                dim("   "),
                Span::styled("━ projection", Style::default().fg(AMBER)),
                dim(format!(
                    "   ─ {:.0}% target · sample = 0h",
                    window.target_percent
                )),
            ])]
        }),
        rows[0],
    );
    let origin = window.observed_at;
    let points: Vec<_> = window
        .history
        .iter()
        .filter(|p| p.observed_at <= window.observed_at && p.used_percent.is_finite())
        .map(|p| {
            (
                (p.observed_at - origin).num_seconds() as f64 / 3600.0,
                p.used_percent.clamp(0.0, 100.0),
            )
        })
        .collect();
    let mut measured = points;
    let latest = (0.0, window.used_percent.clamp(0.0, 100.0));
    if measured.last() != Some(&latest) {
        measured.push(latest);
    }
    let future = projection(window, origin);
    let xmin = measured.first().map(|p| p.0).unwrap_or(-1.0).min(-0.05);
    let xmax = future.last().map(|p| p.0).unwrap_or(0.05).max(0.05);
    let target = vec![(xmin, window.target_percent), (xmax, window.target_percent)];
    let today = vec![(0.0, 0.0), (0.0, 100.0)];
    let datasets = vec![
        Dataset::default()
            .data(&target)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Dot)
            .style(Style::default().fg(BORDER)),
        Dataset::default()
            .data(&today)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Dot)
            .style(Style::default().fg(BORDER)),
        Dataset::default()
            .data(&measured)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(accent)),
        Dataset::default()
            .data(&future)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(AMBER)),
    ];
    let right = if future.is_empty() {
        "sample".to_owned()
    } else {
        format!(
            "+{} reset",
            duration((window.resets_at - origin).num_seconds())
        )
    };
    let left = if xmin <= -0.05 {
        format!("−{}", duration((-xmin * 3600.0) as i64))
    } else {
        "history".into()
    };
    let midpoint = (xmin + xmax) / 2.0;
    let middle = if midpoint.abs() < 0.01 {
        "sample".into()
    } else {
        format!(
            "{}{}",
            if midpoint < 0.0 { "−" } else { "+" },
            duration((midpoint.abs() * 3600.0) as i64)
        )
    };
    let xlabels = if narrow {
        vec![
            Line::from(format!("{xmin:.1}h")),
            Line::from(if future.is_empty() {
                "sample".into()
            } else {
                format!("{xmax:.1}h reset")
            }),
        ]
    } else {
        vec![Line::from(left), Line::from(middle), Line::from(right)]
    };
    let chart = Chart::new(datasets)
        .legend_position(None)
        .x_axis(
            Axis::default()
                .bounds([xmin, xmax])
                .labels(xlabels)
                .style(Style::default().fg(BORDER))
                .labels_alignment(Alignment::Center),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, 100.0])
                .labels(["0%", "50%", "100%"])
                .style(Style::default().fg(MUTED)),
        )
        .style(Style::default().bg(PANEL));
    frame.render_widget(chart, rows[1]);
    let note = if !window.fresh {
        format!(
            "STALE observation · {} · forecast unavailable",
            age(now, Some(window.observed_at))
        )
    } else if future.is_empty() {
        if window.projection_assumption.contains("unknown") {
            human(&window.projection_assumption)
        } else {
            format!("Forecast unavailable · {}", human(&window.rate_status))
        }
    } else if narrow {
        format!(
            "{} saved · {:.2} pp/h\nSame pace · stops at reset",
            window.history.len(),
            window
                .consumption_percentage_points_per_hour
                .unwrap_or_default()
        )
    } else {
        format!(
            "{} saved points · {:.2} pp/h · constant historical pace, includes interactive usage",
            window.history.len(),
            window
                .consumption_percentage_points_per_hour
                .unwrap_or_default()
        )
    };
    frame.render_widget(
        Paragraph::new(note)
            .style(Style::default().fg(MUTED))
            .wrap(Wrap { trim: true }),
        rows[2],
    );
}

fn controller_lines(account: &AccountCard, now: DateTime<Utc>, full: bool) -> Vec<Line<'static>> {
    let (status, color) = health(account);
    let mut lines = vec![
        Line::from(vec![
            bold(format!("● {status}"), color),
            dim(format!("  {}", human(&account.readiness))),
        ]),
        Line::from(""),
    ];
    lines.push(Line::from(bold(human(&account.reason), TEXT)));
    lines.push(Line::from(""));
    if let Some(live) = &account.live_workers {
        lines.push(Line::from(vec![
            bold(live.live.to_string(), TEXT),
            dim(" live  "),
            bold(live.executing.to_string(), GREEN),
            dim(" active  "),
            bold(live.idle.to_string(), TEXT),
            dim(" idle"),
        ]));
        lines.push(Line::from(dim(format!(
            "{} other · {} stale/unverified excluded",
            live.other, live.stale_or_unverified
        ))));
    } else {
        lines.push(Line::from(dim("Local agents: unknown")));
    }
    lines.push(Line::from(vec![
        dim("Controller "),
        bold(number(account.controller_observed_workers), TEXT),
        dim(" observed → "),
        bold(number(account.target_workers), TEXT),
        dim(" target"),
    ]));
    lines.push(Line::from(dim(format!(
        "Controller: {}",
        age(now, account.controller_observed_at)
    ))));
    lines.push(Line::from(""));
    let mut signals = account.cached_signals.iter().collect::<Vec<_>>();
    signals.sort_by_key(|signal| {
        if signal.starts_with("Policy:") {
            0
        } else if signal.to_ascii_lowercase().contains("hold")
            && !signal.starts_with("Banked policy:")
        {
            1
        } else {
            2
        }
    });
    for signal in signals {
        // The full source text is available in details. Overview keeps one copy
        // of each real policy/hold signal and lets normal wrapping preserve it.
        if full {
            lines.push(Line::from(dim(human(signal))));
        } else {
            lines.extend(signal_summary(signal));
        }
        if full {
            lines.push(Line::from(""));
        }
    }
    lines
}

/// Summaries only rearrange values present in the cached signal. Unknown source
/// formats remain visible verbatim; details always retain the complete source.
fn signal_summary(signal: &str) -> Vec<Line<'static>> {
    let pair = |label: &str, value: &str, color: Color| {
        Line::from(vec![
            dim(format!("{label}  ")),
            Span::styled(human(value), Style::default().fg(color)),
        ])
    };
    let without_cache = |s: &str| {
        s.trim()
            .trim_end_matches(" (cached)")
            .trim_end_matches(" (cached demand)")
            .to_owned()
    };
    if let Some(policy) = signal.strip_prefix("Policy: ") {
        let parts = policy.split(" | ").collect::<Vec<_>>();
        let mut result = vec![pair("Policy", parts[0], TEXT)];
        let bounds = parts.iter().find_map(|p| p.strip_prefix("agents "));
        let step = parts.iter().find_map(|p| p.strip_prefix("step "));
        if let (Some(bounds), Some(step)) = (bounds, step) {
            result.push(pair("Agents", &format!("{bounds} · step {step}"), TEXT));
        }
        return result;
    }
    if let Some(holds) = signal.strip_prefix("Reviewed work holds: ") {
        return vec![pair(
            "Work holds",
            &without_cache(holds).replace("native-policy", "native"),
            AMBER,
        )];
    }
    if let Some(retiring) = signal.strip_prefix("Pending retirements: ") {
        return vec![pair("Retiring", &without_cache(retiring), TEXT)];
    }
    if let Some(demand) = signal.strip_prefix("Eligible backlog: ") {
        return vec![pair("Demand", &without_cache(demand), TEXT)];
    }
    if let Some((_, credits)) = signal.split_once("banked credits: ") {
        if let Some((count, deadline)) = credits.split_once("; earliest expiry: ") {
            let expires = DateTime::parse_from_rfc3339(deadline)
                .map(|d| d.with_timezone(&Utc).format("%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|_| deadline.to_owned());
            return vec![
                pair("Credits", count, TEXT),
                pair("Expiry", &expires, MUTED),
            ];
        }
    }
    if let Some(policy) = signal.strip_prefix("Banked policy: ") {
        let parts = policy.split(" | ").collect::<Vec<_>>();
        let mode = match parts[0] {
            "full speed capped by demand" => "full speed / demand capped",
            other => other,
        };
        let mut result = vec![pair("Banked", mode, TEXT)];
        for part in parts.iter().skip(1) {
            if let Some(capacity) = part.strip_prefix("eligible capacity ") {
                result.push(pair("Eligible cap", capacity, TEXT));
            }
            if let Some(hold) = part.strip_prefix("redemption hold ") {
                result.push(pair(
                    "Redemption",
                    &without_cache(hold),
                    if hold == "not observed" { MUTED } else { AMBER },
                ));
            }
        }
        return result;
    }
    if let Some((_, throughput)) = signal.split_once("throughput: ") {
        if let Some((counts, tail)) = throughput.split_once(" (") {
            if let Some((rate, _)) = tail.split_once(')') {
                return vec![
                    pair("Throughput", rate, TEXT),
                    pair("Completed", counts, MUTED),
                ];
            }
        }
    }
    if let Some((_, backpressure)) = signal.split_once("upstream 429: ") {
        return vec![pair("Upstream 429", backpressure, AMBER)];
    }
    if let Some(backpressure) = signal.strip_prefix("controller backpressure ") {
        if let Some((mode, rest)) = backpressure.split_once("; saturation workers ") {
            return vec![
                pair("Backpressure", mode, TEXT),
                pair(
                    "Sat. agents",
                    rest.split(" at ").next().unwrap_or("unknown"),
                    MUTED,
                ),
            ];
        }
    }
    vec![Line::from(dim(human(signal)))]
}

fn controller(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    now: DateTime<Utc>,
    full: bool,
    scroll: u16,
) {
    frame.render_widget(
        Paragraph::new(controller_lines(account, now, full))
            .block(panel(" CONTROLLER / CACHED DECISION ", BORDER))
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        area,
    );
}

fn quota_table(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    now: DateTime<Utc>,
    full: bool,
) {
    let selected_id = primary(account).map(|w| w.id.as_str());
    let header = Row::new([
        "QUOTA WINDOW",
        "USED",
        "TARGET",
        "PACE / H",
        "RESET IN",
        "LEFT @ RESET",
        "SAMPLE AGE",
    ])
    .style(Style::default().fg(MUTED))
    .bottom_margin(1);
    let rows = account.windows.iter().map(|w| {
        let selected = Some(w.id.as_str()) == selected_id;
        let label = format!("{}{}", if selected { "◆ " } else { "  " }, w.id);
        Row::new(vec![
            Cell::from(label),
            Cell::from(format!("{:.1}%", w.used_percent)),
            Cell::from(format!("{:.0}%", w.target_percent)),
            Cell::from(rate(w)),
            Cell::from(duration((w.resets_at - now).num_seconds())),
            Cell::from(forecast(w)),
            Cell::from(format!(
                "{}{}",
                if w.fresh { "" } else { "STALE " },
                age(now, Some(w.observed_at))
            )),
        ])
        .style(Style::default().fg(if !w.fresh {
            AMBER
        } else if selected {
            TEXT
        } else {
            MUTED
        }))
    });
    let title = if full {
        " ALL QUOTA WINDOWS · ◆ limiting headroom "
    } else {
        " QUOTA WINDOWS · ◆ limiting headroom · Enter details "
    };
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(23),
                Constraint::Percentage(8),
                Constraint::Percentage(8),
                Constraint::Percentage(13),
                Constraint::Percentage(13),
                Constraint::Percentage(15),
                Constraint::Percentage(20),
            ],
        )
        .header(header)
        .block(panel(title, BORDER))
        .column_spacing(1),
        area,
    );
}

fn all_governor_lines(
    data: &Snapshot,
    state: &UiState,
    width: u16,
    condensed: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, account) in data.accounts.iter().enumerate() {
        if index > 0 && !condensed {
            lines.push(Line::from(""));
        }
        let name = format!(
            "{}{}  ",
            if index == state.selected {
                "› "
            } else {
                "  "
            },
            provider_name(&account.account)
        );
        let accent = ACCENTS[index % 3];
        if let Some(pressure) = &account.pressure {
            let mut first = vec![
                bold(name, accent),
                dim("429 "),
                bold(percentage(pressure_fraction(pressure, false)), TEXT),
                dim(" · 1302 "),
                bold(percentage(pressure_fraction(pressure, true)), TEXT),
            ];
            if condensed {
                first.extend([dim(" · "), dim(pressure_throughput(pressure))]);
            }
            lines.push(Line::from(first));
            if !condensed {
                let (status, color) = pressure_status(pressure);
                lines.push(Line::from(vec![
                    dim("  "),
                    bold(pressure_throughput(pressure), TEXT),
                    dim(format!(
                        " · {}→{} agents · ",
                        number(account.live_workers.as_ref().map(|l| l.live)),
                        number(account.target_workers)
                    )),
                    bold(status, color),
                ]));
                lines.push(Line::from(dim(format!(
                    "  attempts 429 {} · 1302 {} · min {}/{}s",
                    pressure_sample(pressure, false),
                    pressure_sample(pressure, true),
                    pressure.minimum_attempts,
                    pressure.minimum_window_seconds
                ))));
            }
        } else if let Some(window) = depletion_window(account) {
            let mut first = vec![
                bold(name, accent),
                bold(
                    format!("{:.1}%", (100.0 - window.used_percent).clamp(0.0, 100.0)),
                    TEXT,
                ),
                dim(" left · "),
                bold(rate(window), TEXT),
            ];
            if condensed {
                first.extend([
                    dim(" · EMPTY "),
                    bold(
                        empty_estimate(window, data.observed_at),
                        if window.fresh { TEXT } else { AMBER },
                    ),
                ]);
            } else {
                first.push(dim(format!(
                    " · {}→{} agents",
                    number(account.live_workers.as_ref().map(|l| l.live)),
                    number(account.target_workers)
                )));
            }
            lines.push(Line::from(first));
            if !condensed {
                lines.push(Line::from(vec![
                    dim("  EMPTY "),
                    bold(
                        empty_estimate(window, data.observed_at),
                        if window.fresh { TEXT } else { AMBER },
                    ),
                    dim(format!(
                        " · reset {}",
                        duration((window.resets_at - data.observed_at).num_seconds())
                    )),
                ]));
                if width >= 70 {
                    lines.push(Line::from(dim(format!(
                        "  At reset {} · sample {}{}",
                        forecast(window),
                        age(data.observed_at, Some(window.observed_at)),
                        if window.fresh { "" } else { " · STALE" }
                    ))));
                }
            }
        } else {
            lines.push(Line::from(vec![
                bold(name, accent),
                dim("quota unknown · no usable cached observation"),
            ]));
            if !condensed {
                lines.push(Line::from(dim(format!(
                    "  {}",
                    human(&account.cache_status)
                ))));
            }
        }
    }
    lines
}

fn compact(frame: &mut Frame, area: Rect, data: &Snapshot, state: &UiState) {
    let full = all_governor_lines(data, state, area.width, false);
    let needed = full.len().saturating_add(2);
    let (mut lines, condensed) = if needed <= area.height as usize {
        (full, false)
    } else {
        (all_governor_lines(data, state, area.width, true), true)
    };
    if condensed {
        let capacity = area.height.saturating_sub(4) as usize;
        let total = lines.len();
        lines.truncate(capacity);
        lines.push(Line::from(bold(
            format!(
                "Enlarge: {} governors need {} rows for full metrics",
                data.accounts.len(),
                needed + 5
            ),
            AMBER,
        )));
        if capacity < total {
            lines.push(Line::from(dim(format!(
                "{} of {} governors fit this terminal",
                capacity, total
            ))));
        }
    }
    let height = (lines.len().saturating_add(2) as u16).min(area.height);
    let summary = Paragraph::new(lines).block(panel(
        format!(
            " ALL {} GOVERNORS / QUOTAS & REQUEST PRESSURE ",
            data.accounts.len()
        ),
        BORDER,
    ));
    if area.height.saturating_sub(height) < 5 {
        frame.render_widget(summary, area);
        return;
    }
    let rows = Layout::vertical([Constraint::Length(height), Constraint::Min(0)])
        .spacing(1)
        .split(area);
    frame.render_widget(summary, rows[0]);
    let account = &data.accounts[state.selected];
    if rows[1].height >= 7 && rows[1].width >= 108 {
        let parts = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .spacing(1)
            .split(rows[1]);
        quota_table(frame, parts[0], account, data.observed_at, false);
        controller(
            frame,
            parts[1],
            account,
            data.observed_at,
            false,
            state.scroll,
        );
    } else if rows[1].height >= 4 {
        compact_summary(frame, rows[1], account, data.observed_at, state.scroll);
    }
}

fn compact_summary(
    frame: &mut Frame,
    area: Rect,
    account: &AccountCard,
    now: DateTime<Utc>,
    scroll: u16,
) {
    let mut lines = vec![];
    if let Some(pressure) = &account.pressure {
        lines.extend(pressure_lines(pressure, now));
    }
    if let Some(w) = primary(account) {
        lines.push(Line::from(vec![
            bold(format!("{}  ", w.id), TEXT),
            dim(format!(
                "{:.1}% used / {:.0}% target",
                w.used_percent, w.target_percent
            )),
        ]));
        lines.push(Line::from(vec![
            dim("At reset "),
            bold(forecast(w), if w.fresh { TEXT } else { AMBER }),
            dim(format!(" · sample {}", age(now, Some(w.observed_at)))),
        ]));
        if w.expected_remaining_percent_at_reset.is_none() {
            lines.push(Line::from(dim(human(&w.projection_assumption))));
        }
    }
    if let Some(live) = &account.live_workers {
        lines.push(Line::from(vec![
            bold(format!("{} active", live.executing), GREEN),
            dim(format!(
                " · {} idle · {} other · {} stale/unverified excluded",
                live.idle, live.other, live.stale_or_unverified
            )),
        ]));
    }
    lines.push(Line::from(bold(human(&account.reason), TEXT)));
    for signal in &account.cached_signals {
        lines.push(Line::from(dim(human(signal))));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                format!(
                    " {} / selected provider · Enter details ",
                    provider_name(&account.account)
                ),
                ACCENTS[0],
            ))
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0)),
        area,
    );
}

fn details(frame: &mut Frame, area: Rect, data: &Snapshot, state: &UiState) {
    let account = &data.accounts[state.selected];
    let mut lines = vec![
        Line::from(bold(
            format!(
                "{} / quota & controller diagnostics",
                account.account.to_uppercase()
            ),
            ACCENTS[state.selected % 3],
        )),
        Line::from(dim(format!(
            "{} · readiness {} · updated {}",
            human(&account.cache_status),
            human(&account.readiness),
            age(data.observed_at, account.readiness_updated_at)
        ))),
        Line::from(""),
    ];
    if let Some(pressure) = &account.pressure {
        lines.push(Line::from(bold("REQUEST PRESSURE / 429 & 1302", TEXT)));
        lines.extend(pressure_lines(pressure, data.observed_at));
        lines.push(Line::from(""));
    }
    for w in &account.windows {
        lines.push(Line::from(bold(
            format!("{}{}", w.id, if w.fresh { "" } else { "  [STALE]" }),
            if w.fresh { TEXT } else { AMBER },
        )));
        lines.push(Line::from(format!(
            "  {:.1}% used · {:.1}% remaining · {:.1}% effective target · {:.1}% configured target",
            w.used_percent,
            (100.0 - w.used_percent).clamp(0.0, 100.0),
            w.target_percent,
            w.configured_target_percent
        )));
        lines.push(Line::from(format!(
            "  Headroom {:.2} pp · reserve {:.1}% · reset in {} ({})",
            w.headroom_percentage_points,
            w.reserve_percent,
            duration((w.resets_at - data.observed_at).num_seconds()),
            w.resets_at.format("%m-%d %H:%M UTC")
        )));
        lines.push(Line::from(format!(
            "  Pace {} · baseline {} · lookback {}",
            rate(w),
            w.baseline_seconds
                .map(duration)
                .unwrap_or_else(|| "unknown".into()),
            duration(w.lookback_seconds as i64)
        )));
        lines.push(Line::from(dim(format!(
            "  Rate evidence: {}",
            human(&w.rate_status)
        ))));
        lines.push(Line::from(format!(
            "  Expected at reset: {} · {}",
            forecast(w),
            human(&w.projection_assumption)
        )));
        lines.push(Line::from(format!(
            "  Conditional EMPTY: {}{}",
            empty_estimate(w, data.observed_at),
            w.projected_empty_at
                .filter(|_| w.fresh)
                .map(|at| format!(" ({})", at.format("%m-%d %H:%M UTC")))
                .unwrap_or_default()
        )));
        lines.push(Line::from(dim(format!(
            "  Source {} · {} saved observations",
            age(data.observed_at, Some(w.observed_at)),
            w.history.len()
        ))));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(bold("CONTROLLER / POLICY & HOLDS", TEXT)));
    lines.extend(controller_lines(account, data.observed_at, true));
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(
                " PROVIDER DETAILS · j/k scroll · Esc overview ",
                ACCENTS[state.selected % 3],
            ))
            .wrap(Wrap { trim: true })
            .scroll((state.scroll, 0)),
        area,
    );
}

fn footer(frame: &mut Frame, area: Rect, state: &UiState, width: u16) {
    let keys = if width >= 100 {
        " ↑↓ / Tab provider   1–3 jump   Enter details   j/k scroll   Space pause   R refresh   ? help   q quit "
    } else if width >= 70 {
        " ↑↓ select · Enter details · j/k scroll · Space pause · ? help · q quit "
    } else {
        " ↑↓ select · Enter detail · ? · q quit "
    };
    let note = if state.details {
        " Esc overview · cached telemetry only "
    } else if state.paused {
        " PAUSED · Space resumes live display "
    } else {
        " Read-only · refresh never queries providers or changes agents "
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(keys).style(Style::default().fg(TEXT)),
            Line::from(note).style(Style::default().fg(MUTED)),
        ]),
        area,
    );
}

fn help(frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(4).min(66);
    let height = area.height.saturating_sub(2).min(22);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(bold("Read the fleet, then inspect the constraint.", TEXT)),
        Line::from(""),
        Line::from("  Tab / Shift-Tab / ↑↓ / ←→   Select provider"),
        Line::from("  1 / 2 / 3                  Jump to provider"),
        Line::from("  Enter / d                  Provider details"),
        Line::from("  Esc                        Return to overview"),
        Line::from("  j / k · Page Up / Down     Scroll diagnostics"),
        Line::from("  Space                      Pause / resume refresh"),
        Line::from("  R                          Refresh cached observations"),
        Line::from("  ?                          Toggle this help"),
        Line::from("  q / Ctrl-C                 Quit"),
        Line::from(""),
        Line::from(dim(
            "Quota = provider account usage, including interactive work.",
        )),
        Line::from(dim(
            "Pace = measured percentage points per hour, same reset.",
        )),
        Line::from(dim("EMPTY = same pace; 'reset first' is conditional.")),
        Line::from(dim(
            "Live agents are locally verified. Targets are controller intent.",
        )),
        Line::from(dim(
            "Stale observations never gain a forecast from this screen.",
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(" SUBGOV / KEYBOARD & READING GUIDE ", ACCENTS[0]))
            .wrap(Wrap { trim: true }),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::{HistoryPoint, LiveWorkers};
    use crate::model::{BackpressureSnapshot, SaturationSnapshot};
    use chrono::{Duration, TimeZone};
    use ratatui::{backend::TestBackend, Terminal};

    fn fixture() -> Snapshot {
        let now = Utc.with_ymd_and_hms(2026, 10, 19, 3, 0, 0).unwrap();
        Snapshot {
            observed_at: now,
            accounts: ["Anthropic", "Codex", "Z.ai"]
                .iter()
                .enumerate()
                .map(|(index, name)| AccountCard {
                    account: name.to_string(),
                    cache_status: "cached state".into(),
                    readiness: "ready".into(),
                    reason: "within_budget".into(),
                    readiness_updated_at: Some(now - Duration::seconds(30)),
                    controller_observed_at: Some(now - Duration::seconds(30)),
                    controller_observed_workers: Some(4),
                    target_workers: Some(5),
                    live_workers: Some(LiveWorkers {
                        live: 4,
                        executing: 2,
                        idle: 1,
                        other: 1,
                        stale_or_unverified: 1,
                    }),
                    cached_signals: vec![
                        "Policy: BurnRate | agents 0..12 | step +1/-1".into(),
                        "Pending retirements: 1 (cached)".into(),
                        "Reviewed work holds: 1 native-policy, 2 external (cached)".into(),
                    ],
                    pressure: (index == 2).then(|| PressureCard {
                        observed_at: Some(now - Duration::seconds(20)),
                        fresh: true,
                        saturation: Some(SaturationSnapshot {
                            completed_requests: 30,
                            attempts: 40,
                            window_seconds: 300,
                            upstream_concurrency_fraction: Some(0.25),
                        }),
                        backpressure: Some(BackpressureSnapshot {
                            upstream_429_fraction: Some(0.10),
                            attempts: 40,
                            window_seconds: 300,
                        }),
                        minimum_attempts: 10,
                        minimum_window_seconds: 60,
                        concurrency_threshold: 0.5,
                    }),
                    windows: vec![WindowCard {
                        id: if index == 2 { "rolling" } else { "weekly" }.into(),
                        observed_at: now - Duration::seconds(20),
                        fresh: true,
                        used_percent: 60.0,
                        target_percent: 95.0,
                        configured_target_percent: 95.0,
                        headroom_percentage_points: 35.0,
                        reserve_percent: 5.0,
                        resets_at: now + Duration::hours(8),
                        consumption_percentage_points_per_hour: Some(8.0),
                        rate_status: "measured".into(),
                        baseline_seconds: Some(3600),
                        lookback_seconds: 3600,
                        expected_remaining_percent_at_reset: if index == 2 {
                            None
                        } else {
                            Some(0.0)
                        },
                        projected_empty_at: (index != 2)
                            .then_some(now + Duration::hours(5) - Duration::seconds(20)),
                        projection_assumption: if index == 2 {
                            "unknown: rolling/request quota; linear projection not valid"
                        } else {
                            "same account pace from sample to reset; includes interactive usage"
                        }
                        .into(),
                        history: vec![
                            HistoryPoint {
                                observed_at: now - Duration::hours(1),
                                used_percent: 52.0,
                            },
                            HistoryPoint {
                                observed_at: now - Duration::seconds(20),
                                used_percent: 60.0,
                            },
                        ],
                    }],
                })
                .collect(),
        }
    }

    fn render(data: &Snapshot, state: &mut UiState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, data, state)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_overview_exposes_fleet_quota_and_controller() {
        let screen = render(&fixture(), &mut UiState::default(), 120, 54);
        if std::env::var_os("SUBGOV_UI_TEST_CAPTURE").is_some() {
            println!("{screen}");
        }
        for label in [
            "ANTHROPIC",
            "CODEX",
            "Z.AI",
            "12 LIVE",
            "6 ACTIVE",
            "3 IDLE",
            "15 TARGET",
            "QUOTA REMAINING",
            "8.00 pp/h",
            "0.0% left",
            "quota trajectory",
            "CONTROLLER",
            "MINIMUM EVIDENCE",
            "ALL GOVERNORS",
            "EMPTY",
            "Upstream 429  10.0%",
            "1302 concurrency  25.0%",
        ] {
            assert!(screen.contains(label), "missing {label}\n{screen}");
        }
    }

    #[test]
    fn responsive_sizes_keep_selection_metrics_and_exit_visible() {
        for (width, height) in [(80, 24), (60, 16), (38, 10), (120, 36), (200, 70)] {
            let screen = render(&fixture(), &mut UiState::default(), width, height);
            assert!(screen.contains("SUBGOV"));
            assert!(screen.contains("q quit"), "{width}x{height}\n{screen}");
            if height >= 16 {
                assert!(screen.contains("Anthropic") || screen.contains("ANTHROPIC"));
            }
        }
        let mut data = fixture();
        data.accounts[0].account = "claude-anthropic".into();
        data.accounts[2].account = "claude-zai".into();
        let screen = render(&data, &mut UiState::default(), 60, 16);
        for name in ["Anthropic", "Codex", "Z.ai"] {
            assert!(screen.contains(name), "missing {name}\n{screen}");
        }
        assert!(!screen.lines().next().unwrap().contains("intelligen"));
    }

    #[test]
    fn stale_and_rolling_quota_cannot_draw_a_projection() {
        let mut data = fixture();
        data.accounts[0].windows[0].fresh = false;
        let screen = render(&data, &mut UiState::default(), 120, 54);
        assert!(screen.contains("STALE observation"));
        assert!(projection(&data.accounts[0].windows[0], data.observed_at).is_empty());
        assert!(projection(&data.accounts[2].windows[0], data.observed_at).is_empty());
        let screen = render(
            &data,
            &mut UiState {
                selected: 2,
                ..UiState::default()
            },
            80,
            24,
        );
        assert!(screen.contains("unknown"));
        assert!(screen.contains("1302"));
    }

    #[test]
    fn saturated_projection_reaches_exhaustion_at_measured_rate() {
        let data = fixture();
        let w = &data.accounts[0].windows[0];
        let projected = projection(w, w.observed_at);
        assert_eq!(projected[0], (0.0, 60.0));
        assert_eq!(projected[1], (5.0, 100.0));
        assert!(projected[2].0 > 8.0);
        assert_eq!(projected[2].1, 100.0);
    }

    #[test]
    fn all_operating_forecasts_and_pressure_are_simultaneous_at_every_supported_size() {
        let mut data = fixture();
        let w = &mut data.accounts[1].windows[0];
        w.used_percent = 40.0;
        w.consumption_percentage_points_per_hour = Some(2.0);
        w.projected_empty_at = Some(w.observed_at + Duration::hours(30));
        w.expected_remaining_percent_at_reset = Some(44.0);
        w.history[0].used_percent = 38.0;
        w.history[1].used_percent = 40.0;
        for (width, height) in [(120, 54), (80, 24), (60, 16)] {
            let screen = render(&data, &mut UiState::default(), width, height);
            assert!(
                screen.matches("EMPTY").count() >= 2,
                "{width}x{height}\n{screen}"
            );
            for label in [
                "8.00 pp/h",
                "2.00 pp/h",
                "~4h 59m",
                "~1d 05h",
                "reset first",
                "1302",
                "25.0%",
                "0.100 req/s",
            ] {
                assert!(
                    screen.contains(label),
                    "missing {label} at {width}x{height}\n{screen}"
                );
            }
            assert!(
                screen.contains("429 10.0%") || screen.contains("429  10.0%"),
                "{screen}"
            );
            if width == 120 {
                assert_eq!(screen.matches("quota trajectory").count(), 2);
            }
        }
    }

    #[test]
    fn after_reset_depletion_is_explicitly_conditional_and_chart_stops_at_reset() {
        let mut data = fixture();
        let w = &mut data.accounts[0].windows[0];
        w.consumption_percentage_points_per_hour = Some(2.0);
        w.projected_empty_at = Some(w.observed_at + Duration::hours(20));
        w.expected_remaining_percent_at_reset = Some(24.0);
        assert!(empty_estimate(w, data.observed_at).contains("reset first"));
        let projected = projection(w, w.observed_at);
        let end = projected.last().unwrap();
        assert_eq!(
            end.0,
            (w.resets_at - w.observed_at).num_seconds() as f64 / 3600.0
        );
        assert!(end.1 < 100.0);
        let screen = render(
            &data,
            &mut UiState {
                details: true,
                ..UiState::default()
            },
            120,
            54,
        );
        assert!(screen.contains("Conditional EMPTY:"));
        assert!(screen.contains("reset first"));
    }

    #[test]
    fn earliest_before_reset_depletion_wins_over_a_smaller_headroom_conditional_window() {
        let mut data = fixture();
        let mut short = fixture().accounts.remove(0).windows.remove(0);
        short.id = "short".into();
        short.headroom_percentage_points = 1.0;
        short.projected_empty_at = Some(data.observed_at + Duration::hours(1));
        short.resets_at = data.observed_at + Duration::minutes(30);
        data.accounts[0].windows.push(short);
        assert_eq!(primary(&data.accounts[0]).unwrap().id, "short");
        assert_eq!(depletion_window(&data.accounts[0]).unwrap().id, "weekly");
        data.accounts[0].windows[1].resets_at = data.observed_at + Duration::hours(3);
        assert_eq!(depletion_window(&data.accounts[0]).unwrap().id, "short");
    }

    #[test]
    fn every_governor_is_present_without_changing_provider_pages() {
        let mut data = fixture();
        for name in ["Fourth", "Fifth", "Sixth", "Seventh"] {
            let mut extra = fixture().accounts.remove(0);
            extra.account = name.into();
            data.accounts.push(extra);
        }
        for (width, height) in [(120, 54), (80, 24), (60, 16)] {
            for selected in [0, 6] {
                let screen = render(
                    &data,
                    &mut UiState {
                        selected,
                        ..UiState::default()
                    },
                    width,
                    height,
                );
                for name in [
                    "Anthropic",
                    "Codex",
                    "Z.ai",
                    "Fourth",
                    "Fifth",
                    "Sixth",
                    "Seventh",
                ] {
                    assert!(
                        screen.contains(name),
                        "missing {name} at {width}x{height}\n{screen}"
                    );
                }
            }
        }
        for i in 8..=12 {
            let mut extra = fixture().accounts.remove(0);
            extra.account = format!("Governor{i}");
            data.accounts.push(extra);
        }
        let screen = render(&data, &mut UiState::default(), 60, 16);
        assert!(screen.contains("Enlarge:"));
        assert!(screen.contains("of 12 governors fit"));
    }

    #[test]
    fn stale_missing_and_insufficient_pressure_stay_unknown_with_evidence() {
        for case in [
            "stale",
            "missing",
            "low count",
            "short interval",
            "fraction absent",
        ] {
            let mut data = fixture();
            let p = data.accounts[2].pressure.as_mut().unwrap();
            let expected = match case {
                "stale" => {
                    p.fresh = false;
                    "stale source"
                }
                "missing" => {
                    p.saturation = None;
                    p.backpressure = None;
                    "no cached sample"
                }
                "low count" => {
                    p.saturation.as_mut().unwrap().attempts = 2;
                    p.saturation.as_mut().unwrap().completed_requests = 1;
                    p.backpressure.as_mut().unwrap().attempts = 2;
                    "2/10 minimum attempts"
                }
                "short interval" => {
                    p.saturation.as_mut().unwrap().window_seconds = 20;
                    p.backpressure.as_mut().unwrap().window_seconds = 20;
                    "20/60s minimum interval"
                }
                _ => {
                    p.saturation.as_mut().unwrap().upstream_concurrency_fraction = None;
                    p.backpressure.as_mut().unwrap().upstream_429_fraction = None;
                    "fraction not reported"
                }
            };
            assert_eq!(pressure_fraction(p, false), None);
            assert_eq!(pressure_fraction(p, true), None);
            let screen = render(&data, &mut UiState::default(), 120, 54);
            assert!(screen.contains(expected), "{case}\n{screen}");
            assert!(screen.contains("Upstream 429  unknown"));
            assert!(screen.contains("1302 concurrency  unknown"));
            assert!(!screen.contains("Upstream 429  0.0%"));
        }
    }

    #[test]
    fn raw_completion_throughput_remains_measurable_below_pressure_gate() {
        let mut data = fixture();
        let p = data.accounts[2].pressure.as_mut().unwrap();
        p.minimum_attempts = 30;
        let sample = p.saturation.as_mut().unwrap();
        sample.completed_requests = 29;
        sample.attempts = 29;
        sample.window_seconds = 309;
        assert_eq!(pressure_throughput(p), "0.094 req/s");
        assert_eq!(pressure_fraction(p, true), None);
        assert!(pressure_unknown_reason(p, true).contains("29/30 minimum attempts"));
        p.fresh = false;
        assert_eq!(pressure_throughput(p), "unknown req/s");
        p.fresh = true;
        p.saturation.as_mut().unwrap().completed_requests = 31;
        assert_eq!(pressure_throughput(p), "unknown req/s");
    }

    #[test]
    fn details_help_and_empty_are_readable_and_selection_is_clamped() {
        let data = fixture();
        let screen = render(
            &data,
            &mut UiState {
                details: true,
                ..UiState::default()
            },
            120,
            54,
        );
        for label in [
            "configured target",
            "baseline 1h 00m",
            "Rate evidence",
            "Reviewed work holds",
            "stale/unverified excluded",
        ] {
            assert!(screen.contains(label), "missing {label}");
        }
        let screen = render(
            &data,
            &mut UiState {
                help: true,
                ..UiState::default()
            },
            120,
            54,
        );
        assert!(screen.contains("KEYBOARD & READING GUIDE"));
        assert!(screen.contains("Pause / resume refresh"));
        let mut empty = Snapshot {
            observed_at: data.observed_at,
            accounts: vec![],
        };
        let mut state = UiState {
            selected: usize::MAX,
            ..UiState::default()
        };
        let screen = render(&empty, &mut state, 80, 24);
        assert!(screen.contains("No accounts available"));
        assert_eq!(state.selected, 0);
        empty
            .accounts
            .push(data.accounts.into_iter().next().unwrap());
        empty.accounts[0].windows.clear();
        let screen = render(&empty, &mut state, 120, 54);
        assert!(screen.contains("No usable quota observation"));
    }
}
