//! Opening the console in the default browser.
//!
//! Only when someone at a desktop started this by hand: the link was printed
//! to a terminal, the session is not over SSH (the browser would open on a
//! screen nobody there is looking at), and outside macOS there is a display
//! (`xdg-open` without one may start a text browser in this very terminal).
//! The browser is handed the one-time launch code, never the token: a
//! command line is readable by every local user.

use std::io::{self, IsTerminal};
use std::process::{Command, Stdio};

use crate::logfmt;

#[cfg(target_os = "macos")]
const OPENER: &str = "open";
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";

/// Why a run should leave the browser alone, if it should. `set` says whether
/// an environment variable is set and not empty.
pub fn unwanted(terminal: bool, set: impl Fn(&str) -> bool) -> Option<&'static str> {
    if !terminal {
        return Some("the link was not printed to a terminal");
    }
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return Some("this is an SSH session");
    }
    if cfg!(not(target_os = "macos")) && !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
        return Some("there is no display");
    }
    None
}

/// Open `url` if this run is one that should, and say why not to a terminal
/// that expected it. Never waits for the browser.
pub fn open(url: &str) {
    let terminal = io::stderr().is_terminal();
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    match unwanted(terminal, set) {
        None => spawn(url),
        Some(reason) if terminal => {
            logfmt::info(&format!(
                "not opening a browser: {reason}; use the link above"
            ));
        }
        Some(_) => {}
    }
}

fn spawn(url: &str) {
    let child = Command::new(OPENER)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => {
            logfmt::warn(&format!(
                "could not open a browser ({OPENER}: {err}); use the link above"
            ));
            return;
        }
    };
    // Reaped on a thread of its own, so a slow opener holds nothing up.
    let _ = std::thread::Builder::new()
        .name("browser".into())
        .spawn(move || {
            if let Ok(status) = child.wait()
                && !status.success()
            {
                logfmt::warn(&format!(
                    "could not open a browser ({OPENER} {status}); use the link above"
                ));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(vars: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| vars.contains(&name)
    }

    #[test]
    fn only_a_desktop_terminal_opens_a_browser() {
        let desktop: &[&str] = if cfg!(target_os = "macos") {
            &[]
        } else {
            &["DISPLAY"]
        };
        assert_eq!(unwanted(true, with(desktop)), None);
        assert!(
            unwanted(false, with(desktop)).is_some(),
            "piped or scripted"
        );
        assert!(unwanted(true, with(&["SSH_CONNECTION", "DISPLAY"])).is_some());
        assert!(unwanted(true, with(&["SSH_TTY", "DISPLAY"])).is_some());
        if cfg!(not(target_os = "macos")) {
            assert!(unwanted(true, with(&[])).is_some(), "no display");
            assert_eq!(unwanted(true, with(&["WAYLAND_DISPLAY"])), None);
        }
    }
}
