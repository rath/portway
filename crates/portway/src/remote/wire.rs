//! The existing web console protocol, decoded without a dependency on `web`.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Deserializer};

use crate::board::{BARS, ModelRow, TRAFFIC_SECONDS, Totals};
use crate::flights::{FlightView, Phase};
use crate::forwarder::{Coding, StatsView};
use crate::telemetry::{Event, RequestRecord};
use crate::{logfmt::Level, spend, usage::Usage};

fn method<'de, D: Deserializer<'de>>(d: D) -> Result<http::Method, D::Error> {
    String::deserialize(d)?
        .parse()
        .map_err(serde::de::Error::custom)
}
fn coding<'de, D: Deserializer<'de>>(d: D) -> Result<Coding, D::Error> {
    match Option::<String>::deserialize(d)?.as_deref() {
        None | Some("identity") => Ok(Coding::None),
        Some("zstd") => Ok(Coding::Zstd),
        Some("gzip") => Ok(Coding::Gzip),
        Some("dcz") => Ok(Coding::Dcz),
        _ => Err(serde::de::Error::custom("unknown coding")),
    }
}
fn phase<'de, D: Deserializer<'de>>(d: D) -> Result<Phase, D::Error> {
    match String::deserialize(d)?.as_str() {
        "upload" => Ok(Phase::Upload),
        "prefill" => Ok(Phase::Prefill),
        "stream" => Ok(Phase::Stream),
        _ => Err(serde::de::Error::custom("unknown phase")),
    }
}

#[derive(Deserialize)]
pub struct Header {
    pub coding: String,
    pub mode: String,
    pub started_unix: f64,
    pub uptime_s: f64,
}
#[derive(Deserialize)]
pub struct Snapshot {
    pub header: Header,
    pub seq: u64,
    pub oldest: u64,
    pub generation: u64,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub bars: Vec<(u64, u64)>,
    pub bars_total: u64,
    pub traffic: Traffic,
    pub flights: Option<Flights>,
    pub events: Vec<WireEvent>,
}
#[derive(Deserialize)]
pub struct Tick {
    pub generation: u64,
    pub uptime_s: f64,
    #[serde(flatten)]
    pub metrics: Metrics,
    pub bars_push: Vec<(u64, u64)>,
    pub bars_total: u64,
    pub traffic_tail: Traffic,
}
#[derive(Deserialize)]
pub struct Metrics {
    #[serde(with = "TotalsData")]
    pub totals: Totals,
    pub counts: Counts,
    pub latency: Latency,
    pub models: Vec<Model>,
    pub coverage: Option<f64>,
}
#[derive(Deserialize)]
#[serde(remote = "Totals")]
struct TotalsData {
    requests: u64,
    encoded: u64,
    in_flight: u64,
    body_bytes: u64,
    wire_bytes: u64,
    down_bytes: u64,
    down_wire_bytes: u64,
    agent_bytes: u64,
    retried_identity: u64,
    aborts: u64,
    upstream_errors: u64,
    idle_conns: usize,
}
#[derive(Deserialize)]
pub struct Counts {
    pub seen: u64,
    pub ok: u64,
    pub redirected: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub reused: u64,
    pub truncated: u64,
}
#[derive(Deserialize, Default)]
pub struct Quantiles {
    pub p50: Option<f64>,
    pub p95: Option<f64>,
}
#[derive(Deserialize, Default)]
pub struct Latency {
    pub ttfb: Quantiles,
    pub upload: Quantiles,
    pub handshake_mean: Option<f64>,
}
#[derive(Deserialize)]
pub struct Model {
    name: String,
    #[serde(deserialize_with = "coding")]
    coding: Coding,
    dict: bool,
    requests: u64,
    encoded: u64,
    in_flight: u64,
    body: u64,
    wire: u64,
    down: u64,
    down_wire: u64,
    agent: u64,
    idle: usize,
    errors: u64,
    aborts: u64,
    retried_identity: u64,
    pub status: String,
}
impl Model {
    pub fn into_row(self) -> ModelRow {
        ModelRow {
            name: self.name,
            view: StatsView {
                coding: self.coding,
                dict: self.dict,
                requests: self.requests,
                encoded_requests: self.encoded,
                in_flight: self.in_flight,
                body_bytes: self.body,
                wire_bytes: self.wire,
                down_bytes: self.down,
                down_wire_bytes: self.down_wire,
                agent_bytes: self.agent,
                idle_conns: self.idle,
                upstream_errors: self.errors,
                client_aborts: self.aborts,
                retried_identity: self.retried_identity,
                ..Default::default()
            },
        }
    }
}
#[derive(Deserialize)]
pub struct Traffic {
    pub end: u64,
    pub up: Vec<u64>,
    pub down: Vec<u64>,
}
#[derive(Deserialize)]
pub struct Events {
    pub events: Vec<WireEvent>,
    pub oldest: u64,
}

#[derive(Deserialize)]
pub struct WireEvent {
    pub seq: u64,
    #[serde(flatten)]
    pub data: EventData,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventData {
    Request(#[serde(deserialize_with = "boxed_record")] Box<RequestRecord>),
    Log {
        stamp: String,
        level: String,
        message: String,
    },
}
impl WireEvent {
    pub fn into_event(self) -> Event {
        match self.data {
            EventData::Request(record) => Event::Request(Arc::from(record)),
            EventData::Log {
                stamp,
                level,
                message,
            } => Event::Log {
                stamp,
                message,
                level: match level.as_str() {
                    "ERROR" => Level::Error,
                    "WARNING" => Level::Warning,
                    _ => Level::Info,
                },
            },
        }
    }
}
fn boxed_record<'de, D: Deserializer<'de>>(d: D) -> Result<Box<RequestRecord>, D::Error> {
    RecordData::deserialize(d).map(Box::new)
}

#[derive(Deserialize)]
#[serde(remote = "RequestRecord")]
struct RecordData {
    stamp: String,
    model: String,
    #[serde(deserialize_with = "method")]
    method: http::Method,
    path: String,
    status: u16,
    dns: Option<f64>,
    tcp: Option<f64>,
    tls: Option<f64>,
    body_len: u64,
    wire_len: u64,
    #[serde(deserialize_with = "coding")]
    coding: Coding,
    upload: Option<f64>,
    ttfb: f64,
    received: u64,
    received_wire: u64,
    #[serde(default)]
    received_agent: u64,
    upstream_encoding: String,
    agent_encoding: Option<String>,
    download: Option<f64>,
    complete: bool,
    #[serde(default, deserialize_with = "optional_usage")]
    usage: Option<Usage>,
    flight: Option<u64>,
}
#[derive(Deserialize)]
struct UsageData {
    prompt: u64,
    completion: u64,
    cached: Option<u64>,
    reasoning: Option<u64>,
}
fn optional_usage<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Usage>, D::Error> {
    Ok(Option::<UsageData>::deserialize(d)?.map(|u| Usage {
        prompt: u.prompt,
        completion: u.completion,
        cached: u.cached,
        reasoning: u.reasoning,
    }))
}

#[derive(Deserialize)]
pub struct Flights {
    pub total: u64,
    pub list: Vec<Flight>,
}
#[derive(Deserialize)]
pub struct Flight(#[serde(with = "FlightData")] pub FlightView);
#[derive(Deserialize)]
#[serde(remote = "FlightView")]
struct FlightData {
    id: u64,
    model: String,
    #[serde(deserialize_with = "method")]
    method: http::Method,
    path: String,
    started_unix: f64,
    #[serde(rename = "age_s")]
    age: f64,
    #[serde(rename = "idle_s")]
    idle: f64,
    #[serde(deserialize_with = "phase")]
    phase: Phase,
    body_len: u64,
    wire_len: u64,
    #[serde(deserialize_with = "coding")]
    coding: Coding,
    status: Option<u16>,
    ttfb: Option<f64>,
    received: u64,
    received_wire: u64,
    received_agent: u64,
    retries: u32,
    #[serde(rename = "upload_s")]
    upload: Option<f64>,
}

#[derive(Deserialize)]
pub struct UsageReply {
    pub title: String,
    #[serde(flatten, with = "TableData")]
    pub table: spend::Table,
}
#[derive(Deserialize)]
#[serde(remote = "spend::Table")]
struct TableData {
    since: f64,
    until: f64,
    #[serde(deserialize_with = "rows")]
    rows: Vec<spend::Row>,
    #[serde(with = "RowData")]
    total: spend::Row,
    unpriced: usize,
    blind: u64,
    cut: u64,
}
#[derive(Deserialize)]
#[serde(remote = "spend::Row")]
struct RowData {
    model: String,
    requests: u64,
    prompt: u64,
    cached: u64,
    unreported: u64,
    completion: u64,
    reasoning: u64,
    #[serde(deserialize_with = "charge")]
    charge: Option<spend::Charge>,
}
fn rows<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<spend::Row>, D::Error> {
    #[derive(Deserialize)]
    struct Row(#[serde(with = "RowData")] spend::Row);
    Ok(Vec::<Row>::deserialize(d)?
        .into_iter()
        .map(|r| r.0)
        .collect())
}
fn charge<'de, D: Deserializer<'de>>(d: D) -> Result<Option<spend::Charge>, D::Error> {
    #[derive(Deserialize)]
    struct Charge {
        input: f64,
        cache_read: f64,
        output: f64,
    }
    Ok(Option::<Charge>::deserialize(d)?.map(|c| spend::Charge {
        input: c.input,
        cache_read: c.cache_read,
        output: c.output,
    }))
}

/// Only remote-specific presentation and cursors; ordinary TUI state remains local.
pub struct RemoteState {
    pub connected: bool,
    pub message: String,
    pub seq: u64,
    pub generation: u64,
    pub coding: String,
    pub latency: Latency,
    pub statuses: BTreeMap<String, String>,
    pub usage_title: Option<String>,
    pub uptime: f64,
    pub sampled: Instant,
    pub flights_at: Instant,
    pub original_flights: Vec<FlightView>,
    pub bars_total: u64,
    pub traffic_end: u64,
    pub traffic: VecDeque<(u64, u64)>,
}
impl Default for RemoteState {
    fn default() -> Self {
        Self {
            connected: false,
            message: "connecting".into(),
            seq: 0,
            generation: 0,
            coding: String::new(),
            latency: Latency::default(),
            statuses: BTreeMap::new(),
            usage_title: None,
            uptime: 0.0,
            sampled: Instant::now(),
            flights_at: Instant::now(),
            original_flights: Vec::new(),
            bars_total: 0,
            traffic_end: 0,
            traffic: VecDeque::new(),
        }
    }
}
impl RemoteState {
    pub fn traffic(&mut self, tail: Traffic, replace: bool) {
        if replace || tail.end < self.traffic_end {
            self.traffic.clear();
        }
        let count = tail.up.len().min(tail.down.len()).min(TRAFFIC_SECONDS);
        let first = tail.end.saturating_sub(count.saturating_sub(1) as u64);
        if !replace && tail.end > self.traffic_end {
            for _ in 0..(tail.end - self.traffic_end).min(TRAFFIC_SECONDS as u64) {
                self.traffic.push_back((0, 0));
            }
        }
        if self.traffic.len() < count {
            self.traffic.resize(count, (0, 0));
        }
        for (offset, pair) in tail.up.into_iter().zip(tail.down).take(count).enumerate() {
            let age = tail.end - (first + offset as u64);
            let index = self.traffic.len().saturating_sub(age as usize + 1);
            if let Some(slot) = self.traffic.get_mut(index) {
                *slot = pair;
            }
        }
        while self.traffic.len() > TRAFFIC_SECONDS {
            self.traffic.pop_front();
        }
        self.traffic_end = tail.end;
    }
}

pub fn append_bars(
    bars: &mut VecDeque<(u64, u64)>,
    previous: u64,
    total: u64,
    fresh: Vec<(u64, u64)>,
) {
    let count = total.saturating_sub(previous).min(fresh.len() as u64) as usize;
    bars.extend(
        fresh
            .into_iter()
            .rev()
            .take(count)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    while bars.len() > BARS {
        bars.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_traffic_tails_replace_seconds_and_bars_do_not_repeat() {
        let mut remote = RemoteState::default();
        remote.traffic(
            Traffic {
                end: 2,
                up: vec![1, 2, 3],
                down: vec![10, 20, 30],
            },
            true,
        );
        remote.traffic(
            Traffic {
                end: 3,
                up: vec![4, 5],
                down: vec![40, 50],
            },
            false,
        );
        remote.traffic(
            Traffic {
                end: 3,
                up: vec![4, 6],
                down: vec![40, 60],
            },
            false,
        );
        assert_eq!(
            remote.traffic.iter().copied().collect::<Vec<_>>(),
            [(1, 10), (2, 20), (4, 40), (6, 60)]
        );
        remote.traffic(
            Traffic {
                end: 1,
                up: vec![7, 8],
                down: vec![70, 80],
            },
            true,
        );
        assert_eq!(
            remote.traffic.iter().copied().collect::<Vec<_>>(),
            [(7, 70), (8, 80)]
        );
        let mut bars = VecDeque::from([(100, 50)]);
        append_bars(&mut bars, 1, 2, vec![(100, 50), (200, 80)]);
        append_bars(&mut bars, 2, 2, vec![(200, 80)]);
        assert_eq!(bars, VecDeque::from([(100, 50), (200, 80)]));
    }
}
