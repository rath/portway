//! Previous-body dictionaries: an agent re-uploads its whole conversation
//! every turn, so the body just sent is a near-perfect zstd dictionary for the
//! next one. Only changes need to be represented when the previous body is shared.
//!
//! The upstream keeps what it was asked to keep (`X-Dict-Store`) under the body's
//! SHA-256 and says so (`X-Dict-Stored`); the next request names that hash in
//! the RFC 9842 `dcz` header. Nothing here identifies a user or a session:
//! the hash is the address. Protocol: `docs/protocol.md`.

use std::collections::VecDeque;
use std::sync::Mutex;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use zstd::zstd_safe::{CCtx, CParameter, compress_bound};

pub const ADVERTISED_AS: &str = "dcz";
pub const ADVERTISE_HEADER: &str = "x-request-dictionary";
pub const STORE_HEADER: &str = "x-dict-store";
pub const STORED_HEADER: &str = "x-dict-stored";
pub const MISS_HEADER: &str = "x-dict-miss";

/// RFC 9842 section 5: a zstd skippable-frame header announcing 32 bytes.
pub const DCZ_MAGIC: [u8; 8] = [0x5e, 0x2a, 0x4d, 0x18, 0x20, 0x00, 0x00, 0x00];
/// Confirmed bases kept per model. One conversation needs one; the rest cover
/// sub-agents and conversations that interleave.
const RING_ENTRIES: usize = 8;
const RING_BYTES: usize = 64 << 20;
/// Sender base limit and default receiver per-entry storage limit.
pub const MAX_BASE_BYTES: usize = 32 << 20;
/// Sender base-plus-body limit and default receiver decoder window limit.
pub const MAX_WINDOW_BYTES: usize = 128 << 20;

pub type Hash = [u8; 32];

pub fn sha256(data: &[u8]) -> Hash {
    Sha256::digest(data).into()
}

pub fn hex(hash: &Hash) -> String {
    use std::fmt::Write;
    hash.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[derive(Clone)]
pub struct Base {
    pub hash: Hash,
    pub body: Bytes,
}

/// Bodies the upstream has confirmed it holds, newest last.
#[derive(Default)]
pub struct Ring(Mutex<VecDeque<(DictionaryScope, Base)>>);

impl Ring {
    /// The confirmed base sharing the longest prefix with `body`. zstd matches
    /// on content, not position, so the prefix only ranks the candidates: even
    /// a base sharing nothing up front is never worse than no dictionary.
    pub fn pick(&self, body: &[u8]) -> Option<Base> {
        self.pick_scoped(body, &DictionaryScope::default())
    }
    pub fn pick_scoped(&self, body: &[u8], scope: &DictionaryScope) -> Option<Base> {
        let candidates: Vec<Base> = self
            .0
            .lock()
            .expect("ring lock")
            .iter()
            .filter(|(s, _)| s == scope)
            .map(|(_, b)| b.clone())
            .collect();
        candidates
            .into_iter()
            .filter(|base| base.body.len() + body.len() <= MAX_WINDOW_BYTES)
            .max_by_key(|base| common_prefix(&base.body, body))
    }

    pub fn confirm(&self, hash: Hash, body: Bytes) {
        self.confirm_scoped(hash, body, DictionaryScope::default());
    }
    pub fn confirm_scoped(&self, hash: Hash, body: Bytes, scope: DictionaryScope) {
        if body.len() > MAX_BASE_BYTES {
            return;
        }
        let mut ring = self.0.lock().expect("ring lock");
        ring.retain(|(s, base)| s != &scope || base.hash != hash);
        ring.push_back((scope, Base { hash, body }));
        while ring.len() > RING_ENTRIES
            || ring.iter().map(|(_, base)| base.body.len()).sum::<usize>() > RING_BYTES
        {
            ring.pop_front();
        }
    }

    pub fn forget(&self, hash: &Hash) {
        self.0
            .lock()
            .expect("ring lock")
            .retain(|(_, base)| &base.hash != hash);
    }

    pub fn clear(&self) {
        self.0.lock().expect("ring lock").clear();
    }

    pub fn len(&self) -> usize {
        self.0.lock().expect("ring lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const STRIDE: usize = 4096;
    let mut at = 0;
    for (x, y) in a.chunks(STRIDE).zip(b.chunks(STRIDE)) {
        if x != y {
            return at + x.iter().zip(y).take_while(|(p, q)| p == q).count();
        }
        at += x.len().min(y.len());
    }
    at
}

/// `body` as a `dcz` stream against `base`. The frame carries a checksum: a
/// raw-content dictionary has ID 0, so the frame alone cannot tell the decoder
/// it was handed the wrong one.
pub fn compress(body: &[u8], base: &Base, level: i32) -> std::io::Result<Vec<u8>> {
    // `ref_prefix` borrows the base for the context's lifetime, so this cannot
    // share the thread-local context plain zstd reuses. Creating one costs
    // well under the ~1ms the whole call takes on a 500KB body.
    let mut cctx = CCtx::create();
    let zstd_error = |code| std::io::Error::other(zstd::zstd_safe::get_error_name(code));
    cctx.set_parameter(CParameter::CompressionLevel(level))
        .map_err(zstd_error)?;
    cctx.set_parameter(CParameter::ChecksumFlag(true))
        .map_err(zstd_error)?;
    cctx.ref_prefix(&base.body).map_err(zstd_error)?;

    let mut out = Vec::with_capacity(40 + compress_bound(body.len()));
    out.extend_from_slice(&DCZ_MAGIC);
    out.extend_from_slice(&base.hash);
    let mut frame = Vec::with_capacity(compress_bound(body.len()));
    cctx.compress2(&mut frame, body).map_err(zstd_error)?;
    out.extend_from_slice(&frame);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_of(body: &[u8]) -> Base {
        Base {
            hash: sha256(body),
            body: Bytes::copy_from_slice(body),
        }
    }

    /// Incompressible on its own, so only the dictionary can shrink it.
    fn noise(len: usize, mut seed: u64) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn stream_is_the_rfc_header_then_a_frame_only_the_base_can_open() {
        let previous = noise(3 << 20, 1);
        let mut body = previous.clone();
        body.extend_from_slice(b"one more turn");
        let base = base_of(&previous);

        let wire = compress(&body, &base, 11).unwrap();
        assert_eq!(wire[..8], DCZ_MAGIC);
        assert_eq!(wire[8..40], base.hash);
        // 3MB of noise plus a tail: the far end of the base is still in reach.
        assert!(wire.len() < 2048, "{} bytes", wire.len());

        let mut decoder = zstd::bulk::Decompressor::with_dictionary(&previous).unwrap();
        assert_eq!(decoder.decompress(&wire[40..], body.len()).unwrap(), body);
        let mut wrong = zstd::bulk::Decompressor::with_dictionary(&noise(3 << 20, 2)).unwrap();
        assert!(wrong.decompress(&wire[40..], body.len()).is_err());
        assert!(zstd::bulk::decompress(&wire[40..], body.len()).is_err());
    }

    #[test]
    fn ring_prefers_the_longest_shared_prefix_and_stays_bounded() {
        let ring = Ring::default();
        assert!(ring.pick(b"anything").is_none());
        ring.confirm(
            sha256(b"conversation A turn 1"),
            "conversation A turn 1".into(),
        );
        ring.confirm(
            sha256(b"conversation B turn 1"),
            "conversation B turn 1".into(),
        );
        let picked = ring.pick(b"conversation A turn 1, then turn 2").unwrap();
        assert_eq!(picked.hash, sha256(b"conversation A turn 1"));

        ring.forget(&picked.hash);
        let fallback = ring.pick(b"conversation A turn 1, then turn 2").unwrap();
        assert_eq!(fallback.hash, sha256(b"conversation B turn 1"));

        for turn in 0..20u8 {
            ring.confirm(sha256(&[turn]), Bytes::from(vec![turn; 16]));
        }
        assert_eq!(ring.len(), RING_ENTRIES);
        ring.confirm(sha256(b"huge"), Bytes::from(vec![0; MAX_BASE_BYTES + 1]));
        assert_eq!(ring.len(), RING_ENTRIES);
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn common_prefix_counts_bytes_across_strides() {
        let a = vec![7u8; 10_000];
        let mut b = a.clone();
        assert_eq!(common_prefix(&a, &b), 10_000);
        b[9_000] = 0;
        assert_eq!(common_prefix(&a, &b), 9_000);
        assert_eq!(common_prefix(&a, &a[..5_000]), 5_000);
        assert_eq!(common_prefix(b"", &a), 0);
    }
}

/// Local dictionary partition, never serialized onto the wire.
/// Applications with their own identity model can set this request extension.
#[derive(Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct DictionaryScope(pub Hash);
impl DictionaryScope {
    pub fn new(context: &[u8]) -> Self {
        Self(sha256(context))
    }
    pub fn from_headers(headers: &http::HeaderMap) -> Self {
        let mut hash = Sha256::new();
        for name in ["authorization", "cookie"] {
            for value in headers.get_all(name) {
                hash.update((value.as_bytes().len() as u64).to_le_bytes());
                hash.update(value.as_bytes());
            }
            hash.update([0xff]);
        }
        Self(hash.finalize().into())
    }
}
