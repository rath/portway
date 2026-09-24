//! Without `--config`, which file the binary opens: the working directory's
//! `portway.toml` first, then `$XDG_CONFIG_HOME/portway/portway.toml` (or
//! `~/.config/portway/portway.toml`), then the built-in default. Each
//! candidate names a different loopback port; the daemon's `listening on`
//! line betrays which one was read.

const BIN: &str = env!("CARGO_BIN_EXE_portway");

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("portway-cfg-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Launch the daemon with no --config, cwd/HOME/XDG overridden, and return
/// the log's `listening on` line. No --port override: the winning config file
/// is the only thing that decides it.
fn listen_line(cwd: &Path, home: &Path, xdg: Option<&Path>) -> String {
    let data = cwd.join("data");
    let mut command = Command::new(BIN);
    command
        .arg("--daemon")
        .arg("--data-dir")
        .arg(&data)
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME");
    if let Some(xdg) = xdg {
        command.env("XDG_CONFIG_HOME", xdg);
    }
    let output = command.output().expect("the forwarder binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log = data.join("portway.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&log)
            && let Some(line) = text.lines().find(|line| line.contains("route(s) (stats:"))
        {
            return line.to_string();
        }
        if std::time::Instant::now() >= deadline {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            panic!("no banner line in {log:?}: {text}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop(dir: &Path) {
    let out = Command::new(BIN)
        .arg("--stop")
        .arg("--data-dir")
        .arg(dir)
        .output()
        .expect("stop runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn cleanup(dirs: &[&Path]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_working_directory_toml_wins_over_xdg() {
    let cwd = sandbox("cwd");
    let home = sandbox("home");
    let xdg = sandbox("xdg");
    let cwd_port = free_port();
    let xdg_port = free_port();
    write(
        &cwd.join("portway.toml"),
        &format!("port = {cwd_port}\nupstream = 'http://127.0.0.1:1'\n"),
    );
    write(
        &xdg.join("portway/portway.toml"),
        &format!("port = {xdg_port}\nupstream = 'http://127.0.0.1:1'\n"),
    );
    let line = listen_line(&cwd, &home, Some(&xdg));
    assert!(line.contains(&cwd_port.to_string()), "{line}");
    stop(&cwd.join("data"));
    cleanup(&[&cwd, &home, &xdg]);
}

#[test]
fn xdg_toml_is_found_when_the_working_directory_has_none() {
    let cwd = sandbox("cwd2");
    let home = sandbox("home2");
    let xdg = sandbox("xdg2");
    let xdg_port = free_port();
    write(
        &xdg.join("portway/portway.toml"),
        &format!("port = {xdg_port}\nupstream = 'http://127.0.0.1:1'\n"),
    );
    let line = listen_line(&cwd, &home, Some(&xdg));
    assert!(line.contains(&xdg_port.to_string()), "{line}");
    stop(&cwd.join("data"));
    cleanup(&[&cwd, &home, &xdg]);
}

#[test]
fn home_config_is_found_without_xdg() {
    let cwd = sandbox("cwd3");
    let home = sandbox("home3");
    let home_port = free_port();
    write(
        &home.join(".config/portway/portway.toml"),
        &format!("port = {home_port}\nupstream = 'http://127.0.0.1:1'\n"),
    );
    let line = listen_line(&cwd, &home, None);
    assert!(line.contains(&home_port.to_string()), "{line}");
    stop(&cwd.join("data"));
    cleanup(&[&cwd, &home]);
}

#[test]
fn neither_existing_is_a_clear_error_not_a_silent_default() {
    let cwd = sandbox("cwd4");
    let home = sandbox("home4");
    let data = cwd.join("data");
    let output = Command::new(BIN)
        .arg("--daemon")
        .arg("--data-dir")
        .arg(&data)
        .current_dir(&cwd)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("the forwarder binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("configure either upstream or [models]"),
        "{stderr}"
    );
    cleanup(&[&cwd, &home]);
}
