//! Upload timing that ends at the peer's acknowledgement, not at `write()`.
//!
//! `write()` returning only proves the local kernel took the bytes, which is
//! why small bodies used to read 0ms. The kernel's unACKed send-queue counter
//! draining to zero proves the far end's TCP has them all. That is still the
//! peer's *kernel*, not its application — the response proves that.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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
/// Capture the socket synchronously, while the caller still owns it. Waiting
/// until this future is polled could instead capture a reused fd after the
/// connection closes. Invalid descriptors simply leave the timing unset.
pub fn watch_drain(clock: Arc<PhaseClock>) -> impl Future<Output = ()> + Send {
    let dup = clock.fd().and_then(|raw| {
        // SAFETY: fcntl validates the integer descriptor; it borrows no Rust
        // object and returns a new owned descriptor on success.
        let fd = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            None
        } else {
            // SAFETY: this successful fcntl created a unique owned descriptor.
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    });
    async move {
        if let Some(dup) = dup {
            drain_loop(&dup, &clock).await;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn the_socket_is_captured_before_the_watcher_is_polled() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let clock = Arc::new(PhaseClock::new());
        clock.set_fd(client.as_raw_fd());
        clock.mark_upload_started();
        let watch = watch_drain(Arc::clone(&clock));
        drop(client);
        let err = server
            .read(&mut [0])
            .expect_err("the watcher owns the socket before its first poll");
        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        watch.await;
        assert!(clock.upload_wall().is_some());
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    }
}
