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

use serde_json::{Map, Value};

/// What one answer cost, in the numbers every engine here agrees on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// `prompt_tokens`: the context this turn's prefill read, cache included.
    pub prompt: u64,
    /// `prompt_tokens_details.cached_tokens`: the part a prefix cache served.
    /// `None` when the engine reported counts without that detail.
    pub cached: Option<u64>,
    /// `completion_tokens`: what the decode generated.
    pub completion: u64,
    /// `completion_tokens_details.reasoning_tokens`, or the top-level
    /// `reasoning_tokens`: the thinking inside `completion`, which on these
    /// models is most of it and is not part of the answer. `None` when the
    /// engine did not break it out.
    pub reasoning: Option<u64>,
}

/// The field the counts live under, quotes included.
const KEY: &[u8] = b"\"usage\"";

/// How long a candidate the chunk boundary cut in half may be held. A real
/// usage object is a few hundred bytes; a stream that opens one and never
/// closes it is content that merely looks like a field, and holding it would
/// be a leak in a process that outlives agent sessions by design.
const HOLD: usize = 8 * 1024;

/// Finds the counts in a response as it is relayed. One per relay, fed every
/// decoded chunk in order.
#[derive(Default)]
pub struct Scanner {
    /// Bytes held back for the next chunk: the tail of a key, or a key whose
    /// object has not closed yet. Empty whenever nothing is undecided.
    held: Vec<u8>,
    /// The byte immediately before `held`, for the escaped-key check.
    before: Option<u8>,
    /// The last byte of the previous chunk: the same check for a key that
    /// starts a chunk.
    prev: Option<u8>,
    /// The last complete usage object seen. A stream may carry more than one —
    /// an engine that reports the running total per chunk — and the last one is
    /// the answer's.
    last: Option<Usage>,
}

impl Scanner {
    /// Feed decoded response bytes, in the order they are relayed.
    pub fn push(&mut self, data: &[u8]) {
        if self.held.is_empty() {
            // The common path: nothing is undecided, so the chunk is scanned
            // where it is and only an unresolved tail is copied out of it.
            if let Some(from) = scan(&mut self.last, data, self.prev) {
                self.before = if from == 0 {
                    self.prev
                } else {
                    Some(data[from - 1])
                };
                self.held.extend_from_slice(&data[from..]);
            }
        } else {
            let mut buf = std::mem::take(&mut self.held);
            buf.extend_from_slice(data);
            match scan(&mut self.last, &buf, self.before) {
                None => buf.clear(),
                Some(from) => {
                    self.before = if from == 0 {
                        self.before
                    } else {
                        Some(buf[from - 1])
                    };
                    buf.drain(..from);
                }
            }
            // The buffer is kept: its capacity is the most a candidate ever
            // held, and a stream that split one key usually splits another.
            self.held = buf;
        }
        if let Some(last) = data.last() {
            self.prev = Some(*last);
        }
    }

    /// The counts of the last complete usage object, if one arrived.
    pub fn usage(&self) -> Option<Usage> {
        self.last
    }
}

/// Scan one run of bytes whose first byte was preceded by `before`, recording
/// every usage object it completes. `Some(i)` means the run is undecided from
/// `i` on, and the caller holds those bytes for the next chunk.
fn scan(last: &mut Option<Usage>, data: &[u8], before: Option<u8>) -> Option<usize> {
    let mut from = 0;
    loop {
        let Some(at) = find(&data[from..]).map(|offset| from + offset) else {
            // The key may still be half-typed at the end of this run.
            let keep = (KEY.len() - 1).min(data.len());
            return (keep > 0).then_some(data.len() - keep);
        };
        let preceded = if at == 0 { before } else { Some(data[at - 1]) };
        if preceded == Some(b'\\') {
            // `\"usage\"`: generated content, not a field.
            from = at + KEY.len();
            continue;
        }
        match object(data, at + KEY.len()) {
            Extent::Object(start, end) => {
                if let Some(usage) = read(&data[start..end]) {
                    *last = Some(usage);
                }
                from = end;
            }
            Extent::No => from = at + KEY.len(),
            // Undecided — unless the candidate has run past what a usage
            // object can be, which makes it content that looked like one.
            Extent::Partial if data.len() - at <= HOLD => return Some(at),
            Extent::Partial => from = at + KEY.len(),
        }
    }
}

/// Seven bytes against one decoded chunk, and only a stream is scanned: the
/// naive scan is the right one.
fn find(haystack: &[u8]) -> Option<usize> {
    haystack.windows(KEY.len()).position(|window| window == KEY)
}

/// Where the value behind a key ends, when it is an object.
enum Extent {
    /// A `{` at the first index, its matching `}` at the second.
    Object(usize, usize),
    /// Something else: `null`, a string, or another field's key.
    No,
    /// The run ends inside what might be one.
    Partial,
}

fn object(data: &[u8], from: usize) -> Extent {
    let mut i = skip_space(data, from);
    if i == data.len() {
        return Extent::Partial;
    }
    if data[i] != b':' {
        return Extent::No;
    }
    i = skip_space(data, i + 1);
    if i == data.len() {
        return Extent::Partial;
    }
    if data[i] != b'{' {
        return Extent::No;
    }

    let start = i;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < data.len() {
        let byte = data[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if matches!(byte, b'{' | b'[') {
            depth += 1;
        } else if matches!(byte, b'}' | b']') {
            depth -= 1;
            if depth == 0 {
                return Extent::Object(start, i + 1);
            }
        }
        i += 1;
    }
    Extent::Partial
}

fn skip_space(data: &[u8], mut i: usize) -> usize {
    while i < data.len() && data[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Read the counts out of one usage object. Anything else — another field's
/// object, an error body, content that looks like this one — is `None`.
fn read(object: &[u8]) -> Option<Usage> {
    let Value::Object(map) = serde_json::from_slice::<Value>(object).ok()? else {
        return None;
    };
    let count = |name: &str| map.get(name).and_then(Value::as_u64);
    // Both counts are required: a body with one of them is not a usage object
    // this process can account for, and half a turn is worse than none.
    let prompt = count("prompt_tokens").or_else(|| count("input_tokens"))?;
    let completion = count("completion_tokens").or_else(|| count("output_tokens"))?;
    Some(Usage {
        prompt,
        cached: nested(&map, "prompt_tokens_details", "cached_tokens")
            .or_else(|| nested(&map, "input_tokens_details", "cached_tokens"))
            .or_else(|| count("cached_tokens"))
            .or_else(|| count("cache_read_input_tokens")),
        completion,
        // vLLM breaks thinking out under `completion_tokens_details`; SGLang
        // reports it beside the totals.
        reasoning: nested(&map, "completion_tokens_details", "reasoning_tokens")
            .or_else(|| count("reasoning_tokens")),
    })
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
            })
        );
    }
}
