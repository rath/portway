//! Exercise the real onboarding commands against disposable client homes.
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_portway");
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "portway-setup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn command(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.0)
            .env("HOME", &self.0)
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME");
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn write(&self, path: &str, text: &str) {
        let p = self.0.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.0.join(path)).unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn ok(o: Output) -> String {
    assert!(
        o.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap()
}
fn backups(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().contains(".bak-"))
        .collect()
}

#[test]
fn preview_is_read_only_and_does_not_require_a_running_server() {
    let h = Home::new();
    let out = ok(h.run(&["setup", "--client", "both", "--local"]));
    assert!(out.contains("Preview only"));
    assert!(out.contains("codex -p portway"));
    assert_eq!(fs::read_dir(&h.0).unwrap().count(), 0);
}

#[test]
fn apply_merges_backs_up_and_is_idempotent_without_touching_logins() {
    let h = Home::new();
    let codex =
        "# keep this comment\nmodel = 'my-model'\n[model_providers.other]\nname = 'other'\n";
    let claude = r#"{"env":{"OTHER":"keep","ANTHROPIC_API_KEY":"sentinel-secret"},"permissions":{"allow":["Read"]}}"#;
    h.write(".codex/portway.config.toml", codex);
    h.write(".codex/config.toml", "model = 'base-model'\n");
    h.write(".codex/auth.json", "do not read or modify");
    h.write(".claude/settings.json", claude);
    let args = [
        "setup",
        "--client",
        "both",
        "--url",
        "http://localhost:9999/",
        "--apply",
    ];
    let out = ok(h.run(&args));
    assert!(!out.contains("sentinel-secret"));
    let updated = h.read(".codex/portway.config.toml");
    assert!(updated.contains("# keep this comment"));
    let t: toml::Value = toml::from_str(&updated).unwrap();
    assert_eq!(t["model"].as_str(), Some("my-model"));
    assert_eq!(
        t["model_providers"]["other"]["name"].as_str(),
        Some("other")
    );
    assert_eq!(
        t["model_providers"]["portway"]["base_url"].as_str(),
        Some("http://localhost:9999/codex")
    );
    assert_eq!(
        t["model_providers"]["portway"]["supports_websockets"].as_bool(),
        Some(false)
    );
    let c: serde_json::Value = serde_json::from_str(&h.read(".claude/settings.json")).unwrap();
    assert_eq!(c["env"]["ANTHROPIC_API_KEY"], "sentinel-secret");
    assert_eq!(c["env"]["OTHER"], "keep");
    assert_eq!(c["permissions"]["allow"][0], "Read");
    assert_eq!(
        c["env"]["ANTHROPIC_BASE_URL"],
        "http://localhost:9999/anthropic"
    );
    assert_eq!(h.read(".codex/auth.json"), "do not read or modify");
    assert_eq!(h.read(".codex/config.toml"), "model = 'base-model'\n");
    for (dir, file, old) in [
        (".codex", "portway.config.toml", codex),
        (".claude", "settings.json", claude),
    ] {
        let b = backups(&h.0.join(dir));
        assert_eq!(b.len(), 1);
        assert_eq!(fs::read_to_string(&b[0]).unwrap(), old);
        for path in [&b[0], &h.0.join(dir).join(file)] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let out = ok(h.run(&args));
    assert!(!out.contains("Backup:"));
    assert_eq!(h.read(".codex/portway.config.toml"), updated);
    assert!(!h.0.join(".config").exists());
}

#[test]
fn custom_client_homes_are_respected() {
    let h = Home::new();
    ok(h.command()
        .env("CODEX_HOME", h.0.join("codex-alt"))
        .env("CLAUDE_CONFIG_DIR", h.0.join("claude-alt"))
        .args([
            "setup",
            "--client",
            "both",
            "--url",
            "https://proxy.example.com",
            "--apply",
        ])
        .output()
        .unwrap());
    assert!(h.0.join("codex-alt/portway.config.toml").exists());
    assert!(h.0.join("claude-alt/settings.json").exists());
    assert!(!h.0.join(".codex").exists());
}

#[test]
fn invalid_client_file_prevents_all_writes() {
    for text in ["not json", "{\"env\":42}", "[]"] {
        let h = Home::new();
        h.write(".claude/settings.json", text);
        assert!(
            !h.run(&["setup", "--client", "both", "--local", "--apply"])
                .status
                .success()
        );
        assert!(!h.0.join(".codex").exists());
        assert!(!h.0.join(".config").exists());
        assert_eq!(h.read(".claude/settings.json"), text);
        assert!(backups(&h.0.join(".claude")).is_empty());
    }
}

#[test]
fn conflicting_auth_and_invalid_tables_are_not_replaced() {
    for text in [
        "model = [",
        "model_providers = 42",
        "[model_providers]\nportway = 42",
        "[model_providers.portway]\nenv_key = 'PRIVATE_KEY'",
        "[model_providers.portway.auth]\ncommand = 'token-helper'",
    ] {
        let h = Home::new();
        h.write(".codex/portway.config.toml", text);
        assert!(
            !h.run(&[
                "setup",
                "--client",
                "codex",
                "--url",
                "http://localhost:8787",
                "--apply"
            ])
            .status
            .success()
        );
        assert_eq!(h.read(".codex/portway.config.toml"), text);
        assert!(backups(&h.0.join(".codex")).is_empty());
    }
}

#[test]
fn refuses_symlinks_and_non_files() {
    let h = Home::new();
    h.write("elsewhere", "untouched");
    fs::create_dir(h.0.join(".claude")).unwrap();
    std::os::unix::fs::symlink(h.0.join("elsewhere"), h.0.join(".claude/settings.json")).unwrap();
    assert!(
        !h.run(&[
            "setup",
            "--client",
            "claude",
            "--url",
            "http://localhost:8787",
            "--apply"
        ])
        .status
        .success()
    );
    assert_eq!(h.read("elsewhere"), "untouched");
}

#[test]
fn local_setup_uses_an_explicit_config_and_keeps_existing_routes() {
    let h = Home::new();
    let args = [
        "setup",
        "--client",
        "codex",
        "--local",
        "--port",
        "9876",
        "--data-dir",
        "local-data",
        "--apply",
    ];
    let out = ok(h.run(&args));
    assert!(out.contains("--config '") && out.contains("--data-dir '"));
    let config = h.read("local-data/portway.toml");
    let parsed: portway::config::Config = toml::from_str(&config).unwrap();
    parsed.validate(portway::cli::Mode::Forward).unwrap();
    assert_eq!(parsed.port, 9876);
    assert_eq!(parsed.host, "127.0.0.1");
    let with_prices = format!("{config}\n[prices.example]\ninput=1\noutput=2\ncache_read=0\n");
    h.write("local-data/portway.toml", &with_prices);
    ok(h.run(&args));
    assert_eq!(h.read("local-data/portway.toml"), with_prices);
    h.write(
        "local-data/portway.toml",
        "upstream='http://private.example.com'\n",
    );
    assert!(!h.run(&args).status.success());
    assert_eq!(
        h.read("local-data/portway.toml"),
        "upstream='http://private.example.com'\n"
    );
}

#[test]
fn bad_endpoints_and_mixed_operations_are_rejected() {
    let h = Home::new();
    for url in [
        "localhost:8787",
        "ftp://localhost",
        "http://u:p@localhost",
        "http://localhost/codex",
        "http://localhost/portway/",
        "http://localhost?key=x",
        "http://localhost/#token=x",
        "http://localhost:0",
        "http://localhost:99999",
    ] {
        assert!(
            !h.run(&["setup", "--client", "codex", "--url", url, "--apply"])
                .status
                .success(),
            "{url}"
        );
    }
    for args in [
        vec!["--daemon", "setup", "--client", "codex", "--local"],
        vec!["receive", "setup", "--client", "codex", "--local"],
        vec![
            "setup",
            "--client",
            "codex",
            "--local",
            "--url",
            "http://localhost",
        ],
        vec![
            "setup",
            "--client",
            "codex",
            "--url",
            "http://localhost",
            "--port",
            "99",
        ],
    ] {
        assert!(!h.run(&args).status.success(), "{args:?}");
    }
    assert_eq!(fs::read_dir(&h.0).unwrap().count(), 0);
}

fn serve(
    health: &'static str,
    stats: &'static str,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in [health, stats] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut b = [0; 1];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut b).unwrap();
                request.push(b[0]);
            }
            requests.push(String::from_utf8(request).unwrap());
            // Exercise normal HTTP framing, including chunked response bodies.
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",body.len()).unwrap();
        }
        requests
    });
    (url, thread)
}

#[test]
fn doctor_checks_the_listener_without_sending_credentials_or_inference() {
    let h = Home::new();
    let (url, server) = serve(
        r#"{"status":"ok","mode":"forward"}"#,
        r#"{"upstreams":{"codex":{},"anthropic":{}}}"#,
    );
    ok(h.run(&["setup", "--client", "both", "--url", &url, "--apply"]));
    let before = h.read(".codex/portway.config.toml");
    let out = ok(h.run(&["doctor", "--client", "both", "--url", &url]));
    assert!(out.contains("OK Codex") && out.contains("OK Claude") && out.contains("OK Portway"));
    assert_eq!(h.read(".codex/portway.config.toml"), before);
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /__portway/health "));
    assert!(requests[1].starts_with("GET /__portway/stats "));
    assert!(
        requests
            .iter()
            .all(|r| !r.to_lowercase().contains("authorization") && !r.contains("/responses"))
    );
}

#[test]
fn doctor_reports_missing_routes_bad_health_and_client_mismatch() {
    for (health, stats) in [
        (
            r#"{"status":"ok","mode":"receive"}"#,
            r#"{"upstreams":{"codex":{}}}"#,
        ),
        (r#"{"status":"ok","mode":"forward"}"#, r#"{"upstreams":{}}"#),
        ("not json", r#"{"upstreams":{}}"#),
    ] {
        let h = Home::new();
        let (url, server) = serve(health, stats);
        let out = h.run(&["doctor", "--client", "codex", "--url", &url]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("FAIL Codex"));
        server.join().unwrap();
        assert_eq!(fs::read_dir(&h.0).unwrap().count(), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_accepts_the_real_named_mount_router() {
    let h = Home::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = portway::router::Router::build(
        &portway_core::ForwarderConfig::default(),
        &[
            ("codex".into(), "http://127.0.0.1:1".into()),
            ("anthropic".into(), "http://127.0.0.1:1".into()),
        ],
        &[],
        None,
    )
    .unwrap();
    let task = tokio::spawn(portway::server::serve(listener, router));
    ok(h.run(&["setup", "--client", "both", "--url", &url, "--apply"]));
    let result = h.run(&["doctor", "--client", "both", "--url", &url]);
    task.abort();
    ok(result);
}

#[test]
fn doctor_rejects_invalid_provider_field_types() {
    for field in ["wire_api = 123", "supports_websockets = 'false'"] {
        let h = Home::new();
        let (url, server) = serve(
            r#"{"status":"ok","mode":"router"}"#,
            r#"{"upstreams":{"codex":{}}}"#,
        );
        h.write(".codex/portway.config.toml", &format!(
            "model_provider = 'portway'\n[model_providers.portway]\nbase_url = '{url}/codex'\nrequires_openai_auth = true\n{field}\n"
        ));
        let out = h.run(&["doctor", "--client", "codex", "--url", &url]);
        server.join().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("FAIL Codex"));
    }
}
