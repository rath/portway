//! Request compression learned from origin responses, independently of Portway discovery.
use crate::{
    dict::{self, DictionaryScope},
    forwarder::Coding,
};
use http::{HeaderMap, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const CAPACITY: usize = 1024;
const TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginCompressionMode {
    #[default]
    Off,
    Auto,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OriginCompressionConfig {
    pub mode: OriginCompressionMode,
}

#[derive(Default)]
struct Counters {
    attempts: AtomicU64,
    encoded_attempts: AtomicU64,
    wire_bytes: AtomicU64,
    retried_identity: AtomicU64,
    refusals: AtomicU64,
}

/// Counts body bytes handed to HTTP, including refused attempts and cancelled requests.
/// This is payload accounting, not a count of TCP/TLS framing or retransmissions.
pub(crate) struct UploadMeter {
    total: Arc<Counters>,
    bytes: AtomicU64,
}
impl UploadMeter {
    pub fn attempt(&self, headers: &HeaderMap) {
        self.total.attempts.fetch_add(1, Ordering::Relaxed);
        if headers.contains_key(http::header::CONTENT_ENCODING) {
            self.total.encoded_attempts.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub fn add(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.total
            .wire_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn retry(&self) {
        self.total.retried_identity.fetch_add(1, Ordering::Relaxed);
    }
}

struct Entry {
    key: dict::Hash,
    generation: u64,
    coding: Option<Coding>,
    expires: Instant,
    refused: bool,
    probing: bool,
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    generation: u64,
}
#[derive(Default)]
pub(crate) struct OriginState {
    cache: Mutex<Cache>,
    counters: Arc<Counters>,
}

/// A trial owns its reservation until the response arrives or the request is cancelled.
pub(crate) struct Attempt {
    state: Arc<OriginState>,
    key: dict::Hash,
    generation: u64,
    trial: bool,
    pub coding: Coding,
}

impl OriginState {
    pub fn meter(&self) -> Arc<UploadMeter> {
        Arc::new(UploadMeter {
            total: self.counters.clone(),
            bytes: AtomicU64::new(0),
        })
    }
    pub fn begin(
        self: &Arc<Self>,
        method: &Method,
        target: &str,
        headers: &HeaderMap,
        scope: &DictionaryScope,
        eligible: bool,
    ) -> Attempt {
        // Hash request context: never retain URLs, API keys or cookies in the cache.
        let mut context = Vec::new();
        for value in [method.as_str().as_bytes(), target.as_bytes(), &scope.0] {
            context.extend_from_slice(&(value.len() as u64).to_le_bytes());
            context.extend_from_slice(value);
        }
        for name in [
            "x-api-key",
            "content-type",
            "anthropic-version",
            "anthropic-beta",
        ] {
            for value in headers.get_all(name) {
                context.extend_from_slice(&(value.as_bytes().len() as u64).to_le_bytes());
                context.extend_from_slice(value.as_bytes());
            }
            context.extend_from_slice(&u64::MAX.to_le_bytes());
        }
        self.begin_key(dict::sha256(&context), eligible, Instant::now())
    }
    fn begin_key(self: &Arc<Self>, key: dict::Hash, eligible: bool, now: Instant) -> Attempt {
        let mut cache = self.cache.lock().expect("origin cache");
        let mut entry = if let Some(at) = cache.entries.iter().position(|e| e.key == key) {
            cache.entries.remove(at).expect("entry exists")
        } else {
            if cache.entries.len() == CAPACITY {
                // Do not evict a trial and allow a second concurrent trial for its key.
                if let Some(at) = cache.entries.iter().position(|e| !e.probing) {
                    cache.entries.remove(at);
                } else {
                    return Attempt {
                        state: self.clone(),
                        key,
                        generation: 0,
                        trial: false,
                        coding: Coding::None,
                    };
                }
            }
            cache.generation += 1;
            Entry {
                key,
                generation: cache.generation,
                coding: None,
                expires: now + TTL,
                refused: false,
                probing: false,
            }
        };
        if now >= entry.expires && !entry.probing {
            cache.generation += 1;
            entry.generation = cache.generation;
            entry.coding = None;
            entry.refused = false;
            entry.expires = now + TTL;
        }
        let trial = eligible && entry.coding.is_none() && !entry.probing;
        let coding = if !eligible || entry.probing {
            Coding::None
        } else {
            entry.coding.unwrap_or(Coding::Gzip)
        };
        if trial {
            cache.generation += 1;
            entry.generation = cache.generation;
            entry.probing = true;
        }
        let attempt = Attempt {
            state: self.clone(),
            key,
            generation: entry.generation,
            trial,
            coding,
        };
        cache.entries.push_back(entry);
        attempt
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let cache = self.cache.lock().expect("origin cache");
        let now = Instant::now();
        let c = &self.counters;
        serde_json::json!({
            "mode": "auto", "cache_entries": cache.entries.len(),
            "gzip_entries": cache.entries.iter().filter(|e| e.coding == Some(Coding::Gzip) && now < e.expires).count(),
            "zstd_entries": cache.entries.iter().filter(|e| e.coding == Some(Coding::Zstd) && now < e.expires).count(),
            "backoff_entries": cache.entries.iter().filter(|e| e.refused && now < e.expires).count(),
            "probing_entries": cache.entries.iter().filter(|e| e.probing).count(),
            "attempts": c.attempts.load(Ordering::Relaxed),
            "encoded_attempts": c.encoded_attempts.load(Ordering::Relaxed),
            "wire_bytes": c.wire_bytes.load(Ordering::Relaxed),
            "retried_identity": c.retried_identity.load(Ordering::Relaxed),
            "refusals": c.refusals.load(Ordering::Relaxed),
        })
    }
}
impl Attempt {
    /// Refusal takes precedence over advertisements, including concurrent stale successes.
    pub fn observe(&self, status: StatusCode, headers: &HeaderMap, sent: Coding) {
        self.observe_at(status, headers, sent, Instant::now());
    }
    fn observe_at(&self, status: StatusCode, headers: &HeaderMap, sent: Coding, now: Instant) {
        let mut cache = self.state.cache.lock().expect("origin cache");
        let Some(at) = cache
            .entries
            .iter()
            .position(|e| e.key == self.key && e.generation == self.generation)
        else {
            return;
        };
        if sent != Coding::None && matches!(status.as_u16(), 400 | 415) {
            cache.generation += 1;
            let generation = cache.generation;
            let entry = &mut cache.entries[at];
            entry.generation = generation;
            entry.coding = Some(Coding::None);
            entry.expires = now + TTL;
            entry.refused = true;
            entry.probing = false;
            self.state.counters.refusals.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let entry = &mut cache.entries[at];
        if entry.refused || (entry.probing && !self.trial) {
            return;
        }
        if self.trial {
            entry.probing = false;
        }
        if let Some(advertisement) = advertisement(headers) {
            entry.coding = Some(advertisement.coding);
            entry.expires = now + TTL;
        } else if sent != Coding::None && status.is_success() {
            entry.coding = Some(sent);
            entry.expires = now + TTL;
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.trial {
            let mut cache = self.state.cache.lock().expect("origin cache");
            if let Some(entry) = cache
                .entries
                .iter_mut()
                .find(|e| e.key == self.key && e.generation == self.generation)
            {
                entry.probing = false;
            }
        }
    }
}

struct Advertisement {
    coding: Coding,
    identity: bool,
}

pub(crate) fn identity_allowed(headers: &HeaderMap) -> bool {
    advertisement(headers).is_none_or(|a| a.identity)
}

fn quality(raw: &str) -> Option<u16> {
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if !matches!(whole, "0" | "1")
        || fraction.len() > 3
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u16>().ok()? * 10u16.pow(3 - fraction.len() as u32)
    };
    match whole {
        "1" if fractional == 0 => Some(1000),
        "0" => Some(fractional),
        _ => None,
    }
}

fn advertisement(headers: &HeaderMap) -> Option<Advertisement> {
    if !headers.contains_key(http::header::ACCEPT_ENCODING) {
        return None;
    }
    let (mut gzip, mut zstd, mut identity, mut wildcard) = (None, None, None, None);
    for value in headers.get_all(http::header::ACCEPT_ENCODING) {
        for item in value
            .to_str()
            .ok()?
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let mut fields = item.split(';');
            let name = fields.next()?.trim();
            let mut q = 1000;
            if let Some(parameter) = fields.next() {
                let (name, value) = parameter.trim().split_once('=')?;
                if !name.trim().eq_ignore_ascii_case("q") {
                    return None;
                }
                q = quality(value.trim())?;
            }
            if fields.next().is_some() {
                return None;
            }
            let slot = match name.to_ascii_lowercase().as_str() {
                "gzip" | "x-gzip" => &mut gzip,
                "zstd" => &mut zstd,
                "identity" => &mut identity,
                "*" => &mut wildcard,
                _ => continue,
            };
            // Conflicting duplicates: respect the most restrictive value.
            *slot = Some(slot.unwrap_or(q).min(q));
        }
    }
    let gz = gzip.or(wildcard).unwrap_or(0);
    // Only use zstd when explicitly advertised, never as a speculative wildcard choice.
    let zs = zstd.unwrap_or(0);
    let coding = if identity.is_some_and(|q| q > gz.max(zs)) || gz.max(zs) == 0 {
        Coding::None
    } else if zs > gz {
        Coding::Zstd
    } else {
        Coding::Gzip
    };
    Some(Advertisement {
        coding,
        identity: identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 }) > 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::ACCEPT_ENCODING, value.parse().unwrap());
        headers
    }
    #[test]
    fn advertisements_obey_weights_exclusions_and_empty_values() {
        for (value, expected) in [
            ("gzip", Coding::Gzip),
            ("gzip;q=0, zstd", Coding::Zstd),
            ("gzip;q=0.5, zstd;q=1", Coding::Zstd),
            ("gzip;q=0.5, identity;q=1", Coding::None),
            ("", Coding::None),
            ("*;q=1,gzip;q=0", Coding::None),
            ("gzip;q=1,gzip;q=0", Coding::None),
            ("GZip;Q=1", Coding::Gzip),
        ] {
            assert_eq!(
                advertisement(&headers(value)).unwrap().coding,
                expected,
                "{value}"
            );
        }
        assert!(!identity_allowed(&headers("gzip, identity;q=0")));
        assert!(!identity_allowed(&headers("*;q=0,gzip")));
        assert!(identity_allowed(&headers("*;q=0,identity;q=0.1")));
        for value in ["gzip;q=NaN", "gzip;q=1.1", "gzip;q=-1", "gzip;q=0.1234"] {
            assert!(advertisement(&headers(value)).is_none());
        }
    }
    #[test]
    fn refusal_survives_concurrent_success_then_only_one_request_retries_after_expiry() {
        let state = Arc::new(OriginState::default());
        let now = Instant::now();
        let first = state.begin_key([1; 32], true, now);
        assert_eq!(first.coding, Coding::Gzip);
        assert_eq!(state.begin_key([1; 32], true, now).coding, Coding::None);
        first.observe_at(StatusCode::OK, &HeaderMap::new(), Coding::Gzip, now);
        let stale = state.begin_key([1; 32], true, now);
        let refusal = state.begin_key([1; 32], true, now);
        refusal.observe_at(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            &headers("gzip"),
            Coding::Gzip,
            now,
        );
        stale.observe_at(StatusCode::OK, &headers("gzip"), Coding::Gzip, now);
        assert_eq!(state.begin_key([1; 32], true, now).coding, Coding::None);
        let retry = state.begin_key([1; 32], true, now + TTL);
        assert_eq!(retry.coding, Coding::Gzip);
        assert_eq!(
            state.begin_key([1; 32], true, now + TTL).coding,
            Coding::None
        );
        drop(retry);
        assert_eq!(
            state.begin_key([1; 32], true, now + TTL).coding,
            Coding::Gzip
        );
    }
    #[test]
    fn dropping_an_old_trial_does_not_release_a_new_trial() {
        let state = Arc::new(OriginState::default());
        let now = Instant::now();
        let old = state.begin_key([0; 32], true, now);
        old.observe_at(
            StatusCode::SERVICE_UNAVAILABLE,
            &HeaderMap::new(),
            Coding::Gzip,
            now,
        );
        let next = state.begin_key([0; 32], true, now);
        drop(old);
        assert_eq!(state.begin_key([0; 32], true, now).coding, Coding::None);
        drop(next);
        assert_eq!(state.begin_key([0; 32], true, now).coding, Coding::Gzip);
    }
    #[test]
    fn identity_response_can_teach_compression_and_cache_is_bounded() {
        let state = Arc::new(OriginState::default());
        let now = Instant::now();
        let first = state.begin_key([0; 32], false, now);
        first.observe_at(StatusCode::OK, &headers("zstd"), Coding::None, now);
        assert_eq!(state.begin_key([0; 32], true, now).coding, Coding::Zstd);
        for i in 0..CAPACITY * 2 {
            state.begin_key(dict::sha256(&i.to_le_bytes()), false, now);
        }
        assert_eq!(state.cache.lock().unwrap().entries.len(), CAPACITY);
    }
}
