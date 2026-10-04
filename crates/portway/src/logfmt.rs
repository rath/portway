//! One line per request, on stderr. Bodies, headers and keys are never logged.
//!
//! The format is inherited verbatim from the Python forwarder: a dim `HH:mm:ss`
//! stamp, no level for INFO so normal traffic stays quiet, and a colored level
//! from WARNING up so trouble stays loud.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::telemetry::{self, Event};

pub const DIM: &str = "2";
pub const BOLD: &str = "1";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const RED: &str = "31";
pub const CYAN: &str = "36";

/// stderr is the console when this runs under a terminal (a hub's pty
/// included); `NO_COLOR` is the common opt-out convention.
static COLOR: AtomicBool = AtomicBool::new(false);

pub fn init_color() {
    let on = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    COLOR.store(on, Ordering::Relaxed);
}

pub fn set_color(on: bool) {
    COLOR.store(on, Ordering::Relaxed);
}

pub fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// Wrap `text` in an ANSI escape, or hand it back untouched when color is off.
pub fn c(code: &str, text: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// HTTP status for the log: green 2xx, cyan 3xx, yellow 4xx, red 5xx.
pub fn status(code: u16) -> String {
    let shade = if code < 300 {
        GREEN
    } else if code < 400 {
        CYAN
    } else if code < 500 {
        YELLOW
    } else {
        RED
    };
    c(shade, &code.to_string())
}

/// Byte count for the log: B under a tenth of a KB, then KB — one decimal
/// below 1 KiB, whole above it — then MB (1024 steps).
pub fn human(size: u64) -> String {
    if size < 1024 {
        // One decimal is what makes a compressed upload readable: `459KB ->
        // 0.3KB` is a glance where `459KB -> 322B` is arithmetic. A tenth is
        // also the smallest step this scale has, so anything that would round
        // to `0.0KB` stays in bytes — `0.0KB` reads as an empty body.
        let kb = size as f64 / 1024.0;
        return if kb < 0.05 {
            format!("{size}B")
        } else {
            format!("{kb:.1}KB")
        };
    }
    let kb = size as f64 / 1024.0;
    if kb < 1024.0 {
        format!("{kb:.0}KB")
    } else {
        format!("{:.1}MB", kb / 1024.0)
    }
}

/// Duration for the log: ms below one second, then seconds, then minutes.
pub fn human_time(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{:.0}ms", seconds * 1000.0)
    } else if seconds < 60.0 {
        format!("{seconds:.2}s")
    } else {
        format!("{}m{:02.1}s", (seconds as u64) / 60, seconds % 60.0)
    }
}

/// A window width as one token: `90s`, `30m`, `24h`, `7d` — the largest unit
/// the span divides into whole, written the way `--since` is.
pub fn span(since: Duration) -> String {
    let seconds = since.as_secs();
    let mut units: &[(&str, u64)] = &[("d", 86_400), ("h", 3_600), ("m", 60)];
    if seconds < 2 * 86_400 {
        // One day is written the way the flag is.
        units = &[("h", 3_600), ("m", 60)];
    }
    for (unit, scale) in units {
        if seconds.is_multiple_of(*scale) && seconds / scale >= 1 {
            return format!("{}{unit}", seconds / scale);
        }
    }
    format!("{seconds}s")
}

pub use portway_core::telemetry::Level;
fn shade(level: Level) -> &'static str {
    match level {
        Level::Info => GREEN,
        Level::Warning => YELLOW,
        Level::Error => RED,
    }
}

/// Tests swap stderr for a buffer so they can assert on whole lines.
static CAPTURE: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();

fn capture() -> &'static Mutex<Option<Vec<String>>> {
    CAPTURE.get_or_init(|| Mutex::new(None))
}

pub fn start_capture() {
    *capture().lock().unwrap() = Some(Vec::new());
}

pub fn take_capture() -> Vec<String> {
    capture().lock().unwrap().take().unwrap_or_default()
}

/// `YYYY-MM-DD HH:mm:ss [LEVEL ]message`, with a dim stamp. INFO stays bare.
///
/// The written line carries its date where the dashboards' stamps do not: a
/// log file outlives the day it was started in, and a reader looking for
/// `05:09:43` in one would otherwise find every day's. The dashboards keep
/// `HH:MM:SS` for width alone.
pub fn format_record(level: Level, message: &str) -> String {
    let prefix = if level >= Level::Warning {
        format!("{} ", c(shade(level), level.name()))
    } else {
        String::new()
    };
    format!("{} {prefix}{message}", c(DIM, &dated_stamp()))
}

/// A log record: the sinks see it, and the console prints it unless the
/// dashboard owns the terminal.
pub fn log(level: Level, message: &str) {
    let mut held = capture().lock().unwrap();
    if let Some(lines) = held.as_mut() {
        lines.push(format_record(level, message));
        return;
    }
    drop(held);
    telemetry::emit(Event::Log {
        stamp: stamp(),
        level,
        message: message.to_string(),
    });
    if !telemetry::tui_installed() {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{}", format_record(level, message));
    }
}

/// The finished-request line. Console only: the same request already went to
/// the sinks as an `Event::Request`, and a `log` record here would put it in
/// the database twice.
pub fn console(level: Level, message: &str) {
    let mut held = capture().lock().unwrap();
    if let Some(lines) = held.as_mut() {
        lines.push(format_record(level, message));
        return;
    }
    drop(held);
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{}", format_record(level, message));
}

/// A record from the thread that *is* behind the recorder — a failed write, a
/// retention pass. The dashboards and the console see it; the recorder does
/// not, because feeding a write failure back into the queue it failed on is a
/// loop.
pub fn from_store(level: Level, message: &str) {
    let mut held = capture().lock().unwrap();
    if let Some(lines) = held.as_mut() {
        lines.push(format_record(level, message));
        return;
    }
    drop(held);
    telemetry::emit_viewers(Event::Log {
        stamp: stamp(),
        level,
        message: message.to_string(),
    });
    if !telemetry::tui_installed() {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{}", format_record(level, message));
    }
}

pub fn info(message: &str) {
    log(Level::Info, message);
}

pub fn warn(message: &str) {
    log(Level::Warning, message);
}

pub fn error(message: &str) {
    log(Level::Error, message);
}

/// Local `HH:MM:SS`, cached for the whole second it describes. A busy turn logs
/// once per request, so this keeps `localtime_r` off the hot path. This is the
/// stamp an event carries to the dashboards, which parse it as a clock.
pub fn stamp() -> String {
    static CACHED: Mutex<(i64, String)> = Mutex::new((i64::MIN, String::new()));
    cached_stamp(&CACHED, false)
}

/// Local `YYYY-MM-DD HH:MM:SS` of this second, cached the same way: the stamp
/// a written log line starts with.
fn dated_stamp() -> String {
    static CACHED: Mutex<(i64, String)> = Mutex::new((i64::MIN, String::new()));
    cached_stamp(&CACHED, true)
}

fn cached_stamp(cache: &Mutex<(i64, String)>, date: bool) -> String {
    let now = epoch() as i64;
    let mut cached = cache.lock().unwrap();
    if cached.0 != now {
        cached.0 = now;
        cached.1 = format_epoch(now, date);
    }
    cached.1.clone()
}

/// Unix seconds with the sub-second fraction — the clock `stamp()` reads, and
/// what the recorder stores as `ts_unix`.
pub fn epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Local `YYYY-MM-DD HH:MM:SS`, from a value `epoch()` produced.
pub fn datetime(ts: f64) -> String {
    format_epoch(ts as i64, true)
}

/// Local `HH:MM:SS` of any epoch second, however old — the stamp the dashboard
/// shows for a row read back out of the database. `stamp()` itself only ever
/// answers for the current second.
pub fn clock(ts: f64) -> String {
    format_epoch(ts as i64, false)
}

/// Where the local day `ts` falls in begins, as unix seconds. `mktime`
/// normalizes the fields it is handed — including `tm_isdst` — so the answer
/// follows the calendar the stamps are printed in, and a day the clocks moved
/// in still starts where a reader would say it did.
pub fn midnight(ts: f64) -> f64 {
    // SAFETY: `localtime_r` fills the caller-owned `tm`, which `mktime`
    // normalizes in place; neither touches a shared static.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = ts as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return ts;
        }
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        let start = libc::mktime(&mut tm);
        if start == -1 { ts } else { start as f64 }
    }
}

fn format_epoch(epoch: i64, date: bool) -> String {
    // SAFETY: `localtime_r` fills the caller-owned `tm`; it is the reentrant
    // form, so no shared static is touched.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = epoch as libc::time_t;
        if libc::localtime_r(&t, &mut tm).is_null() {
            return if date {
                "0000-00-00 00:00:00".to_string()
            } else {
                "00:00:00".to_string()
            };
        }
        tm
    };
    if date {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    } else {
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// `891`, `18.2K`, `1.2M`: a token count, short enough for a line that has to
/// fit beside everything else. The log line prints these whole — it exists to
/// be accounted from — while a dashboard column is read at a glance, and the
/// popup has the exact numbers.
pub fn human_count(count: u64) -> String {
    if count < 10_000 {
        return count.to_string();
    }
    let (scaled, unit) = if count < 1_000_000 {
        (count as f64 / 1_000.0, "K")
    } else {
        (count as f64 / 1_000_000.0, "M")
    };
    // 18.2K, but 182K: three digits before the point already say the scale.
    if scaled < 100.0 {
        format!("{scaled:.1}{unit}")
    } else {
        format!("{scaled:.0}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_starts_the_day_the_stamps_are_printed_in() {
        let ts = 1_790_008_865.0;
        let start = midnight(ts);
        assert!(start <= ts && ts - start < 86_400.0, "{start}");
        assert!(
            datetime(start).ends_with(" 00:00:00"),
            "{}",
            datetime(start)
        );
        assert_eq!(datetime(midnight(start)), datetime(start));
        assert_eq!(datetime(midnight(ts + 1.0)), datetime(start));
    }

    #[test]
    fn a_span_uses_the_largest_whole_unit() {
        assert_eq!(span(Duration::from_secs(604_800)), "7d");
        assert_eq!(span(Duration::from_secs(86_400)), "24h");
        assert_eq!(span(Duration::from_secs(5_400)), "90m");
        assert_eq!(span(Duration::from_secs(90)), "90s");
        assert_eq!(span(Duration::from_secs(3_600)), "1h");
    }

    /// Both read the same clock; only the date differs.
    #[test]
    fn the_clock_is_the_datetime_without_its_date() {
        let ts = 1_700_000_000.5;
        let datetime = datetime(ts);
        assert_eq!(clock(ts), datetime[datetime.len() - 8..]);
    }
}
