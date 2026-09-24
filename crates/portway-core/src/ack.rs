//! Upload timing that ends at the peer's acknowledgement, not at `write()`.
//!
//! `write()` returning only proves the local kernel took the bytes, which is
//! why small bodies used to read 0ms. The kernel's unACKed send-queue counter
//! draining to zero proves the far end's TCP has them all. That is still the
//! peer's *kernel*, not its application — the response proves that.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::PhaseClock;

/// Give up rather than hold a dup'd fd forever on a stalled path.
const DEADLINE: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(2);

/// Bytes this socket has written but the far end has not yet acknowledged.
///
/// Darwin reports them as `SO_NWRITE`; Linux as `SIOCOUTQ`, which `libc`
/// exports under its kernel alias `TIOCOUTQ` (same 0x5411 request).
fn unacked(fd: RawFd) -> std::io::Result<i32> {
    let mut value: libc::c_int = 0;
    #[cfg(target_vendor = "apple")]
    // SAFETY: `value` is a live c_int and `len` matches its size; getsockopt
    // writes at most that many bytes.
    let rc = unsafe {
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NWRITE,
            (&raw mut value).cast(),
            &mut len,
        )
    };
    #[cfg(not(target_vendor = "apple"))]
    // SAFETY: SIOCOUTQ takes a pointer to a single int, which `value` is.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &raw mut value) };

    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

/// Watch the send queue until it hits zero, i.e. the last byte was ACKed.
///
/// The socket is dup'd first: the pool may close the connection at any moment
/// and a bare fd number could be reused for another socket.
pub async fn watch_drain(clock: Arc<PhaseClock>) {
    let Some(raw) = clock.fd() else { return };
    // SAFETY: the connection that owns `raw` is alive here — the body has just
    // been written to it — and the borrow ends with this statement.
    let dup = match unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned() {
        Ok(dup) => dup,
        Err(_) => return,
    };
    drain_loop(&dup, &clock).await;
}

async fn drain_loop(dup: &OwnedFd, clock: &PhaseClock) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        match unacked(dup.as_raw_fd()) {
            Ok(0) => {
                clock.mark_drained();
                return;
            }
            Ok(_) => {}
            // Connection gone; the log falls back to the write time.
            Err(_) => return,
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Fire the watcher as a detached task: the request must not wait on it, and
/// the response arriving is what actually ends the request.
pub fn spawn_watch(clock: Arc<PhaseClock>) {
    if clock.fd().is_some() {
        tokio::spawn(watch_drain(clock));
    }
}
