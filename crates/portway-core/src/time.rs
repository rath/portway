//! Timestamp helpers, without process-wide state.
pub fn stamp() -> String {
    clock(epoch())
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
