//! `--daemon`: detach, then stay reachable through the pid file.
//!
//! One `fork` and a `setsid` — the process must not hold the terminal it was
//! started from — plus a pipe that tells the launcher whether the listener
//! actually came up. The launcher waits for that byte instead of guessing: a
//! pid file exists either way, and a daemon that died on a taken port must not
//! look started.
//!
//! The pid file is held with an exclusive `flock` for as long as the daemon
//! lives. That lock *is* the liveness check: a caught signal, a panic or a
//! `SIGKILL` all release it, so a leftover file cannot be mistaken for a
//! running daemon, and a second daemon cannot start on the same data dir.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::store;

/// The launcher waits this long for the readiness byte. Binding the listener
/// and logging the banner is all that has to happen first.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `--stop` gives the daemon to flush the recorder and go.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(100);
/// A reason longer than a pipe buffer cannot be read in one go; the message is
/// a diagnosis, not a document.
const REASON_MAX: usize = 512;

pub struct Daemon {
    /// Held for the exclusive lock, which is the whole point of the file.
    pid_file: File,
    pid_path: PathBuf,
    ready: Cell<Option<RawFd>>,
    log: PathBuf,
}

/// Fork, detach, take the pid file, point stderr at the log file.
///
/// Returns in the child only: the parent waits for the readiness byte, prints
/// the outcome and exits, so a caller that sees `Ok` is the daemon itself.
pub fn start(dir: &Path) -> Result<Daemon, String> {
    // On the parent's side of the fork, so a data dir that cannot be created
    // is reported normally instead of through the pipe.
    store::ensure_dir(dir)?;
    let pid_path = dir.join(store::PID_FILE);
    let log = dir.join(store::LOG_FILE);

    let (read_fd, write_fd) = pipe()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(read_fd) };
        unsafe { libc::close(write_fd) };
        return Err(format!("fork: {err}"));
    }
    if pid > 0 {
        unsafe { libc::close(write_fd) };
        // Never returns: the launcher's process ends here.
        report(read_fd, pid, &log);
    }

    unsafe { libc::close(read_fd) };
    detach();

    let mut pid_file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&pid_path)
    {
        Ok(file) => file,
        Err(err) => fail_bare(write_fd, &format!("{}: {err}", pid_path.display())),
    };

    // The lock lives as long as this process does; whoever else holds it is
    // the daemon already running here.
    if unsafe { libc::flock(pid_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        let reason = if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            match read_pid(&mut pid_file) {
                Some(pid) => format!("{}: already running (pid {pid})", pid_path.display()),
                None => format!("{} is locked by another process", pid_path.display()),
            }
        } else {
            format!("{}: {err}", pid_path.display())
        };
        fail_bare(write_fd, &reason);
    }

    let daemon = Daemon {
        pid_file,
        pid_path,
        ready: Cell::new(Some(write_fd)),
        log,
    };
    if let Err(err) = daemon.write_pid() {
        daemon.fail(&err);
    }
    if let Err(err) = daemon.attach_log() {
        daemon.fail(&err);
    }
    Ok(daemon)
}

impl Daemon {
    /// The listener is bound and the banner is logged: let the launcher go.
    pub fn ready(&self) {
        self.ready_with(None);
    }

    /// The same, handing the launcher the web console's address to print:
    /// the one terminal that may see its token is the one that started it.
    pub fn ready_with(&self, console: Option<&str>) {
        if let Some(fd) = self.ready.replace(None) {
            let message = match console {
                Some(url) => format!("k\n{url}"),
                None => "k".to_string(),
            };
            let bytes = message.as_bytes();
            unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len().min(REASON_MAX)) };
            unsafe { libc::close(fd) };
        }
    }

    /// Report a startup failure to the launcher, then exit. The message also
    /// goes to stderr, which is the terminal before the log is attached and
    /// the log file after.
    pub fn fail(&self, message: &str) -> ! {
        let fd = self.ready.replace(None);
        eprintln!("portway: {message}");
        std::process::exit(fail_now(fd, message));
    }

    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// Drop the pid file on the way out. The lock goes with the process either
    /// way; what this removes is a file that would only ever read as stale.
    pub fn remove_pid_file(&self) {
        let _ = fs::remove_file(&self.pid_path);
    }

    fn write_pid(&self) -> Result<(), String> {
        let mut file = &self.pid_file;
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| writeln!(file, "{}", std::process::id()))
            .and_then(|()| file.flush())
            .map_err(|err| format!("{}: {err}", self.pid_path.display()))
    }

    /// stderr becomes the log file, stdin and stdout become `/dev/null`. Color
    /// is off because stderr is a file now, whatever `init_color` saw on the
    /// terminal the daemon was launched from.
    fn attach_log(&self) -> Result<(), String> {
        let log = open_log(&self.log)?;
        let null = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .map_err(|err| format!("/dev/null: {err}"))?;
        if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0
            || unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) } < 0
            || unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } < 0
        {
            return Err(format!("redirect: {}", io::Error::last_os_error()));
        }
        crate::logfmt::set_color(false);
        Ok(())
    }
}

/// Whether a daemon holds `<dir>/portway.pid`.
pub fn status(dir: &Path) -> Result<String, String> {
    let path = dir.join(store::PID_FILE);
    match liveness(&path)? {
        Liveness::Running(pid) => Ok(format!("running (pid {pid})")),
        Liveness::Absent => Err(not_running("no pid file at", &path)),
        Liveness::Stale => Err(not_running("stale pid file", &path)),
    }
}

/// SIGTERM the daemon and wait for it to go. In-flight generations are
/// aborted, the way the dashboard's `q` aborts them.
pub fn stop(dir: &Path) -> Result<String, String> {
    let path = dir.join(store::PID_FILE);
    let pid = match liveness(&path)? {
        Liveness::Running(pid) => pid,
        Liveness::Absent => return Err(not_running("no pid file at", &path)),
        Liveness::Stale => {
            let _ = fs::remove_file(&path);
            return Err(not_running("stale pid file", &path));
        }
    };
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("pid {pid}: {err}"));
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    while matches!(liveness(&path)?, Liveness::Running(_)) {
        if Instant::now() >= deadline {
            return Err(format!("pid {pid} did not exit within 10s"));
        }
        std::thread::sleep(STOP_POLL);
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(format!("{}: {err}", path.display())),
    }
    Ok(format!("stopped (pid {pid})"))
}

/// SIGHUP: the daemon reopens its log file and asks the serving process to reload.
pub fn reload(dir: &Path) -> Result<String, String> {
    let path = dir.join(store::PID_FILE);
    match liveness(&path)? {
        Liveness::Running(pid) => {
            if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
                return Err(format!("pid {pid}: {}", io::Error::last_os_error()));
            }
            Ok(format!("reloaded (pid {pid})"))
        }
        Liveness::Absent => Err(not_running("no pid file at", &path)),
        Liveness::Stale => Err(not_running("stale pid file", &path)),
    }
}

/// Point fd 2 at `<dir>/portway.log` again, for a rotation that moved
/// the old file aside. The daemon calls this on SIGHUP.
pub fn reopen_log(log: &Path) -> Result<(), String> {
    let file = open_log(log)?;
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(format!("{}: {}", log.display(), io::Error::last_os_error()));
    }
    Ok(())
}

enum Liveness {
    Running(i32),
    Absent,
    Stale,
}

/// Open the pid file and ask the kernel who holds it. `flock` belongs to the
/// open file description, so a second open in this same process conflicts with
/// the first — which is what makes this testable without a second process.
fn liveness(pid_file: &Path) -> Result<Liveness, String> {
    let mut file = match OpenOptions::new().read(true).write(true).open(pid_file) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Liveness::Absent),
        Err(err) => return Err(format!("{}: {err}", pid_file.display())),
    };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // Nobody holds it, so no daemon is running here. Closing the file
        // releases the probe's own lock.
        return Ok(Liveness::Stale);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
        return Err(format!("{}: {err}", pid_file.display()));
    }
    match read_pid(&mut file) {
        Some(pid) => Ok(Liveness::Running(pid)),
        None => Err(format!(
            "{} is locked but holds no pid; remove it if no daemon is running",
            pid_file.display()
        )),
    }
}

fn read_pid(file: &mut File) -> Option<i32> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.take(64).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// The launcher's side of `start`. Diverges: it is the parent process.
fn report(fd: RawFd, pid: i32, log: &Path) -> ! {
    let outcome = wait_ready(fd);
    unsafe { libc::close(fd) };
    match outcome {
        Ready::Started(console) => {
            println!(
                "portway: daemon started (pid {pid}), logging to {}",
                log.display()
            );
            if let Some(url) = console {
                println!("portway: console at {url}");
            }
            std::process::exit(0);
        }
        Ready::Refused(reason) => {
            eprintln!("portway: {reason}");
            std::process::exit(1);
        }
        Ready::Silent => {
            eprintln!("portway: daemon failed to start (see {})", log.display());
            std::process::exit(1);
        }
        Ready::Timeout => {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            eprintln!("portway: daemon did not become ready within 10s");
            std::process::exit(1);
        }
        Ready::Unreadable(err) => {
            eprintln!("portway: waiting for the daemon: {err}");
            std::process::exit(1);
        }
    }
}

enum Ready {
    /// Up, with the web console's address when it serves one.
    Started(Option<String>),
    /// The daemon said why it could not start.
    Refused(String),
    /// The pipe closed with nothing in it: it died before saying anything.
    Silent,
    Timeout,
    /// The launcher itself could not read the pipe.
    Unreadable(String),
}

fn wait_ready(fd: RawFd) -> Ready {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut pfd, 1, READY_TIMEOUT.as_millis() as i32) };
        if ready == 0 {
            return Ready::Timeout;
        }
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Ready::Unreadable(err.to_string());
        }
        let mut buffer = [0u8; REASON_MAX];
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Ready::Unreadable(err.to_string());
        }
        if read == 0 {
            return Ready::Silent;
        }
        let text = String::from_utf8_lossy(&buffer[..read as usize])
            .trim()
            .to_string();
        return match text.split_once('\n') {
            _ if text == "k" => Ready::Started(None),
            Some(("k", url)) => Ready::Started(Some(url.trim().to_string())),
            _ => Ready::Refused(text),
        };
    }
}

/// `fork` and friends, minus the terminal: a new session, so the daemon
/// survives the shell that launched it, and a cwd that cannot pin a mount.
fn detach() {
    unsafe {
        libc::setsid();
    }
    let _ = std::env::set_current_dir("/");
}

/// A failure before there is a `Daemon` to attach: the child's pid file could
/// not be opened or locked, so there is nothing to clean up but the pipe. The
/// reason goes to the launcher alone — the child's stderr is still the same
/// terminal, and printing it here would say everything twice.
fn fail_bare(write_fd: RawFd, message: &str) -> ! {
    std::process::exit(fail_now(Some(write_fd), message))
}

/// Write the reason to the launcher, close the pipe, and hand back the exit
/// code.
fn fail_now(ready: Option<RawFd>, message: &str) -> i32 {
    if let Some(fd) = ready {
        let bytes = message.as_bytes();
        let len = bytes.len().min(REASON_MAX);
        unsafe { libc::write(fd, bytes.as_ptr().cast(), len) };
        unsafe { libc::close(fd) };
    }
    1
}

fn pipe() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", io::Error::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

fn open_log(log: &Path) -> Result<File, String> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(log)
        .map_err(|err| format!("{}: {err}", log.display()))
}

fn not_running(what: &str, path: &Path) -> String {
    format!("not running ({what} {})", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("portway-daemon-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The lock, not the file, is what says "running": a leftover pid file
    /// with no process behind it must not be reported as one.
    #[test]
    fn only_a_locked_pid_file_reads_as_running() {
        let dir = dir("liveness");
        let path = dir.join(store::PID_FILE);

        let absent = status(&dir).unwrap_err();
        assert!(absent.contains("no pid file"), "{absent}");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}\n", std::process::id()).unwrap();
        file.flush().unwrap();

        let stale = status(&dir).unwrap_err();
        assert!(stale.contains("stale pid file"), "{stale}");

        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let running = status(&dir).unwrap();
        assert_eq!(running, format!("running (pid {})", std::process::id()));

        // Releasing the lock is what a daemon's exit does.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }, 0);
        let stale = status(&dir).unwrap_err();
        assert!(stale.contains("stale pid file"), "{stale}");

        // `--stop` on a stale file cleans it up and still reports failure.
        let stopped = stop(&dir).unwrap_err();
        assert!(stopped.contains("stale pid file"), "{stopped}");
        assert!(!path.exists(), "the stale pid file was left behind");
    }
}
