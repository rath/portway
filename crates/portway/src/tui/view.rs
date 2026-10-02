//! Laying the dashboard out and drawing it. Pure: a `State` and a `Rect` in,
//! cells out, so `TestBackend` can assert on whole screens.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Sparkline, Table};

use crate::board::{self, compression_status};
use crate::flights::{FlightView, Phase};
use crate::forwarder::Coding;
use crate::logfmt::{self, Level, human, human_count, human_time};
use crate::spend;
use crate::telemetry::RequestRecord;
use crate::tui::chart::TwoToneBars;
use crate::tui::state::{COLUMNS, Column, Columns, Entry, State, USAGE_REFRESH, mean, percentile};
use crate::tui::theme::{THEMES, Theme};

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
    let t = state.theme;
    let area = frame.area();
    // Everything starts on the theme's own ground; a theme that leaves it to
    // the terminal paints `Reset`, which is what the cell held already.
    frame
        .buffer_mut()
        .set_style(area, Style::default().fg(t.text).bg(t.surface));
    if state.usage_open {
        usage_screen(frame, state, area);
        if state
            .remote
            .as_ref()
            .is_some_and(|remote| !remote.connected)
            && area.height > 0
        {
            frame.render_widget(
                footer(state),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        }
        return;
    }
    let Some(panes) = panes(area, state.recent_models().len()) else {
        frame.render_widget(
            // Short enough to survive the truncation it is warning about.
            Paragraph::new("too small")
                .style(Style::default().fg(t.bad))
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
        // Keep the live dashboard visible, but reserve color and emphasis for
        // the dialog. Clear below restores normal styles inside its bounds.
        frame.buffer_mut().set_style(
            area,
            Style::default()
                .fg(t.border)
                .bg(t.surface)
                .remove_modifier(Modifier::all()),
        );
        flights_dialog(frame, state, area);
    } else if state.help {
        popup(t, frame, area, "keys", help_lines(t));
    } else if state.picker {
        popup(t, frame, area, "columns", column_lines(state));
    } else if state.themes {
        popup(t, frame, area, "theme", theme_lines(state));
    } else if state.detail
        && let Some(record) = state.selected()
    {
        popup(t, frame, area, "request", detail_lines(t, record));
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

fn flight_phase(t: &Theme, flight: &FlightView) -> (&'static str, Color) {
    if is_slow(flight) {
        return ("slow prefill", t.time);
    }
    if is_stalled(flight) {
        return ("stalled", t.bad);
    }
    match flight.phase {
        Phase::Upload => ("upload", t.wire),
        Phase::Prefill => ("prefill", t.time),
        Phase::Stream => ("stream", t.good),
    }
}

fn flight_route(flight: &FlightView) -> String {
    format!("{} {}", flight.method, flight.path)
}

/// The size the answer crossed a hop as, for the line's download pair: the
/// upstream hop when it was coded, else the agent leg when the hop recoded
/// it, else nothing.
pub fn down_wire(record: &RequestRecord) -> Option<u64> {
    if record.upstream_encoding != "identity" && record.received_wire > 0 {
        Some(record.received_wire)
    } else if record.received_agent > 0 && record.received_agent != record.received {
        Some(record.received_agent)
    } else {
        None
    }
}

/// A request that named no model — a catalog, a health check — shows a dash
/// where the name would be, rather than a gap that reads as a missing cell.
pub fn shown_model(model: &str) -> &str {
    if model.is_empty() { "-" } else { model }
}

/// What a flight is called in the dialog: the model it asked for, or the
/// upstream it is going to when it asked for none. The dialog has one name
/// column and no detail view, so the most specific name takes it.
fn flight_name(flight: &FlightView) -> &str {
    if flight.model.is_empty() {
        &flight.upstream
    } else {
        &flight.model
    }
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
fn flight_alarm(t: &Theme, flights: &[FlightView]) -> Option<(String, Color)> {
    let stalled = flights.iter().filter(|flight| is_stalled(flight)).count();
    let slow = flights.iter().filter(|flight| is_slow(flight)).count();
    match (stalled, slow) {
        (0, 0) => None,
        (0, slow) => Some((format!("{slow} slow"), t.time)),
        (stalled, 0) => Some((format!("{stalled} stalled"), t.bad)),
        (stalled, slow) => Some((format!("{stalled} stalled, {slow} slow"), t.bad)),
    }
}

/// `f`: every request in flight, oldest first, redrawn with each 250ms sample.
///
/// A dialog rather than a pane, so the list can fill and empty without moving
/// the dashboard under it. It is as tall as its rows, up to two thirds of the
/// terminal, and as wide as its columns; its title counts the rows that did
/// not fit.
fn flights_dialog(frame: &mut Frame, state: &State, area: Rect) {
    let t = state.theme;
    let flights = &state.flights;
    // The header row over the rows, or one line of message.
    let body = if flights.is_empty() {
        1
    } else {
        flights.len() + 1
    };
    let tallest = area.height - 2 * (area.height / 6);
    // Two borders and one blank row above and below the content.
    let height = (body + 4).min(tallest as usize) as u16;
    let shown = flights.len().min(height.saturating_sub(5) as usize);
    let columns = FlightColumns::fit(
        &flights[..shown],
        area.width.saturating_sub(2 * FLIGHTS_GUTTER),
    );
    let width = columns.width().min(area.width);
    // Placed as though it held `FLIGHTS_SETTLED` rows, so up to there a
    // request arriving or leaving moves the bottom border, not the title and
    // header the eye is on. Past that it is simply centred.
    let placed = height.max((FLIGHTS_SETTLED + 5).min(tallest));
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
        .border_type(BorderType::Double)
        .title_top(Line::styled(format!(" {title} "), t.title()))
        .title_bottom(
            Line::from(vec![
                Span::styled(
                    " f / esc",
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" close ", Style::default().fg(t.dim)),
            ])
            .right_aligned(),
        )
        .border_style(Style::default().fg(t.accent))
        .style(Style::default().fg(t.text).bg(t.raised))
        .padding(Padding::uniform(1));
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
            Style::default().fg(t.time),
        ))
    } else if flights.is_empty() {
        Some(Span::styled(
            "nothing in flight",
            Style::default().fg(t.dim),
        ))
    } else {
        None
    };
    match message {
        Some(message) => frame.render_widget(Paragraph::new(message).block(block), box_area),
        None => frame.render_widget(
            flights_table(t, &flights[..shown], &columns).block(block),
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
        let grow = longest(|flight| flight_name(flight).to_string())
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
fn flights_table(t: &Theme, flights: &[FlightView], columns: &FlightColumns) -> Table<'static> {
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
        let (phase, shade) = flight_phase(t, flight);
        let mut cells = vec![
            Cell::from(Span::styled(
                clipped(flight_name(flight), columns.model),
                Style::default().fg(t.model),
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
        .header(Row::new(headers).style(Style::default().fg(t.dim)))
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

/// `part` as a whole percentage of `whole`, `92%`: truncated like `ratio`, so
/// it never rounds up to a 100% the counts do not reach. `None` when there is
/// no whole to take a share of.
fn share(part: u64, whole: u64) -> Option<String> {
    (whole > 0).then(|| format!("{}%", part.min(whole) * 100 / whole))
}

fn stat<'a>(t: &Theme, label: &'a str, value: String, shade: Color) -> Vec<Span<'a>> {
    vec![
        Span::styled(label, Style::default().fg(t.dim)),
        Span::raw(" "),
        Span::styled(value, Style::default().fg(shade)),
        Span::raw("  "),
    ]
}

fn hud<'a>(state: &State, header: &'a Header) -> Paragraph<'a> {
    let t = state.theme;
    let totals = &state.totals;

    let mut title = vec![
        Span::styled(" portway ", t.chosen()),
        Span::raw("  "),
        Span::styled(header.listen.clone(), Style::default().fg(t.accent)),
        Span::raw("  "),
    ];
    let up = state
        .remote
        .as_ref()
        .map(|remote| {
            remote.uptime
                + if remote.connected {
                    remote.sampled.elapsed().as_secs_f64()
                } else {
                    0.0
                }
        })
        .map(|up| up as u64)
        .unwrap_or_else(|| {
            state
                .coverage
                .unwrap_or_else(|| state.started.elapsed())
                .as_secs()
        });
    title.extend(stat(t, "up", uptime(up), t.time));
    // A window cannot see the compressor this process never asked for; the
    // slot says what it is instead: how wide the window it reads is.
    if let Some(remote) = &state.remote {
        title.extend(stat(
            t,
            "remote",
            remote.message.clone(),
            if remote.connected { t.good } else { t.time },
        ));
        title.extend(stat(t, "coding", remote.coding.clone(), t.good));
    } else {
        match header.watching {
            Some(span) => title.extend(stat(t, "watching", logfmt::span(span), t.good)),
            None => title.extend(stat(t, "coding", header.coding.clone(), t.good)),
        }
    }
    title.extend(stat(t, "upstreams", state.models.len().to_string(), t.text));

    let reuse = match (state.reused * 100).checked_div(state.seen) {
        Some(share) => format!("{share}%"),
        None => "-".to_string(),
    };
    let mut requests = vec![Span::styled(" REQUESTS ", Style::default().fg(t.dim))];
    requests.extend(stat(t, "total", totals.requests.to_string(), t.text));
    if state.flights_available {
        requests.extend(stat(t, "live", totals.in_flight.to_string(), t.good));
        // The one thing about the flights that cannot wait for `f`: something
        // stopped moving. It goes between the count and the gap after it.
        if let Some((alarm, shade)) = flight_alarm(t, &state.flights) {
            requests.insert(
                requests.len() - 1,
                Span::styled(format!(" ({alarm})"), Style::default().fg(shade)),
            );
        }
    } else {
        requests.push(Span::styled(
            "in flight unavailable  ",
            Style::default().fg(t.time),
        ));
    }
    requests.extend(stat(t, "2xx", state.ok.to_string(), t.good));
    requests.extend(stat(
        t,
        "4xx",
        state.client_errors.to_string(),
        if state.client_errors > 0 {
            t.time
        } else {
            t.dim
        },
    ));
    requests.extend(stat(
        t,
        "5xx",
        state.server_errors.to_string(),
        if state.server_errors > 0 {
            t.bad
        } else {
            t.dim
        },
    ));
    requests.extend(stat(t, "cut", state.truncated.to_string(), t.dim));
    requests.extend(stat(t, "aborts", totals.aborts.to_string(), t.dim));
    requests.extend(stat(
        t,
        "up-err",
        totals.upstream_errors.to_string(),
        if totals.upstream_errors > 0 {
            t.bad
        } else {
            t.dim
        },
    ));
    requests.extend(stat(t, "reuse", reuse, t.good));

    let saved = totals.body_bytes.saturating_sub(totals.wire_bytes);
    let mut upload = vec![Span::styled(" UPLOAD   ", Style::default().fg(t.dim))];
    upload.extend(stat(t, "raw", human(totals.body_bytes), t.raw));
    upload.extend(stat(t, "wire", human(totals.wire_bytes), t.wire));
    upload.extend(stat(
        t,
        "saved",
        format!(
            "{} ({})",
            human(saved),
            ratio(totals.body_bytes, totals.wire_bytes)
        ),
        t.good,
    ));
    upload.extend(stat(
        t,
        "encoded",
        format!("{}/{}", totals.encoded, totals.requests),
        t.text,
    ));
    upload.extend(stat(
        t,
        "415-retry",
        totals.retried_identity.to_string(),
        if totals.retried_identity > 0 {
            t.time
        } else {
            t.dim
        },
    ));

    let mut download = vec![Span::styled(" DOWNLOAD ", Style::default().fg(t.dim))];
    download.extend(stat(t, "wire", human(totals.down_wire_bytes), t.wire));
    download.extend(stat(
        t,
        "decoded",
        format!(
            "{} ({})",
            human(totals.down_bytes),
            ratio(totals.down_bytes, totals.down_wire_bytes)
        ),
        t.raw,
    ));
    // The hop's own saving, next to the upstream leg's: the same shape the
    // upload row uses, and the decoded size it came out of is the stat beside
    // it. Hidden until a response actually went out encoded.
    if totals.agent_bytes > 0 {
        download.extend(stat(
            t,
            "agent",
            format!(
                "{} ({})",
                human(totals.agent_bytes),
                ratio(totals.down_bytes, totals.agent_bytes)
            ),
            t.good,
        ));
    }
    download.extend(stat(t, "idle conns", totals.idle_conns.to_string(), t.text));

    let quantile = |samples: &_, q, remote: Option<f64>| {
        (if state.remote.is_some() {
            remote
        } else {
            percentile(samples, q)
        })
        .map(human_time)
        .unwrap_or_else(|| "-".to_string())
    };
    let mut latency = vec![Span::styled(" LATENCY  ", Style::default().fg(t.dim))];
    latency.extend(stat(
        t,
        "ttfb p50/p95",
        format!(
            "{} / {}",
            quantile(
                &state.ttfb,
                0.5,
                state.remote.as_ref().and_then(|r| r.latency.ttfb.p50)
            ),
            quantile(
                &state.ttfb,
                0.95,
                state.remote.as_ref().and_then(|r| r.latency.ttfb.p95)
            )
        ),
        t.time,
    ));
    latency.extend(stat(
        t,
        "upload p50/p95",
        format!(
            "{} / {}",
            quantile(
                &state.upload,
                0.5,
                state.remote.as_ref().and_then(|r| r.latency.upload.p50)
            ),
            quantile(
                &state.upload,
                0.95,
                state.remote.as_ref().and_then(|r| r.latency.upload.p95)
            )
        ),
        t.time,
    ));
    latency.extend(stat(
        t,
        "handshake avg",
        (match &state.remote {
            Some(remote) => remote.latency.handshake_mean,
            None => mean(&state.handshake),
        })
        .map(human_time)
        .unwrap_or_else(|| "-".to_string()),
        t.time,
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

fn coding_span(t: &Theme, coding: Coding, dict: bool) -> Span<'static> {
    let color = if coding == Coding::None {
        t.dim
    } else {
        t.good
    };
    Span::styled(
        board::coding_label(coding, dict),
        Style::default().fg(color),
    )
}

/// Below this the table sheds columns rather than letting every one of them
/// shrink until the model names are unreadable.
const ROOMY_TABLE: u16 = 114;
const STATUS_TABLE: u16 = 146;

/// The upstream table's byte columns, drawn right-aligned.
const SIZE_COLUMNS: [&str; 5] = ["raw", "wire", "saved", "down", "↓ saved"];

fn models_table(state: &State, width: u16) -> Table<'_> {
    let t = state.theme;
    let roomy = width >= ROOMY_TABLE;
    let show_status = width >= STATUS_TABLE;
    let mut names = vec!["upstream", "coding", "reqs", "live", "raw", "wire", "saved"];
    if roomy {
        names.extend(["down", "↓ saved", "idle", "err", "abort"]);
    }
    if show_status {
        names.push("compression status");
    }
    // Sizes are right-aligned, headings with them: a column of them is read
    // by its last digit, as the usage table's money is.
    let header = Row::new(names.into_iter().map(|name| {
        if SIZE_COLUMNS.contains(&name) {
            number(name)
        } else {
            Cell::from(name)
        }
    }))
    .style(Style::default().fg(t.dim));

    let models = state.recent_models();
    let title = format!("recent upstreams · {}/{}", models.len(), state.models.len());
    let rows = models.into_iter().map(|row| {
        let view = &row.view;
        let errors = view.upstream_errors;
        let saved = view.saved_bytes().max(0) as u64;
        let mut cells = vec![
            Cell::from(Span::styled(row.name.clone(), Style::default().fg(t.model))),
            Cell::from(coding_span(t, view.coding, view.dict)),
            Cell::from(view.requests.to_string()),
            Cell::from(Span::styled(
                if state.flights_available {
                    view.in_flight.to_string()
                } else {
                    "-".to_string()
                },
                Style::default().fg(if view.in_flight > 0 { t.good } else { t.dim }),
            )),
            number(Span::styled(
                human(view.body_bytes),
                Style::default().fg(t.raw),
            )),
            number(Span::styled(
                human(view.wire_bytes),
                Style::default().fg(t.wire),
            )),
            // The saving with the ratio it came out of, as the HUD's upload
            // row prints it: `879KB (-96%)`.
            number(Span::styled(
                format!(
                    "{} ({})",
                    human(saved),
                    ratio(view.body_bytes, view.wire_bytes)
                ),
                Style::default().fg(t.good),
            )),
        ];
        if roomy {
            cells.extend([
                number(Span::styled(
                    human(view.down_bytes),
                    Style::default().fg(t.raw),
                )),
                number(Span::styled(
                    human(view.down_saved_bytes().max(0) as u64),
                    Style::default().fg(t.good),
                )),
                Cell::from(view.idle_conns.to_string()),
                Cell::from(Span::styled(
                    errors.to_string(),
                    Style::default().fg(if errors > 0 { t.bad } else { t.dim }),
                )),
                Cell::from(Span::styled(
                    view.client_aborts.to_string(),
                    Style::default().fg(t.dim),
                )),
            ]);
        }
        if show_status {
            cells.push(Cell::from(Span::styled(
                state
                    .remote
                    .as_ref()
                    .and_then(|r| r.statuses.get(&row.name))
                    .cloned()
                    .unwrap_or_else(|| compression_status(view)),
                Style::default().fg(t.time),
            )));
        }
        Row::new(cells)
    });

    let mut widths = vec![
        Constraint::Length(20),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(15),
    ];
    if roomy {
        widths.extend([
            Constraint::Length(8),
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
                .border_type(BorderType::Rounded)
                .title_top(Line::styled(format!(" {title} "), t.title()))
                .border_style(Style::default().fg(t.border))
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
    let t = state.theme;
    let Some(table) = &state.usage else {
        let reason = state
            .usage_error
            .as_deref()
            .unwrap_or("no counts have been read");
        frame.render_widget(
            Paragraph::new(reason)
                .style(Style::default().fg(t.bad))
                .alignment(Alignment::Center),
            area,
        );
        return;
    };
    if area.width < USAGE_MIN_WIDTH || area.height < USAGE_MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("too small")
                .style(Style::default().fg(t.bad))
                .alignment(Alignment::Center),
            area,
        );
        return;
    }

    let notes = usage_notes(t, table);
    // Both ends carry their date: a window that is over ends at a midnight,
    // and `00:00:00` alone would not say which one.
    let title = state
        .remote
        .as_ref()
        .and_then(|r| r.usage_title.clone())
        .unwrap_or_else(|| {
            format!(
                "usage — {} .. {}",
                logfmt::datetime(table.since),
                logfmt::datetime(table.until)
            )
        });
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title_top(titled(t, &title))
        .border_style(Style::default().fg(t.border))
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
                Style::default().fg(t.dim),
            ))),
            rows[1],
        );
    } else {
        frame.render_widget(usage_table(t, table, area.width, rows[1].width), rows[1]);
    }
    frame.render_widget(Paragraph::new(notes), rows[3]);

    if state.usage_rates {
        popup(t, frame, area, "costs", cost_lines(t, table));
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

/// The usage label column's width when every label fits in it, as it did
/// before a label could carry a tier.
const USAGE_LABEL: u16 = 20;

/// The label column of the usage tables: as wide as the longest label shown
/// — a model and its tier can outgrow `USAGE_LABEL` — but never narrower than
/// `USAGE_LABEL`, and never wider than `most` unless that is narrower still.
fn label_width(table: &spend::Table, most: u16) -> u16 {
    let longest = table
        .rows
        .iter()
        .map(|row| Span::raw(row.label()).width())
        .max()
        .unwrap_or(0);
    (longest as u16).clamp(USAGE_LABEL, most.max(USAGE_LABEL))
}

/// `room` is the width the table is drawn in; the numbers keep their columns
/// and the label takes what they leave, up to what it needs.
fn usage_table(t: &Theme, table: &spend::Table, width: u16, room: u16) -> Table<'static> {
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
        .map(|model| usage_row(t, model, roomy, false))
        .collect();
    rows.push(usage_row(t, &table.total, roomy, true));

    let mut numbers: Vec<u16> = vec![5, 9, 9, 6, 9];
    if roomy {
        numbers.extend([9, 9, 9]);
    }
    numbers.push(9);
    // One space between every pair of columns, the label's included.
    let taken: u16 = numbers.iter().sum::<u16>() + numbers.len() as u16;
    let label = label_width(table, room.saturating_sub(taken));
    let widths = std::iter::once(label)
        .chain(numbers)
        .map(Constraint::Length)
        .collect::<Vec<_>>();
    Table::new(rows, widths)
        .header(Row::new(header).style(Style::default().fg(t.dim)))
        .column_spacing(1)
}

/// The window selector, drawn above the table: what is being measured, with
/// the one in force reversed and the arrows that move it spelled out.
fn usage_ranges(state: &State) -> Line<'static> {
    let t = state.theme;
    let mut spans = vec![Span::raw(" ")];
    for (at, range) in spend::Range::ALL.into_iter().enumerate() {
        if at > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(t.dim)));
        }
        if range == state.usage_range {
            spans.push(Span::styled(format!(" {} ", range.label()), t.chosen()));
        } else {
            spans.push(Span::styled(range.label(), Style::default().fg(t.dim)));
        }
    }
    spans.push(Span::styled(
        "   ←→ window · p costs",
        Style::default().fg(t.dim),
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
/// What the costs popup leaves its label column: the popup's widest inner
/// line, less the margin and the four money columns.
const COST_LABEL_MAX: u16 = 70 - 1 - 41;

fn cost_lines(t: &Theme, table: &spend::Table) -> Vec<Line<'static>> {
    let width = label_width(table, COST_LABEL_MAX) as usize;
    let mut lines = vec![Line::from(Span::styled(
        " cost by source",
        Style::default().fg(t.accent),
    ))];
    lines.push(Line::from(vec![
        Span::styled(format!(" {:<width$}", "model"), Style::default().fg(t.dim)),
        Span::styled(
            format!(
                "{:>10}{:>10}{:>10}{:>11}",
                "prompt", "cached", "output", "total$"
            ),
            Style::default().fg(t.dim),
        ),
    ]));
    for row in &table.rows {
        let label = row.label();
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
                format!(" {label:<width$.width$}"),
                Style::default().fg(if priced { t.model } else { t.dim }),
            ),
            Span::styled(
                format!("{:>10}{:>10}{:>10}", prompt, cached, output),
                Style::default().fg(if priced { t.good } else { t.dim }),
            ),
            Span::styled(
                format!("{:>11}", total.as_deref().unwrap_or("-")),
                Style::default().fg(if priced { t.good } else { t.dim }),
            ),
        ]));
    }
    if let Some(sum) = table.total.charge {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<width$}", "total"),
                Style::default().fg(t.good).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>10}{:>10}{:>10}",
                    dollars(sum.input),
                    dollars(sum.cache_read),
                    dollars(sum.output)
                ),
                Style::default().fg(t.good),
            ),
            Span::styled(
                format!("{:>11}", dollars(sum.total())),
                Style::default().fg(t.good).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        " p / esc close",
        Style::default().fg(t.dim),
    )));
    lines
}

/// One line of the usage table. `total` is the same row with the day's pooled
/// counts in it, drawn bold — the columns are identical on purpose.
fn usage_row(t: &Theme, model: &spend::Row, roomy: bool, total: bool) -> Row<'static> {
    let charge = model.charge;
    let mut cells = vec![
        Cell::from(Span::styled(
            model.label(),
            Style::default().fg(if total { t.good } else { t.model }),
        )),
        number(model.requests.to_string()),
        number(human_count(model.prompt)),
        number(
            if model.unreported == model.requests && model.requests > 0 && !total {
                // Not a zero: the engine said nothing about its cache, and a dash
                // is the only honest thing to put in the column.
                Span::styled("-", Style::default().fg(t.dim))
            } else {
                Span::raw(human_count(model.cached))
            },
        ),
        number(hit_span(t, model.hit_rate())),
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
        Style::default().fg(t.good).add_modifier(Modifier::BOLD),
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
fn hit_span(t: &Theme, rate: Option<f64>) -> Span<'static> {
    match rate {
        Some(rate) => Span::styled(
            format!("{:.1}%", rate * 100.0),
            Style::default().fg(if rate >= 0.5 { t.good } else { t.time }),
        ),
        None => Span::styled("n/a", Style::default().fg(t.dim)),
    }
}

/// One money cell: a part of the charge, or a dash when nothing could price it.
fn part(amount: Option<f64>) -> String {
    amount.map(dollars).unwrap_or_else(|| "-".to_string())
}

/// What the numbers above are not: written under them, in the order a reader
/// would ask. A screen full of money has to say whose money, and which part of
/// it is a floor.
fn usage_notes(t: &Theme, table: &spend::Table) -> Vec<Line<'static>> {
    let mut notes: Vec<Line<'static>> = spend::notes(table)
        .into_iter()
        .map(|text| note(t, text))
        .collect();
    notes.push(Line::from(vec![
        Span::styled(" u / esc", Style::default().fg(t.accent)),
        Span::styled(
            format!(
                "  back to the dashboard · re-read every {}s",
                USAGE_REFRESH.as_secs()
            ),
            Style::default().fg(t.dim),
        ),
    ]));
    notes
}

fn note(t: &Theme, text: String) -> Line<'static> {
    Line::from(Span::styled(format!(" {text}"), Style::default().fg(t.dim)))
}

// -------------------------------------------------------------------- charts

fn titled<'a>(t: &Theme, text: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(text, t.title()),
        Span::raw(" "),
    ])
}

fn draw_charts(frame: &mut Frame, state: &State, area: Rect, mode: Charts) {
    let t = state.theme;
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
            Span::styled(" raw ", Style::default().fg(t.dim)),
            Span::styled("█", Style::default().fg(t.raw)),
            Span::styled(" wire ", Style::default().fg(t.dim)),
            Span::styled("█", Style::default().fg(t.wire)),
            Span::styled(
                format!(" {} turns · {} ", samples.len(), ratio(window.0, window.1)),
                Style::default().fg(t.dim),
            ),
        ]);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title_top(titled(t, "request body / turn"))
            .title_bottom(legend.right_aligned())
            .border_style(Style::default().fg(t.border));
        let inner = block.inner(bars_area);
        frame.render_widget(block, bars_area);
        frame.render_widget(TwoToneBars::new(&samples, t.raw, t.wire), inner);
    }

    // Socket throughput.
    let Some(traffic_area) = traffic_area else {
        return;
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title_top(titled(t, "socket bytes/s"))
        .title_bottom(
            Line::from(Span::styled(
                format!(" {}s buckets · peak ", state.scale),
                Style::default().fg(t.dim),
            ))
            .right_aligned(),
        )
        .border_style(Style::default().fg(t.border));
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
        (halves[0], &series.up, "↑ up", t.wire),
        (halves[1], &series.down, "↓ down", t.raw),
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
                Span::styled(format!(" {:>7}", human(peak)), Style::default().fg(t.dim)),
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

/// The route reads abbreviated — `POST ../messages` — so the line still
/// names where the request went without re-spelling the mount and the `/v1/`
/// trunk every line; anything an agent's turns do not share keeps its full
/// path. Either way it is dim: the model beside it already says whose turn it
/// was. The query and the full method/path live in the detail popup — which
/// is also where `POST /v1/completions` is told apart from its chat sibling,
/// whose tail the abbreviation happens to match.
fn route(t: &Theme, record: &RequestRecord) -> Span<'static> {
    let (shown, _) = board::route(&record.method, &record.path);
    Span::styled(shown, Style::default().fg(t.dim))
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

fn request_line(t: &Theme, record: &RequestRecord, columns: Columns) -> Line<'static> {
    let status_shade = match record.status {
        status if status < 300 => t.good,
        status if status < 400 => t.wire,
        status if status < 500 => t.time,
        _ => t.bad,
    };
    let mut line = Fields::default();
    for column in COLUMNS
        .into_iter()
        .filter(|column| columns.contains(*column))
    {
        match column {
            Column::Time => line.word(Span::styled(
                record.stamp.clone(),
                Style::default().fg(t.dim),
            )),
            Column::Status => line.word(Span::styled(
                record.status.to_string(),
                Style::default().fg(status_shade),
            )),
            // The relay ended before the upstream body did. Nothing to say
            // about one that finished, which is what the space it takes would
            // have said anyway.
            Column::Cut => {
                if !record.complete {
                    line.glue(Span::styled("✂", Style::default().fg(t.time)));
                }
            }
            Column::Model => {
                line.word(Span::styled(
                    shown_model(&record.model).to_string(),
                    Style::default().fg(t.model),
                ));
            }
            Column::Route => {
                line.word(route(t, record));
                // Every turn rides a pooled connection, so reuse needs no
                // announcing — a line without the mark is a reused one. A
                // fresh dial costs latency and says so with the mark alone;
                // how much it cost is the detail popup's business. `.` because
                // it is ASCII — one byte, one cell in every terminal, unlike
                // `●`/`•` (East Asian ambiguous width).
                if record.handshake().is_some() {
                    line.bare(Span::styled(".", Style::default().fg(t.time)));
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
                    Style::default().fg(t.raw),
                ));
                line.glue(Span::styled("→", Style::default().fg(t.dim)));
                line.word(Span::styled(
                    human(record.wire_len),
                    Style::default().fg(t.wire),
                ));
                if record.coding != Coding::None {
                    line.word(Span::styled(
                        ratio(record.body_len, record.wire_len),
                        Style::default().fg(t.good),
                    ));
                }
                if let Some(upload) = record.upload {
                    line.word(Span::styled(
                        human_time(upload),
                        Style::default().fg(t.time),
                    ));
                }
            }
            Column::Ttfb => {
                line.word(Span::styled("ttfb", Style::default().fg(t.dim)));
                line.word(Span::styled(
                    human_time(record.ttfb),
                    Style::default().fg(t.time),
                ));
            }
            Column::Down => {
                line.word(Span::styled("down", Style::default().fg(t.dim)));
                line.word(Span::styled(
                    human(record.received),
                    Style::default().fg(t.raw),
                ));
                // The answer and what it crossed a hop as: the same pair the
                // upload column shows, spaced the same way. The upstream hop
                // first, since behind a receiver that is where the download is
                // saved; the agent leg when only it was coded. An identity
                // answer keeps the single size it always had.
                if let Some(wire) = down_wire(record) {
                    line.glue(Span::styled("→", Style::default().fg(t.dim)));
                    line.word(Span::styled(human(wire), Style::default().fg(t.wire)));
                    line.word(Span::styled(
                        ratio(record.received, wire),
                        Style::default().fg(t.good),
                    ));
                }
                if let Some(download) = record.download {
                    line.word(Span::styled(
                        human_time(download),
                        Style::default().fg(t.time),
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
                line.word(Span::styled("tok", Style::default().fg(t.dim)));
                line.word(Span::styled(
                    human_count(usage.prompt),
                    Style::default().fg(t.raw),
                ));
                // The share of the prompt a prefix cache read instead of
                // prefilling, glued to the count it is a share of:
                // 91.2K(99% cached) says at a glance what 91.2K(91.1K cached)
                // leaves to arithmetic. The popup keeps the exact count. An
                // engine that reported counts without the breakdown gets none
                // at all.
                if let Some(cached) = usage.cached.and_then(|count| share(count, usage.prompt)) {
                    line.glue(Span::styled(
                        format!("({cached} cached)"),
                        Style::default().fg(t.dim),
                    ));
                }
                line.glue(Span::styled("→", Style::default().fg(t.dim)));
                line.word(Span::styled(
                    human_count(usage.completion),
                    Style::default().fg(t.wire),
                ));
            }
        }
    }
    Line::from(line.spans)
}

fn log_line(t: &Theme, stamp: &str, level: Level, message: &str) -> Line<'static> {
    let shade = match level {
        Level::Info => t.text,
        Level::Warning => t.time,
        Level::Error => t.bad,
    };
    let mut spans = vec![
        Span::styled(stamp.to_string(), Style::default().fg(t.dim)),
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
    let t = state.theme;
    let lines: Vec<Line> = state
        .visible()
        .into_iter()
        .map(|(entry, selected)| {
            let line = match entry {
                Entry::Request(record) => request_line(t, record, state.columns),
                Entry::Log {
                    stamp,
                    level,
                    message,
                } => log_line(t, stamp, *level, message),
            };
            if selected {
                line.style(t.cursor())
            } else {
                line
            }
        })
        .collect();

    let position = if state.follow {
        Span::styled("FOLLOW", Style::default().fg(t.good))
    } else {
        let hidden = state.below();
        let suffix = if hidden > 0 {
            format!(" · {hidden} below")
        } else {
            String::new()
        };
        Span::styled(format!("PAUSED{suffix}"), Style::default().fg(t.time))
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
            Style::default().fg(t.dim),
        ),
    ]);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title_top(titled(t, "events"))
        .border_style(Style::default().fg(t.border));
    // A narrow pane has no room for both titles.
    let block = if width > 60 {
        block.title_top(status.right_aligned())
    } else {
        block
    };
    Paragraph::new(lines).block(block)
}

fn footer(state: &State) -> Paragraph<'static> {
    let t = state.theme;
    if let Some(remote) = &state.remote
        && !remote.connected
    {
        return Paragraph::new(format!(" q close · {}", remote.message))
            .style(Style::default().fg(t.time));
    }
    if state.confirm_quit {
        return Paragraph::new(Line::from(Span::styled(
            format!(
                " {} request(s) still streaming — press q again to cut them off, f to see them ",
                state.totals.in_flight
            ),
            Style::default().fg(t.bad).add_modifier(Modifier::BOLD),
        )));
    }
    let keys = [
        ("q", "quit"),
        ("↑↓/jk", "scroll"),
        ("g/G", "top/live"),
        ("↵", "detail"),
        ("f", "flights"),
        ("e", "trouble"),
        ("m", "upstream"),
        ("u", "usage"),
        ("t", "buckets"),
        ("c", "columns"),
        ("T", "theme"),
        ("?", "keys"),
    ];
    // Each key a chip with its word after it, one space to the next: the
    // chip's own padding is what tells one hint from the next.
    let mut spans = Vec::new();
    for (key, what) in keys {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!(" {key} "), t.key()));
        spans.push(Span::styled(format!(" {what}"), Style::default().fg(t.dim)));
    }
    Paragraph::new(Line::from(spans))
}

/// The picker: every column, on or off, with the one under the cursor
/// reversed the way a selected line is.
fn column_lines(state: &State) -> Vec<Line<'static>> {
    let t = state.theme;
    let mut lines: Vec<Line<'static>> = COLUMNS
        .into_iter()
        .enumerate()
        .map(|(at, column)| {
            let on = state.columns.contains(column);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", if on { '✓' } else { '·' }),
                    Style::default().fg(if on { t.good } else { t.dim }),
                ),
                Span::styled(
                    format!("{:<8}", column.name()),
                    if on {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(t.dim)
                    },
                ),
                Span::styled(column.note(), Style::default().fg(t.dim)),
            ]);
            if at == state.picker_at {
                line.style(t.raised_cursor())
            } else {
                line
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        " space/↵ toggle · esc close (kept for the next run)",
        Style::default().fg(t.dim),
    )));
    lines
}

/// The theme picker: every theme by name, with a chip of each color it
/// draws data in. The dashboard around it already wears the theme under the
/// cursor, so the list only has to say what the others would be.
fn theme_lines(state: &State) -> Vec<Line<'static>> {
    let t = state.theme;
    let mut lines: Vec<Line<'static>> = THEMES
        .iter()
        .enumerate()
        .map(|(at, theme)| {
            let mut spans = vec![Span::raw(format!(" {:<18}", theme.name))];
            // Spaces on a background rather than a glyph in the color: a
            // square is East Asian ambiguous width, and two cells of nothing
            // are two cells everywhere. The color goes on both sides of the
            // cell, so a reversed cursor line swaps it for itself.
            for color in [
                theme.raw,
                theme.wire,
                theme.good,
                theme.time,
                theme.bad,
                theme.model,
            ] {
                spans.push(Span::styled("  ", Style::default().fg(color).bg(color)));
                spans.push(Span::raw(" "));
            }
            let line = Line::from(spans);
            if at == state.themes_at {
                line.style(t.raised_cursor())
            } else {
                line
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        " ↑↓ try · ↵ keep (for the next run too) · esc put back",
        Style::default().fg(t.dim),
    )));
    lines
}

// -------------------------------------------------------------------- popups

fn popup(t: &Theme, frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let width = area.width.clamp(MIN_WIDTH, 72.max(MIN_WIDTH));
    let height = (lines.len() as u16 + 2).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title_top(titled(t, title))
        .border_style(Style::default().fg(t.accent))
        .style(Style::default().fg(t.text).bg(t.raised));
    frame.render_widget(Clear, box_area);
    frame.render_widget(Paragraph::new(lines).block(block), box_area);
}

fn field(t: &Theme, name: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {name:<16}"), Style::default().fg(t.dim)),
        Span::raw(value),
    ])
}

fn detail_lines(t: &Theme, record: &RequestRecord) -> Vec<Line<'static>> {
    let optional = |value: Option<f64>| {
        value
            .map(human_time)
            .unwrap_or_else(|| "not measured".to_string())
    };
    vec![
        field(t, "when", record.stamp.clone()),
        field(t, "upstream", record.upstream.clone()),
        field(
            t,
            "model",
            spend::label(shown_model(&record.model), record.tier.as_deref()),
        ),
        field(
            t,
            "request",
            format!("{} {} -> {}", record.method, record.path, record.status),
        ),
        field(
            t,
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
            t,
            "upload",
            format!(
                "{} -> {} ({}, {})",
                human(record.body_len),
                human(record.wire_len),
                record.coding.name().unwrap_or("identity"),
                ratio(record.body_len, record.wire_len),
            ),
        ),
        field(t, "upload acked in", optional(record.upload)),
        field(t, "ttfb", human_time(record.ttfb)),
        field(
            t,
            "download",
            format!(
                "{} on the wire -> {} decoded ({}{}){}",
                human(record.received_wire),
                human(record.received),
                record.upstream_encoding,
                // The upload field's `(coding, -N%)`, for the upstream hop.
                if record.upstream_encoding == "identity" {
                    String::new()
                } else {
                    format!(", {}", ratio(record.received, record.received_wire))
                },
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
        field(t, "download took", optional(record.download)),
        field(
            t,
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
            t,
            "ended",
            if record.complete {
                "upstream body finished".to_string()
            } else {
                "cut short (agent abort, error or read timeout)".to_string()
            },
        ),
    ]
}

fn help_lines(t: &Theme) -> Vec<Line<'static>> {
    [
        ("q / ctrl-c", "quit (confirms while a stream is live)"),
        ("j / ↓ / k / ↑", "move the cursor, wheel scrolls too"),
        ("PgDn / PgUp", "move by a page"),
        ("g / G", "oldest line / back to following"),
        ("Enter", "details of the highlighted request"),
        ("f", "requests in flight, live"),
        ("e", "only 4xx/5xx, cut streams and warnings"),
        ("m", "cycle the upstream filter"),
        ("u", "tokens and cost per model, by window"),
        ("t", "1s / 10s / 60s traffic buckets"),
        ("c", "choose what a request line shows"),
        ("T", "pick a color theme, kept for the next run"),
        ("?", "close this"),
    ]
    .into_iter()
    .map(|(key, what)| {
        Line::from(vec![
            Span::styled(format!(" {key:<16}"), Style::default().fg(t.accent)),
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
    use crate::tui::theme::{TERMINAL, THEMES};
    use crate::usage::Usage;
    use crate::watch;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;

    fn record(model: &str, status: u16) -> RequestRecord {
        RequestRecord {
            stamp: "23:41:02".to_string(),
            upstream: model.to_string(),
            model: model.to_string(),
            tier: None,
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
                let byte = text.find("╔ in flight")?;
                Some((at, text[..byte].chars().count()))
            })
            .unwrap_or_else(|| panic!("the dialog is not up:\n{screen}"));
        let right = left + rows[top][left..].iter().position(|c| *c == '╗').unwrap();
        let bottom = (top..rows.len())
            .find(|at| rows[*at].get(left) == Some(&'╚'))
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
            &[],
            &[("alpha".to_string(), "https://example.invalid".to_string())],
            None,
        )
        .unwrap();
        let registry = router.telemetry().flights();
        let flight = registry.begin(
            "alpha",
            "",
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
        let cancelled = registry.begin("alpha", "", &http::Method::GET, "/v1/models", 0);
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
                "",
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
        for (width, height, shown) in [(140, 44, 8), (80, 24, 8), (44, 10, 3)] {
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
                .position(|line| line.contains("╔ in flight"))
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

        // The backdrop loses emphasis without losing content; the dialog
        // keeps its semantic colors, and closing it restores every cell.
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        state.flights_open = false;
        terminal
            .draw(|frame| draw(frame, &state, &header()))
            .unwrap();
        let dashboard = terminal.backend().buffer().clone();
        state.flights_open = true;
        terminal
            .draw(|frame| draw(frame, &state, &header()))
            .unwrap();
        let overlay = terminal.backend().buffer();
        let top = overlay
            .content()
            .iter()
            .position(|c| c.symbol() == "╔")
            .unwrap();
        let bottom = overlay
            .content()
            .iter()
            .position(|c| c.symbol() == "╝")
            .unwrap();
        for (index, cell) in overlay.content().iter().enumerate() {
            let inside = (top / 80..=bottom / 80).contains(&(index / 80))
                && (top % 80..=bottom % 80).contains(&(index % 80));
            if !inside {
                assert_eq!(cell.symbol(), dashboard.content()[index].symbol());
                assert_eq!(cell.fg, TERMINAL.border);
                assert_eq!(cell.bg, Color::Reset);
                assert!(cell.modifier.is_empty());
            }
        }
        assert!(overlay.content().iter().any(|c| c.fg == TERMINAL.model));
        assert_eq!(overlay.content()[top + 2].fg, TERMINAL.accent);
        assert!(overlay.content()[top + 2].modifier.contains(Modifier::BOLD));
        state.flights_open = false;
        terminal
            .draw(|frame| draw(frame, &state, &header()))
            .unwrap();
        assert_eq!(terminal.backend().buffer(), &dashboard);
        state.flights_open = true;

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
                "",
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
                .position(|row| row.contains("╔ in flight"))
                .unwrap();
            let out = dialog(&screen);
            let left = rows[top]
                .split("╔ in flight")
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
            out.lines().nth(3).unwrap().ends_with("POST /v1/messages ║"),
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
        assert!(out.contains("POST /v1/messages/count_tokens ║"), "{out}");
        let out = dialog(&screen(80, 24, &state));
        assert!(out.contains("claude-haiku-4-5-20251001"), "{out}");
        assert!(out.contains("POST /v1/message… ║"), "{out}");
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
            registry.begin("alpha", "", &http::Method::GET, "/v1/models", 0);
        }
        let bounded = registry.snapshot(crate::live::MAX_FLIGHTS);
        let mut snapshot = crate::live::Snapshot {
            version: crate::live::VERSION,
            instance: "a".repeat(32),
            listen: "127.0.0.1:8789".parse().unwrap(),
            total: bounded.total,
            upstreams: bounded.upstreams,
            flights: bounded.flights,
        };
        state.apply_live(Some(&snapshot));
        assert_eq!(state.totals.requests, 1);
        assert_eq!(state.totals.body_bytes, bytes);
        assert_eq!(state.models[0].view.in_flight, 205);
        let out = screen(140, 44, &state);
        assert!(out.contains("live 205"), "{out}");
        // The snapshot's count, not its capped list: 25 rows fit, and the
        // title owns up to the other 180.
        state.flights_open = true;
        let out = dialog(&screen(140, 44, &state));
        assert!(out.contains("in flight · 205 · +180 more"), "{out}");

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
        snapshot.upstreams.clear();
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
        let flight = registry.begin("alpha", "", &http::Method::GET, "/v1/models", 0);
        let mut view = flight.view();
        view.age = 30.0;
        assert_eq!(flight_phase(&TERMINAL, &view).0, "prefill");
        view.age = 30.1;
        assert_eq!(flight_phase(&TERMINAL, &view).0, "slow prefill");
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
        assert_eq!(flight_phase(&TERMINAL, &view).0, "stream");
        state.flights = vec![view.clone()];
        assert!(!screen(44, 14, &state).contains("live 1 ("));
        view.idle = 60.1;
        assert_eq!(flight_phase(&TERMINAL, &view), ("stalled", TERMINAL.bad));
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
            .find(|line| line.starts_with("│ upstream"))
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
        detail_lines(&TERMINAL, record)
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
        assert!(!text(&request_line(&TERMINAL, &quiet, Columns::ALL)).contains("tok"));
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
        let line = text(&request_line(&TERMINAL, &counted, Columns::ALL));
        assert!(line.ends_with(" tok 18.2K(99% cached)→891"), "{line}");
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
        let line = text(&request_line(&TERMINAL, &bare, Columns::ALL));
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
        // An encoded hop: a megabyte of answer left as a tenth of one. The
        // upstream hop is identity, so the agent leg is the pair the line shows.
        let mut paired = record("model-zeta", 200);
        paired.received = 1_000_000;
        paired.received_wire = 1_000_000;
        paired.upstream_encoding = "identity".to_string();
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

    /// Behind a receiver the download is saved on the upstream hop, and an
    /// agent that asks for no coding (Codex) gets the answer as decoded. The
    /// line pairs the answer with what that hop carried, as the upload side
    /// does, and the upstream table counts what it kept off the wire.
    #[test]
    fn the_download_row_shows_what_the_upstream_hop_saved() {
        let mut state = State::new();
        state.viewport = 4;
        state.recorded = true;
        let mut hop = record("model-zeta", 200);
        hop.received = 1_000_000;
        hop.received_wire = 100_000;
        hop.upstream_encoding = "zstd".to_string();
        hop.received_agent = 0;
        state.push(Event::Request(Arc::new(hop.clone())));
        // Coded on both legs: the upstream hop is the pair, since that is
        // the one a receiver saves on.
        let mut both = hop;
        both.received_agent = 50_000;
        state.push(Event::Request(Arc::new(both)));
        state.tick_recorded(&crate::watch::Window::default());
        let out = screen(140, 44, &state);
        assert_eq!(out.matches("down 977KB→98KB -90%").count(), 2, "{out}");

        let mut view = StatsView {
            down_bytes: 1_000_000,
            down_wire_bytes: 100_000,
            ..StatsView::default()
        };
        assert_eq!(view.down_saved_bytes(), 900_000);
        view.down_wire_bytes = 1_100_000;
        assert_eq!(
            view.down_saved_bytes(),
            -100_000,
            "clamped where it is drawn"
        );
        state.models = vec![crate::tui::state::ModelRow {
            name: "codex".into(),
            view: StatsView {
                requests: 1,
                down_bytes: 1_000_000,
                down_wire_bytes: 100_000,
                ..StatsView::default()
            },
        }];
        let wide = screen(ROOMY_TABLE, 44, &state);
        let header = table_header(&wide);
        assert!(
            header.contains("down ") && header.contains("↓ saved"),
            "{wide}"
        );
        let row = wide
            .lines()
            .find(|line| line.starts_with("│ codex"))
            .expect("the codex row");
        assert!(row.contains("879KB"), "900,000 bytes saved:\n{wide}");
    }

    /// On the narrowest table that still has them, the sizes line up on the
    /// right under their headings, and the saved cell carries the ratio the
    /// HUD's upload row prints; a route that carried no body has none to give.
    #[test]
    fn the_sizes_line_up_on_the_right_and_the_saving_gives_its_ratio() {
        let mut state = State::new();
        state.models = vec![
            crate::tui::state::ModelRow {
                name: "codex".into(),
                view: StatsView {
                    requests: 1,
                    body_bytes: 1_000_000,
                    wire_bytes: 35_000,
                    ..StatsView::default()
                },
            },
            crate::tui::state::ModelRow {
                name: "catalog".into(),
                view: StatsView {
                    requests: 1,
                    ..StatsView::default()
                },
            },
        ];
        let out = screen(78, 44, &state);
        let cells = |name: &str| {
            out.lines()
                .find(|line| line.starts_with(&format!("│ {name} ")))
                .unwrap_or_else(|| panic!("the {name} row:\n{out}"))
                .trim_end_matches(['│', ' '])
                .to_string()
        };
        // 965,000 of 1,000,000 bytes: 96.5%, truncated.
        assert!(cells("codex").ends_with(" 942KB (-96%)"), "{out}");
        assert!(cells("catalog").ends_with(" 0B (-)"), "{out}");
        let header = table_header(&out);
        let end = |line: &str, text: &str| line.find(text).map(|at| at + text.len());
        assert_eq!(end(&cells("codex"), "977KB"), end(&header, "raw"), "{out}");
        assert_eq!(end(&cells("codex"), "34KB"), end(&header, "wire"), "{out}");
        assert_eq!(cells("codex").len(), header.len(), "{out}");
    }

    #[test]
    fn eleven_models_leave_room_for_events_and_rank_recent_activity() {
        let mut state = State::new();
        for n in 0..11 {
            state.models.push(crate::tui::state::ModelRow {
                name: format!("model-{n:02}"),
                view: StatsView::default(),
            });
        }
        assert!(state.recent_models().is_empty());
        assert!(!screen(100, 24, &state).contains("recent upstreams"));
        for n in [9, 2, 7, 4, 7] {
            state.push(Event::Request(Arc::new(record(
                &format!("model-{n:02}"),
                200,
            ))));
        }
        let names = |state: &State| {
            state
                .recent_models()
                .iter()
                .map(|row| row.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&state), ["model-07", "model-04", "model-02"]);
        state.models[10].view.in_flight = 1;
        assert_eq!(names(&state), ["model-10", "model-07", "model-04"]);
        state.set_filter(crate::tui::state::Filter::Upstream("model-02".into()));
        assert_eq!(names(&state), ["model-10", "model-07", "model-04"]);
        for width in [78, 140] {
            let out = screen(width, 24, &state);
            assert!(out.contains("recent upstreams · 3/11"), "{out}");
            assert!(out.contains("socket bytes/s"), "{out}");
            let panes = panes(Rect::new(0, 0, width, 24), state.recent_models().len()).unwrap();
            assert_eq!(panes.models.unwrap().height, 6);
            assert!(panes.events.height >= 6);
            let table = out.lines().skip(5).take(6).collect::<Vec<_>>().join("\n");
            assert!(table.contains("model-10"), "{table}");
            assert!(!table.contains("model-02"), "{table}");
        }
        state.flights_available = false;
        assert_eq!(names(&state), ["model-07", "model-04", "model-02"]);
        assert_eq!(state.models.len(), 11);
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
                &[],
                &[(
                    "model-zeta".to_string(),
                    "https://example.invalid".to_string(),
                )],
                None,
            )
            .unwrap(),
        );

        let medium = screen(140, 14, &state);
        assert!(medium.contains("upstreams"), "{medium}");
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
        assert!(out.contains("cycle the upstream filter"), "{out}");
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
            tier: None,
            requests: 869,
            prompt: 112_546_008,
            cached: 111_147_008,
            unreported: 0,
            completion: 478_284,
            reasoning: 211_829,
            long: 0,
            charge: Some(spend::Charge {
                input: 2.3783,
                cache_read: 18.8950,
                output: 4.0654,
            }),
        };
        let flash = spend::Row {
            model: "model-alpha".to_string(),
            tier: None,
            requests: 389,
            prompt: 42_881_773,
            cached: 0,
            unreported: 389,
            completion: 240_377,
            reasoning: 126_559,
            long: 0,
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
                tier: None,
                requests: 1_258,
                prompt: 155_427_781,
                cached: 111_147_008,
                unreported: 389,
                completion: 718_661,
                reasoning: 338_388,
                long: 0,
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

    /// A model and its tier outgrow the 20 columns a label always had: the
    /// column widens to keep the whole name, in the table and in the costs
    /// popup, and the numbers stay in their columns beside it.
    #[test]
    fn a_tier_widens_the_label_column_rather_than_losing_its_name() {
        let mut state = usage_state();
        state.usage.as_mut().unwrap().rows[1].tier = Some("tier-ultrafast".into());
        let label = "model-alpha · tier-ultrafast";

        let out = screen(120, 20, &state);
        assert!(out.contains(label), "{out}");
        state.usage_rates = true;
        let out = screen(120, 20, &state);
        assert!(out.contains(label), "{out}");

        let lines: Vec<String> = cost_lines(&TERMINAL, state.usage.as_ref().unwrap())
            .iter()
            .map(text)
            .collect();
        let column = |needle: &str| {
            let line = lines.iter().find(|line| line.contains(needle)).unwrap();
            line.chars().count()
        };
        assert_eq!(column(label), column("model-epsilon"), "{lines:#?}");
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
        let lines: Vec<String> = cost_lines(&TERMINAL, state.usage.as_ref().unwrap())
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
                &TERMINAL,
                &record,
                Columns::parse("time,tokens").unwrap()
            )),
            "23:41:02 tok 18.2K(99% cached)→891"
        );

        let narrowed = Columns::parse("status,cut,route").unwrap();
        assert_eq!(
            text(&request_line(&TERMINAL, &record, narrowed)),
            "200 POST ../completions"
        );
        record.complete = false;
        assert_eq!(
            text(&request_line(&TERMINAL, &record, narrowed)),
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
            text(&request_line(&TERMINAL, &fresh, narrowed)),
            "200 POST ../completions."
        );

        // Everything off is a request too, and an empty line answers it.
        assert_eq!(
            text(&request_line(
                &TERMINAL,
                &fresh,
                Columns::parse("").unwrap()
            )),
            ""
        );
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
        for case in cases("share") {
            let pair = (case[0][0].as_u64().unwrap(), case[0][1].as_u64().unwrap());
            assert_eq!(share(pair.0, pair.1).as_deref(), case[1].as_str());
        }
        for case in cases("label") {
            let (model, tier) = (case[0][0].as_str().unwrap(), case[0][1].as_str());
            assert_eq!(spend::label(model, tier), text(&case));
        }
    }

    /// One of each screen the dashboard draws, with something on every part
    /// of it: the panes with a highlighted line, then each popup and dialog.
    fn scenes() -> Vec<(&'static str, State)> {
        let dashboard = || {
            let mut state = populated();
            state.models = vec![crate::tui::state::ModelRow {
                name: "codex".into(),
                view: StatsView {
                    requests: 2,
                    in_flight: 1,
                    body_bytes: 1_000_000,
                    wire_bytes: 35_000,
                    down_bytes: 1_000_000,
                    down_wire_bytes: 100_000,
                    upstream_errors: 1,
                    ..StatsView::default()
                },
            }];
            state.scroll(-1);
            state
        };
        let registry = crate::flights::Flights::default();
        let flight = registry.begin("alpha", "", &http::Method::POST, "/v1/messages", 100);
        let mut scenes = vec![("dashboard", dashboard())];
        type Open = fn(&mut State);
        let opened: [(&str, Open); 6] = [
            ("help", |state| state.help = true),
            ("columns", |state| state.picker = true),
            ("themes", |state| state.themes = true),
            ("detail", |state| state.detail = true),
            ("flights", |state| state.flights_open = true),
            ("flights-empty", |state| {
                state.flights.clear();
                state.flights_open = true;
            }),
        ];
        for (name, open) in opened {
            let mut state = dashboard();
            state.flights = vec![flight.view()];
            open(&mut state);
            scenes.push((name, state));
        }
        scenes.push(("usage", usage_state()));
        let mut costs = usage_state();
        costs.usage_rates = true;
        scenes.push(("costs", costs));
        scenes
    }

    /// `T` lists every theme with chips of its data colors, on the cursor's
    /// ground where the cursor is, with the keys under the list.
    #[test]
    fn the_theme_picker_names_every_theme_and_marks_the_cursor() {
        let mut state = populated();
        state.theme = &crate::tui::theme::CATPPUCCIN_LATTE;
        state.themes = true;
        state.themes_at = 3;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| draw(frame, &state, &header()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = buffer
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        for theme in THEMES {
            assert!(
                rows.iter().any(|row| row.contains(theme.name)),
                "{}",
                theme.name
            );
        }
        assert!(rows.iter().any(|row| row.contains("↵ keep")), "{rows:#?}");
        let at = rows
            .iter()
            .position(|row| row.contains("Catppuccin Latte"))
            .unwrap();
        let column = rows[at].find("Catppuccin Latte").unwrap();
        let cell = &buffer.content()[at * 100 + rows[at][..column].chars().count()];
        assert_eq!(Some(cell.bg), state.theme.select_raised);
        let chip = |theme: &Theme| {
            buffer.content()[at * 100..(at + 1) * 100]
                .iter()
                .any(|cell| cell.bg == theme.model)
        };
        assert!(chip(state.theme), "the row's own colors");
    }

    /// A catalog fetch counts, but a successful one is not a line: the event
    /// list is the agent's turns. One that failed is trouble, and shows.
    #[test]
    fn a_catalog_fetch_is_counted_but_not_listed() {
        let mut state = State::new();
        state.viewport = 4;
        let mut fetch = record("", 200);
        fetch.upstream = "codex".to_string();
        fetch.method = http::Method::GET;
        fetch.path = "/codex/models".to_string();
        fetch.body_len = 0;
        fetch.wire_len = 0;
        state.push(Event::Request(Arc::new(fetch.clone())));
        state.push(Event::Request(Arc::new(record("model-alpha", 200))));
        assert_eq!(state.len(), 1);
        let out = screen(140, 30, &state);
        assert!(!out.contains("/models"), "{out}");
        assert!(out.contains("model-alpha"), "{out}");

        fetch.status = 503;
        state.push(Event::Request(Arc::new(fetch)));
        assert_eq!(state.len(), 2);
        assert!(screen(140, 30, &state).contains("GET /codex/models"));
    }

    /// A theme that paints its own ground paints all of it. A cell left at
    /// `Reset` is the terminal's own color showing through: a dark hole in a
    /// light theme, with whatever text it holds drawn for the other one.
    #[test]
    fn a_painted_theme_leaves_no_cell_to_the_terminal() {
        for theme in &THEMES[1..] {
            for (name, mut state) in scenes() {
                state.theme = theme;
                for (width, height) in [(150, 44), (80, 24), (40, 8)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| draw(frame, &state, &header()))
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    for (at, cell) in buffer.content().iter().enumerate() {
                        assert!(
                            cell.fg != Color::Reset && cell.bg != Color::Reset,
                            "{} {name} at {width}x{height}: ({}, {}) is {cell:?}",
                            theme.id,
                            at % width as usize,
                            at / width as usize,
                        );
                    }
                }
            }
        }
    }
}
