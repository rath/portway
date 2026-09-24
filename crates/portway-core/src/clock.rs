//! The connection and upload phases of one upstream request.
//!
//! Three tasks touch a clock — the request handler, the body that feeds the
//! connection driver, and the send-queue watcher — so every field is atomic.
//! Durations are nanoseconds; instants are nanoseconds since `origin`.
//! `UNSET` distinguishes "never measured" from a genuine zero.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::Instant;

const UNSET: u64 = u64::MAX;

pub struct PhaseClock {
    origin: Instant,
    dns: AtomicU64,
    tcp: AtomicU64,
    tls: AtomicU64,
    /// Socket backing the connection in use, for the send-queue counter.
    fd: AtomicI32,
    upload_started: AtomicU64,
    upload_finished: AtomicU64,
    /// Send queue hit zero: the last byte was ACKed by the peer's kernel.
    drained: AtomicU64,
    /// The request body has handed its last frame to hyper; the next completed
    /// write on the connection ends the upload.
    body_handed: AtomicBool,
}

impl PhaseClock {
    pub fn new() -> Self {
        PhaseClock {
            origin: Instant::now(),
            dns: AtomicU64::new(UNSET),
            tcp: AtomicU64::new(UNSET),
            tls: AtomicU64::new(UNSET),
            fd: AtomicI32::new(-1),
            upload_started: AtomicU64::new(UNSET),
            upload_finished: AtomicU64::new(UNSET),
            drained: AtomicU64::new(UNSET),
            body_handed: AtomicBool::new(false),
        }
    }

    fn now(&self) -> u64 {
        self.origin.elapsed().as_nanos() as u64
    }

    fn read(slot: &AtomicU64) -> Option<f64> {
        match slot.load(Ordering::Relaxed) {
            UNSET => None,
            nanos => Some(nanos as f64 / 1e9),
        }
    }

    /// Record a phase only once: a pooled connection keeps the timings of the
    /// dial that created it, and a retry gets a clock of its own.
    fn stamp_once(slot: &AtomicU64, value: u64) {
        let _ = slot.compare_exchange(UNSET, value, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn set_dns(&self, elapsed: std::time::Duration) {
        self.dns.store(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn set_tcp(&self, elapsed: std::time::Duration) {
        self.tcp.store(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn set_tls(&self, elapsed: std::time::Duration) {
        self.tls.store(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn dns(&self) -> Option<f64> {
        Self::read(&self.dns)
    }

    pub fn tcp(&self) -> Option<f64> {
        Self::read(&self.tcp)
    }

    pub fn tls(&self) -> Option<f64> {
        Self::read(&self.tls)
    }

    /// True when this request dialed its own connection rather than reusing a
    /// pooled one — the log prints `reused` for the latter.
    pub fn fresh_connection(&self) -> bool {
        self.dns().is_some() || self.tcp().is_some() || self.tls().is_some()
    }

    pub fn set_fd(&self, fd: RawFd) {
        self.fd.store(fd, Ordering::Relaxed);
    }

    pub fn fd(&self) -> Option<RawFd> {
        match self.fd.load(Ordering::Relaxed) {
            -1 => None,
            fd => Some(fd),
        }
    }

    pub fn mark_upload_started(&self) {
        Self::stamp_once(&self.upload_started, self.now());
    }

    pub fn mark_body_handed(&self) {
        self.body_handed.store(true, Ordering::Release);
    }

    pub fn body_handed(&self) -> bool {
        self.body_handed.load(Ordering::Acquire)
    }

    /// Stamps the end of the write block. Returns true the first time, so the
    /// caller knows it owns starting the send-queue watcher.
    pub fn mark_upload_finished(&self) -> bool {
        self.upload_finished
            .compare_exchange(UNSET, self.now(), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    pub fn mark_drained(&self) {
        Self::stamp_once(&self.drained, self.now());
    }

    /// Socket-write time of the request body, as the client sees it. A body the
    /// kernel's send buffer swallows whole ends at 0ms by construction.
    fn upload_seconds(&self) -> Option<f64> {
        let started = Self::read(&self.upload_started)?;
        let finished = Self::read(&self.upload_finished)?;
        Some(finished - started)
    }

    /// First body byte to last byte ACKed — the upload as the wire saw it.
    ///
    /// `write()` returning only proves the local kernel took the bytes; the
    /// send queue draining to zero is the far end's TCP confirming it has them
    /// all. Falls back to the write-block time when the counter was
    /// unavailable (unreachable socket, unknown platform).
    pub fn upload_wall(&self) -> Option<f64> {
        match (Self::read(&self.drained), Self::read(&self.upload_started)) {
            (Some(drained), Some(started)) => Some(drained - started),
            _ => self.upload_seconds(),
        }
    }
}

impl Default for PhaseClock {
    fn default() -> Self {
        Self::new()
    }
}
