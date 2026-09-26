//! The `--tui` dashboard: terminal lifetime, input, and the redraw loop.
//!
//! Two dedicated OS threads, never the tokio runtime: crossterm's input poll
//! blocks, and nothing here may ever sit in front of a request. The only
//! contact with the forwarder is a `Receiver<Event>` and read-only samples of
//! the per-model counters and flight registry. The registry lock is held only
//! while cloning its entries; rendering never holds it.
//!
//! Input gets a thread of its own because crossterm's reader **cannot be
//! trusted to return**: on a hung-up tty its `read()` comes back `Ok(0)`
//! forever and its internal loop has no exit for that, so `poll` never
//! returns. Joining that reader can prevent shutdown after a terminal closes.
//! The render loop therefore owns the clock, and the reader is never joined —
//! a draw onto the dead terminal fails with EIO, which is what ends the loop.

pub mod chart;
pub use crate::spend;
pub mod state;
pub mod view;

use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;

use crate::router::Router;
use crate::telemetry::Event;
use crate::tui::state::{COLUMNS, Column, Columns, Filter, State};
use crate::tui::view::Header;
use crate::watch;

/// The longest the render loop sleeps, so `stop` is always noticed promptly.
/// Input does not wait for it: a keystroke wakes the loop immediately.
const NAP: Duration = Duration::from_millis(100);
/// How often the counters are resampled and the clock advances.
const TICK: Duration = Duration::from_millis(250);
/// Both dashboard threads share this prefix; the panic hook keys off it.
const THREAD_PREFIX: &str = "tui";
const RENDER_THREAD: &str = "tui";
const INPUT_THREAD: &str = "tui-input";

type Screen = Terminal<CrosstermBackend<Stdout>>;

/// Restores the terminal exactly once, from whichever end gets there first —
/// a clean exit, a panic in the dashboard, or a signal.
struct TerminalGuard(Screen);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Where the 250ms sample comes from: the counters this process keeps in the
/// request path, or the window of a forwarder it is watching instead.
pub enum Feed {
    Live(Arc<Router>),
    Recorded {
        window: Arc<Mutex<watch::Window>>,
        live: tokio::sync::watch::Receiver<Option<Arc<crate::live::Snapshot>>>,
    },
}

/// The file a session leaves its dashboard settings in, beside the database.
pub const SETTINGS_FILE: &str = "portway.tui";

/// What the dashboard starts with, and where a change is written back so the
/// next one starts the same way. `--event-columns` overrides the file for one
/// run without rewriting it: a flag is an argument, not a decision.
pub struct Settings {
    pub prices: crate::config::Prices,
    pub columns: Columns,
    file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            prices: Default::default(),
            columns: Columns::ALL,
            file: None,
        }
    }
}

impl Settings {
    /// The flag if it was given, else what the last session left, else every
    /// column. A settings file that cannot be read is not worth stopping a
    /// dashboard for — the default is what it would have said anyway.
    pub fn load(dir: &Path, asked: Option<&[Column]>) -> Settings {
        let file = dir.join(SETTINGS_FILE);
        if let Some(asked) = asked {
            let mut columns = Columns::parse("").expect("an empty list parses");
            for column in asked {
                columns.toggle(*column);
            }
            return Settings {
                prices: Default::default(),
                columns,
                file: Some(file),
            };
        }
        let columns = fs::read_to_string(&file)
            .ok()
            .and_then(|text| stored_columns(&text))
            .unwrap_or(Columns::ALL);
        Settings {
            prices: Default::default(),
            columns,
            file: Some(file),
        }
    }

    /// Remember `columns` for the next session. Best effort: a dashboard that
    /// cannot write its settings still draws.
    fn remember(&self, columns: Columns) {
        let Some(file) = &self.file else {
            return;
        };
        if let Err(err) = fs::write(file, format!("columns={}\n", columns.names())) {
            crate::logfmt::warn(&format!("{}: {err}", file.display()));
        }
    }
}

/// The `columns=` line of a settings file, if it has one.
fn stored_columns(text: &str) -> Option<Columns> {
    let value = text
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("columns="))?;
    Columns::parse(value).ok()
}

pub struct Handle {
    thread: JoinHandle<()>,
    stop: Arc<AtomicBool>,
}

impl Handle {
    /// Ask the dashboard to stop and wait for it, which is what puts the
    /// terminal back. Only the render thread is joined, and it wakes at least
    /// every `NAP`; the reader may be stuck in crossterm forever and is left
    /// to die with the process.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Take over the terminal and start drawing. The returned receiver fires when
/// the user quits; the caller then drives `Handle::shutdown`.
pub fn start(
    feed: Feed,
    events: Receiver<Event>,
    header: Header,
    settings: Settings,
    db: Option<PathBuf>,
) -> io::Result<(Handle, tokio::sync::oneshot::Receiver<()>)> {
    install_panic_hook();
    let terminal = enter()?;
    let (quit_tx, quit_rx) = tokio::sync::oneshot::channel();
    let stop = Arc::new(AtomicBool::new(false));

    let (keys_tx, keys) = std::sync::mpsc::channel();
    let reader_stop = Arc::clone(&stop);
    // Detached on purpose: see the module header. Dropping its handle is what
    // says so.
    std::thread::Builder::new()
        .name(INPUT_THREAD.to_string())
        .spawn(move || read_input(&keys_tx, &reader_stop))?;

    let render_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(RENDER_THREAD.to_string())
        .spawn(move || {
            let mut guard = TerminalGuard(terminal);
            if let Err(err) = run(
                &mut guard.0,
                &feed,
                &events,
                &keys,
                &header,
                &settings,
                &render_stop,
                db.as_deref(),
            ) {
                drop(guard); // put the terminal back before saying anything
                crate::logfmt::error(&format!("tui: {err}"));
            }
            let _ = quit_tx.send(());
        })?;
    Ok((Handle { thread, stop }, quit_rx))
}

/// Blocks on the terminal and forwards what it sees. Ends when the render loop
/// drops the receiver, or when `stop` is set and crossterm hands control back —
/// which, on a dead tty, it never does.
fn read_input(keys: &Sender<TermEvent>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match event::poll(NAP) {
            Ok(false) => continue,
            Ok(true) => match event::read() {
                Ok(event) => {
                    if keys.send(event).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            Err(_) => return,
        }
    }
}

fn enter() -> io::Result<Screen> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(err) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        restore();
        return Err(err);
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(err) => {
            restore();
            Err(err)
        }
    }
}

/// Idempotent, and deliberately ignores errors: it runs from a panic hook too,
/// where there is nobody left to tell.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

/// `ratatui::init`'s hook would restore the terminal on *any* thread's panic
/// and then exit. This crate keeps `panic = unwind` on purpose so one bad
/// upstream response cannot take down the other streams, so a panic somewhere
/// in the request path must leave the dashboard running — it goes to the event
/// pane instead of tearing the screen with a stderr write.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let in_dashboard = std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with(THREAD_PREFIX));
            if in_dashboard {
                restore();
                previous(info);
            } else {
                crate::logfmt::error(&format!("panic: {info}"));
            }
        }));
    });
}

#[allow(clippy::too_many_arguments)]
fn run(
    terminal: &mut Screen,
    feed: &Feed,
    events: &Receiver<Event>,
    keys: &Receiver<TermEvent>,
    header: &Header,
    settings: &Settings,
    stop: &AtomicBool,
    db: Option<&Path>,
) -> io::Result<()> {
    let mut state = State::new();
    state.prices = settings.prices.clone();
    state.columns = settings.columns;
    state.db = db.map(Path::to_path_buf);
    state.recorded = matches!(feed, Feed::Recorded { .. });
    // Whatever is already queued — a watching dashboard starts with an hour of
    // it — is on the first frame rather than the one after it.
    for event in events.try_iter() {
        state.push(event);
    }
    tick(&mut state, feed);
    let mut last_tick = Instant::now();
    let mut dirty = true;

    while !stop.load(Ordering::Relaxed) {
        for event in events.try_iter() {
            state.push(event);
            dirty = true;
        }
        if last_tick.elapsed() >= TICK {
            tick(&mut state, feed);
            last_tick = Instant::now();
            dirty = true;
        }
        if dirty {
            let size = terminal.size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            state.viewport = view::events_height(area, state.models.len(), state.flights.len());
            // The write that notices a terminal that went away.
            terminal.draw(|frame| view::draw(frame, &state, header))?;
            dirty = false;
        }

        // Sleep until the next tick, but no longer than `NAP`, and wake at
        // once for a keystroke.
        let nap = TICK.saturating_sub(last_tick.elapsed()).min(NAP);
        match keys.recv_timeout(nap) {
            Ok(event) => {
                if apply(event, &mut state, settings) {
                    return Ok(());
                }
                dirty = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The reader is gone, so there is no input left to wait for; keep
            // drawing until a failed draw or `stop` ends the loop.
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(nap),
        }
    }
    Ok(())
}

/// One sample of whichever counters this dashboard is drawing.
fn tick(state: &mut State, feed: &Feed) {
    match feed {
        Feed::Live(router) => state.tick(router),
        Feed::Recorded { window, live } => {
            state.tick_recorded(&window.lock().unwrap());
            let snapshot = live.borrow().clone();
            state.apply_live(snapshot.as_deref());
        }
    }
    state.refresh_usage();
}

/// Returns true when the user asked to quit.
fn apply(event: TermEvent, state: &mut State, settings: &Settings) -> bool {
    match event {
        TermEvent::Key(key) => return key_press(key, state, settings),
        TermEvent::Mouse(mouse) => {
            // The wheel scrolls the event pane, which the usage screen covers:
            // moving a cursor nobody can see leaves the pane somewhere else
            // when the dashboard comes back.
            if state.usage_open {
                return false;
            }
            match mouse.kind {
                MouseEventKind::ScrollUp => state.scroll(-3),
                MouseEventKind::ScrollDown => state.scroll(3),
                _ => {}
            }
        }
        // Resize and focus changes just need a redraw.
        _ => {}
    }
    false
}

/// Returns true when the user asked to quit.
fn key_press(key: KeyEvent, state: &mut State, settings: &Settings) -> bool {
    // Windows sends a Release for every Press; everywhere else this is a no-op.
    if key.kind != KeyEventKind::Press {
        return false;
    }
    // Ctrl-C is the escape hatch it is everywhere else: raw mode means no
    // SIGINT, so it is handled here and never confirms.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    // The picker has the keyboard while it is open: `q` closes it rather than
    // quitting, and a toggle is written down as it happens.
    if state.picker {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                state.picker_at = (state.picker_at + 1).min(COLUMNS.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.picker_at = state.picker_at.saturating_sub(1);
            }
            KeyCode::Char(' ') | KeyCode::Char('x') | KeyCode::Enter => {
                state.columns.toggle(COLUMNS[state.picker_at]);
                settings.remember(state.columns);
            }
            KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q') => state.picker = false,
            _ => {}
        }
        return false;
    }

    // While the usage screen is up it holds the keyboard, the way the picker
    // does: `u`, `esc` and `q` put the dashboard back, the arrows move its
    // window, and no other keystroke means something underneath a screen that
    // is covering it.
    if state.usage_open {
        // The costs popup sits over the screen the way the picker does: it takes
        // the keyboard while it is up, `p` and `esc` put it away, and `u` still
        // means "back to the dashboard" — it leaves and takes the popup along.
        if state.usage_rates {
            match key.code {
                KeyCode::Char('p') | KeyCode::Esc | KeyCode::Char('q') => state.usage_rates = false,
                KeyCode::Char('u') => state.close_usage(),
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('u') | KeyCode::Esc | KeyCode::Char('q') => state.close_usage(),
            KeyCode::Left | KeyCode::Char('h') => state.step_usage_range(-1),
            KeyCode::Right | KeyCode::Char('l') => state.step_usage_range(1),
            KeyCode::Char('p') => state.usage_rates = true,
            _ => {}
        }
        return false;
    }

    let confirming = state.confirm_quit;
    state.confirm_quit = false;

    match key.code {
        KeyCode::Char('q') => {
            // An attached viewer owns no relays and can always leave.
            if state.recorded || confirming || state.totals.in_flight == 0 {
                return true;
            }
            state.confirm_quit = true;
        }
        KeyCode::Char('j') | KeyCode::Down => state.scroll(1),
        KeyCode::Char('k') | KeyCode::Up => state.scroll(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => state.page(1),
        KeyCode::PageUp => state.page(-1),
        KeyCode::Char('g') | KeyCode::Home => state.to_oldest(),
        KeyCode::Char('G') | KeyCode::End => state.to_newest(),
        // A log line has no record to show, so Enter stays a no-op there
        // rather than arming a popup that appears on the next scroll.
        KeyCode::Enter => state.detail = !state.detail && state.selected().is_some(),
        KeyCode::Char('e') => {
            let next = if state.filter == Filter::Trouble {
                Filter::All
            } else {
                Filter::Trouble
            };
            state.set_filter(next);
        }
        KeyCode::Char('m') => state.cycle_filter(),
        KeyCode::Char('u') => {
            state.open_usage();
            state.help = false;
            state.picker = false;
            state.detail = false;
        }
        KeyCode::Char('t') => state.cycle_scale(),
        KeyCode::Char('c') => {
            state.picker = true;
            state.picker_at = 0;
            state.help = false;
            state.detail = false;
        }
        KeyCode::Char('?') => {
            state.help = !state.help;
            state.picker = false;
            state.detail = false;
        }
        KeyCode::Esc => {
            if state.help || state.detail {
                state.help = false;
                state.detail = false;
            } else {
                state.to_newest();
            }
        }
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logfmt::Level;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A key press with nowhere to write settings: what a toggle does to the
    /// line is the test's business, what a file must remember is not.
    fn key(code: KeyCode, state: &mut State) -> bool {
        key_press(press(code), state, &Settings::default())
    }

    fn noisy() -> State {
        let mut state = State::new();
        state.viewport = 3;
        for index in 0..10 {
            state.push(Event::Log {
                stamp: "00:00:00".to_string(),
                level: if index == 4 {
                    Level::Error
                } else {
                    Level::Info
                },
                message: format!("line {index}"),
            });
        }
        state
    }

    #[test]
    fn quitting_confirms_only_while_a_stream_is_live() {
        let mut state = State::new();
        assert!(key(KeyCode::Char('q'), &mut state));

        state.totals.in_flight = 1;
        assert!(!key(KeyCode::Char('q'), &mut state));
        assert!(state.confirm_quit);
        assert!(key(KeyCode::Char('q'), &mut state));

        // Any other key takes the confirmation back.
        state.confirm_quit = true;
        assert!(!key(KeyCode::Char('j'), &mut state));
        assert!(!state.confirm_quit);
        // Ctrl-C never waits for a confirmation.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(key_press(ctrl_c, &mut state, &Settings::default()));

        state.recorded = true;
        state.totals.in_flight = 200;
        assert!(key(KeyCode::Char('q'), &mut state));
        assert!(!state.confirm_quit);
    }

    #[test]
    fn scrolling_leaves_follow_and_g_brings_it_back() {
        let mut state = noisy();
        assert!(state.follow);

        key(KeyCode::Up, &mut state);
        assert!(!state.follow);
        assert_eq!(state.below(), 1, "one line slid under the viewport");

        key(KeyCode::PageUp, &mut state);
        assert_eq!(state.below(), 2);

        key(KeyCode::Char('G'), &mut state);
        assert!(state.follow);
        assert_eq!(state.below(), 0);

        // Ten lines, three rows: seven are hidden below the oldest screen.
        key(KeyCode::Char('g'), &mut state);
        assert!(!state.follow);
        assert_eq!(state.below(), 7);
    }

    #[test]
    fn the_trouble_filter_toggles_and_the_popups_close_on_esc() {
        let mut state = noisy();
        key(KeyCode::Char('e'), &mut state);
        assert_eq!(state.filter, Filter::Trouble);
        assert_eq!(state.len(), 1, "only the ERROR line survives");
        key(KeyCode::Char('e'), &mut state);
        assert_eq!(state.filter, Filter::All);
        assert_eq!(state.len(), 10);

        key(KeyCode::Char('?'), &mut state);
        assert!(state.help);
        key(KeyCode::Esc, &mut state);
        assert!(!state.help);
    }

    /// A data dir holding one recorded answer, for the screens that read the file
    /// rather than the counters.
    fn seeded(name: &str) -> std::path::PathBuf {
        use crate::forwarder::Coding;
        use crate::telemetry::RequestRecord;
        use crate::usage::Usage;

        let dir = std::env::temp_dir().join(format!("portway-tui-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = crate::store::spawn(&dir, 0).unwrap();
        store
            .sender()
            .send(Event::Request(Arc::new(RequestRecord {
                stamp: "12:00:00".to_string(),
                model: "model-epsilon".to_string(),
                method: http::Method::POST,
                path: "/v1/chat/completions".to_string(),
                status: 200,
                dns: None,
                tcp: None,
                tls: None,
                body_len: 1024,
                wire_len: 1024,
                coding: Coding::None,
                upload: None,
                ttfb: 0.5,
                received: 512,
                received_wire: 512,
                received_agent: 512,
                upstream_encoding: "identity".to_string(),
                agent_encoding: None,
                download: None,
                complete: true,
                usage: Some(Usage {
                    prompt: 3_000,
                    cached: Some(2_700),
                    completion: 300,
                    reasoning: Some(150),
                }),
                flight: None,
            })))
            .unwrap();
        store.shutdown();
        dir
    }

    /// The usage screen is the one reading that does not come from the counters:
    /// it goes to the database, so this is the seam worth holding.
    #[test]
    fn u_reads_the_day_out_of_the_database() {
        let dir = seeded("usage");
        let mut state = State::new();
        state.db = Some(dir.join(crate::store::DB_FILE));
        assert!(!key(KeyCode::Char('u'), &mut state));
        assert!(state.usage_open);
        let table = state.usage.as_ref().expect("the day was read");
        assert_eq!(table.rows.len(), 1, "{:?}", table.rows);
        assert_eq!(table.total.prompt, 3_000);
        assert_eq!(table.total.cached, 2_700);
        assert_eq!(table.total.completion, 300);

        // `esc` puts the dashboard back and keeps the read, so a second look
        // does not wait on the database again.
        assert!(!key(KeyCode::Esc, &mut state));
        assert!(!state.usage_open);
        assert!(state.usage.is_some());

        // While it is up it holds the keyboard: `q` closes it rather than
        // leaving, and only Ctrl-C quits from anywhere.
        assert!(!key(KeyCode::Char('u'), &mut state));
        assert!(state.usage_open);
        assert!(!key(KeyCode::Char('q'), &mut state), "the screen closed");
        assert!(!state.usage_open);
        // And with it down, nothing is streaming, so `q` is the quit it was.
        assert!(key(KeyCode::Char('q'), &mut state));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The window an arrow key picks is a different read of the file, not the
    /// same numbers under another heading.
    #[test]
    fn an_arrow_moves_the_window_and_reads_it_again() {
        let dir = seeded("usage-range");
        let mut state = State::new();
        state.db = Some(dir.join(crate::store::DB_FILE));
        key(KeyCode::Char('u'), &mut state);
        assert_eq!(state.usage_range, spend::Range::Today);
        let today = state.usage.as_ref().expect("read").clone();
        assert_eq!(today.rows.len(), 1, "the answer was written now");

        // Nothing has been recorded yesterday, so the same file answers with
        // nothing — and the window itself moved to cover that day.
        key(KeyCode::Right, &mut state);
        assert_eq!(state.usage_range, spend::Range::Yesterday);
        let yesterday = state.usage.as_ref().expect("read again");
        assert!(yesterday.is_empty(), "{:?}", yesterday.rows);
        assert!(
            yesterday.until <= today.since,
            "yesterday ends where today began"
        );
        assert_eq!(yesterday.cost(), None, "nothing to price");

        // `h` walks back the way `→` came, and the day is there again.
        key(KeyCode::Char('h'), &mut state);
        assert_eq!(state.usage_range, spend::Range::Today);
        assert_eq!(state.usage.as_ref().unwrap().total.prompt, 3_000);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `p` is a question about the screen it covers: it takes the keyboard while
    /// it is up, and leaving the screen leaves it behind.
    #[test]
    fn p_lays_the_cost_breakdown_over_the_window() {
        let dir = seeded("usage-rates");
        let mut state = State::new();
        state.db = Some(dir.join(crate::store::DB_FILE));
        key(KeyCode::Char('u'), &mut state);
        assert!(!state.usage_rates);

        key(KeyCode::Char('p'), &mut state);
        assert!(state.usage_rates);
        // An arrow does not move the window underneath it.
        key(KeyCode::Right, &mut state);
        assert_eq!(state.usage_range, spend::Range::Today);

        key(KeyCode::Char('p'), &mut state);
        assert!(!state.usage_rates);

        // `esc` closes the innermost thing first: the popup goes and the
        // screen stays.
        key(KeyCode::Char('p'), &mut state);
        key(KeyCode::Esc, &mut state);
        assert!(!state.usage_rates);
        assert!(state.usage_open, "the screen is still there");

        // Leaving the screen takes the popup with it.
        key(KeyCode::Char('p'), &mut state);
        key(KeyCode::Char('u'), &mut state);
        assert!(!state.usage_open);
        assert!(!state.usage_rates, "closed with the screen it was asked of");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bucket_scale_cycles() {
        let mut state = State::new();
        assert_eq!(state.scale, 1);
        key(KeyCode::Char('t'), &mut state);
        assert_eq!(state.scale, 10);
        key(KeyCode::Char('t'), &mut state);
        assert_eq!(state.scale, 60);
        key(KeyCode::Char('t'), &mut state);
        assert_eq!(state.scale, 1);
    }

    /// `c` opens the picker, the cursor walks the columns, a toggle lands in
    /// the state and in the file, and none of it quits the dashboard.
    #[test]
    fn the_picker_toggles_columns_and_the_next_run_remembers() {
        let dir = std::env::temp_dir().join("portway-tui-test-columns");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let settings = Settings {
            prices: Default::default(),
            columns: Columns::ALL,
            file: Some(dir.join(SETTINGS_FILE)),
        };
        let mut state = State::new();
        state.columns = settings.columns;

        assert!(!key_press(press(KeyCode::Char('c')), &mut state, &settings));
        assert!(state.picker);
        assert_eq!(state.picker_at, 0);

        for _ in 0..4 {
            assert!(!key_press(press(KeyCode::Char('j')), &mut state, &settings));
        }
        assert_eq!(COLUMNS[state.picker_at], Column::Route);
        assert!(!key_press(press(KeyCode::Char(' ')), &mut state, &settings));
        assert!(!state.columns.contains(Column::Route));

        // The cursor stops on the last column instead of running off the list.
        for _ in 0..20 {
            key_press(press(KeyCode::Char('j')), &mut state, &settings);
        }
        assert_eq!(state.picker_at, COLUMNS.len() - 1);
        assert!(state.columns.contains(Column::Tokens), "not toggled yet");
        assert!(!key_press(press(KeyCode::Char('x')), &mut state, &settings));
        assert_eq!(
            state.columns.names(),
            "time,status,cut,model,sizes,ttfb,down"
        );

        // `q` closes the picker rather than quitting, and what it changed was
        // written down as it happened.
        assert!(!key_press(press(KeyCode::Char('q')), &mut state, &settings));
        assert!(!state.picker);
        assert_eq!(Settings::load(&dir, None).columns, state.columns);

        // The flag overrides the file for one run and leaves it alone.
        let asked = Settings::load(&dir, Some(&[Column::Time, Column::Tokens]));
        assert!(asked.columns.contains(Column::Tokens));
        assert!(!asked.columns.contains(Column::Route));
        assert_eq!(Settings::load(&dir, None).columns, state.columns);

        // A file that says something unreadable falls back, it does not stop
        // the dashboard.
        fs::write(dir.join(SETTINGS_FILE), "columns=nope\n").unwrap();
        assert_eq!(Settings::load(&dir, None).columns, Columns::ALL);
        let _ = fs::remove_dir_all(&dir);
    }
}
