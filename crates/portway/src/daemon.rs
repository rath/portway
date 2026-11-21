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
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::store;

/// The launcher waits this long for the readiness byte. Binding the listener
/// and logging the banner is all that has to happen first.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `--stop` gives the daemon to flush the recorder and go.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(100);
/// Enough for a startup reason or a wildcard console's full address list.
const REASON_MAX: usize = 8192;
const LOCK_ATTEMPTS: usize = 8;
const PID_READ_ATTEMPTS: usize = 20;
const PID_READ_RETRY: Duration = Duration::from_millis(10);

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

    let pid_file = claim_pid_file(&pid_path, write_fd);

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

fn claim_pid_file(pid_path: &Path, write_fd: RawFd) -> File {
    for _ in 0..LOCK_ATTEMPTS {
        let opened = match crate::private_file::open(
            pid_path,
            OpenOptions::new().read(true).write(true).create(true),
        ) {
            Ok(file) => file,
            Err(err) => fail_bare(write_fd, &format!("{}: {err}", pid_path.display())),
        };
        let mut pid_file = match move_file_above_stdio(opened) {
            Ok(file) => file,
            Err(err) => fail_bare(write_fd, &format!("{}: {err}", pid_path.display())),
        };
        // The lock lives as long as this process does; whoever else holds it is
        // the daemon already running here.
        if let Err(err) = flock_nb(pid_file.as_raw_fd()) {
            let reason = if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                match read_pid_retry(&mut pid_file) {
                    Some(pid) => format!("{}: already running (pid {pid})", pid_path.display()),
                    None => format!("{} is locked by another process", pid_path.display()),
                }
            } else {
                format!("{}: {err}", pid_path.display())
            };
            fail_bare(write_fd, &reason);
        }
        match same_inode(&pid_file, pid_path) {
            Ok(true) => return pid_file,
            Ok(false) => continue,
            Err(err) => fail_bare(write_fd, &format!("{}: {err}", pid_path.display())),
        }
    }
    fail_bare(
        write_fd,
        &format!("{}: pid file changed while locking", pid_path.display()),
    )
}

impl Daemon {
    /// The listener is bound and the banner is logged: let the launcher go.
    pub fn ready(&self) {
        self.ready_with(None);
    }

    /// The same, handing the launcher the web console's token and addresses
    /// to print: the one terminal that may see the token is the one that
    /// started it. The token goes once, so the addresses a wildcard lists
    /// still fit the readiness message.
    pub fn ready_with(&self, console: Option<(&str, &[String])>) {
        if let Some(fd) = self.ready.replace(None) {
            let mut message = "ok".to_string();
            if let Some((token, urls)) = console {
                message.push('\n');
                message.push_str(token);
                for url in urls {
                    if message.len() + 1 + url.len() > REASON_MAX {
                        break;
                    }
                    message.push('\n');
                    message.push_str(url);
                }
            }
            let bytes = message.as_bytes();
            write_all_fd(fd, &bytes[..bytes.len().min(REASON_MAX)]);
            unsafe { libc::close(fd) };
        }
    }

    /// Report a startup failure to the launcher, then exit. The message also
    /// goes to stderr, which is the terminal before the log is attached and
    /// the log file after.
    pub fn fail(&self, message: &str) -> ! {
        let fd = self.ready.replace(None);
        eprintln!("portway: {message}");
        self.remove_pid_file();
        std::process::exit(fail_now(fd, message));
    }

    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// Drop the pid file on the way out. The lock goes with the process either
    /// way; what this removes is a file that would only ever read as stale.
    pub fn remove_pid_file(&self) {
        crate::private_file::remove_if_same(&self.pid_file, &self.pid_path);
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
        let log = move_file_above_stdio(open_log(&self.log)?)
            .map_err(|err| format!("{}: {err}", self.log.display()))?;
        let null = move_file_above_stdio(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .map_err(|err| format!("/dev/null: {err}"))?,
        )
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
        Liveness::Running(pid, _) => Ok(format!("running (pid {pid})")),
        Liveness::Absent => Err(not_running("no pid file at", &path)),
        Liveness::Stale(_) => Err(not_running("stale pid file", &path)),
    }
}

/// SIGTERM the daemon and wait for it to go. In-flight generations are
/// aborted, the way the dashboard's `q` aborts them.
pub fn stop(dir: &Path) -> Result<String, String> {
    let path = dir.join(store::PID_FILE);
    // The file stays open until the daemon has gone. The daemon unlinks the
    // path on its way out, before it has finished exiting, so the path
    // vanishing proves nothing; the lock on this inode is released only when
    // the daemon's own descriptor is closed.
    let (pid, file) = match liveness(&path)? {
        Liveness::Running(pid, file) => (pid, file),
        Liveness::Absent => return Err(not_running("no pid file at", &path)),
        Liveness::Stale(file) => {
            crate::private_file::remove_if_same(&file, &path);
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
    while held(&file, &path)? {
        if Instant::now() >= deadline {
            return Err(format!("pid {pid} did not exit within 10s"));
        }
        std::thread::sleep(STOP_POLL);
    }
    // A daemon that died before it could unlink its file leaves it behind. The
    // path is removed only while it still names the file this process locked:
    // a daemon started since has a file of its own there.
    let ours = same_inode(&file, &path).map_err(|err| format!("{}: {err}", path.display()))?;
    if ours
        && let Err(err) = fs::remove_file(&path)
        && err.kind() != ErrorKind::NotFound
    {
        return Err(format!("{}: {err}", path.display()));
    }
    Ok(format!("stopped (pid {pid})"))
}

/// SIGHUP: the daemon reopens its log file and asks the serving process to reload.
pub fn reload(dir: &Path) -> Result<String, String> {
    let path = dir.join(store::PID_FILE);
    match liveness(&path)? {
        Liveness::Running(pid, _) => {
            if unsafe { libc::kill(pid, libc::SIGHUP) } != 0 {
                return Err(format!("pid {pid}: {}", io::Error::last_os_error()));
            }
            Ok(format!("reloaded (pid {pid})"))
        }
        Liveness::Absent => Err(not_running("no pid file at", &path)),
        Liveness::Stale(_) => Err(not_running("stale pid file", &path)),
    }
}

/// Point fd 2 at `<dir>/portway.log` again, for a rotation that moved
/// the old file aside. The daemon calls this on SIGHUP.
pub fn reopen_log(log: &Path) -> Result<(), String> {
    let file =
        move_file_above_stdio(open_log(log)?).map_err(|err| format!("{}: {err}", log.display()))?;
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(format!("{}: {}", log.display(), io::Error::last_os_error()));
    }
    Ok(())
}

enum Liveness {
    /// The pid, and the pid file opened to find it: its lock outlives the path.
    Running(i32, File),
    Absent,
    /// Unlocked when probed; this handle now holds the lock so cleanup stays
    /// scoped to the exact inode that was stale.
    Stale(File),
}

/// Open the pid file and ask the kernel who holds it. `flock` belongs to the
/// open file description, so a second open in this same process conflicts with
/// the first — which is what makes this testable without a second process.
fn liveness(pid_file: &Path) -> Result<Liveness, String> {
    let mut file = match crate::private_file::open(pid_file, OpenOptions::new().read(true)) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Liveness::Absent),
        Err(err) => return Err(format!("{}: {err}", pid_file.display())),
    };
    if !held(&file, pid_file)? {
        // Nobody holds it, so no daemon is running here. Keeping the handle
        // lets `--stop` unlink the exact stale inode while still locked.
        return Ok(Liveness::Stale(file));
    }
    match read_pid_retry(&mut file) {
        Some(pid) => Ok(Liveness::Running(pid, file)),
        None => Err(format!(
            "{} is locked but holds no pid; remove it if no daemon is running",
            pid_file.display()
        )),
    }
}

/// Whether another open file description holds the lock on `file`. When
/// nobody does, the probe takes it, and keeps it until `file` is closed.
fn held(file: &File, path: &Path) -> Result<bool, String> {
    match flock_nb(file.as_raw_fd()) {
        Ok(()) => Ok(false),
        Err(err) if err.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(true),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn read_pid_retry(file: &mut File) -> Option<i32> {
    for attempt in 0..PID_READ_ATTEMPTS {
        if let Some(pid) = read_pid(file) {
            return Some(pid);
        }
        if attempt + 1 < PID_READ_ATTEMPTS {
            std::thread::sleep(PID_READ_RETRY);
        }
    }
    None
}

fn read_pid(file: &mut File) -> Option<i32> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.take(64).read_to_string(&mut text).ok()?;
    text.trim().parse().ok().filter(|pid| *pid > 1)
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
            for url in console {
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
    /// Up, with the web console's addresses when it serves one.
    Started(Vec<String>),
    /// The daemon said why it could not start.
    Refused(String),
    /// The pipe closed with nothing in it: it died before saying anything.
    Silent,
    Timeout,
    /// The launcher itself could not read the pipe.
    Unreadable(String),
}

fn wait_ready(fd: RawFd) -> Ready {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ready::Timeout;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout) };
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
        let space = REASON_MAX.saturating_sub(buffer.len());
        if space == 0 {
            break;
        }
        let want = chunk.len().min(space);
        let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), want) };
        if read < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Ready::Unreadable(err.to_string());
        }
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read as usize]);
    }
    if buffer.is_empty() {
        return Ready::Silent;
    }
    let text = String::from_utf8_lossy(&buffer);
    let trimmed = text.trim();
    if trimmed == "ok" {
        return Ready::Started(Vec::new());
    }
    if let Some(rest) = trimmed.strip_prefix("ok\n") {
        let mut lines = rest.lines().map(str::trim);
        let token = lines.next().unwrap_or_default().to_owned();
        return Ready::Started(
            lines
                .filter(|url| !url.is_empty())
                .map(|url| format!("{url}#token={token}"))
                .collect(),
        );
    }
    let reason = trimmed.strip_prefix("err\n").unwrap_or(trimmed).trim();
    Ready::Refused(reason.to_string())
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
        let mut payload = String::from("err\n");
        payload.push_str(message);
        let bytes = payload.as_bytes();
        let len = bytes.len().min(REASON_MAX);
        write_all_fd(fd, &bytes[..len]);
        unsafe { libc::close(fd) };
    }
    1
}

fn pipe() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as RawFd; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", io::Error::last_os_error()));
    }
    let read_fd = match move_fd_above_stdio(fds[0]) {
        Ok(fd) => fd,
        Err(err) => {
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(format!("pipe: {err}"));
        }
    };
    let write_fd = match move_fd_above_stdio(fds[1]) {
        Ok(fd) => fd,
        Err(err) => {
            unsafe {
                libc::close(read_fd);
                libc::close(fds[1]);
            }
            return Err(format!("pipe: {err}"));
        }
    };
    Ok((read_fd, write_fd))
}

fn move_fd_above_stdio(fd: RawFd) -> io::Result<RawFd> {
    if fd > libc::STDERR_FILENO {
        return Ok(fd);
    }
    let moved = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::close(fd) };
    Ok(moved)
}

fn move_file_above_stdio(file: File) -> io::Result<File> {
    if file.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(file);
    }
    let fd = file.into_raw_fd();
    match move_fd_above_stdio(fd) {
        Ok(moved) => Ok(unsafe { File::from_raw_fd(moved) }),
        Err(err) => {
            unsafe { libc::close(fd) };
            Err(err)
        }
    }
}

fn flock_nb(fd: RawFd) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn write_all_fd(fd: RawFd, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
        } else if written < 0 && io::Error::last_os_error().kind() == ErrorKind::Interrupted {
            continue;
        } else {
            break;
        }
    }
}

fn same_inode(file: &File, path: &Path) -> io::Result<bool> {
    let held = file.metadata()?;
    match fs::symlink_metadata(path) {
        Ok(now) => Ok((held.dev(), held.ino()) == (now.dev(), now.ino())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn open_log(log: &Path) -> Result<File, String> {
    crate::private_file::open(log, OpenOptions::new().append(true).create(true))
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

    #[test]
    fn private_runtime_pid_never_names_a_process_group_or_init() {
        let dir = dir("invalid-pid");
        let path = dir.join(store::PID_FILE);
        for value in ["-1", "0", "1", "-42", "2147483648", "invalid"] {
            fs::write(&path, value).unwrap();
            let mut file = File::open(&path).unwrap();
            assert_eq!(read_pid(&mut file), None, "{value}");
        }
        fs::write(&path, "42\n").unwrap();
        assert_eq!(read_pid(&mut File::open(path).unwrap()), Some(42));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readiness_pipe_distinguishes_ok_from_refused_messages() {
        let (r, w) = pipe().unwrap();
        write_all_fd(w, b"err\nk");
        unsafe { libc::close(w) };
        match wait_ready(r) {
            Ready::Refused(msg) => assert_eq!(msg, "k"),
            _ => panic!("expected refused"),
        }
        unsafe { libc::close(r) };

        let (r, w) = pipe().unwrap();
        write_all_fd(w, b"ok\nabc123\nhttp://127.0.0.1:8790/\nhttp://[::1]:8790/");
        unsafe { libc::close(w) };
        match wait_ready(r) {
            Ready::Started(urls) => assert_eq!(
                urls,
                vec![
                    "http://127.0.0.1:8790/#token=abc123",
                    "http://[::1]:8790/#token=abc123",
                ]
            ),
            _ => panic!("expected started"),
        }
        unsafe { libc::close(r) };
    }
}
