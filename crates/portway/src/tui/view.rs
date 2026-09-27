//! Laying the dashboard out and drawing it. Pure: a `State` and a `Rect` in,
//! cells out, so `TestBackend` can assert on whole screens.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Padding, Paragraph, Row, Sparkline, Table};

use crate::board::{self, compression_status};
use crate::flights::{FlightView, Phase};
use crate::forwarder::Coding;
use crate::logfmt::{self, Level, human, human_count, human_time};
use crate::spend;
use crate::telemetry::RequestRecord;
use crate::tui::chart::TwoToneBars;
use crate::tui::state::{COLUMNS, Column, Columns, Entry, State, USAGE_REFRESH, mean, percentile};

// The 16 terminal colors only: the dashboard has to sit inside whatever theme
// the user's terminal already has.
const DIM: Color = Color::DarkGray;
/// The body before compression, and the bytes that actually left.
const RAW: Color = Color::Blue;
const WIRE: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const TIME: Color = Color::Yellow;
const BAD: Color = Color::Red;
const MODEL: Color = Color::Magenta;

const HUD_HEIGHT: u16 = 5;
const FOOTER_HEIGHT: u16 = 1;
const EVENTS_MIN: u16 = 4;
/// Two borders plus four rows of drawing: two apiece for the up and down
/// sparklines, and enough for the bars to show a difference in height rather
/// than just presence.
const CHART_HEIGHT: u16 = 6;
/// Gutter in front of each sparkline: `↓ down` plus a right-aligned peak.
const LABEL: u16 = 14;
/// Below this the charts stack instead of sitting side by side.
const WIDE: u16 = 80;
const MIN_WIDTH: u16 = 44;

/// What the header line says about the process itself.
pub struct Header {
    pub listen: String,
    pub coding: String,
    /// `Some` when the dashboard is reading another forwarder's database
    /// rather than owning one: what the compressor slot says instead, since
    /// this process never negotiated anything.
    pub watching: Option<Duration>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Charts {
    SideBySide,
    Stacked,
    /// Only room for one, which is the one that moves between requests.
    TrafficOnly,
}

pub struct Panes {
    pub hud: Rect,
    pub models: Option<Rect>,
    pub charts: Option<(Rect, Charts)>,
    pub events: Rect,
    pub footer: Rect,
}

/// Split `area`, dropping the charts first and then the model table when the
/// terminal is too short. `None` means there is no room for a dashboard at all.
///
/// Requests in flight take no part: they come and go several times a turn,
/// and a pane sized by them pushed everything under it up and down with every
/// one. They live in the `f` dialog, over a layout that holds still.
pub fn panes(area: Rect, models: usize) -> Option<Panes> {
    if area.width < MIN_WIDTH || area.height < HUD_HEIGHT + FOOTER_HEIGHT + EVENTS_MIN {
        return None;
    }
    let mut spare = area.height - (HUD_HEIGHT + FOOTER_HEIGHT + EVENTS_MIN);

    let table_height = models as u16 + 3;
    let table = models > 0 && spare >= table_height;
    if table {
        spare -= table_height;
    }

    let charts = if area.width >= WIDE && spare >= CHART_HEIGHT {
        Some(Charts::SideBySide)
    } else if spare >= CHART_HEIGHT * 2 {
        Some(Charts::Stacked)
    } else if spare >= CHART_HEIGHT {
        Some(Charts::TrafficOnly)
    } else {
        None
    };
    let chart_height = match charts {
        Some(Charts::Stacked) => CHART_HEIGHT * 2,
        Some(_) => CHART_HEIGHT,
        None => 0,
    };

    let mut constraints = vec![Constraint::Length(HUD_HEIGHT)];
    if table {
        constraints.push(Constraint::Length(table_height));
    }
    if charts.is_some() {
        constraints.push(Constraint::Length(chart_height));
    }
    constraints.push(Constraint::Min(EVENTS_MIN));
    constraints.push(Constraint::Length(FOOTER_HEIGHT));

    let rows = Layout::vertical(constraints).split(area);
    let mut next = 1;
    let models = table.then(|| {
        next += 1;
        rows[next - 1]
    });
    let charts = charts.map(|mode| {
        next += 1;
        (rows[next - 1], mode)
    });
    Some(Panes {
        hud: rows[0],
        models,
        charts,
        events: rows[next],
        footer: rows[next + 1],
    })
}

/// Rows the event pane can show, which is what a page of scrolling moves by.
pub fn events_height(area: Rect, models: usize) -> usize {
    panes(area, models)
        .map(|panes| panes.events.height.saturating_sub(2) as usize)
        .unwrap_or(1)
        .max(1)
}

pub fn draw(frame: &mut Frame, state: &State, header: &Header) {
    let area = frame.area();
    if state.usage_open {
        usage_screen(frame, state, area);
        return;
    }
    let Some(panes) = panes(area, state.models.len()) else {
        frame.render_widget(
            // Short enough to survive the truncation it is warning about.
            Paragraph::new("too small")
                .style(Style::default().fg(BAD))
                .alignment(Alignment::Center),
            area,
        );
        return;
    };

    frame.render_widget(hud(state, header), panes.hud);
    if let Some(area) = panes.models {
        frame.render_widget(models_table(state, area.width), area);
    }
    if let Some((area, mode)) = panes.charts {
        draw_charts(frame, state, area, mode);
    }
    frame.render_widget(events(state, panes.events.width), panes.events);
    frame.render_widget(footer(state), panes.footer);

    if state.flights_open {
        flights_dialog(frame, state, area);
    } else if state.help {
        popup(frame, area, "keys", help_lines());
    } else if state.picker {
        popup(frame, area, "columns", column_lines(state));
    } else if state.detail
        && let Some(record) = state.selected()
    {
        popup(frame, area, "request", detail_lines(record));
    }
}

// ------------------------------------------------------------------- flights

/// Past these a prefill reads `slow prefill` and an idle stream `stalled`: the
/// web console's thresholds (`flights.js`), so the two never disagree about
/// the same request.
const SLOW_PREFILL: f64 = 30.0;
const STALLED: f64 = 60.0;

fn is_slow(flight: &FlightView) -> bool {
    flight.phase == Phase::Prefill && flight.age > SLOW_PREFILL
}

fn is_stalled(flight: &FlightView) -> bool {
    flight.phase == Phase::Stream && flight.idle > STALLED
}

fn flight_phase(flight: &FlightView) -> (&'static str, Color) {
    if is_slow(flight) {
        return ("slow prefill", TIME);
    }
    if is_stalled(flight) {
        return ("stalled", BAD);
    }
    match flight.phase {
        Phase::Upload => ("upload", WIRE),
        Phase::Prefill => ("prefill", TIME),
        Phase::Stream => ("stream", GOOD),
    }
}

fn flight_route(flight: &FlightView) -> String {
    format!("{} {}", flight.method, flight.path)
}

/// `text` in `width` columns, ending in an ellipsis when it did not fit: a
/// route cut short otherwise reads as a different, shorter route.
fn clipped(text: &str, width: u16) -> String {
    let width = width as usize;
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The flights that are worth a word in the HUD while the dialog is closed:
/// `1 stalled` in red, or `2 slow` when nothing has stalled. `None` while
/// every one of them is moving.
fn flight_alarm(flights: &[FlightView]) -> Option<(String, Color)> {
    let stalled = flights.iter().filter(|flight| is_stalled(flight)).count();
    let slow = flights.iter().filter(|flight| is_slow(flight)).count();
    match (stalled, slow) {
        (0, 0) => None,
        (0, slow) => Some((format!("{slow} slow"), TIME)),
        (stalled, 0) => Some((format!("{stalled} stalled"), BAD)),
        (stalled, slow) => Some((format!("{stalled} stalled, {slow} slow"), BAD)),
    }
}

/// `f`: every request in flight, oldest first, redrawn with each 250ms sample.
///
/// A dialog rather than a pane, so the list can fill and empty without moving
/// the dashboard under it. It is as tall as its rows, up to two thirds of the
/// terminal, and as wide as its columns; its title counts the rows that did
/// not fit.
fn flights_dialog(frame: &mut Frame, state: &State, area: Rect) {
    let flights = &state.flights;
    // The header row over the rows, or one line of message.
    let body = if flights.is_empty() {
        1
    } else {
        flights.len() + 1
    };
    let tallest = area.height - 2 * (area.height / 6);
    let height = (body + 2).min(tallest as usize) as u16;
    let shown = flights.len().min(height.saturating_sub(3) as usize);
    let columns = FlightColumns::fit(
        &flights[..shown],
        area.width.saturating_sub(2 * FLIGHTS_GUTTER),
    );
    let width = columns.width().min(area.width);
    // Placed as though it held `FLIGHTS_SETTLED` rows, so up to there a
    // request arriving or leaving moves the bottom border, not the title and
    // header the eye is on. Past that it is simply centred.
    let placed = height.max((FLIGHTS_SETTLED + 3).min(tallest));
    // An owning dashboard holds every flight and drops one the moment its
    // event lands, up to a sample before the counter follows. A viewer's
    // snapshot is capped, and its count is the one that knows what was left
    // out.
    let total = if state.recorded {
        state.totals.in_flight.max(flights.len() as u64)
    } else {
        flights.len() as u64
    };
    let more = total.saturating_sub(shown as u64);
    let title = if !state.flights_available {
        "in flight".to_string()
    } else if more > 0 {
        format!("in flight · {total} · +{more} more")
    } else {
        format!("in flight · {total}")
    };
    let block = Block::bordered()
        .title_top(titled(&title))
        .title_bottom(
            Line::from(Span::styled(" f / esc close ", Style::default().fg(DIM))).right_aligned(),
        )
        .border_style(Style::default().fg(WIRE))
        .padding(Padding::horizontal(1));
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - placed) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, box_area);

    let message = if !state.flights_available {
        // The HUD says the same: a failed snapshot is not an empty one.
        Some(Span::styled(
            "unavailable: the server did not answer",
            Style::default().fg(TIME),
        ))
    } else if flights.is_empty() {
        Some(Span::styled("nothing in flight", Style::default().fg(DIM)))
    } else {
        None
    };
    match message {
        Some(message) => frame.render_widget(Paragraph::new(message).block(block), box_area),
        None => frame.render_widget(
            flights_table(&flights[..shown], &columns).block(block),
            box_area,
        ),
    }
}

/// Dashboard left showing either side of the dialog, so it reads as one.
const FLIGHTS_GUTTER: u16 = 2;
/// The rows the dialog is placed as though it had, however few it has.
const FLIGHTS_SETTLED: u16 = 8;
/// A model name and a route most traffic fits (`POST /v1/messages`): the
/// dialog keeps its width from one request to the next, and only a longer
/// name widens it, as far as the cap and the terminal allow.
const FLIGHTS_MODEL: (u16, u16) = (16, 28);
const FLIGHTS_ROUTE: (u16, u16) = (17, 40);
/// Phase, age and bytes down, which every dialog has after the model.
const FLIGHTS_CORE: [u16; 3] = [12, 7, 7];
/// Status, ttfb, idle and retries, for a dialog with room for them.
const FLIGHTS_ROOMY: [u16; 4] = [4, 7, 7, 5];

/// The dialog's columns: which of them it has room for, and how wide the two
/// that hold names are.
struct FlightColumns {
    model: u16,
    roomy: bool,
    route: Option<u16>,
}

impl FlightColumns {
    /// The most of the table that fits in `room`, sized to what it holds. The
    /// columns are chosen at the narrowest names, so a long one arriving
    /// widens the dialog into the spare room rather than trading a column
    /// for it.
    fn fit(flights: &[FlightView], room: u16) -> Self {
        let (model, route) = (FLIGHTS_MODEL.0, Some(FLIGHTS_ROUTE.0));
        let mut columns = [
            Self {
                model,
                roomy: true,
                route,
            },
            Self {
                model,
                roomy: false,
                route,
            },
        ]
        .into_iter()
        .find(|columns| columns.width() <= room)
        // Too narrow for either: a shorter model, and no route.
        .unwrap_or(Self {
            model: 10,
            roomy: false,
            route: None,
        });
        let longest = |text: fn(&FlightView) -> String| {
            flights
                .iter()
                .map(|flight| text(flight).chars().count())
                .max()
                .unwrap_or(0) as u16
        };
        let mut spare = room.saturating_sub(columns.width());
        let grow = longest(|flight| flight.model.clone())
            .min(FLIGHTS_MODEL.1)
            .saturating_sub(columns.model)
            .min(spare);
        columns.model += grow;
        spare -= grow;
        if let Some(route) = &mut columns.route {
            *route += longest(flight_route)
                .min(FLIGHTS_ROUTE.1)
                .saturating_sub(*route)
                .min(spare);
        }
        columns
    }

    fn widths(&self) -> Vec<u16> {
        let mut widths = vec![self.model];
        widths.extend(FLIGHTS_CORE);
        if self.roomy {
            widths.extend(FLIGHTS_ROOMY);
        }
        widths.extend(self.route);
        widths
    }

    /// Across the whole dialog: the columns, a space between each, and a
    /// border and a column of padding either side.
    fn width(&self) -> u16 {
        let widths = self.widths();
        widths.iter().sum::<u16>() + widths.len() as u16 - 1 + 4
    }
}

/// The rows of the dialog, in the columns it has room for.
fn flights_table(flights: &[FlightView], columns: &FlightColumns) -> Table<'static> {
    let roomy = columns.roomy;
    let mut headers = vec![
        Cell::from("model"),
        Cell::from("phase"),
        number("age"),
        number("down"),
    ];
    if roomy {
        headers.extend([
            number("code"),
            number("ttfb"),
            number("idle"),
            number("retry"),
        ]);
    }
    if columns.route.is_some() {
        headers.push(Cell::from("route"));
    }
    let widths: Vec<Constraint> = columns
        .widths()
        .into_iter()
        .map(Constraint::Length)
        .collect();
    let rows = flights.iter().map(|flight| {
        let (phase, shade) = flight_phase(flight);
        let mut cells = vec![
            Cell::from(Span::styled(
                clipped(&flight.model, columns.model),
                Style::default().fg(MODEL),
            )),
            Cell::from(Span::styled(phase, Style::default().fg(shade))),
            number(human_time(flight.age)),
            number(human(flight.received)),
        ];
        if roomy {
            cells.extend([
                number(
                    flight
                        .status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
                number(
                    flight
                        .ttfb
                        .map(human_time)
                        .unwrap_or_else(|| "-".to_string()),
                ),
                number(human_time(flight.idle)),
                number(flight.retries.to_string()),
            ]);
        }
        if let Some(width) = columns.route {
            cells.push(Cell::from(clipped(&flight_route(flight), width)));
        }
        Row::new(cells)
    });
    Table::new(rows, widths)
        .header(Row::new(headers).style(Style::default().fg(DIM)))
        .column_spacing(1)
}

// ----------------------------------------------------------------------- hud

fn uptime(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// `-76%` for a raw/wire pair, or a dash before any traffic. The percentage
/// alone: the multiplier beside it says the same thing twice, and the raw and
/// wire sizes it came from are always on the line already.
///
/// It truncates exactly as the stderr log line's does, so the same request
/// never reads `-99%` in the log and `-100%` here.
fn ratio(raw: u64, wire: u64) -> String {
    if raw == 0 || wire == 0 {
        return "-".to_string();
    }
    format!("-{}%", raw.saturating_sub(wire) * 100 / raw)
}

fn stat<'a>(label: &'a str, value: String, shade: Color) -> Vec<Span<'a>> {
    vec![
        Span::styled(label, Style::default().fg(DIM)),
        Span::raw(" "),
        Span::styled(value, Style::default().fg(shade)),
        Span::raw("  "),
    ]
}

fn hud<'a>(state: &State, header: &'a Header) -> Paragraph<'a> {
    let totals = &state.totals;

    let mut title = vec![
        Span::styled(" portway", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(header.listen.clone(), Style::default().fg(WIRE)),
        Span::raw("  "),
    ];
    title.extend(stat(
        "up",
        uptime(
            state
                .coverage
                .unwrap_or_else(|| state.started.elapsed())
                .as_secs(),
        ),
        TIME,
    ));
    // A window cannot see the compressor this process never asked for; the
    // slot says what it is instead: how wide the window it reads is.
    match header.watching {
        Some(span) => title.extend(stat("watching", logfmt::span(span), GOOD)),
        None => title.extend(stat("coding", header.coding.clone(), GOOD)),
    }
    title.extend(stat("models", state.models.len().to_string(), Color::Reset));

    let reuse = match (state.reused * 100).checked_div(state.seen) {
        Some(share) => format!("{share}%"),
        None => "-".to_string(),
    };
    let mut requests = vec![Span::styled(" REQUESTS ", Style::default().fg(DIM))];
    requests.extend(stat("total", totals.requests.to_string(), Color::Reset));
    if state.flights_available {
        requests.extend(stat("live", totals.in_flight.to_string(), GOOD));
        // The one thing about the flights that cannot wait for `f`: something
        // stopped moving. It goes between the count and the gap after it.
        if let Some((alarm, shade)) = flight_alarm(&state.flights) {
            requests.insert(
                requests.len() - 1,
                Span::styled(format!(" ({alarm})"), Style::default().fg(shade)),
            );
        }
    } else {
        requests.push(Span::styled(
            "in flight unavailable  ",
            Style::default().fg(TIME),
        ));
    }
    requests.extend(stat("2xx", state.ok.to_string(), GOOD));
    requests.extend(stat(
        "4xx",
        state.client_errors.to_string(),
        if state.client_errors > 0 { TIME } else { DIM },
    ));
    requests.extend(stat(
        "5xx",
        state.server_errors.to_string(),
        if state.server_errors > 0 { BAD } else { DIM },
    ));
    requests.extend(stat("cut", state.truncated.to_string(), DIM));
    requests.extend(stat("aborts", totals.aborts.to_string(), DIM));
    requests.extend(stat(
        "up-err",
        totals.upstream_errors.to_string(),
        if totals.upstream_errors > 0 { BAD } else { DIM },
    ));
    requests.extend(stat("reuse", reuse, GOOD));

    let saved = totals.body_bytes.saturating_sub(totals.wire_bytes);
    let mut upload = vec![Span::styled(" UPLOAD   ", Style::default().fg(DIM))];
    upload.extend(stat("raw", human(totals.body_bytes), RAW));
    upload.extend(stat("wire", human(totals.wire_bytes), WIRE));
    upload.extend(stat(
        "saved",
        format!(
            "{} ({})",
            human(saved),
            ratio(totals.body_bytes, totals.wire_bytes)
        ),
        GOOD,
    ));
    upload.extend(stat(
        "encoded",
        format!("{}/{}", totals.encoded, totals.requests),
        Color::Reset,
    ));
    upload.extend(stat(
        "415-retry",
        totals.retried_identity.to_string(),
        if totals.retried_identity > 0 {
            TIME
        } else {
            DIM
        },
    ));

    let mut download = vec![Span::styled(" DOWNLOAD ", Style::default().fg(DIM))];
    download.extend(stat("wire", human(totals.down_wire_bytes), WIRE));
    download.extend(stat(
        "decoded",
        format!(
            "{} ({})",
            human(totals.down_bytes),
            ratio(totals.down_bytes, totals.down_wire_bytes)
        ),
        RAW,
    ));
    // The hop's own saving, next to the upstream leg's: the same shape the
    // upload row uses, and the decoded size it came out of is the stat beside
    // it. Hidden until a response actually went out encoded.
    if totals.agent_bytes > 0 {
        download.extend(stat(
            "agent",
            format!(
                "{} ({})",
                human(totals.agent_bytes),
                ratio(totals.down_bytes, totals.agent_bytes)
            ),
            GOOD,
        ));
    }
    download.extend(stat(
        "idle conns",
        totals.idle_conns.to_string(),
        Color::Reset,
    ));

    let quantile = |samples: &_, q| {
        percentile(samples, q)
            .map(human_time)
            .unwrap_or_else(|| "-".to_string())
    };
    let mut latency = vec![Span::styled(" LATENCY  ", Style::default().fg(DIM))];
    latency.extend(stat(
        "ttfb p50/p95",
        format!(
            "{} / {}",
            quantile(&state.ttfb, 0.5),
            quantile(&state.ttfb, 0.95)
        ),
        TIME,
    ));
    latency.extend(stat(
        "upload p50/p95",
        format!(
            "{} / {}",
            quantile(&state.upload, 0.5),
            quantile(&state.upload, 0.95)
        ),
        TIME,
    ));
    latency.extend(stat(
        "handshake avg",
        mean(&state.handshake)
            .map(human_time)
            .unwrap_or_else(|| "-".to_string()),
        TIME,
    ));

    Paragraph::new(vec![
        Line::from(title),
        Line::from(requests),
        Line::from(upload),
        Line::from(download),
        Line::from(latency),
    ])
}

// -------------------------------------------------------------------- models

fn coding_span(coding: Coding, dict: bool) -> Span<'static> {
    let color = if coding == Coding::None { DIM } else { GOOD };
    Span::styled(
        board::coding_label(coding, dict),
        Style::default().fg(color),
    )
}

/// Below this the table sheds columns rather than letting every one of them
/// shrink until the model names are unreadable.
const ROOMY_TABLE: u16 = 100;
const STATUS_TABLE: u16 = 136;

fn models_table(state: &State, width: u16) -> Table<'_> {
    let roomy = width >= ROOMY_TABLE;
    let show_status = width >= STATUS_TABLE;
    let mut names = vec!["model", "coding", "reqs", "live", "raw", "wire", "saved"];
    if roomy {
        names.extend(["down", "idle", "err", "abort"]);
    }
    if show_status {
        names.push("compression status");
    }
    let header = Row::new(names).style(Style::default().fg(DIM));

    let rows = state.models.iter().map(|row| {
        let view = &row.view;
        let errors = view.upstream_errors;
        let mut cells = vec![
            Cell::from(Span::styled(row.name.clone(), Style::default().fg(MODEL))),
            Cell::from(coding_span(view.coding, view.dict)),
            Cell::from(view.requests.to_string()),
            Cell::from(Span::styled(
                if state.flights_available {
                    view.in_flight.to_string()
                } else {
                    "-".to_string()
                },
                Style::default().fg(if view.in_flight > 0 { GOOD } else { DIM }),
            )),
            Cell::from(Span::styled(
                human(view.body_bytes),
                Style::default().fg(RAW),
            )),
            Cell::from(Span::styled(
                human(view.wire_bytes),
                Style::default().fg(WIRE),
            )),
            Cell::from(Span::styled(
                human(view.saved_bytes().max(0) as u64),
                Style::default().fg(GOOD),
            )),
        ];
        if roomy {
            cells.extend([
                Cell::from(Span::styled(
                    human(view.down_bytes),
                    Style::default().fg(RAW),
                )),
                Cell::from(view.idle_conns.to_string()),
                Cell::from(Span::styled(
                    errors.to_string(),
                    Style::default().fg(if errors > 0 { BAD } else { DIM }),
                )),
                Cell::from(Span::styled(
                    view.client_aborts.to_string(),
                    Style::default().fg(DIM),
                )),
            ]);
        }
        if show_status {
            cells.push(Cell::from(Span::styled(
                compression_status(view),
                Style::default().fg(TIME),
            )));
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(20),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
    ];
    if roomy {
        widths.extend([
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(5),
        ]);
    }
    if show_status {
        widths.push(Constraint::Length(30));
    }
    Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .block(
            Block::bordered()
                .title_top(titled("models"))
                .border_style(Style::default().fg(DIM))
                .padding(Padding::horizontal(1)),
        )
}

// ------------------------------------------------------------- usage screen

/// Where a model name, the token columns and the money columns all fit.
const USAGE_ROOMY: u16 = 106;
/// A border, a header row, one model and the total: below this the screen has
/// nothing left to say. The compact tier below `USAGE_ROOMY` still carries the
/// counts and the total, so it wants the same 75 columns they add up to.
const USAGE_MIN_WIDTH: u16 = 76;
const USAGE_MIN_HEIGHT: u16 = 6;

/// `u`: the day, per model, out of the database.
///
/// This is the one screen that reads the record rather than the counters, so
/// it is also the only place the two can be held against each other — and the
/// only one that can answer for a day that started before the dashboard did.
fn usage_screen(frame: &mut Frame, state: &State, area: Rect) {
    let Some(table) = &state.usage else {
        let reason = state
            .usage_error
            .as_deref()
            .unwrap_or("no counts have been read");
        frame.render_widget(
            Paragraph::new(reason)
                .style(Style::default().fg(BAD))
                .alignment(Alignment::Center),
            area,
        );
        return;
    };
    if area.width < USAGE_MIN_WIDTH || area.height < USAGE_MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("too small")
                .style(Style::default().fg(BAD))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let notes = usage_notes(table);
    // Both ends carry their date: a window that is over ends at a midnight,
    // and `00:00:00` alone would not say which one.
    let title = format!(
        "usage — {} .. {}",
        logfmt::datetime(table.since),
        logfmt::datetime(table.until)
    );
    let block = Block::bordered()
        .title_top(titled(&title))
        .border_style(Style::default().fg(DIM))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The selector, then a header row, one row per model and the total. What is
    // left under them is the room the notes get, so they sit against the
    // numbers they qualify rather than at the bottom of a tall terminal.
    let height = table.rows.len() as u16 + 2;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(height),
        Constraint::Length(1),
        Constraint::Length(notes.len() as u16),
        Constraint::Min(0),
    ])
    .split(inner);
    frame.render_widget(Paragraph::new(usage_ranges(state)), rows[0]);
    if table.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no usage recorded in this window",
                Style::default().fg(DIM),
            ))),
            rows[1],
        );
    } else {
        frame.render_widget(usage_table(table, area.width), rows[1]);
    }
    frame.render_widget(Paragraph::new(notes), rows[3]);

    if state.usage_rates {
        popup(frame, area, "costs", cost_lines(table));
    }
}

/// `18.43` as a column reads it: more decimals the smaller the number, because
/// a day of cached context can come to less than a cent and `0.00` says
/// nothing at all.
fn dollars(amount: f64) -> String {
    if amount == 0.0 {
        "$0".to_string()
    } else if amount >= 1.0 {
        format!("${amount:.2}")
    } else if amount >= 0.01 {
        format!("${amount:.3}")
    } else {
        format!("${amount:.4}")
    }
}

fn usage_table(table: &spend::Table, width: u16) -> Table<'static> {
    let roomy = width >= USAGE_ROOMY;
    let mut names = vec!["model", "reqs", "prompt", "cached", "hit", "output"];
    if roomy {
        names.extend(["in$", "cache$", "out$"]);
    }
    names.push("total$");
    // Every heading but the first sits over a right-aligned column, so it is
    // drawn the same way: a heading that hangs off the left edge of its column
    // reads as belonging to the one before it.
    let mut header = vec![Cell::from("model")];
    header.extend(names[1..].iter().map(|name| number(*name)));

    let mut rows: Vec<Row> = table
        .rows
        .iter()
        .map(|model| usage_row(model, roomy, false))
        .collect();
    rows.push(usage_row(&table.total, roomy, true));

    let mut widths = vec![
        Constraint::Length(20),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(6),
        Constraint::Length(9),
    ];
    if roomy {
        widths.extend([
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(9),
        ]);
    }
    widths.push(Constraint::Length(9));
    Table::new(rows, widths)
        .header(Row::new(header).style(Style::default().fg(DIM)))
        .column_spacing(1)
}

/// The window selector, drawn above the table: what is being measured, with
/// the one in force reversed and the arrows that move it spelled out.
fn usage_ranges(state: &State) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (at, range) in spend::Range::ALL.into_iter().enumerate() {
        if at > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(DIM)));
        }
        if range == state.usage_range {
            spans.push(Span::styled(
                format!(" {} ", range.label()),
                Style::default()
                    .fg(WIRE)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ));
        } else {
            spans.push(Span::styled(range.label(), Style::default().fg(DIM)));
        }
    }
    spans.push(Span::styled(
        "   ←→ window · p costs",
        Style::default().fg(DIM),
    ));
    Line::from(spans)
}

/// The money behind the `$` column, split into the three sources it came from:
/// the prompt tokens that were not cached, the cached tokens that were served
/// from prefix cache, and the output tokens the engine generated. Prices are
/// not repeated here — the point is what the window actually cost, not what
/// rate produced the number.
///
/// A model the price table does not name is shown as dashes; its tokens are
/// already counted in the main table's totals, but its money cannot be summed.
fn cost_lines(table: &spend::Table) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " cost by source",
        Style::default().fg(WIRE),
    ))];
    lines.push(Line::from(vec![
        Span::styled(format!(" {:<20}", "model"), Style::default().fg(DIM)),
        Span::styled(
            format!(
                "{:>10}{:>10}{:>10}{:>11}",
                "prompt", "cached", "output", "total$"
            ),
            Style::default().fg(DIM),
        ),
    ]));
    for row in &table.rows {
        let model = &row.model;
        let (prompt, cached, output, total) = match row.charge {
            Some(c) => (
                dollars(c.input),
                dollars(c.cache_read),
                dollars(c.output),
                Some(dollars(c.total())),
            ),
            None => ("-".to_string(), "-".to_string(), "-".to_string(), None),
        };
        let priced = total.is_some();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {model:<20}"),
                Style::default().fg(if priced { MODEL } else { DIM }),
            ),
            Span::styled(
                format!("{:>10}{:>10}{:>10}", prompt, cached, output),
                Style::default().fg(if priced { GOOD } else { DIM }),
            ),
            Span::styled(
                format!("{:>11}", total.as_deref().unwrap_or("-")),
                Style::default().fg(if priced { GOOD } else { DIM }),
            ),
        ]));
    }
    if let Some(sum) = table.total.charge {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<20}", "total"),
                Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>10}{:>10}{:>10}",
                    dollars(sum.input),
                    dollars(sum.cache_read),
                    dollars(sum.output)
                ),
                Style::default().fg(GOOD),
            ),
            Span::styled(
                format!("{:>11}", dollars(sum.total())),
                Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        " p / esc close",
        Style::default().fg(DIM),
    )));
    lines
}

/// One line of the usage table. `total` is the same row with the day's pooled
/// counts in it, drawn bold — the columns are identical on purpose.
fn usage_row(model: &spend::Row, roomy: bool, total: bool) -> Row<'static> {
    let charge = model.charge;
    let mut cells = vec![
        Cell::from(Span::styled(
            model.model.clone(),
            Style::default().fg(if total { GOOD } else { MODEL }),
        )),
        number(model.requests.to_string()),
        number(human_count(model.prompt)),
        number(
            if model.unreported == model.requests && model.requests > 0 && !total {
                // Not a zero: the engine said nothing about its cache, and a dash
                // is the only honest thing to put in the column.
                Span::styled("-", Style::default().fg(DIM))
            } else {
                Span::raw(human_count(model.cached))
            },
        ),
        number(hit_span(model.hit_rate())),
        number(human_count(model.completion)),
    ];
    if roomy {
        cells.extend([
            number(part(charge.map(|charge| charge.input))),
            number(part(charge.map(|charge| charge.cache_read))),
            number(part(charge.map(|charge| charge.output))),
        ]);
    }
    cells.push(number(Span::styled(
        model
            .cost()
            .map(dollars)
            .unwrap_or_else(|| "unpriced".to_string()),
        Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
    )));
    let row = Row::new(cells);
    if total {
        row.style(Style::default().add_modifier(Modifier::BOLD))
    } else {
        row
    }
}

/// A numeric cell, right-aligned: a column of counts and money is read by its
/// last digit, and neither has a fixed width.
fn number<'a>(content: impl Into<Text<'a>>) -> Cell<'a> {
    let mut text: Text<'a> = content.into();
    text.alignment = Some(Alignment::Right);
    Cell::from(text)
}

/// A rate worth reading at a glance: green once half the context is coming off
/// the cache, because that is the price of the other half.
fn hit_span(rate: Option<f64>) -> Span<'static> {
    match rate {
        Some(rate) => Span::styled(
            format!("{:.1}%", rate * 100.0),
            Style::default().fg(if rate >= 0.5 { GOOD } else { TIME }),
        ),
        None => Span::styled("n/a", Style::default().fg(DIM)),
    }
}

/// One money cell: a part of the charge, or a dash when nothing could price it.
fn part(amount: Option<f64>) -> String {
    amount.map(dollars).unwrap_or_else(|| "-".to_string())
}

/// What the numbers above are not: written under them, in the order a reader
/// would ask. A screen full of money has to say whose money, and which part of
/// it is a floor.
fn usage_notes(table: &spend::Table) -> Vec<Line<'static>> {
    let mut notes: Vec<Line<'static>> = spend::notes(table).into_iter().map(note).collect();
    notes.push(Line::from(vec![
        Span::styled(" u / esc", Style::default().fg(WIRE)),
        Span::styled(
            format!(
                "  back to the dashboard · re-read every {}s",
                USAGE_REFRESH.as_secs()
            ),
            Style::default().fg(DIM),
        ),
    ]));
    notes
}

fn note(text: String) -> Line<'static> {
    Line::from(Span::styled(format!(" {text}"), Style::default().fg(DIM)))
}

// -------------------------------------------------------------------- charts

fn titled(text: &str) -> Line<'_> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(text, Style::default().fg(DIM).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
    ])
}

fn draw_charts(frame: &mut Frame, state: &State, area: Rect, mode: Charts) {
    let (bars_area, traffic_area) = match mode {
        Charts::SideBySide => {
            let split =
                Layout::horizontal([Constraint::Percentage(55), Constraint::Min(30)]).split(area);
            (Some(split[0]), Some(split[1]))
        }
        Charts::Stacked => {
            let split = Layout::vertical([
                Constraint::Length(CHART_HEIGHT),
                Constraint::Length(CHART_HEIGHT),
            ])
            .split(area);
            (Some(split[0]), Some(split[1]))
        }
        Charts::TrafficOnly => (None, Some(area)),
    };

    // One column per turn: the body the agent sent, and what was left of it
    // after the compressor.
    if let Some(bars_area) = bars_area {
        let shown = bars_area.width.saturating_sub(2) as usize;
        let samples: Vec<(u64, u64)> = state.bars.iter().rev().take(shown).rev().copied().collect();
        let window: (u64, u64) = samples
            .iter()
            .fold((0, 0), |sum, (raw, wire)| (sum.0 + raw, sum.1 + wire));
        let legend = Line::from(vec![
            Span::styled(" raw ", Style::default().fg(DIM)),
            Span::styled("█", Style::default().fg(RAW)),
            Span::styled(" wire ", Style::default().fg(DIM)),
            Span::styled("█", Style::default().fg(WIRE)),
            Span::styled(
                format!(" {} turns · {} ", samples.len(), ratio(window.0, window.1)),
                Style::default().fg(DIM),
            ),
        ]);
        let block = Block::bordered()
            .title_top(titled("request body / turn"))
            .title_bottom(legend.right_aligned())
            .border_style(Style::default().fg(DIM));
        let inner = block.inner(bars_area);
        frame.render_widget(block, bars_area);
        frame.render_widget(TwoToneBars::new(&samples, RAW, WIRE), inner);
    }

    // Socket throughput.
    let Some(traffic_area) = traffic_area else {
        return;
    };
    let block = Block::bordered()
        .title_top(titled("socket bytes/s"))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {}s buckets · peak ", state.scale),
                Style::default().fg(DIM),
            ))
            .right_aligned(),
        )
        .border_style(Style::default().fg(DIM));
    let inner = block.inner(traffic_area);
    frame.render_widget(block, traffic_area);
    if inner.width <= LABEL + 4 || inner.height < 2 {
        return;
    }

    let series = state
        .traffic
        .series(state.scale, (inner.width - 12) as usize);
    let halves =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    for (half, data, arrow, shade) in [
        (halves[0], &series.up, "↑ up", WIRE),
        (halves[1], &series.down, "↓ down", RAW),
    ] {
        let peak = data.iter().copied().max().unwrap_or(0);
        let columns =
            Layout::horizontal([Constraint::Length(LABEL), Constraint::Min(4)]).split(half);
        // On the baseline the bars grow from, not the top of the half, so the
        // empty rows are where the tall bars will be.
        let label = Rect {
            y: columns[0].bottom() - 1,
            height: 1,
            ..columns[0]
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(arrow, Style::default().fg(shade)),
                Span::styled(format!(" {:>7}", human(peak)), Style::default().fg(DIM)),
            ])),
            label,
        );
        frame.render_widget(
            Sparkline::default()
                .data(data.clone())
                .style(Style::default().fg(shade)),
            columns[1],
        );
    }
}

// -------------------------------------------------------------------- events

/// The route reads abbreviated — `POST ../completions` — so the line still
/// names where the request went without re-spelling the `/v1/` trunk every
/// line. The routes an agent's turns share stay dim; anything else keeps its
/// full path and is bold, because that is the line worth noticing. The query
/// and the full method/path live in the detail popup — which is also where
/// `POST /v1/completions` is told apart from its chat sibling, whose tail the
/// abbreviation happens to match.
fn route(record: &RequestRecord) -> (String, Style) {
    let (shown, known) = board::route(&record.method, &record.path);
    let style = if known {
        Style::default().fg(DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    (shown, style)
}

/// A line built field by field, so a field that is turned off closes the gap
/// instead of leaving a hole — and so the two one-cell marks can take the
/// place of a separator rather than adding one.
#[derive(Default)]
struct Fields {
    spans: Vec<Span<'static>>,
    /// Whether a space is due in front of the next field.
    gap: bool,
    /// Set by a mark whose neighbour follows it with no space either.
    held: bool,
}

impl Fields {
    /// One field, separated from the last by one space.
    fn word(&mut self, span: Span<'static>) {
        if self.gap && !std::mem::take(&mut self.held) {
            self.spans.push(Span::raw(" "));
        }
        self.spans.push(span);
        self.gap = true;
    }

    /// A field glued to the one in front of it, with the next one spaced as
    /// usual: the `.` that marks a fresh dial.
    fn bare(&mut self, span: Span<'static>) {
        self.spans.push(span);
        self.gap = true;
        self.held = false;
    }

    /// A one-cell mark glued to both neighbours: the `✂` of a cut relay. It
    /// takes the place of a separator, which is what keeps a marked line
    /// exactly as wide as an unmarked one.
    fn glue(&mut self, span: Span<'static>) {
        self.spans.push(span);
        self.gap = true;
        self.held = true;
    }
}

fn request_line(record: &RequestRecord, columns: Columns) -> Line<'static> {
    let status_shade = match record.status {
        status if status < 300 => GOOD,
        status if status < 400 => WIRE,
        status if status < 500 => TIME,
        _ => BAD,
    };
    let mut line = Fields::default();
    for column in COLUMNS
        .into_iter()
        .filter(|column| columns.contains(*column))
    {
        match column {
            Column::Time => line.word(Span::styled(record.stamp.clone(), Style::default().fg(DIM))),
            Column::Status => line.word(Span::styled(
                record.status.to_string(),
                Style::default().fg(status_shade),
            )),
            // The relay ended before the upstream body did. Nothing to say
            // about one that finished, which is what the space it takes would
            // have said anyway.
            Column::Cut => {
                if !record.complete {
                    line.glue(Span::styled("✂", Style::default().fg(TIME)));
                }
            }
            Column::Model => {
                line.word(Span::styled(
                    record.model.clone(),
                    Style::default().fg(MODEL),
                ));
            }
            Column::Route => {
                let (route, style) = route(record);
                line.word(Span::styled(route, style));
                // Every turn rides a pooled connection, so reuse needs no
                // announcing — a line without the mark is a reused one. A
                // fresh dial costs latency and says so with the mark alone;
                // how much it cost is the detail popup's business. `.` because
                // it is ASCII — one byte, one cell in every terminal, unlike
                // `●`/`•` (East Asian ambiguous width).
                if record.handshake().is_some() {
                    line.bare(Span::styled(".", Style::default().fg(TIME)));
                }
            }
            Column::Sizes => {
                if record.body_len == 0 {
                    continue;
                }
                // zstd can round up on incompressible input; the ratio never
                // claims a negative saving.
                line.word(Span::styled(
                    human(record.body_len),
                    Style::default().fg(RAW),
                ));
                line.glue(Span::styled("→", Style::default().fg(DIM)));
                line.word(Span::styled(
                    human(record.wire_len),
                    Style::default().fg(WIRE),
                ));
                if record.coding != Coding::None {
                    line.word(Span::styled(
                        ratio(record.body_len, record.wire_len),
                        Style::default().fg(GOOD),
                    ));
                }
                if let Some(upload) = record.upload {
                    line.word(Span::styled(human_time(upload), Style::default().fg(TIME)));
                }
            }
            Column::Ttfb => {
                line.word(Span::styled("ttfb", Style::default().fg(DIM)));
                line.word(Span::styled(
                    human_time(record.ttfb),
                    Style::default().fg(TIME),
                ));
            }
            Column::Down => {
                line.word(Span::styled("down", Style::default().fg(DIM)));
                line.word(Span::styled(
                    human(record.received),
                    Style::default().fg(RAW),
                ));
                // What the hop actually sent, when it re-encoded: the same pair
                // the upload column shows, spaced the same way — one cell, no
                // gap. A row from before the column, or an identity hop, keeps
                // the single size it always had.
                if record.received_agent > 0 && record.received_agent != record.received {
                    line.glue(Span::styled("→", Style::default().fg(DIM)));
                    line.word(Span::styled(
                        human(record.received_agent),
                        Style::default().fg(WIRE),
                    ));
                    line.word(Span::styled(
                        ratio(record.received, record.received_agent),
                        Style::default().fg(GOOD),
                    ));
                }
                if let Some(download) = record.download {
                    line.word(Span::styled(
                        human_time(download),
                        Style::default().fg(TIME),
                    ));
                }
            }
            // Last on the line, so a narrow pane cuts the counts before it
            // cuts the sizes and times: `in(cached)→out`, the same pair the
            // compression column reads, with the details the popup has room for
            // left to the popup.
            Column::Tokens => {
                let Some(usage) = record.usage else {
                    continue;
                };
                line.word(Span::styled("tok", Style::default().fg(DIM)));
                line.word(Span::styled(
                    human_count(usage.prompt),
                    Style::default().fg(RAW),
                ));
                // The part of the prompt a prefix cache read instead of
                // prefilling, glued to the count it came out of. Spelled out
                // like the log line's: 91.2K(91.1K) leaves a reader guessing at
                // what the parentheses hold. An engine that reported counts
                // without the breakdown gets none at all.
                if let Some(cached) = usage.cached {
                    line.glue(Span::styled(
                        format!("({} cached)", human_count(cached)),
                        Style::default().fg(DIM),
                    ));
                }
                line.glue(Span::styled("→", Style::default().fg(DIM)));
                line.word(Span::styled(
                    human_count(usage.completion),
                    Style::default().fg(WIRE),
                ));
            }
        }
    }
    Line::from(line.spans)
}

fn log_line(stamp: &str, level: Level, message: &str) -> Line<'static> {
    let shade = match level {
        Level::Info => Color::Reset,
        Level::Warning => TIME,
        Level::Error => BAD,
    };
    let mut spans = vec![
        Span::styled(stamp.to_string(), Style::default().fg(DIM)),
        Span::raw(" "),
    ];
    if level >= Level::Warning {
        spans.push(Span::styled(
            format!("{} ", level.name()),
            Style::default().fg(shade),
        ));
    }
    spans.push(Span::styled(
        message.to_string(),
        Style::default().fg(shade),
    ));
    Line::from(spans)
}

fn events(state: &State, width: u16) -> Paragraph<'static> {
    let lines: Vec<Line> = state
        .visible()
        .into_iter()
        .map(|(entry, selected)| {
            let line = match entry {
                Entry::Request(record) => request_line(record, state.columns),
                Entry::Log {
                    stamp,
                    level,
                    message,
                } => log_line(stamp, *level, message),
            };
            if selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();

    let position = if state.follow {
        Span::styled("FOLLOW", Style::default().fg(GOOD))
    } else {
        let hidden = state.below();
        let suffix = if hidden > 0 {
            format!(" · {hidden} below")
        } else {
            String::new()
        };
        Span::styled(format!("PAUSED{suffix}"), Style::default().fg(TIME))
    };
    let status = Line::from(vec![
        Span::raw(" "),
        position,
        Span::styled(
            format!(
                " · {} lines · filter {} ",
                state.len(),
                state.filter.label()
            ),
            Style::default().fg(DIM),
        ),
    ]);
    let block = Block::bordered()
        .title_top(titled("events"))
        .border_style(Style::default().fg(DIM));
    // A narrow pane has no room for both titles.
    let block = if width > 60 {
        block.title_top(status.right_aligned())
    } else {
        block
    };
    Paragraph::new(lines).block(block)
}

fn footer(state: &State) -> Paragraph<'static> {
    if state.confirm_quit {
        return Paragraph::new(Line::from(Span::styled(
            format!(
                " {} request(s) still streaming — press q again to cut them off, f to see them ",
                state.totals.in_flight
            ),
            Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        )));
    }
    let keys = [
        ("q", "quit"),
        ("↑↓/jk", "scroll"),
        ("g/G", "top/live"),
        ("↵", "detail"),
        ("f", "flights"),
        ("e", "trouble"),
        ("m", "model"),
        ("u", "usage"),
        ("t", "buckets"),
        ("c", "columns"),
        ("?", "keys"),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(key, Style::default().fg(WIRE)));
        spans.push(Span::styled(format!(" {what}  "), Style::default().fg(DIM)));
    }
    Paragraph::new(Line::from(spans))
}

/// The picker: every column, on or off, with the one under the cursor
/// reversed the way a selected line is.
fn column_lines(state: &State) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = COLUMNS
        .into_iter()
        .enumerate()
        .map(|(at, column)| {
            let on = state.columns.contains(column);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", if on { '✓' } else { '·' }),
                    Style::default().fg(if on { GOOD } else { DIM }),
                ),
                Span::styled(
                    format!("{:<8}", column.name()),
                    if on {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
                Span::styled(column.note(), Style::default().fg(DIM)),
            ]);
            if at == state.picker_at {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        " space/↵ toggle · esc close (kept for the next run)",
        Style::default().fg(DIM),
    )));
    lines
}

// -------------------------------------------------------------------- popups

fn popup(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let width = area.width.clamp(MIN_WIDTH, 72.max(MIN_WIDTH));
    let height = (lines.len() as u16 + 2).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .title_top(titled(title))
        .border_style(Style::default().fg(WIRE));
    frame.render_widget(Clear, box_area);
    frame.render_widget(Paragraph::new(lines).block(block), box_area);
}

fn field(name: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {name:<16}"), Style::default().fg(DIM)),
        Span::raw(value),
    ])
}

fn detail_lines(record: &RequestRecord) -> Vec<Line<'static>> {
    let optional = |value: Option<f64>| {
        value
            .map(human_time)
            .unwrap_or_else(|| "not measured".to_string())
    };
    vec![
        field("when", record.stamp.clone()),
        field("model", record.model.clone()),
        field(
            "request",
            format!("{} {} -> {}", record.method, record.path, record.status),
        ),
        field(
            "connection",
            match record.handshake() {
                None => "reused from the pool".to_string(),
                Some(total) => format!(
                    "dialed in {} (dns {}, tcp {}, tls {})",
                    human_time(total),
                    optional(record.dns),
                    optional(record.tcp),
                    optional(record.tls),
                ),
            },
        ),
        field(
            "upload",
            format!(
                "{} -> {} ({}, {})",
                human(record.body_len),
                human(record.wire_len),
                record.coding.name().unwrap_or("identity"),
                ratio(record.body_len, record.wire_len),
            ),
        ),
        field("upload acked in", optional(record.upload)),
        field("ttfb", human_time(record.ttfb)),
        field(
            "download",
            format!(
                "{} on the wire -> {} decoded ({}){}",
                human(record.received_wire),
                human(record.received),
                record.upstream_encoding,
                match record.received_agent {
                    agent if agent > 0 && agent != record.received => format!(
                        " -> {} sent to the agent ({})",
                        human(agent),
                        ratio(record.received, agent)
                    ),
                    _ => String::new(),
                },
            ),
        ),
        field("download took", optional(record.download)),
        field(
            "tokens",
            match record.usage {
                None => "not reported".to_string(),
                Some(usage) => format!(
                    "{} in{} -> {} out{}",
                    usage.prompt,
                    usage
                        .cached
                        .map(|cached| format!(" ({cached} cached)"))
                        .unwrap_or_default(),
                    usage.completion,
                    usage
                        .reasoning
                        .map(|reasoning| format!(" ({reasoning} reasoning)"))
                        .unwrap_or_default(),
                ),
            },
        ),
        field(
            "ended",
            if record.complete {
                "upstream body finished".to_string()
            } else {
                "cut short (agent abort, error or read timeout)".to_string()
            },
        ),
    ]
}

fn help_lines() -> Vec<Line<'static>> {
    [
        ("q / ctrl-c", "quit (confirms while a stream is live)"),
        ("j / ↓ / k / ↑", "move the cursor, wheel scrolls too"),
        ("PgDn / PgUp", "move by a page"),
        ("g / G", "oldest line / back to following"),
        ("Enter", "details of the highlighted request"),
        ("f", "requests in flight, live"),
        ("e", "only 4xx/5xx, cut streams and warnings"),
        ("m", "cycle the model filter"),
        ("u", "tokens and cost per model, by window"),
        ("t", "1s / 10s / 60s traffic buckets"),
        ("c", "choose what a request line shows"),
        ("?", "close this"),
    ]
    .into_iter()
    .map(|(key, what)| {
        Line::from(vec![
            Span::styled(format!(" {key:<16}"), Style::default().fg(WIRE)),
            Span::raw(what),
        ])
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forwarder::{CompressionIssue, StatsView};
    use crate::telemetry::Event;
    use crate::usage::Usage;
    use crate::watch;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn record(model: &str, status: u16) -> RequestRecord {
        RequestRecord {
            stamp: "23:41:02".to_string(),
            model: model.to_string(),
            method: http::Method::POST,
            path: "/v1/chat/completions".to_string(),
            status,
            dns: None,
            tcp: None,
            tls: None,
            body_len: 705_000,
            wire_len: 211_000,
            coding: Coding::Zstd,
            upload: Some(0.31),
            ttfb: 17.44,
            received: 521,
            received_wire: 63,
            // No agent figure: the shared fixture predates the hop encoding, so
            // the header stays the shape every other test asserts.
            received_agent: 0,
            upstream_encoding: "gzip".to_string(),
            agent_encoding: None,
            download: Some(0.018),
            complete: true,
            usage: None,
            flight: None,
        }
    }

    fn header() -> Header {
        Header {
            listen: "http://127.0.0.1:8787/v1".to_string(),
            coding: "zstd L9".to_string(),
            watching: None,
        }
    }

    fn screen(width: u16, height: u16, state: &State) -> String {
        screen_of(width, height, state, &header())
    }

    fn screen_of(width: u16, height: u16, state: &State, header: &Header) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state, header)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The in-flight dialog alone, cut out of the dashboard around it.
    fn dialog(screen: &str) -> String {
        let rows: Vec<Vec<char>> = screen.lines().map(|row| row.chars().collect()).collect();
        let (top, left) = rows
            .iter()
            .enumerate()
            .find_map(|(at, row)| {
                let text: String = row.iter().collect();
                let byte = text.find("┌ in flight")?;
                Some((at, text[..byte].chars().count()))
            })
            .unwrap_or_else(|| panic!("the dialog is not up:\n{screen}"));
        let right = left + rows[top][left..].iter().position(|c| *c == '┐').unwrap();
        let bottom = (top..rows.len())
            .find(|at| rows[*at].get(left) == Some(&'└'))
            .unwrap();
        rows[top..=bottom]
            .iter()
            .map(|row| row[left..=right].iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn live_requests_advance_and_leave_before_the_next_sample() {
        let mut config = crate::cli::Args::default().forwarder_config();
        config.telemetry = Arc::new(crate::telemetry::Telemetry::default());
        let router = crate::router::Router::build(
            &config,
            &[("alpha".to_string(), "https://example.invalid".to_string())],
            None,
        )
        .unwrap();
        let registry = router.telemetry().flights();
        let flight = registry.begin(
            "alpha",
            &http::Method::POST,
            "/v1/messages?secret=hidden",
            100,
        );
        let mut state = State::new();
        state.flights_open = true;
        state.tick(&router);
        assert_eq!(state.flights.len(), 1);
        assert!(dialog(&screen(140, 30, &state)).contains("upload"));

        let clock = Arc::new(crate::clock::PhaseClock::new());
        clock.mark_upload_started();
        flight.attempt(&clock, 50, Coding::Zstd);
        clock.mark_upload_finished();
        state.tick(&router);
        assert_eq!(state.flights[0].phase, Phase::Prefill);
        assert!(dialog(&screen(140, 30, &state)).contains("prefill"));

        flight.responded(200, 0.5, 50, Coding::Zstd);
        flight.progress(2048, 1024, 2048);
        state.tick(&router);
        let out = dialog(&screen(140, 30, &state));
        for text in [
            "in flight · 1",
            "stream",
            "500ms",
            "2KB",
            "POST /v1/messages",
        ] {
            assert!(out.contains(text), "{out}");
        }
        assert!(!out.contains("secret"), "{out}");
        flight.progress(4096, 2048, 4096);
        state.tick(&router);
        assert!(dialog(&screen(140, 30, &state)).contains("4KB"));

        // An unrelated completion cannot remove the live row. The matching
        // record does, even when the 250ms sampling clock has not fired yet.
        state.push(Event::Request(Arc::new(record("other", 200))));
        assert_eq!(state.flights.len(), 1);
        registry.end(flight.id());
        let mut completed = record("alpha", 200);
        completed.flight = Some(flight.id());
        state.push(Event::Request(Arc::new(completed)));
        assert!(state.flights.is_empty());
        let out = dialog(&screen(140, 30, &state));
        assert!(out.contains("in flight · 0"), "{out}");
        assert!(out.contains("nothing in flight"), "{out}");
        assert!(!out.contains("POST /v1/messages"), "{out}");
        assert_eq!(state.len(), 2);
        state.tick(&router);
        assert!(state.flights.is_empty());

        // Cancellation has no completion record; the next sample removes it.
        let cancelled = registry.begin("alpha", &http::Method::GET, "/v1/models", 0);
        state.tick(&router);
        assert_eq!(state.flights.len(), 1);
        registry.end(cancelled.id());
        state.tick(&router);
        assert!(state.flights.is_empty());
    }

    /// Closed, the dialog costs the dashboard nothing: requests coming and
    /// going move no pane. Open, it lists the oldest first, as many as the
    /// terminal has rows for, and counts the rest.
    #[test]
    fn flights_wait_in_a_dialog_and_leave_the_layout_alone() {
        let registry = crate::flights::Flights::default();
        for index in 0..8 {
            registry.begin(
                &format!("live-{index}"),
                &http::Method::POST,
                "/v1/messages",
                100,
            );
        }
        let mut state = populated();
        // Everything under the HUD, whose clock and `live` count move anyway.
        let below_hud = |screen: String| {
            screen
                .lines()
                .skip(HUD_HEIGHT as usize)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let idle = below_hud(screen(140, 44, &state));
        state.flights = registry.views();
        state.totals.in_flight = state.flights.len() as u64;
        let busy = screen(140, 44, &state);
        assert!(busy.contains("live 8"), "{busy}");
        assert!(!busy.contains("live-0"), "{busy}");
        assert_eq!(below_hud(busy), idle);

        state.flights_open = true;
        for (width, height, shown) in [(140, 44, 8), (80, 24, 8), (44, 10, 5)] {
            let out = dialog(&screen(width, height, &state));
            if shown < 8 {
                assert!(
                    out.contains(&format!("in flight · 8 · +{} more", 8 - shown)),
                    "{out}"
                );
                assert!(!out.contains(&format!("live-{shown}")), "{out}");
            } else {
                assert!(out.contains("in flight · 8"), "{out}");
                assert!(!out.contains("more"), "{out}");
            }
            assert!(out.contains("live-0"), "{out}");
            assert!(out.contains(&format!("live-{}", shown - 1)), "{out}");
            assert!(out.contains("f / esc close"), "{out}");
        }
        // Open, it holds still too: a request arriving or leaving moves the
        // bottom border, not the title a reader is on.
        let title_row = |state: &State| {
            screen(140, 44, state)
                .lines()
                .position(|line| line.contains("┌ in flight"))
        };
        let full = title_row(&state);
        let all = std::mem::take(&mut state.flights);
        state.flights = all[..1].to_vec();
        assert_eq!(title_row(&state), full);
        state.flights.clear();
        assert_eq!(title_row(&state), full);
        state.flights = all;

        // A narrow dialog keeps model, phase, age and bytes.
        assert!(dialog(&screen(140, 44, &state)).contains("POST /v1/messages"));
        assert!(!dialog(&screen(44, 24, &state)).contains("POST"));

        // Filters and scrolling are the event pane's; the flights ignore them.
        state.set_filter(crate::tui::state::Filter::Trouble);
        state.scroll(-1);
        assert!(dialog(&screen(80, 24, &state)).contains("live-0"));
        for (width, height) in [(1, 1), (44, 1), (44, 9), (44, 10), (44, 13), (200, 5)] {
            screen(width, height, &state);
        }
        state.tick_recorded(&watch::Window::default());
        assert!(state.flights.is_empty());
        assert!(dialog(&screen(140, 44, &state)).contains("nothing in flight"));
    }

    #[test]
    fn the_flights_dialog_fits_its_columns_and_sits_over_the_middle() {
        let registry = crate::flights::Flights::default();
        for index in 0..20 {
            registry.begin(
                &format!("live-{index}"),
                &http::Method::POST,
                "/v1/messages",
                100,
            );
        }
        let mut state = populated();
        state.flights_open = true;
        let all = registry.views();
        // Rows above the box, rows below it, and columns either side.
        let margins = |width: u16, height: u16, state: &State| {
            let screen = screen(width, height, state);
            let rows: Vec<&str> = screen.lines().collect();
            let top = rows
                .iter()
                .position(|row| row.contains("┌ in flight"))
                .unwrap();
            let out = dialog(&screen);
            let left = rows[top]
                .split("┌ in flight")
                .next()
                .unwrap()
                .chars()
                .count();
            let across = out.lines().next().unwrap().chars().count();
            let below = height as usize - top - out.lines().count();
            (top, below, left, width as usize - left - across)
        };

        // As wide as its columns, and no wider: the route ends at the border,
        // and a wide terminal gets dashboard either side, not a blank route.
        state.flights = all[..8].to_vec();
        let out = dialog(&screen(140, 44, &state));
        assert!(
            out.lines().nth(2).unwrap().ends_with("POST /v1/messages │"),
            "{out}"
        );
        for (width, height) in [(140, 44), (100, 30), (80, 24)] {
            let (top, below, left, right) = margins(width, height, &state);
            assert!(left >= FLIGHTS_GUTTER as usize, "{width}x{height}");
            assert!(left.abs_diff(right) <= 1, "{width}x{height}");
            // Eight rows is as many as it is placed for: centred.
            assert!(top >= HUD_HEIGHT as usize, "{width}x{height}");
            assert!(top.abs_diff(below) <= 1, "{width}x{height}");
        }
        // Fewer keep the title where eight put it, a little above the middle;
        // more than that are centred as they come.
        let (eight, ..) = margins(140, 44, &state);
        state.flights.truncate(1);
        let (top, below, ..) = margins(140, 44, &state);
        assert_eq!(top, eight);
        assert!(top < below);
        state.flights = all.clone();
        let (top, below, ..) = margins(140, 44, &state);
        assert!(top < eight, "{top}");
        assert!(top.abs_diff(below) <= 1);

        // A long name widens it into the spare room, and one that still does
        // not fit says so rather than passing for a shorter route.
        state.flights = all[..1].to_vec();
        state.flights[0].model = "claude-haiku-4-5-20251001".to_string();
        state.flights[0].path = "/v1/messages/count_tokens".to_string();
        let out = dialog(&screen(140, 44, &state));
        assert!(out.contains("claude-haiku-4-5-20251001"), "{out}");
        assert!(out.contains("POST /v1/messages/count_tokens │"), "{out}");
        let out = dialog(&screen(80, 24, &state));
        assert!(out.contains("claude-haiku-4-5-20251001"), "{out}");
        assert!(out.contains("POST /v1/message… │"), "{out}");
        assert!(!out.contains("POST /v1/messages "), "{out}");
    }

    #[test]
    fn attached_live_counts_preserve_history_and_failures_clear_the_table() {
        let mut state = State::new();
        state.recorded = true;
        state.push(Event::Request(Arc::new(record("alpha", 200))));
        state.tick_recorded(&watch::Window::default());
        let bytes = state.totals.body_bytes;
        let registry = crate::flights::Flights::default();
        for _ in 0..205 {
            registry.begin("alpha", &http::Method::GET, "/v1/models", 0);
        }
        let bounded = registry.snapshot(crate::live::MAX_FLIGHTS);
        let mut snapshot = crate::live::Snapshot {
            version: 1,
            instance: "a".repeat(32),
            listen: "127.0.0.1:8789".parse().unwrap(),
            total: bounded.total,
            models: bounded.models,
            flights: bounded.flights,
        };
        state.apply_live(Some(&snapshot));
        assert_eq!(state.totals.requests, 1);
        assert_eq!(state.totals.body_bytes, bytes);
        assert_eq!(state.models[0].view.in_flight, 205);
        let out = screen(140, 44, &state);
        assert!(out.contains("live 205"), "{out}");
        // The snapshot's count, not its capped list: 27 rows fit, and the
        // title owns up to the other 178.
        state.flights_open = true;
        let out = dialog(&screen(140, 44, &state));
        assert!(out.contains("in flight · 205 · +178 more"), "{out}");

        state.apply_live(None);
        assert!(state.flights.is_empty());
        assert_eq!(state.models[0].view.in_flight, 0);
        // A failed snapshot is not an empty one, in the dialog or the HUD.
        let out = dialog(&screen(44, 14, &state));
        assert!(
            out.contains("unavailable: the server did not answer"),
            "{out}"
        );
        assert!(!out.contains("nothing in flight"), "{out}");
        state.flights_open = false;
        assert!(screen(44, 14, &state).contains("in flight unavailable"));
        // Recorded events and filters keep working while the socket is down.
        state.push(Event::Request(Arc::new(record("alpha", 500))));
        state.tick_recorded(&watch::Window::default());
        state.apply_live(None);
        state.set_filter(crate::tui::state::Filter::Trouble);
        assert_eq!(state.totals.requests, 2);
        assert!(screen(140, 44, &state).contains("500"));

        snapshot.instance = "b".repeat(32);
        snapshot.total = 0;
        snapshot.models.clear();
        snapshot.flights.clear();
        state.apply_live(Some(&snapshot));
        let out = screen(140, 44, &state);
        assert!(!out.contains("in flight unavailable"), "{out}");
        assert!(out.contains("live 0"), "{out}");
        assert_eq!(state.totals.requests, 2);
    }

    #[test]
    fn flights_flag_slow_prefill_and_stalled_streams_at_the_web_thresholds() {
        let registry = crate::flights::Flights::default();
        let flight = registry.begin("alpha", &http::Method::GET, "/v1/models", 0);
        let mut view = flight.view();
        view.age = 30.0;
        assert_eq!(flight_phase(&view).0, "prefill");
        view.age = 30.1;
        assert_eq!(flight_phase(&view).0, "slow prefill");
        let mut state = State::new();
        state.flights.push(view.clone());
        state.totals.in_flight = 1;
        state.flights_open = true;
        assert!(dialog(&screen(44, 14, &state)).contains("slow prefill"));
        // Closed, the dialog still gets the word out: the HUD says which
        // requests stopped moving beside the count they are part of.
        state.flights_open = false;
        let out = screen(44, 14, &state);
        assert!(out.contains("live 1 (1 slow)"), "{out}");
        let slow = view.clone();

        view.phase = Phase::Stream;
        view.idle = 60.0;
        assert_eq!(flight_phase(&view).0, "stream");
        state.flights = vec![view.clone()];
        assert!(!screen(44, 14, &state).contains("live 1 ("));
        view.idle = 60.1;
        assert_eq!(flight_phase(&view), ("stalled", BAD));
        state.flights = vec![view.clone()];
        state.flights_open = true;
        assert!(dialog(&screen(44, 14, &state)).contains("stalled"));
        state.flights_open = false;
        assert!(screen(44, 14, &state).contains("live 1 (1 stalled)"));

        state.flights = vec![view, slow];
        state.totals.in_flight = 2;
        assert!(screen(80, 24, &state).contains("live 2 (1 stalled, 1 slow)"));
    }

    /// The model table's header row, which is where the shed columns show.
    fn table_header(screen: &str) -> String {
        screen
            .lines()
            .find(|line| line.starts_with("│ model"))
            .unwrap_or_default()
            .trim_end_matches(['│', ' '])
            .to_string()
    }

    /// A line's spans glued back together, presentation stripped.
    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// One popup field's value, whatever the column padding came out as.
    fn field_text(record: &RequestRecord, name: &str) -> String {
        detail_lines(record)
            .iter()
            .map(text)
            .find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some(name)).then(|| words.collect::<Vec<_>>().join(" "))
            })
            .unwrap_or_else(|| panic!("no {name:?} field in the popup"))
    }

    /// The counts are a column like any other, and the popup is where the
    /// detail they are cut down to lives. An answer the engine said nothing
    /// about leaves the line exactly as it was.
    #[test]
    fn the_counts_the_engine_reported_ride_the_line_and_the_popup() {
        let quiet = record("model-zeta", 200);
        assert!(!text(&request_line(&quiet, Columns::ALL)).contains("tok"));
        assert!(
            field_text(&quiet, "tokens") == "not reported",
            "the popup still says what happened"
        );

        let mut counted = record("model-zeta", 200);
        counted.usage = Some(Usage {
            prompt: 18_234,
            cached: Some(18_200),
            completion: 891,
            reasoning: Some(742),
        });
        let line = text(&request_line(&counted, Columns::ALL));
        assert!(line.ends_with(" tok 18.2K(18.2K cached)→891"), "{line}");
        assert_eq!(
            field_text(&counted, "tokens"),
            "18234 in (18200 cached) -> 891 out (742 reasoning)"
        );

        // Counts reported without the cache breakdown draw both counts and no
        // parentheses: there is nothing to put in them.
        let mut bare = counted.clone();
        bare.usage = Some(Usage {
            prompt: 18_234,
            cached: None,
            completion: 891,
            reasoning: None,
        });
        let line = text(&request_line(&bare, Columns::ALL));
        assert!(line.ends_with(" tok 18.2K→891"), "{line}");
    }

    #[test]
    fn a_token_count_reads_whole_until_it_does_not_have_to() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(891), "891");
        assert_eq!(human_count(9_999), "9999");
        assert_eq!(human_count(10_000), "10.0K");
        assert_eq!(human_count(18_234), "18.2K");
        assert_eq!(human_count(182_000), "182K");
        assert_eq!(human_count(1_250_000), "1.2M");
    }

    fn populated() -> State {
        let mut state = State::new();
        state.viewport = 4;
        state.push(Event::Request(Arc::new(record("model-alpha", 200))));
        state.push(Event::Request(Arc::new(record("model-zeta", 500))));
        state.push(Event::Log {
            stamp: "23:41:03".to_string(),
            level: Level::Warning,
            message: "415 for zstd: resending identity".to_string(),
        });
        state
    }

    #[test]
    fn the_download_row_pairs_the_answer_with_what_the_agent_got() {
        let mut state = State::new();
        state.viewport = 4;
        // The tally only runs for the rows this state replays.
        state.recorded = true;
        // An encoded hop: a megabyte of answer left as a tenth of one.
        let mut paired = record("model-zeta", 200);
        paired.received = 1_000_000;
        paired.received_agent = 100_000;
        state.push(Event::Request(Arc::new(paired)));
        state.tick_recorded(&crate::watch::Window::default());
        let out = screen(140, 44, &state);
        assert!(out.contains("DOWNLOAD"), "{out}");
        assert!(out.contains("agent"), "the hop's own stat:\n{out}");
        assert!(out.contains("-90%"), "the hop's saving:\n{out}");
        // Both surfaces read like the upload side: one glued arrow, no gaps,
        // and a saving that is a size rather than a repeated pair.
        assert!(out.contains("down 977KB→98KB -90%"), "{out}");
        assert!(out.contains("agent 98KB (-90%)"), "{out}");

        // No agent figure — an identity hop, or a row from before the column —
        // keeps the single-size row every other test asserts.
        let plain = screen(140, 44, &populated());
        assert!(!plain.contains("agent"), "{plain}");
    }

    #[test]
    fn compression_diagnostics_fit_without_hiding_the_coding() {
        let mut state = populated();
        state.models.push(crate::tui::state::ModelRow {
            name: "model-alpha".into(),
            view: StatsView::default(),
        });
        let view = &mut state.models[0].view;
        view.coding = Coding::Zstd;
        view.dict = false;
        view.dict_backoff_reason = Some(CompressionIssue::HashMismatch);
        view.dict_backoff_secs = 600;
        let wide = screen(STATUS_TABLE, 44, &state);
        assert!(wide.contains("compression status"), "{wide}");
        assert!(wide.contains("dcz hash mismatch; 600s"), "{wide}");
        assert!(wide.contains("zstd"), "{wide}");

        state.models[0].view.dict_backoff_secs = 0;
        let expired = screen(STATUS_TABLE, 44, &state);
        assert!(
            expired.contains("dcz hash mismatch; probe due"),
            "{expired}"
        );
        let narrow = screen(78, 44, &state);
        assert!(!narrow.contains("compression status"), "{narrow}");
        assert!(table_header(&narrow).ends_with("saved"), "{narrow}");

        state.models[0].view = StatsView::default();
        let historical = screen(STATUS_TABLE, 44, &state);
        assert!(!historical.contains("probe failed"), "{historical}");
        assert!(!historical.contains("backoff"), "{historical}");
        state.models[0].view.identity_reason = Some(CompressionIssue::EncodingRefused);
        state.models[0].view.identity_backoff_secs = 420;
        let identity = screen(STATUS_TABLE, 44, &state);
        assert!(identity.contains("identity"), "{identity}");
        assert!(identity.contains("415 backoff; 420s"), "{identity}");
    }

    #[test]
    fn a_roomy_terminal_shows_every_panel() {
        let out = screen(140, 44, &populated());
        assert!(out.contains("portway"), "{out}");
        assert!(out.contains("REQUESTS"), "{out}");
        assert!(out.contains("UPLOAD"), "{out}");
        assert!(out.contains("request body / turn"), "{out}");
        assert!(out.contains("socket bytes/s"), "{out}");
        assert!(out.contains("events"), "{out}");
        // Known routes read as `../<tail>` with the trunk elided; a pooled
        // connection is silent — only a fresh dial's line carries the mark.
        assert!(
            out.contains("model-alpha POST ../completions 688KB"),
            "{out}"
        );
        assert!(!out.contains("reused"), "{out}");
        assert!(out.contains("FOLLOW"), "{out}");
        assert!(out.contains("model-alpha"), "{out}");
        assert!(out.contains("415 for zstd"), "{out}");
        assert!(out.contains("quit"), "{out}");
    }

    #[test]
    fn a_short_terminal_drops_the_charts_then_the_table() {
        let mut state = populated();
        state.tick(
            &crate::router::Router::build(
                &crate::cli::Args::default().forwarder_config(),
                &[(
                    "model-zeta".to_string(),
                    "https://example.invalid".to_string(),
                )],
                None,
            )
            .unwrap(),
        );

        let medium = screen(140, 14, &state);
        assert!(medium.contains("models"), "{medium}");
        assert!(!medium.contains("socket bytes/s"), "{medium}");

        // Narrow: the tail columns go so the model names stay whole.
        let narrow = screen(78, 20, &state);
        assert!(table_header(&narrow).ends_with("saved"), "{narrow}");
        assert!(narrow.contains("model-zeta"), "{narrow}");
        assert!(table_header(&medium).contains("abort"), "{medium}");

        let short = screen(140, 11, &state);
        assert!(!short.contains("request body / turn"), "{short}");
        assert!(short.contains("events"), "{short}");
        assert!(short.contains("REQUESTS"), "{short}");
    }

    #[test]
    fn a_narrow_terminal_stacks_the_charts() {
        let out = screen(78, 40, &populated());
        assert!(out.contains("request body / turn"), "{out}");
        assert!(out.contains("socket bytes/s"), "{out}");
    }

    #[test]
    fn the_last_chart_standing_is_the_one_that_moves() {
        // Narrow enough to stack, with room for exactly one of the two.
        let out = screen(78, 16, &populated());
        assert!(out.contains("socket bytes/s"), "{out}");
        assert!(!out.contains("request body / turn"), "{out}");
    }

    #[test]
    fn an_unexpected_route_keeps_its_verb_and_path() {
        let mut state = populated();
        let mut probe = record("model-zeta", 200);
        probe.method = http::Method::GET;
        probe.path = "/metrics-tiny?model=model-zeta".to_string();
        probe.body_len = 0;
        state.push(Event::Request(Arc::new(probe)));
        let out = screen(140, 44, &state);
        assert!(out.contains("GET /metrics-tiny"), "{out}");
        // The query repeats the model span; that pair is for the popup.
        assert!(!out.contains("model=other_model"), "{out}");
    }

    #[test]
    fn a_fresh_dial_announces_itself_and_a_pooled_connection_stays_silent() {
        let mut state = populated();
        let mut fresh = record("model-zeta", 200);
        fresh.dns = Some(0.004);
        fresh.tcp = Some(0.028);
        fresh.tls = Some(0.061);
        state.push(Event::Request(Arc::new(fresh)));
        // One mark glued to the route, and no number beside it: the pooled
        // turns before it spell their route and say nothing else. No space in
        // the mark's own span — that would cost two columns, not one.
        let out = screen(140, 44, &state);
        let line = out
            .lines()
            .find(|line| line.contains("POST ../completions."))
            .expect("the fresh dial's line");
        assert!(line.contains("POST ../completions. 688KB→206KB"), "{line}");
        assert!(!line.contains(['●', '•']), "{line}");
        assert!(!line.contains("dial"), "{line}");

        // The cost itself is one keypress away, not gone.
        state.detail = true;
        let popup = screen(140, 44, &state);
        assert!(
            popup.contains("dialed in 93ms (dns 4ms, tcp 28ms, tls 61ms)"),
            "{popup}"
        );
    }

    #[test]
    fn a_tiny_terminal_says_so_instead_of_panicking() {
        // Too narrow, and too short even though it is wide.
        assert!(screen(10, 3, &populated()).contains("too small"));
        assert!(screen(200, 5, &State::new()).contains("too small"));
        // One cell: nothing to say, but it must still not panic.
        screen(1, 1, &State::new());
    }

    #[test]
    fn the_popups_render_over_the_dashboard() {
        let mut state = populated();
        state.help = true;
        let out = screen(140, 44, &state);
        assert!(out.contains("cycle the model filter"), "{out}");
        assert!(out.contains("requests in flight, live"), "{out}");

        state.help = false;
        state.detail = true;
        state.scroll(-1);
        let out = screen(140, 44, &state);
        assert!(out.contains("upload acked in"), "{out}");
        assert!(
            out.contains("cut short") || out.contains("upstream body finished"),
            "{out}"
        );
    }

    /// A usage screen with a day in it: model-epsilon dominating through cache reads,
    /// and a model whose engine reports no cache detail at all. The numbers are
    /// the ones a real day produced.
    fn usage_state() -> State {
        let priced_model = spend::Row {
            model: "model-epsilon".to_string(),
            requests: 869,
            prompt: 112_546_008,
            cached: 111_147_008,
            unreported: 0,
            completion: 478_284,
            reasoning: 211_829,
            charge: Some(spend::Charge {
                input: 2.3783,
                cache_read: 18.8950,
                output: 4.0654,
            }),
        };
        let flash = spend::Row {
            model: "model-alpha".to_string(),
            requests: 389,
            prompt: 42_881_773,
            cached: 0,
            unreported: 389,
            completion: 240_377,
            reasoning: 126_559,
            charge: Some(spend::Charge {
                input: 3.8594,
                cache_read: 0.0,
                output: 0.0721,
            }),
        };
        let mut state = State::new();
        state.usage_open = true;
        state.usage = Some(spend::Table {
            since: 1_789_941_600.0,
            until: 1_790_008_865.0,
            total: spend::Row {
                model: "total".to_string(),
                requests: 1_258,
                prompt: 155_427_781,
                cached: 111_147_008,
                unreported: 389,
                completion: 718_661,
                reasoning: 338_388,
                charge: Some(spend::Charge {
                    input: 6.2377,
                    cache_read: 18.8950,
                    output: 4.1375,
                }),
            },
            rows: vec![priced_model, flash],
            unpriced: 0,
            blind: 111,
            cut: 94,
        });
        state
    }

    #[test]
    fn the_usage_screen_shows_the_day_by_model() {
        let state = usage_state();

        let out = screen(120, 20, &state);
        assert!(out.contains("usage — "), "{out}");
        assert!(out.contains("model-epsilon"), "{out}");
        // The window selector sits above the table, with the one in force
        // marked and the keys that move it named.
        assert!(out.contains("today"), "{out}");
        assert!(out.contains("yesterday"), "{out}");
        assert!(out.contains("7 days"), "{out}");
        assert!(out.contains("30 days"), "{out}");
        assert!(out.contains("←→ window"), "{out}");
        // 112,546,008 reads as `113M` at a glance: three digits already say
        // the scale.
        assert!(out.contains("113M"), "{out}");
        assert!(out.contains("98.8%"), "{out}");
        assert!(out.contains("478K"), "{out}");
        assert!(
            out.contains("$25.34"),
            "priced_model's share of the day:\n{out}"
        );
        assert!(out.contains("$29.27"), "the day's total:\n{out}");
        // The column that matters is present with the money split out beside
        // it, and the row that could not report a cache says so.
        assert!(out.contains("cache$"), "{out}");
        assert!(out.contains("model-alpha"), "{out}");
        assert!(out.contains("n/a"), "an unknown rate is not 0.0%:\n{out}");

        assert!(out.contains("111 request(s)"), "{out}");
        assert!(
            out.contains("model-alpha: no cache detail reported"),
            "{out}"
        );
        assert!(out.contains("they are not a bill"), "{out}");
        assert!(
            out.contains("338K of the 719K output tokens were thinking"),
            "the day's thinking share:\n{out}"
        );

        // A heading is drawn the way the cells under it are, so the column has one
        // right edge rather than two: the header's last cell ends where the
        // day's total does.
        let row = |needle: &str| {
            out.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{out}"))
        };
        let header = row("reqs");
        for (heading, value) in [("total$", "$25.34"), ("cached", "111M")] {
            let head = header.find(heading).expect("heading") + heading.len();
            let cell = row("model-epsilon").find(value).expect("value") + value.len();
            assert_eq!(head, cell, "{heading} is not over its column:\n{out}");
        }

        // A narrower terminal keeps the counts and the total and drops the
        // breakdown; what is left still says what the day cost.
        let compact = screen(90, 20, &state);
        assert!(compact.contains("model-epsilon"), "{compact}");
        assert!(compact.contains("$25.34"), "{compact}");
        assert!(compact.contains("total$"), "{compact}");
        assert!(!compact.contains("cache$"), "{compact}");

        // Below that there is no room for the columns, so it says so rather
        // than drawing half a table.
        assert!(screen(60, 20, &state).contains("too small"));
    }

    #[test]
    fn the_costs_popup_shows_the_breakdown_by_source() {
        let mut state = usage_state();
        // Closed, the cost breakdown is nowhere on the screen: it is a
        // question, not a column.
        let out = screen(120, 20, &state);
        assert!(!out.contains("cost by source"), "{out}");
        assert!(out.contains("p costs"), "the hint is always there:\n{out}");

        state.usage_rates = true;
        let out = screen(120, 20, &state);
        assert!(out.contains("cost by source"), "{out}");
        assert!(out.contains("model-epsilon"), "{out}");
        assert!(
            out.contains("total$"),
            "the window's money, per upstream:\n{out}"
        );
        // The three sources of a row that is in play, beside what that upstream
        // cost in this window.
        let lines: Vec<String> = cost_lines(state.usage.as_ref().unwrap())
            .iter()
            .map(text)
            .collect();
        let priced_model = lines
            .iter()
            .find(|line| line.contains("model-epsilon"))
            .expect("priced_model has a cost row");
        for cell in ["$2.38", "$18.89", "$4.07", "$25.34"] {
            assert!(
                priced_model.contains(cell),
                "{cell} missing from {priced_model:?}"
            );
        }
        // A model with no cache detail still has its uncached prompt charged
        // at the input rate; its cache column is a real zero, not a dash.
        let flash = lines
            .iter()
            .find(|line| line.contains("model-alpha"))
            .expect("flash has a cost row");
        for cell in ["$3.86", "$0", "$0.072", "$3.93"] {
            assert!(flash.contains(cell), "{cell} missing from {flash:?}");
        }
        // The pooled total is the last row, bold and summed the same way.
        let total = lines
            .iter()
            .find(|line| line.starts_with(" total"))
            .expect("total row exists");
        for cell in ["$6.24", "$18.89", "$4.14", "$29.27"] {
            assert!(total.contains(cell), "{cell} missing from {total:?}");
        }
    }

    #[test]
    fn a_usage_screen_with_nothing_to_read_says_why() {
        let mut state = State::new();
        state.usage_open = true;
        state.usage_error =
            Some("no database at /tmp/x: nothing has been recorded yet".to_string());
        let out = screen(120, 20, &state);
        assert!(out.contains("nothing has been recorded yet"), "{out}");
    }

    #[test]
    fn ratios_survive_zero_traffic() {
        assert_eq!(ratio(0, 0), "-");
        assert_eq!(ratio(100, 0), "-");
        assert_eq!(ratio(1000, 250), "-75%");
        // Truncated, never rounded up to a -100% that claims nothing was sent.
        assert_eq!(ratio(547_000, 700), "-99%");
        // An incompressible body can come back bigger; the bar is clamped, so
        // the label must not claim a negative saving.
        assert_eq!(ratio(100, 200), "-0%");
    }

    /// The dashboard watching a forwarder it did not start: the model table is
    /// the tally of the rows that were replayed, the HUD says what window it is
    /// reading, and the counters a finished row cannot carry stay at zero.
    #[test]
    fn a_watched_forwarder_is_drawn_from_the_rows_it_recorded() {
        let mut state = State::new();
        state.recorded = true;
        state.push(Event::Request(Arc::new(record("model-zeta", 200))));

        let mut refused = record("model-zeta", 502);
        refused.coding = Coding::None;
        state.push(Event::Request(Arc::new(refused)));
        // The two counters the request path only wrote to the log.
        for (level, message) in [
            (Level::Warning, "415 for zstd: resending identity"),
            (
                Level::Info,
                "POST /v1/chat/completions -> agent left before the first byte",
            ),
        ] {
            state.push(Event::Log {
                stamp: "23:41:02".to_string(),
                level,
                message: message.to_string(),
            });
        }
        state.tick_recorded(&watch::Window {
            traffic: vec![(1024, 512); 3600],
            coverage: Duration::from_secs(1_830),
        });

        assert_eq!(state.totals.requests, 2);
        assert_eq!(state.totals.encoded, 1);
        assert_eq!(state.totals.upstream_errors, 1);
        assert_eq!(state.totals.aborts, 1);
        assert_eq!(state.totals.retried_identity, 1);
        // No row can say either, so a window shows none of it.
        assert_eq!(state.totals.in_flight, 0);
        assert_eq!(state.totals.idle_conns, 0);

        let watching = Header {
            coding: String::new(),
            watching: Some(watch::WINDOW),
            ..header()
        };
        let out = screen_of(140, 44, &state, &watching);
        // The slot the compressor has in a live run, and how far back this one
        // reaches: an hour window, half an hour of rows to fill it.
        assert!(out.contains("watching 1h"), "{out}");
        assert!(out.contains("up 00:30:30"), "{out}");
        // The compressor slot is gone from the header line: nothing was
        // negotiated here, and no flag of this process is being honoured.
        let title = out.lines().next().unwrap();
        assert!(!title.contains("coding"), "{title}");
        assert!(out.contains("total 2"), "{out}");
        assert!(out.contains("encoded 1/2"), "{out}");
        assert!(out.contains("415-retry 1"), "{out}");
        assert!(out.contains("aborts 1"), "{out}");
        // The window is what the traffic chart draws: its peak is read off
        // the buckets the sample handed over, not off the events.
        let up = out
            .lines()
            .find(|line| line.contains("↑ up"))
            .expect("the up row");
        assert!(up.contains("1KB"), "{up}");
        let down = out
            .lines()
            .find(|line| line.contains("↓ down"))
            .expect("the down row");
        assert!(down.contains("0.5KB"), "{down}");
        // The model row adds up to the same totals.
        let row = out
            .lines()
            .find(|line| line.contains("model-zeta "))
            .expect("the model row");
        assert!(row.contains(" 2 "), "{row}");
    }

    /// A line carries the columns it was given and no others: turning one off
    /// closes its gap rather than leaving a hole, and the two one-cell marks
    /// take the place of a separator instead of adding one.
    #[test]
    fn a_request_line_carries_the_columns_it_was_given() {
        let mut record = record("model-zeta", 200);
        record.usage = Some(Usage {
            prompt: 18_234,
            cached: Some(18_200),
            completion: 891,
            reasoning: Some(742),
        });
        assert_eq!(
            text(&request_line(
                &record,
                Columns::parse("time,tokens").unwrap()
            )),
            "23:41:02 tok 18.2K(18.2K cached)→891"
        );

        let narrowed = Columns::parse("status,cut,route").unwrap();
        assert_eq!(
            text(&request_line(&record, narrowed)),
            "200 POST ../completions"
        );
        record.complete = false;
        assert_eq!(
            text(&request_line(&record, narrowed)),
            "200✂POST ../completions",
            "the mark is one cell, so a cut line is no wider than a whole one"
        );

        // A fresh dial marks its route, glued, and moves nothing after it.
        let mut fresh = record;
        fresh.complete = true;
        fresh.dns = Some(0.004);
        fresh.tcp = Some(0.028);
        fresh.tls = Some(0.061);
        assert_eq!(
            text(&request_line(&fresh, narrowed)),
            "200 POST ../completions."
        );

        // Everything off is a request too, and an empty line answers it.
        assert_eq!(text(&request_line(&fresh, Columns::parse("").unwrap())), "");
    }
    /// The web console ports these formatters to JavaScript; one fixture,
    /// asserted on both sides, keeps the two printing the same text.
    #[test]
    fn the_web_formatters_share_this_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/webui/test/fixtures/format.json"
        );
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = |name: &str| fixture[name].as_array().unwrap().clone();
        let text = |case: &serde_json::Value| case[1].as_str().unwrap().to_string();
        for case in cases("human") {
            assert_eq!(logfmt::human(case[0].as_u64().unwrap()), text(&case));
        }
        for case in cases("human_time") {
            assert_eq!(logfmt::human_time(case[0].as_f64().unwrap()), text(&case));
        }
        for case in cases("human_count") {
            assert_eq!(logfmt::human_count(case[0].as_u64().unwrap()), text(&case));
        }
        for case in cases("span") {
            let seconds = Duration::from_secs(case[0].as_u64().unwrap());
            assert_eq!(logfmt::span(seconds), text(&case));
        }
        for case in cases("uptime") {
            assert_eq!(uptime(case[0].as_u64().unwrap()), text(&case));
        }
        for case in cases("ratio") {
            let pair = (case[0][0].as_u64().unwrap(), case[0][1].as_u64().unwrap());
            assert_eq!(ratio(pair.0, pair.1), text(&case));
        }
        for case in cases("dollars") {
            assert_eq!(dollars(case[0].as_f64().unwrap()), text(&case));
        }
    }
}
