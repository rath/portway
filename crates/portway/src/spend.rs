//! What the day has cost, per model: the engines' own token counts added up
//! and priced.
//!
//! The `u` screen (and the web console's usage view) reads this straight out of the database rather than out of
//! the counters in memory, so it covers the whole local day even for a
//! dashboard that started a minute ago, and the written record is the only
//! place the two can be compared.
//!
//! Prices are supplied by the user in `portway.toml`. These are estimates,
//! not billing records. There is no built-in provider or price table.
//!
//! Three shapes of "not known" stay apart instead of folding into a zero:
//! a request whose answer carried no `usage` object is counted as blind and
//! left out of the sums, an answer whose engine reported no cache detail is
//! billed at the input rate and flagged, and a model the price table does not
//! name gets no cost at all rather than a free one.

use std::path::Path;

use rusqlite::params;

use crate::logfmt;
use crate::store;

/// What the usage screen is measuring: four windows, each of which starts at a
/// local midnight, in the order the selector draws them.
///
/// `yesterday` is the whole of the day before and the only one that ends at a
/// midnight; the rest run to now, so `7 days` is today and the six days under
/// it rather than 168 hours — a spend question is asked about days.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Today,
    Yesterday,
    Week,
    Month,
}

impl Range {
    /// Draw order, and the order the arrow keys walk.
    pub const ALL: [Range; 4] = [Range::Today, Range::Yesterday, Range::Week, Range::Month];

    /// What the selector calls it.
    pub fn label(self) -> &'static str {
        match self {
            Range::Today => "today",
            Range::Yesterday => "yesterday",
            Range::Week => "7 days",
            Range::Month => "30 days",
        }
    }

    /// The half-open window `[since, until)` this range names, measured from
    /// `now`.
    ///
    /// Every boundary is the local midnight of the day it starts, taken from
    /// that day: a clock change inside the window makes it an hour longer or
    /// shorter rather than moving its start.
    pub fn window(self, now: f64) -> (f64, f64) {
        const DAY: f64 = 86_400.0;
        let midnight = logfmt::midnight(now);
        match self {
            Range::Today => (midnight, now),
            // A second before today's midnight is yesterday, whatever length
            // the clocks gave that one.
            Range::Yesterday => (logfmt::midnight(midnight - 1.0), midnight),
            Range::Week => (logfmt::midnight(midnight - 6.0 * DAY), now),
            Range::Month => (logfmt::midnight(midnight - 29.0 * DAY), now),
        }
    }

    /// The name a URL or a JSON body uses for it.
    pub fn key(self) -> &'static str {
        match self {
            Range::Today => "today",
            Range::Yesterday => "yesterday",
            Range::Week => "week",
            Range::Month => "month",
        }
    }

    pub fn from_key(key: &str) -> Option<Range> {
        Range::ALL.into_iter().find(|range| range.key() == key)
    }

    /// The range `delta` steps away in `ALL`, wrapping at both ends.
    pub fn step(self, delta: isize) -> Range {
        let count = Range::ALL.len() as isize;
        let at = Range::ALL
            .iter()
            .position(|range| *range == self)
            .unwrap_or(0) as isize;
        Range::ALL[(at + delta).rem_euclid(count) as usize]
    }
}

pub use crate::config::Price;
use crate::config::Prices;

/// Where the money went: the three parts of one model's bill, kept apart so
/// the screen can show which of them the day is actually made of.
#[derive(Clone, Copy, Debug, Default)]
pub struct Charge {
    /// The context that was prefilled, at the input rate.
    pub input: f64,
    /// The context the prefix cache served, at the cache rate.
    pub cache_read: f64,
    /// Everything the engine generated, thinking included, at the output rate.
    pub output: f64,
}

impl Charge {
    pub fn total(&self) -> f64 {
        self.input + self.cache_read + self.output
    }
}

/// One model's day: what the engines counted, and what it would cost.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub model: String,
    pub requests: u64,
    /// The context every answer read, the cached part included.
    pub prompt: u64,
    /// The part a prefix cache served. An answer that reported no cache detail
    /// rides here at zero and is counted in `unreported`.
    pub cached: u64,
    /// Answers that said nothing about their cache — the count that makes
    /// `cached` a floor rather than a fact.
    pub unreported: u64,
    pub completion: u64,
    /// The thinking inside `completion`, which is billed as output.
    pub reasoning: u64,
    /// `None` when no price row names this model.
    pub charge: Option<Charge>,
}

impl Row {
    /// What was prefilled rather than read from the cache.
    pub fn uncached(&self) -> u64 {
        self.prompt.saturating_sub(self.cached)
    }

    /// The share the prefix cache served, or `None` when nothing reported one.
    pub fn hit_rate(&self) -> Option<f64> {
        (self.prompt > 0 && self.unreported < self.requests)
            .then(|| self.cached as f64 / self.prompt as f64)
    }

    pub fn cost(&self) -> Option<f64> {
        self.charge.map(|charge| charge.total())
    }
}

/// A local day, added up. `blind` is what the sums are missing: requests whose
/// answer carried no `usage` object, `cut` of which ended early.
#[derive(Default, Debug, Clone)]
pub struct Table {
    pub since: f64,
    pub until: f64,
    pub rows: Vec<Row>,
    /// The same columns as a model, pooled: what the day costs is one row of
    /// the same arithmetic, and the screen draws it with the same code.
    pub total: Row,
    /// Models the price table does not name: their tokens are in `total`, their
    /// money is not.
    pub unpriced: usize,
    pub blind: u64,
    pub cut: u64,
}

impl Table {
    /// Nothing was recorded in the window — said apart from a database that
    /// could not be read at all.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn cost(&self) -> Option<f64> {
        self.total.cost()
    }

    fn absorb(&mut self, row: &Row) {
        let total = &mut self.total;
        total.model = "total".to_string();
        total.requests += row.requests;
        total.prompt += row.prompt;
        total.cached += row.cached;
        total.unreported += row.unreported;
        total.completion += row.completion;
        total.reasoning += row.reasoning;
        match (row.charge, &mut total.charge) {
            (Some(charge), Some(sum)) => {
                sum.input += charge.input;
                sum.cache_read += charge.cache_read;
                sum.output += charge.output;
            }
            (Some(charge), sum @ None) => *sum = Some(charge),
            (None, _) => self.unpriced += 1,
        }
    }
}

/// What the numbers are not, in the order a reader would ask: whose money,
/// and which part of it is a floor. Every screen that shows a `Table` writes
/// these under it, word for word.
pub fn notes(table: &Table) -> Vec<String> {
    let mut notes = Vec::new();
    if table.blind > 0 {
        notes.push(format!(
            "{} request(s) in this window reported no usage ({} ended early): their tokens are missing from these sums",
            table.blind, table.cut
        ));
    }
    for row in &table.rows {
        if row.requests > 0 && row.unreported == row.requests {
            notes.push(format!(
                "{}: no cache detail reported, so its prompt is charged at the input rate — an upper bound",
                row.model
            ));
        }
    }
    if table.unpriced > 0 {
        notes.push(format!(
            "{} model(s) have no price here: their tokens are in the totals, their money is not",
            table.unpriced
        ));
    }
    if table.total.completion > 0 {
        notes.push(format!(
            "{} of the {} output tokens were thinking — inside the output rate, not on top of it",
            logfmt::human_count(table.total.reasoning),
            logfmt::human_count(table.total.completion),
        ));
    }
    notes.push(
        "estimates use configured USD rates per million tokens; they are not a bill".to_string(),
    );
    notes
}

/// `(prompt - cached) × input + cached × cache_read + completion × output`,
/// per million tokens. Reasoning tokens are inside `completion` and never
/// added twice.
fn charge(uncached: u64, cached: u64, completion: u64, price: Price) -> Charge {
    Charge {
        input: uncached as f64 * price.input / 1e6,
        cache_read: cached as f64 * price.cache_read / 1e6,
        output: completion as f64 * price.output / 1e6,
    }
}

/// The per-model tokens, and the requests that carried none.
///
/// A model whose answers all reported no cache detail sums `cached` to zero,
/// which is exactly how it gets billed at the input rate — an upper bound
/// until the engine says otherwise.
const AGGREGATE: &str = "SELECT
    model,
    COUNT(*),
    COALESCE(SUM(prompt_tokens), 0),
    COALESCE(SUM(cached_tokens), 0),
    SUM(cached_tokens IS NULL),
    COALESCE(SUM(completion_tokens), 0),
    COALESCE(SUM(reasoning_tokens), 0)
  FROM requests
  WHERE ts_unix >= ?1 AND ts_unix < ?2 AND prompt_tokens IS NOT NULL
  GROUP BY model";

/// Requests the sums above had to skip, and how many of them were cut short.
const BLIND: &str = "SELECT COUNT(*), COALESCE(SUM(complete = 0), 0)
  FROM requests
  WHERE ts_unix >= ?1 AND ts_unix < ?2 AND prompt_tokens IS NULL";

/// Read `[since, until)` back out of the database, priced. Anything that makes
/// the read impossible is an error for the screen to show: a dashboard with a
/// wrong number on it is worse than one that says why it has none.
pub fn load(db: &Path, since: f64, until: f64, prices: &Prices) -> Result<Table, String> {
    let Some(connection) = store::open_existing(db)? else {
        return Err(format!(
            "no database at {}: nothing has been recorded yet",
            db.display()
        ));
    };
    if !store::has_token_columns(&connection)? {
        return Err(format!(
            "{} predates the token counts: nothing to add up",
            db.display()
        ));
    }

    let mut table = Table {
        since,
        until,
        ..Table::default()
    };

    let mut statement = connection.prepare(AGGREGATE).map_err(sqlite)?;
    let rows = statement
        .query_map(params![since, until], |row| {
            Ok(Row {
                model: row.get(0)?,
                requests: row.get::<_, i64>(1)? as u64,
                prompt: row.get::<_, i64>(2)? as u64,
                cached: row.get::<_, i64>(3)? as u64,
                unreported: row.get::<_, i64>(4)? as u64,
                completion: row.get::<_, i64>(5)? as u64,
                reasoning: row.get::<_, i64>(6)? as u64,
                charge: None,
            })
        })
        .map_err(sqlite)?;
    for row in rows {
        let mut row = row.map_err(sqlite)?;
        row.charge = prices
            .get(&row.model)
            .copied()
            .map(|price| charge(row.uncached(), row.cached, row.completion, price));
        table.absorb(&row);
        table.rows.push(row);
    }

    let (blind, cut) = connection
        .query_row(BLIND, params![since, until], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
        })
        .map_err(sqlite)?;
    table.blind = blind;
    table.cut = cut;

    // Most expensive first, which is the one question the screen is opened to
    // answer. A model that could not be priced goes last: its tokens are still
    // in the totals, its money is not.
    table.rows.sort_by(|a, b| match (a.cost(), b.cost()) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.prompt.cmp(&a.prompt),
    });
    Ok(table)
}

fn sqlite(err: rusqlite::Error) -> String {
    format!("sqlite: {err}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::Method;

    use super::*;
    fn price(model: &str) -> Option<Price> {
        fixture_prices().get(model).copied()
    }
    fn fixture_prices() -> Prices {
        [
            (
                "model-alpha".into(),
                Price {
                    input: 0.09,
                    output: 0.3,
                    cache_read: 0.018,
                },
            ),
            (
                "model-beta".into(),
                Price {
                    input: 0.16,
                    output: 0.47,
                    cache_read: 0.016,
                },
            ),
            (
                "model-gamma".into(),
                Price {
                    input: 0.2,
                    output: 2.5,
                    cache_read: 0.05,
                },
            ),
            (
                "model-delta".into(),
                Price {
                    input: 0.15,
                    output: 0.6,
                    cache_read: 0.003,
                },
            ),
            (
                "model-epsilon".into(),
                Price {
                    input: 1.7,
                    output: 8.5,
                    cache_read: 0.17,
                },
            ),
            (
                "model-zeta".into(),
                Price {
                    input: 0.91,
                    output: 2.86,
                    cache_read: 0.169,
                },
            ),
        ]
        .into()
    }
    fn load(db: &Path, since: f64, until: f64) -> Result<Table, String> {
        super::load(db, since, until, &fixture_prices())
    }

    use crate::forwarder::Coding;
    const MODEL_UPSTREAMS: &[(&str, &str)] = &[("model-alpha", "http://example.test")];
    use crate::telemetry::{Event, RequestRecord};
    use crate::usage::Usage;

    /// A budget in one row: a million tokens of context, nine tenths of it
    /// served by the cache, and a thousand generated.
    fn row(model: &str, prompt: u64, cached: u64, completion: u64) -> Row {
        Row {
            model: model.to_string(),
            requests: 1,
            prompt,
            cached,
            unreported: 0,
            completion,
            reasoning: 0,
            charge: price(model).map(|price| charge(prompt - cached, cached, completion, price)),
        }
    }

    /// Every boundary is the local midnight of the day it starts, whatever the
    /// machine's zone is — so the assertions are in `logfmt`'s calendar rather
    /// than in fixed numbers.
    #[test]
    fn a_range_starts_at_the_midnight_of_the_day_it_names() {
        let now = 1_790_008_865.0;
        let day = logfmt::datetime(logfmt::midnight(now));
        assert!(day.ends_with(" 00:00:00"), "{day}");

        let (since, until) = Range::Today.window(now);
        assert_eq!(logfmt::datetime(since), day);
        assert_eq!(until, now);

        // Yesterday is the whole day before, and the only window that ends at
        // a midnight rather than at now.
        let (since, until) = Range::Yesterday.window(now);
        assert!(until < now, "yesterday is over");
        assert_eq!(logfmt::datetime(until), day, "it ends where today starts");
        assert!(logfmt::datetime(since).ends_with(" 00:00:00"));
        assert_ne!(
            &logfmt::datetime(since)[..10],
            &day[..10],
            "a different day from today"
        );
        assert_eq!(logfmt::midnight(since), since, "and is a midnight itself");

        // A week of days is today plus the six under it, however long the
        // clocks made them.
        let (since, until) = Range::Week.window(now);
        assert_eq!(until, now);
        assert!(since <= logfmt::midnight(now) - 6.0 * 86_400.0 + 3_600.0);
        assert!(since >= logfmt::midnight(now) - 6.0 * 86_400.0 - 3_600.0);
        assert_eq!(logfmt::midnight(since), since);

        let (since, _) = Range::Month.window(now);
        assert!(since < Range::Week.window(now).0);
    }

    #[test]
    fn the_arrows_walk_the_ranges_and_wrap() {
        assert_eq!(Range::Today.step(1), Range::Yesterday);
        assert_eq!(Range::Month.step(1), Range::Today);
        assert_eq!(Range::Today.step(-1), Range::Month);
        assert_eq!(Range::Week.step(-2), Range::Today);
        // The labels are what the selector draws, in the same order.
        assert_eq!(
            Range::ALL.map(Range::label),
            ["today", "yesterday", "7 days", "30 days"]
        );
    }

    #[test]
    fn every_registered_upstream_has_a_price() {
        for (model, _) in MODEL_UPSTREAMS {
            assert!(
                price(model).is_some(),
                "{model} is registered but unpriced: add it to PRICES"
            );
        }
    }

    #[test]
    fn a_cost_splits_the_context_into_cached_and_not() {
        // model-epsilon: 0.1M uncached at 1.7, 0.9M cached at 0.17, 1k out at 8.5.
        let priced = row("model-epsilon", 1_000_000, 900_000, 1_000);
        let charge = priced.charge.unwrap();
        assert!((charge.input - 0.17).abs() < 1e-9, "{charge:?}");
        assert!((charge.cache_read - 0.153).abs() < 1e-9, "{charge:?}");
        assert!((charge.output - 0.0085).abs() < 1e-9, "{charge:?}");
        assert!((charge.total() - 0.3315).abs() < 1e-9, "{charge:?}");
        assert_eq!(priced.uncached(), 100_000);
        assert_eq!(priced.hit_rate(), Some(0.9));
    }

    #[test]
    fn an_engine_that_reports_no_cache_is_billed_as_uncached() {
        // model-alpha reports no `prompt_tokens_details` at all, so its
        // whole context is charged at the input rate and the row is marked.
        let mut priced = row("model-alpha", 1_000_000, 0, 0);
        priced.unreported = 1;
        priced.charge = price("model-alpha").map(|price| charge(1_000_000, 0, 0, price));
        assert_eq!(priced.hit_rate(), None, "no rate is not a rate of zero");
        assert!((priced.cost().unwrap() - 0.09).abs() < 1e-9);
    }

    fn record(model: &str, usage: Option<Usage>) -> RequestRecord {
        RequestRecord {
            stamp: "23:41:02".to_string(),
            model: model.to_string(),
            method: Method::POST,
            path: "/v1/chat/completions".to_string(),
            status: 200,
            dns: None,
            tcp: None,
            tls: None,
            body_len: 1024,
            wire_len: 1024,
            coding: Coding::None,
            upload: None,
            ttfb: 0.5,
            received: 512,
            received_wire: 512,
            received_agent: 512,
            upstream_encoding: "identity".to_string(),
            agent_encoding: None,
            download: None,
            complete: true,
            usage,
            flight: None,
        }
    }

    fn populated(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("portway-spend-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = store::spawn(&dir, 0).unwrap();
        let sender = store.sender();
        let answers = [
            ("model-epsilon", 1000, Some(900), 100),
            ("model-epsilon", 2000, Some(1800), 200),
            ("model-alpha", 500, None, 50),
        ];
        for (model, prompt, cached, completion) in answers {
            sender
                .send(Event::Request(Arc::new(record(
                    model,
                    Some(Usage {
                        prompt,
                        cached,
                        completion,
                        reasoning: Some(completion / 2),
                    }),
                ))))
                .unwrap();
        }
        // An engine that was never asked for usage, and a stream cut before
        // its last chunk: neither can be added up.
        let mut silent = record("model-epsilon", None);
        sender
            .send(Event::Request(Arc::new(silent.clone())))
            .unwrap();
        silent.complete = false;
        sender.send(Event::Request(Arc::new(silent))).unwrap();
        store.shutdown();
        dir
    }

    /// A window starting at `since`: the recorder stamps rows with its own
    /// clock as they reach it, so a test only ever knows where they landed to
    /// within the second it wrote them.
    fn at(dir: &Path, since: f64) -> Table {
        load(&dir.join(store::DB_FILE), since, since + 3600.0).unwrap()
    }

    #[test]
    fn a_day_adds_up_per_model_and_keeps_the_silent_requests_apart() {
        let dir = populated("day");
        let table = at(&dir, crate::logfmt::epoch() - 1.0);
        assert_eq!(table.rows.len(), 2, "{:?}", table.rows);
        assert_eq!(table.total.requests, 3, "only answers that reported counts");
        assert_eq!(table.total.prompt, 3_500);
        assert_eq!(table.total.cached, 2_700);
        assert_eq!(table.total.completion, 350);
        assert_eq!(table.blind, 2);
        assert_eq!(table.cut, 1);
        assert_eq!(table.unpriced, 0);

        // Most expensive first: 2.7k cached tokens of model-alpha cost less
        // than the priced_model turns they outnumber.
        assert_eq!(table.rows[0].model, "model-epsilon");
        assert_eq!(table.rows[0].requests, 2);
        assert_eq!(table.rows[0].reasoning, 150);
        assert_eq!(table.rows[1].model, "model-alpha");
        assert_eq!(table.rows[1].unreported, 1);

        let priced_model = &table.rows[0];
        // 300 uncached at 1.7, 2700 cached at 0.17, 300 out at 8.5.
        assert!(
            (priced_model.cost().unwrap() - (0.000_51 + 0.000_459 + 0.002_55)).abs() < 1e-9,
            "{:?}",
            priced_model.charge
        );
        // 500 tokens with no cache detail at all: the whole prompt at input.
        let other_model = &table.rows[1];
        assert!(
            (other_model.cost().unwrap() - 0.000_060).abs() < 1e-9,
            "{:?}",
            other_model.charge
        );
        assert!(
            (table.cost().unwrap() - (priced_model.cost().unwrap() + other_model.cost().unwrap()))
                .abs()
                < 1e-9,
            "{:?}",
            table.total.charge
        );
    }

    #[test]
    fn a_window_with_nothing_in_it_is_empty_rather_than_missing() {
        let dir = populated("empty");
        let yesterday = crate::logfmt::epoch() - 86_400.0;
        let table = at(&dir, yesterday);
        assert!(table.is_empty());
        assert_eq!(table.cost(), None, "no rows, no money");
        assert_eq!(table.blind, 0, "the blind count is the window's, too");

        // A file that is not there at all reads as an error, so the screen can
        // say so instead of drawing an empty day.
        let missing = load(&dir.join("nothing.sqlite3"), 0.0, 1e12);
        assert!(missing.is_err(), "{missing:?}");
    }
}
