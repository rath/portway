//! Token counts read back out of the upstream's own answer.
//!
//! vLLM and SGLang both report a `usage` object: in the JSON body of a
//! buffered answer, and — for a stream that asked with
//! `stream_options.include_usage` — in the last SSE chunk. Either way it
//! arrives in the decoded byte stream the relay already passes through, so the
//! counts are lifted from there: no body is buffered to find them, and the
//! request is not inspected to know which shape to expect.
//!
//! What is searched for is the key, not a parse of the whole response: a
//! stream has no whole response to parse, and a megabyte of generated text
//! does not need one. An escaped `\"usage\"` is inside a string, and an object
//! that does not read back as counts is dropped, so prose that happens to look
//! like usage cannot become a number in the record.
//!
//! The object is read as it streams past, keeping only what the counts can
//! be: its own fields and the `…_details` objects beside them. Anything
//! nested deeper is skipped except the cache-write lifetime breakdown; its
//! brackets are kept so what is held still parses.
//! That is what makes the size of the object irrelevant: the ChatGPT backend
//! attributes the turn to every item of the conversation inside its `usage`,
//! about 250 bytes an item, so a long Codex session reports counts in an
//! object of tens of kilobytes that any chunk boundary may cut.

use serde_json::{Map, Value};

/// What one answer cost, in the numbers every engine here agrees on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// `prompt_tokens`: the context this turn's prefill read, cache included.
    /// For Anthropic, `input_tokens` plus the cache read and cache write
    /// counts it reports beside it.
    pub prompt: u64,
    /// `prompt_tokens_details.cached_tokens`: the part a prefix cache served.
    /// `None` when the engine reported counts without that detail.
    pub cached: Option<u64>,
    /// Tokens written to cache; None means the provider did not report them.
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub cache_write_30m: Option<u64>,
    pub cache_write_12h: Option<u64>,

    /// `completion_tokens`: what the decode generated.
    pub completion: u64,
    /// `completion_tokens_details.reasoning_tokens` (the Responses API's
    /// `output_tokens_details`), or the top-level `reasoning_tokens`: the
    /// thinking inside `completion`, which on these models is most of it and
    /// is not part of the answer. `None` when the engine did not break it out.
    pub reasoning: Option<u64>,
}

impl Usage {
    /// Bad provider partitions must not charge tokens twice. Preserve the
    /// total input/output and leave the inconsistent cache counts unknown.
    fn normalized(mut self) -> Self {
        if self.cached.is_some_and(|n| n > self.prompt) {
            self.cached = None;
        }
        if self
            .cache_write
            .is_some_and(|n| n > self.prompt - self.cached.unwrap_or(0))
        {
            self.cache_write = None;
            self.cache_write_5m = None;
            self.cache_write_1h = None;
            self.cache_write_30m = None;
            self.cache_write_12h = None;
        }
        self
    }
}

/// The field the counts live under, quotes included.
const KEY: &[u8] = b"\"usage\"";

/// How deep inside the usage object a byte may sit and still be kept: its own
/// fields are at depth 1, the fields of a `…_details` object at depth 2.
const KEEP_DEPTH: usize = 2;

/// How much of one candidate may be kept. A real usage object's shallow part
/// is a few hundred bytes; one that outgrows this is content that merely
/// looks like a field, and holding it would be a leak in a process that
/// outlives agent sessions by design.
const HOLD: usize = 8 * 1024;

/// Finds the counts in a response as it is relayed. One per relay, fed every
/// decoded chunk in order.
#[derive(Default)]
pub struct Scanner {
    state: State,
    /// The last byte of the previous chunk: the escaped-key check for a key
    /// that starts a chunk.
    prev: Option<u8>,
    /// The latest complete counts, including partial cumulative updates.
    last: Option<Usage>,
    /// Anthropic streams report input and output counts in separate events.
    input_counts: InputCounts,
}

#[derive(Default)]
struct InputCounts {
    input: Option<u64>,
    cache_read: Option<u64>,
    anthropic: bool,
    writes: Writes,
    cached: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
}

/// Where the scan is. Every state survives a chunk boundary, so a key or an
/// object cut anywhere reads the same as one that arrived whole.
enum State {
    /// Looking for the key, this many of its bytes just seen.
    Search(usize),
    /// After the key: whitespace, then the colon.
    Colon,
    /// After the colon: whitespace, then the object.
    Open,
    /// Inside the object.
    Object(Candidate),
}

impl Default for State {
    fn default() -> Self {
        State::Search(0)
    }
}

/// A usage object being read, reduced to its shallow part.
struct Candidate {
    kept: Vec<u8>,
    depth: usize,
    in_string: bool,
    escaped: bool,
    input_details: bool,
    write_details: bool,
}

enum Step {
    More,
    Closed,
    /// Its shallow part outgrew `HOLD`.
    Abandoned,
}

impl Candidate {
    fn new() -> Self {
        Candidate {
            kept: vec![b'{'],
            depth: 1,
            in_string: false,
            escaped: false,
            input_details: false,
            write_details: false,
        }
    }

    fn ends_with_key(&self, key: &str) -> bool {
        let prefix = String::from_utf8_lossy(&self.kept);
        prefix
            .trim_end()
            .strip_suffix(':')
            .is_some_and(|prefix| prefix.trim_end().ends_with(&format!("\"{key}\"")))
    }

    fn take(&mut self, byte: u8) -> Step {
        // The depth the byte sits at: a bracket belongs to the container
        // around it, not to the one it opens or closes.
        let mut at = self.depth;
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
        } else {
            match byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => {
                    if self.depth == 1 {
                        self.input_details = self.ends_with_key("input_tokens_details")
                            || self.ends_with_key("prompt_tokens_details");
                    }
                    if self.depth == 2 {
                        self.write_details =
                            self.input_details && self.ends_with_key("cache_write_tokens_details");
                    }
                    self.depth += 1;
                }
                b'}' | b']' => {
                    self.depth -= 1;
                    at = self.depth;
                }
                _ => {}
            }
        }
        if at <= KEEP_DEPTH || (self.write_details && at == 3) {
            self.kept.push(byte);
        }
        if self.depth == 0 {
            Step::Closed
        } else if self.kept.len() > HOLD {
            Step::Abandoned
        } else {
            Step::More
        }
    }
}

impl Scanner {
    /// Feed decoded response bytes, in the order they are relayed.
    pub fn push(&mut self, data: &[u8]) {
        let mut i = 0;
        while i < data.len() {
            if matches!(self.state, State::Search(0)) {
                // Nothing is under way, and only a quote can start the key.
                match data[i..].iter().position(|&byte| byte == b'"') {
                    Some(skip) => i += skip,
                    None => break,
                }
            }
            let before = if i == 0 { self.prev } else { Some(data[i - 1]) };
            self.step(data[i], before);
            i += 1;
        }
        if let Some(&last) = data.last() {
            self.prev = Some(last);
        }
    }

    /// The latest counts, once both input and output have been reported.
    pub fn usage(&self) -> Option<Usage> {
        self.last
    }

    /// Advance by one byte, which `before` preceded.
    fn step(&mut self, byte: u8, before: Option<u8>) {
        match &mut self.state {
            State::Search(seen) => {
                // `\"usage\"` is generated content, not a field: an escaped
                // quote never opens the key.
                let opens = byte == b'"' && before != Some(b'\\');
                *seen = if *seen > 0 && byte == KEY[*seen] {
                    *seen + 1
                } else {
                    usize::from(opens)
                };
                if *seen == KEY.len() {
                    self.state = State::Colon;
                }
            }
            State::Colon | State::Open if byte.is_ascii_whitespace() => {}
            State::Colon if byte == b':' => self.state = State::Open,
            State::Open if byte == b'{' => self.state = State::Object(Candidate::new()),
            // `null`, a string, or another field: this byte may itself start
            // the next key.
            State::Colon | State::Open => {
                self.state = State::Search(0);
                self.step(byte, before);
            }
            State::Object(candidate) => match candidate.take(byte) {
                Step::More => {}
                Step::Closed => {
                    if let Some(usage) = read(&candidate.kept, &mut self.input_counts) {
                        self.last = Some(usage.normalized());
                    }
                    self.state = State::Search(0);
                }
                Step::Abandoned => self.state = State::Search(0),
            },
        }
    }
}

/// Read the counts out of one usage object. Anything else — another field's
/// object, an error body, content that looks like this one — is `None`.
fn read(object: &[u8], input: &mut InputCounts) -> Option<Usage> {
    let Value::Object(map) = serde_json::from_slice::<Value>(object).ok()? else {
        return None;
    };
    let count = |name: &str| map.get(name).and_then(Value::as_u64);
    if !map.contains_key("prompt_tokens") && !map.contains_key("completion_tokens") {
        if ![
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "cache_creation",
            "input_tokens_details",
            "cache_write_tokens",
        ]
        .iter()
        .any(|key| map.contains_key(*key))
        {
            return None;
        }
        // These are cumulative totals, not increments. Missing fields keep
        // their previous value; explicit zeroes replace it.
        input.input = count("input_tokens").or(input.input);
        input.output = count("output_tokens").or(input.output);
        input.cache_read = count("cache_read_input_tokens").or(input.cache_read);
        input.anthropic |= map.contains_key("cache_creation_input_tokens")
            || map.contains_key("cache_read_input_tokens")
            || map.contains_key("cache_creation");
        input.writes.update(&map);
        input.cached = nested(&map, "input_tokens_details", "cached_tokens")
            .or_else(|| count("cached_tokens"))
            .or(input.cached);
        input.reasoning = nested(&map, "output_tokens_details", "reasoning_tokens")
            .or_else(|| count("reasoning_tokens"))
            .or(input.reasoning);
        return Some(Usage {
            // Anthropic reports cache reads and writes beside input_tokens;
            // Responses includes its cache in input_tokens already.
            prompt: input
                .input?
                .checked_add(input.cache_read.unwrap_or(0))?
                .checked_add(if input.anthropic {
                    input.writes.usage().cache_write.unwrap_or(0)
                } else {
                    0
                })?,
            cached: input.cached.or(input.cache_read),
            completion: input.output?,
            reasoning: input.reasoning,
            ..input.writes.usage()
        });
    }
    // Chat Completions supplies complete objects. Do not merge its fields
    // with an input/output schema from another candidate.
    *input = InputCounts::default();
    let prompt = count("prompt_tokens")?;
    let completion = count("completion_tokens")?;
    let mut writes = Writes::default();
    writes.update(&map);
    Some(Usage {
        prompt,
        cached: nested(&map, "prompt_tokens_details", "cached_tokens")
            .or_else(|| nested(&map, "input_tokens_details", "cached_tokens"))
            .or_else(|| count("cached_tokens"))
            .or_else(|| count("cache_read_input_tokens")),
        completion,
        // vLLM breaks thinking out under `completion_tokens_details`, the
        // Responses API under `output_tokens_details`; SGLang reports it
        // beside the totals.
        reasoning: nested(&map, "completion_tokens_details", "reasoning_tokens")
            .or_else(|| nested(&map, "output_tokens_details", "reasoning_tokens"))
            .or_else(|| count("reasoning_tokens")),
        ..writes.usage()
    })
}

/// Cumulative provider counts. Duration fields are disjoint parts of total.
#[derive(Default)]
struct Writes {
    total: Option<u64>,
    periods: [Option<u64>; 4],
}
impl Writes {
    fn update(&mut self, map: &Map<String, Value>) {
        let details = map
            .get("input_tokens_details")
            .or_else(|| map.get("prompt_tokens_details"));
        let total = map
            .get("cache_creation_input_tokens")
            .or_else(|| details.and_then(|v| v.get("cache_write_tokens")))
            .or_else(|| map.get("cache_write_tokens"))
            .and_then(Value::as_u64);
        let creation = map.get("cache_creation");
        let ttl = details.and_then(|v| v.get("cache_write_tokens_details"));
        let periods = [
            creation.and_then(|v| v.get("ephemeral_5m_input_tokens")),
            creation.and_then(|v| v.get("ephemeral_1h_input_tokens")),
            ttl.and_then(|v| v.get("ttl_30m_tokens")),
            ttl.and_then(|v| v.get("ttl_12h_tokens")),
        ]
        .map(|v| v.and_then(Value::as_u64));
        self.total = total.or(self.total);
        for (old, new) in self.periods.iter_mut().zip(periods) {
            *old = new.or(*old);
        }
    }

    fn usage(&self) -> Usage {
        let mut periods = self.periods;
        let sum = periods
            .iter()
            .flatten()
            .try_fold(0u64, |a, b| a.checked_add(*b));
        let complete = (periods[0].is_some() && periods[1].is_some())
            || (periods[2].is_some() && periods[3].is_some());
        let total = self.total.or_else(|| complete.then_some(sum).flatten());
        if total.is_none()
            || sum.is_none()
            || total.is_some_and(|t| sum.is_some_and(|s| s > t || (complete && s != t)))
        {
            periods = [None; 4];
        }
        Usage {
            cache_write: total,
            cache_write_5m: periods[0],
            cache_write_1h: periods[1],
            cache_write_30m: periods[2],
            cache_write_12h: periods[3],
            ..Usage::default()
        }
    }
}

/// One number inside a `…_details` object.
fn nested(map: &Map<String, Value>, name: &str, field: &str) -> Option<u64> {
    map.get(name)?.get(field)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Option<Usage> {
        let mut scanner = Scanner::default();
        for chunk in chunks {
            scanner.push(chunk);
        }
        scanner.usage()
    }

    /// The shape a buffered answer carries, and the one the edge's own gzip
    /// turns into arbitrary chunk boundaries.
    const ANSWER: &[u8] = br#"{"id":"c1","choices":[{"index":0,"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":18234,"prompt_tokens_details":{"cached_tokens":18200},"completion_tokens":891,"completion_tokens_details":{"reasoning_tokens":742},"total_tokens":19125}}"#;

    #[test]
    fn a_buffered_answer_reports_its_counts() {
        assert_eq!(
            scan_all(&[ANSWER]),
            Some(Usage {
                prompt: 18234,
                cached: Some(18200),
                completion: 891,
                reasoning: Some(742),
                cache_write: None,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: None,
                cache_write_12h: None,
            })
        );
    }

    /// Nothing about a chunk boundary is known in advance, so a feed that cuts
    /// every byte is the honest test: SSE chunks arrive on their own framing,
    /// not the codec's.
    #[test]
    fn a_stream_split_anywhere_still_reports_them() {
        for cut in 1..ANSWER.len() {
            let (head, tail) = ANSWER.split_at(cut);
            assert_eq!(
                scan_all(&[head, tail]),
                Some(Usage {
                    prompt: 18234,
                    cached: Some(18200),
                    completion: 891,
                    reasoning: Some(742),
                    cache_write: None,
                    cache_write_5m: None,
                    cache_write_1h: None,
                    cache_write_30m: None,
                    cache_write_12h: None,
                }),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_streamed_answer_reports_the_last_usage_it_carried() {
        let stream = [
            &b"data: {\"choices\":[{\"delta\":{\"content\":\"tok\"}}]}\n\n"[..],
            // SGLang-style `null` on the way through.
            &b"data: {\"choices\":[],\"usage\":null}\n\n"[..],
            &b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"reasoning_tokens\":1}}\n\n"
                [..],
            &b"data: [DONE]\n\n"[..],
        ];
        assert_eq!(
            scan_all(&stream),
            Some(Usage {
                prompt: 10,
                cached: None,
                completion: 2,
                reasoning: Some(1),
                cache_write: None,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: None,
                cache_write_12h: None,
            })
        );
    }

    #[test]
    fn a_later_usage_replaces_an_earlier_one() {
        let first = br#"{"usage":{"prompt_tokens":10,"completion_tokens":2}}"#;
        let second = br#"{"usage":{"prompt_tokens":10,"completion_tokens":40}}"#;
        assert_eq!(
            scan_all(&[first, second]).map(|usage| usage.completion),
            Some(40)
        );
    }

    #[test]
    fn content_that_merely_looks_like_usage_is_not_counted() {
        // The word escaped inside generated content, and a field-like object
        // in that content: neither is a usage object.
        let escaped = br#"{"choices":[{"message":{"content":"a \"usage\" field"}}]}"#;
        let object_in_text = br#"{"choices":[{"message":{"content":"{\"usage\":{\"prompt_tokens\":999,\"completion_tokens\":1}}"}}]}"#;
        let other_field = br#"{"echo":{"usage":{"prompt_tokens":1,"completion_tokens":1}}}"#;
        let unterminated = br#"{"usage":{"prompt_tokens":1,"completion_tokens":1"#;

        assert_eq!(scan_all(&[escaped]), None);
        assert_eq!(scan_all(&[object_in_text]), None);
        assert_eq!(scan_all(&[unterminated]), None);
        // A nested object that is not the usage field still reads as counts —
        // it is the key's own object, and every engine here means that key.
        assert_eq!(scan_all(&[other_field]).map(|usage| usage.prompt), Some(1));
    }

    #[test]
    fn a_usage_object_that_never_closes_is_abandoned() {
        let mut stream = b"data: {\"usage\":{\"prompt_tokens\":1,".to_vec();
        stream.resize(HOLD + 4096, b'x');
        assert_eq!(scan_all(&[&stream]), None);

        // The next real one is still read: the abandoned candidate did not
        // wedge the scanner.
        let mut scanner = Scanner::default();
        scanner.push(&stream);
        scanner.push(br#"{"usage":{"prompt_tokens":7,"completion_tokens":3}}"#);
        assert_eq!(scanner.usage().map(|usage| usage.prompt), Some(7));
    }

    /// Anthropic reports the cached and the newly cached prefix beside
    /// `input_tokens`; the prompt is all three, so `cached` never exceeds it.
    #[test]
    fn an_anthropic_prompt_includes_what_its_cache_served_and_wrote() {
        let answer = br#"{"type":"message","usage":{"input_tokens":12,"cache_creation_input_tokens":800,"cache_read_input_tokens":17000,"output_tokens":431}}"#;
        assert_eq!(
            scan_all(&[answer]),
            Some(Usage {
                prompt: 17_812,
                cached: Some(17_000),
                completion: 431,
                reasoning: None,
                cache_write: Some(800),
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: None,
                cache_write_12h: None,
            })
        );

        // A stream: `message_start` opens with a placeholder output count, and
        // the closing `message_delta` carries the cumulative totals.
        let stream = [
            &br#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":12,"cache_creation_input_tokens":800,"cache_read_input_tokens":17000,"output_tokens":1}}}

"#[..],
            &br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":431}}

"#[..],
        ];
        assert_eq!(
            scan_all(&stream).map(|usage| (usage.prompt, usage.cached, usage.completion)),
            Some((17_812, Some(17_000), 431))
        );
        let joined = stream.concat();
        for size in [1, 7, 31, joined.len()] {
            assert_eq!(
                scan_all(&joined.chunks(size).collect::<Vec<_>>()),
                scan_all(&stream)
            );
        }
    }

    #[test]
    fn partial_counts_preserve_input_and_replace_cumulative_output() {
        let mut scanner = Scanner::default();
        scanner.push(br#"{"usage":{"output_tokens":2}}"#);
        assert_eq!(scanner.usage(), None);
        scanner.push(br#"{"usage":{"input_tokens":10,"cache_read_input_tokens":20}}"#);
        assert_eq!(scanner.usage().unwrap().prompt, 30);
        scanner.push(br#"{"usage":{"output_tokens":40}}"#);
        assert_eq!(scanner.usage().unwrap().completion, 40);
        scanner.push(
            br#"{"usage":{"input_tokens":0,"cache_creation_input_tokens":5,"output_tokens":0}}"#,
        );
        assert_eq!(
            scanner.usage(),
            Some(Usage {
                prompt: 25,
                cached: Some(20),
                completion: 0,
                reasoning: None,
                cache_write: Some(5),
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: None,
                cache_write_12h: None,
            })
        );
    }

    /// The Responses API's `input_tokens` already counts its cache.
    #[test]
    fn a_responses_prompt_is_not_counted_twice() {
        let answer = br#"{"usage":{"input_tokens":5000,"input_tokens_details":{"cached_tokens":4096},"output_tokens":20}}"#;
        assert_eq!(
            scan_all(&[answer]).map(|usage| (usage.prompt, usage.cached)),
            Some((5000, Some(4096)))
        );
    }

    /// The ChatGPT backend's `usage` attributes the turn to every item of the
    /// conversation, so a long Codex session reports its counts in an object
    /// far larger than `HOLD`. Only its shallow part is kept, so its size does
    /// not matter, nor where a chunk boundary cuts it.
    #[test]
    fn a_usage_object_of_any_size_is_read_wherever_it_is_cut() {
        let items = (0..2_000)
            .map(|item| {
                format!(
                    r#""at_{item:04}":{{"cache_write_tokens":0,"cached_tokens":96,"input_tokens":96,"output_tokens":0}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let event = format!(
            "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"tools\":[],\"usage\":{{\"attribution\":{{\"items\":{{{items}}}}},\"input_tokens\":204339,\"input_tokens_details\":{{\"cache_write_tokens\":0,\"cached_tokens\":203392}},\"output_tokens\":737,\"output_tokens_details\":{{\"reasoning_tokens\":516}},\"total_tokens\":205076}},\"user\":null}}}}\n\n"
        );
        assert!(event.len() > 20 * HOLD, "{}", event.len());

        for size in [1, 7, 1_000, 4_096, 16_384, 65_536, event.len()] {
            let chunks = event.as_bytes().chunks(size).collect::<Vec<_>>();
            assert_eq!(
                scan_all(&chunks),
                Some(Usage {
                    prompt: 204_339,
                    cached: Some(203_392),
                    completion: 737,
                    reasoning: Some(516),
                    cache_write: Some(0),
                    cache_write_5m: None,
                    cache_write_1h: None,
                    cache_write_30m: None,
                    cache_write_12h: None,
                }),
                "chunks of {size}"
            );
        }
    }

    #[test]
    fn counts_without_both_numbers_are_not_a_usage_object() {
        assert_eq!(scan_all(&[br#"{"usage":{"total_tokens":12}}"#]), None);
        assert_eq!(
            scan_all(&[br#"{"usage":{"prompt_tokens":12,"completion_tokens":0}}"#]),
            Some(Usage {
                prompt: 12,
                reasoning: None,
                cached: None,
                completion: 0,
                cache_write: None,
                cache_write_5m: None,
                cache_write_1h: None,
                cache_write_30m: None,
                cache_write_12h: None,
            })
        );
    }

    #[test]
    fn lifetime_writes_survive_chunk_boundaries_and_cumulative_updates() {
        let cases: &[(&[u8], Usage)] = &[
            (br#"{"usage":{"input_tokens":12,"cache_read_input_tokens":17000,"cache_creation_input_tokens":800,"cache_creation":{"ephemeral_5m_input_tokens":500,"ephemeral_1h_input_tokens":300},"output_tokens":431}}"#,
                Usage { prompt: 17812, cached: Some(17000), completion: 431, cache_write: Some(800), cache_write_5m: Some(500), cache_write_1h: Some(300), ..Usage::default() }),
            (br#"{"usage":{"input_tokens":12000,"input_tokens_details":{"cached_tokens":4000,"cache_write_tokens":7000,"cache_write_tokens_details":{"ttl_30m_tokens":3000,"ttl_12h_tokens":4000}},"output_tokens":20}}"#,
                Usage { prompt: 12000, cached: Some(4000), completion: 20, cache_write: Some(7000), cache_write_30m: Some(3000), cache_write_12h: Some(4000), ..Usage::default() }),
            (br#"{"usage":{"prompt_tokens":12000,"prompt_tokens_details":{"cached_tokens":4000,"cache_write_tokens":7000},"completion_tokens":20}}"#,
                Usage { prompt: 12000, cached: Some(4000), completion: 20, cache_write: Some(7000), ..Usage::default() }),
        ];
        for (bytes, expected) in cases {
            for cut in 0..bytes.len() {
                assert_eq!(
                    scan_all(&[&bytes[..cut], &bytes[cut..]]),
                    Some(*expected),
                    "cut {cut}"
                );
            }
        }
        let mut scanner = Scanner::default();
        scanner.push(cases[0].0);
        scanner.push(br#"data: {"usage":{"output_tokens":500}}"#);
        assert_eq!(
            scanner.usage(),
            Some(Usage {
                completion: 500,
                ..cases[0].1
            })
        );
        scanner.push(br#"data: {"usage":{"cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":0}}"#);
        let usage = scanner.usage().unwrap();
        assert_eq!(
            (
                usage.prompt,
                usage.cache_write,
                usage.cache_write_5m,
                usage.cache_write_1h,
                usage.completion
            ),
            (17012, Some(0), Some(0), Some(0), 0)
        );
    }

    #[test]
    fn a_details_only_update_preserves_cumulative_counts() {
        let mut scanner = Scanner::default();
        scanner.push(br#"{"usage":{"input_tokens":100,"output_tokens":10}}"#);
        scanner.push(br#"{"usage":{"input_tokens_details":{"cache_write_tokens":40,"cache_write_tokens_details":{"ttl_30m_tokens":30,"ttl_12h_tokens":10}}}}"#);
        let usage = scanner.usage().unwrap();
        assert_eq!(usage.prompt, 100);
        assert_eq!(usage.completion, 10);
        assert_eq!(usage.cache_write, Some(40));
        assert_eq!(usage.cache_write_30m, Some(30));
        assert_eq!(usage.cache_write_12h, Some(10));
    }

    #[test]
    fn invalid_partitions_are_unknown_instead_of_double_billed() {
        let scan = |s: &str| scan_all(&[s.as_bytes()]).unwrap();
        let inconsistent = scan(
            r#"{"usage":{"input_tokens":100,"input_tokens_details":{"cache_write_tokens":80,"cache_write_tokens_details":{"ttl_30m_tokens":50,"ttl_12h_tokens":50}},"output_tokens":1}}"#,
        );
        assert_eq!(inconsistent.cache_write, Some(80));
        assert_eq!(
            (inconsistent.cache_write_30m, inconsistent.cache_write_12h),
            (None, None)
        );
        let too_many = scan(
            r#"{"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":50,"cache_write_tokens":80},"output_tokens":1}}"#,
        );
        assert_eq!(too_many.cache_write, None);
        let incomplete = scan(
            r#"{"usage":{"input_tokens":100,"input_tokens_details":{"cache_write_tokens_details":{"ttl_30m_tokens":50}},"output_tokens":1}}"#,
        );
        assert_eq!(
            (incomplete.cache_write, incomplete.cache_write_30m),
            (None, None)
        );
        let derived = scan(
            r#"{"usage":{"input_tokens":10,"cache_creation":{"ephemeral_5m_input_tokens":50,"ephemeral_1h_input_tokens":0},"output_tokens":1}}"#,
        );
        assert_eq!((derived.prompt, derived.cache_write), (60, Some(50)));
        let overflow = scan(
            r#"{"usage":{"input_tokens":100,"input_tokens_details":{"cache_write_tokens":80,"cache_write_tokens_details":{"ttl_30m_tokens":18446744073709551615,"ttl_12h_tokens":1}},"output_tokens":1}}"#,
        );
        assert_eq!(
            (overflow.cache_write_30m, overflow.cache_write_12h),
            (None, None)
        );
    }

    #[test]
    fn deep_attribution_does_not_hide_the_lifetime_breakdown() {
        let attribution = (0..2000)
            .map(|i| format!(r#""item{i}":{{"tokens":123}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let answer = format!(
            r#"{{"usage":{{"attribution":{{"items":{{{attribution}}}}},"input_tokens":100,"input_tokens_details":{{"cache_write_tokens":80,"cache_write_tokens_details":{{"ttl_30m_tokens":30,"ttl_12h_tokens":50}}}},"output_tokens":1}}}}"#
        );
        for size in [1, 7, 4096, answer.len()] {
            let usage = scan_all(&answer.as_bytes().chunks(size).collect::<Vec<_>>()).unwrap();
            assert_eq!(
                (usage.cache_write_30m, usage.cache_write_12h),
                (Some(30), Some(50))
            );
        }
    }
}
