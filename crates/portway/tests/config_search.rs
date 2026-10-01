//! Without `--config`, which file the binary opens: the working directory's
//! `portway.toml` first, then `portway.toml` in the data directory (the one
//! `--data-dir` names, else `$XDG_CONFIG_HOME/portway` or
//! `~/.config/portway`), then the built-in default. Each candidate names a
//! different loopback port; the daemon's `listening on` line betrays which
//! one was read.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::free_port;

const BIN: &str = env!("CARGO_BIN_EXE_portway");

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

fn single(port: u16) -> String {
    format!("port = {port}\nupstream = 'http://127.0.0.1:1'\n")
}

/// The binary with cwd, HOME, and XDG_CONFIG_HOME overridden, so no test
/// reads or writes the real user's configuration.
fn portway(cwd: &Path, home: &Path, xdg: Option<&Path>) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME");
    if let Some(xdg) = xdg {
        command.env("XDG_CONFIG_HOME", xdg);
    }
    command
}

/// Start `command` as a daemon with no --config and no --port override, so
/// the winning config file alone decides the port, and return the log's
/// banner line. `data` is where the daemon keeps its log, whether `command`
/// names it with --data-dir or the daemon falls back to its default.
fn start(mut command: Command, data: &Path) -> String {
    let output = command
        .arg("--daemon")
        .output()
        .expect("the forwarder binary runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_line(&data.join("portway.log"), |line| {
        line.contains("route(s) (stats:")
    })
}

fn wait_for_line(log: &Path, wanted: impl Fn(&str) -> bool) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(log)
            && let Some(line) = text.lines().find(|line| wanted(line))
        {
            return line.to_string();
        }
        if std::time::Instant::now() >= deadline {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            panic!("no matching line in {log:?}: {text}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn control(operation: &str, dir: &Path) {
    let out = Command::new(BIN)
        .arg(operation)
        .arg("--data-dir")
        .arg(dir)
        .output()
        .expect("the control command runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn stop(dir: &Path) {
    control("--stop", dir);
}

fn cleanup(dirs: &[&Path]) {
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn the_working_directory_toml_wins_over_the_data_directory() {
    let cwd = sandbox("cwd");
    let home = sandbox("home");
    let data = cwd.join("data");
    let cwd_port = free_port();
    let data_port = free_port();
    write(&cwd.join("portway.toml"), &single(cwd_port));
    write(&data.join("portway.toml"), &single(data_port));
    let mut command = portway(&cwd, &home, None);
    command.arg("--data-dir").arg(&data);
    let line = start(command, &data);
    stop(&data);
    assert!(line.contains(&cwd_port.to_string()), "{line}");
    cleanup(&[&cwd, &home]);
}

#[test]
fn the_data_dir_toml_is_found_when_the_working_directory_has_none() {
    let cwd = sandbox("cwd2");
    let home = sandbox("home2");
    let xdg = sandbox("xdg2");
    let data = cwd.join("data");
    let data_port = free_port();
    let xdg_port = free_port();
    write(&data.join("portway.toml"), &single(data_port));
    write(&xdg.join("portway/portway.toml"), &single(xdg_port));
    let mut command = portway(&cwd, &home, Some(&xdg));
    command.arg("--data-dir").arg(&data);
    let line = start(command, &data);
    stop(&data);
    assert!(line.contains(&data_port.to_string()), "{line}");
    cleanup(&[&cwd, &home, &xdg]);
}

#[test]
fn an_explicit_data_dir_never_falls_back_to_the_default_one() {
    // A separate instance must not quietly take the default instance's routes.
    let cwd = sandbox("cwd3");
    let home = sandbox("home3");
    let xdg = sandbox("xdg3");
    write(&xdg.join("portway/portway.toml"), &single(free_port()));
    let output = portway(&cwd, &home, Some(&xdg))
        .arg("--daemon")
        .arg("--data-dir")
        .arg(cwd.join("data"))
        .output()
        .expect("the forwarder binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("configure an upstream, an [upstreams] table or a [models] table"),
        "{stderr}"
    );
    cleanup(&[&cwd, &home, &xdg]);
}

#[test]
fn xdg_toml_is_found_without_a_data_dir() {
    let cwd = sandbox("cwd4");
    let home = sandbox("home4");
    let xdg = sandbox("xdg4");
    let data = xdg.join("portway");
    let xdg_port = free_port();
    write(&data.join("portway.toml"), &single(xdg_port));
    let line = start(portway(&cwd, &home, Some(&xdg)), &data);
    stop(&data);
    assert!(line.contains(&xdg_port.to_string()), "{line}");
    cleanup(&[&cwd, &home, &xdg]);
}

#[test]
fn home_config_is_found_without_xdg() {
    let cwd = sandbox("cwd5");
    let home = sandbox("home5");
    let data = home.join(".config/portway");
    let home_port = free_port();
    write(&data.join("portway.toml"), &single(home_port));
    let line = start(portway(&cwd, &home, None), &data);
    stop(&data);
    assert!(line.contains(&home_port.to_string()), "{line}");
    cleanup(&[&cwd, &home]);
}

#[test]
fn neither_existing_is_a_clear_error_not_a_silent_default() {
    let cwd = sandbox("cwd6");
    let home = sandbox("home6");
    let output = portway(&cwd, &home, None)
        .arg("--daemon")
        .arg("--data-dir")
        .arg(cwd.join("data"))
        .output()
        .expect("the forwarder binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("configure an upstream, an [upstreams] table or a [models] table"),
        "{stderr}"
    );
    cleanup(&[&cwd, &home]);
}

#[test]
fn a_reload_rereads_the_file_a_relative_path_named_at_startup() {
    // The daemon moves to `/` before SIGHUP ever arrives, so each relative
    // path below must already have been resolved against the launch directory.
    let cases: [(&str, &[&str], &str); 3] = [
        ("explicit", &["--config", "./custom.toml"], "custom.toml"),
        ("cwd", &[], "portway.toml"),
        ("datadir", &[], "data/portway.toml"),
    ];
    for (name, args, file) in cases {
        let cwd = sandbox(&format!("reload-{name}"));
        let home = sandbox(&format!("reload-home-{name}"));
        let data = cwd.join("data");
        let port = free_port();
        write(&cwd.join(file), &single(port));
        let mut command = portway(&cwd, &home, None);
        command.args(args).args(["--data-dir", "data"]);
        let line = start(command, &data);
        assert!(line.contains(&port.to_string()), "{name}: {line}");
        // Same listener, two routes: only a reread of this file says so.
        write(
            &cwd.join(file),
            &format!(
                "port = {port}\n[models]\n\"a\" = 'http://127.0.0.1:1'\n\"b\" = 'http://127.0.0.1:1'\n"
            ),
        );
        control("--reload", &data);
        let reloaded = wait_for_line(&data.join("portway.log"), |line| {
            line.contains("configuration reloaded") || line.contains("keeping existing")
        });
        stop(&data);
        assert!(
            reloaded.contains("reloaded (2 route(s))"),
            "{name}: {reloaded}"
        );
        cleanup(&[&cwd, &home]);
    }
}
