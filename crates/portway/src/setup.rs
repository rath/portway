//! Client onboarding: preview before applying, preserve unrelated settings, and
//! probe only Portway's management endpoints (never a paid inference request).
use clap::{Args, Subcommand, ValueEnum};
use http_body_util::{BodyExt, Limited};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use toml_edit::{DocumentMut, Item, Table, TableLike, value};

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Preview or apply client settings for an existing or new local Portway.
    Setup(SetupArgs),
    /// Check client settings and Portway's listener without changing files.
    Doctor(DoctorArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Client {
    Codex,
    Claude,
    Both,
}
impl Client {
    fn codex(self) -> bool {
        self != Self::Claude
    }
    fn claude(self) -> bool {
        self != Self::Codex
    }
}

#[derive(Debug, Clone, Args)]
pub struct SetupArgs {
    /// Client whose connection settings to configure; existing logins are kept.
    #[arg(long, value_enum)]
    pub client: Client,
    /// Existing Portway API listener, e.g. http://localhost:8787 (no mount path).
    #[arg(long, required_unless_present = "local", conflicts_with = "local")]
    pub url: Option<String>,
    /// Prepare a local forwarder with direct Anthropic and ChatGPT upstreams.
    #[arg(long)]
    pub local: bool,
    /// Local listener port (default: 8787).
    #[arg(long, requires = "local", value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// Local Portway data directory; defaults to the normal user data directory.
    #[arg(long, requires = "local")]
    pub data_dir: Option<PathBuf>,
    /// Apply the previewed changes, backing up each existing file first.
    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Clone, Args)]
pub struct DoctorArgs {
    #[arg(long, value_enum)]
    pub client: Client,
    /// Expected Portway API listener, without /codex or /anthropic.
    #[arg(long)]
    pub url: String,
}

pub fn run(command: &Command) -> Result<(), String> {
    match command {
        Command::Setup(args) => setup(args),
        Command::Doctor(args) => doctor(args),
    }
}

fn endpoint(raw: &str) -> Result<String, String> {
    let uri: http::Uri = raw.parse().map_err(|_| "invalid Portway URL")?;
    let upstream = crate::pool::Upstream::new(raw, None)?;
    if uri.path() != "/" && !uri.path().is_empty() {
        return Err("use the Portway API listener URL without a path (not /codex, /anthropic, /v1, or the web console)".into());
    }
    let authority = uri.authority().ok_or("Portway URL needs a host")?.as_str();
    let suffix = authority
        .strip_prefix(uri.host().unwrap_or(""))
        .unwrap_or("");
    if !suffix.is_empty()
        && suffix
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .is_none_or(|p| p == 0)
    {
        return Err("the Portway URL needs a port between 1 and 65535".into());
    }
    Ok(upstream.base)
}

fn client_dir(variable: &str, fallback: &str) -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(variable).filter(|s| !s.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(|home| PathBuf::from(home).join(fallback))
        .ok_or_else(|| format!("set HOME or {variable} to locate client settings"))
}

fn codex_path() -> Result<PathBuf, String> {
    Ok(client_dir("CODEX_HOME", ".codex")?.join("portway.config.toml"))
}
fn claude_path() -> Result<PathBuf, String> {
    Ok(client_dir("CLAUDE_CONFIG_DIR", ".claude")?.join("settings.json"))
}

const MAX_CONFIG: u64 = 1024 * 1024;
fn read(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
        Ok(meta) if !meta.is_file() => {
            return Err(format!(
                "{} is not a regular file; edit it manually",
                path.display()
            ));
        }
        Ok(_) => (),
    }
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|f| f.take(MAX_CONFIG + 1).read_to_string(&mut text))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() as u64 > MAX_CONFIG {
        return Err(format!("{} exceeds 1 MiB", path.display()));
    }
    Ok(Some(text))
}
fn document(text: Option<&str>, path: &Path) -> Result<DocumentMut, String> {
    text.unwrap_or("")
        .parse()
        .map_err(|_| format!("{}: invalid TOML; fix it before setup", path.display()))
}
fn table<'a>(parent: &'a mut dyn TableLike, key: &str) -> Result<&'a mut dyn TableLike, String> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| format!("{key} must be a table"))
}
fn codex_settings(old: Option<&str>, path: &Path, url: &str) -> Result<String, String> {
    let mut doc = document(old, path)?;
    doc["model_provider"] = value("portway");
    let providers = table(doc.as_table_mut(), "model_providers")?;
    let provider = table(providers, "portway")?;
    for key in ["env_key", "experimental_bearer_token", "auth"] {
        if provider.contains_key(key) {
            return Err(format!(
                "{}: portway provider already defines {key}; this setup uses ChatGPT login, so resolve that authentication setting first",
                path.display()
            ));
        }
    }
    for (key, val) in [
        ("name", value("portway")),
        ("base_url", value(format!("{url}/codex"))),
        ("requires_openai_auth", value(true)),
        ("wire_api", value("responses")),
        ("supports_websockets", value(false)),
    ] {
        provider.insert(key, val);
    }
    Ok(doc.to_string())
}
fn claude_settings(old: Option<&str>, path: &Path, url: &str) -> Result<String, String> {
    let mut doc: Value = serde_json::from_str(old.unwrap_or("{}"))
        .map_err(|_| format!("{}: invalid JSON; fix it before setup", path.display()))?;
    let root = doc
        .as_object_mut()
        .ok_or("Claude settings must be a JSON object")?;
    let env = root
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("Claude settings.env must be a JSON object")?;
    let desired = Value::String(format!("{url}/anthropic"));
    if env.get("ANTHROPIC_BASE_URL") == Some(&desired) {
        return Ok(old.unwrap().to_string());
    }
    env.insert("ANTHROPIC_BASE_URL".into(), desired);
    Ok(format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?
    ))
}

struct Edit {
    path: PathBuf,
    old: Option<String>,
    new: String,
    description: String,
}
impl Edit {
    fn client(path: PathBuf, url: &str, codex: bool) -> Result<Self, String> {
        let old = read(&path)?;
        let new = if codex {
            codex_settings(old.as_deref(), &path, url)?
        } else {
            claude_settings(old.as_deref(), &path, url)?
        };
        Ok(Self {
            path,
            old,
            new,
            description: if codex {
                format!("Codex profile: {url}/codex, existing ChatGPT login, HTTP Responses")
            } else {
                format!("Claude env.ANTHROPIC_BASE_URL: {url}/anthropic (existing login/key kept)")
            },
        })
    }
    fn changed(&self) -> bool {
        self.old.as_deref() != Some(self.new.as_str())
    }
}

fn local_config(args: &SetupArgs) -> Result<Edit, String> {
    let dir = crate::store::data_dir(args.data_dir.as_deref())?;
    let path = std::path::absolute(dir.join("portway.toml")).map_err(|e| e.to_string())?;
    let old = read(&path)?;
    let new = format!(
        "host = \"127.0.0.1\"\nport = {}\n\n[upstreams]\nanthropic = \"https://api.anthropic.com\"\ncodex = \"https://chatgpt.com/backend-api/codex\"\n",
        args.port.unwrap_or(8787)
    );
    if let Some(text) = &old {
        let config: crate::config::Config = toml::from_str(text)
            .map_err(|_| format!("{}: invalid Portway configuration", path.display()))?;
        config.validate(crate::cli::Mode::Forward)?;
        if config.host != "127.0.0.1"
            || config.port != args.port.unwrap_or(8787)
            || config.upstream.is_some()
            || config.upstreams.get("anthropic").map(String::as_str)
                != Some("https://api.anthropic.com")
            || config.upstreams.get("codex").map(String::as_str)
                != Some("https://chatgpt.com/backend-api/codex")
        {
            return Err(format!(
                "{} already has a different listener or routes; use --url to connect to that instance, or --local --data-dir with a separate directory",
                path.display()
            ));
        }
    }
    Ok(Edit {
        path,
        new: old.clone().unwrap_or(new),
        old,
        description:
            "Local forwarder with direct vendor upstreams (no delta compression without a receiver)"
                .into(),
    })
}

fn setup(args: &SetupArgs) -> Result<(), String> {
    if !args.local && args.url.is_none() {
        return Err("choose --local or --url <Portway API listener>".into());
    }
    if !args.local && (args.port.is_some() || args.data_dir.is_some()) {
        return Err("--port and --data-dir require --local".into());
    }
    let url = endpoint(
        &args
            .url
            .clone()
            .unwrap_or_else(|| format!("http://127.0.0.1:{}", args.port.unwrap_or(8787))),
    )?;
    // Plan every file before writing any of them. Malformed settings in either
    // client must not leave half of an otherwise predictable setup applied.
    let mut edits = Vec::new();
    if args.local {
        edits.push(local_config(args)?);
    }
    if args.client.codex() {
        edits.push(Edit::client(codex_path()?, &url, true)?);
    }
    if args.client.claude() {
        edits.push(Edit::client(claude_path()?, &url, false)?);
    }
    println!(
        "{}",
        if args.apply {
            "Applying Portway setup"
        } else {
            "Preview only; add --apply to write these changes"
        }
    );
    for edit in &edits {
        println!(
            "{} {}\n  {}",
            if !edit.changed() {
                "Keep"
            } else if edit.old.is_some() {
                "Update"
            } else {
                "Create"
            },
            edit.path.display(),
            edit.description
        );
    }
    if args.apply {
        // Refuse an intervening edit rather than overwriting it.
        for edit in &edits {
            if read(&edit.path)? != edit.old {
                return Err(format!(
                    "{} changed during setup; run again",
                    edit.path.display()
                ));
            }
        }
        for edit in edits.iter().filter(|e| e.changed()) {
            apply(edit)?;
        }
    }
    if args.local {
        let config = &edits[0].path;
        println!(
            "\nStart the local forwarder in another terminal{}:\n  portway --config {} --data-dir {}",
            if args.apply { "" } else { " after applying" },
            shell_quote(&config.to_string_lossy()),
            shell_quote(&config.parent().unwrap().to_string_lossy())
        );
        println!(
            "Add --daemon to keep it running. This command does not start or restart a process."
        );
    }
    if args.client.codex() {
        println!(
            "\nCodex: from your project directory, run codex -p portway. Keep your existing ChatGPT login; use codex login if you have not signed in."
        );
    }
    if args.client.claude() {
        println!(
            "\nClaude Code: start a new claude session. User settings.env applies to every project; project or managed settings may override it."
        );
    }
    println!(
        "\nCheck the listener and client settings:\n  portway doctor --client {} --url {}",
        match args.client {
            Client::Codex => "codex",
            Client::Claude => "claude",
            Client::Both => "both",
        },
        shell_quote(&url)
    );
    println!(
        "Send a short CLI message and confirm it appears in Portway; doctor does not verify vendor login or inference."
    );
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(
        ".{suffix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    PathBuf::from(name)
}
fn private_file(path: &Path, text: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}
fn apply(edit: &Edit) -> Result<(), String> {
    crate::store::ensure_dir(edit.path.parent().ok_or("config path has no parent")?)?;
    if read(&edit.path)? != edit.old {
        return Err(format!(
            "{} changed during setup; run again",
            edit.path.display()
        ));
    }
    if let Some(old) = &edit.old {
        let backup = unique_sibling(&edit.path, "bak");
        private_file(&backup, old)?;
        println!("Backup: {}", backup.display());
    }
    let temp = unique_sibling(&edit.path, "tmp");
    let result = private_file(&temp, &edit.new).and_then(|()| {
        fs::rename(&temp, &edit.path).map_err(|e| format!("{}: {e}", edit.path.display()))
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn configured(client: Client, url: &str) -> Result<Vec<String>, String> {
    let mut failures = Vec::new();
    if client.codex() {
        let path = codex_path()?;
        let doc = document(read(&path)?.as_deref(), &path)?;
        let p = doc.get("model_providers").and_then(|p| p.get("portway"));
        let valid = doc.get("model_provider").and_then(Item::as_str) == Some("portway")
            && p.and_then(|p| p.get("base_url")).and_then(Item::as_str)
                == Some(format!("{url}/codex").as_str())
            && p.and_then(|p| p.get("requires_openai_auth"))
                .and_then(Item::as_bool)
                == Some(true)
            && p.and_then(|p| p.get("wire_api"))
                .is_none_or(|v| v.as_str() == Some("responses"))
            && p.and_then(|p| p.get("supports_websockets"))
                .is_none_or(|v| v.as_bool() == Some(false))
            && ["env_key", "experimental_bearer_token", "auth"]
                .iter()
                .all(|key| p.and_then(|p| p.get(key)).is_none());
        println!(
            "{} Codex profile: {}",
            if valid { "OK" } else { "FAIL" },
            path.display()
        );
        if !valid {
            failures.push(
                "Codex profile is missing or differs from the requested ChatGPT connection".into(),
            );
        }
    }
    if client.claude() {
        let path = claude_path()?;
        let doc: Value = serde_json::from_str(read(&path)?.as_deref().unwrap_or("{}"))
            .map_err(|_| format!("{}: invalid JSON", path.display()))?;
        let valid = doc
            .pointer("/env/ANTHROPIC_BASE_URL")
            .and_then(Value::as_str)
            == Some(format!("{url}/anthropic").as_str());
        println!(
            "{} Claude user settings: {}",
            if valid { "OK" } else { "FAIL" },
            path.display()
        );
        if !valid {
            failures
                .push("Claude user settings are missing or point at a different listener".into());
        }
    }
    Ok(failures)
}
fn doctor(args: &DoctorArgs) -> Result<(), String> {
    let url = endpoint(&args.url)?;
    let mut failures = configured(args.client, &url)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        match get_json(&url, "/__portway/health").await {
            Ok(v)
                if v["status"] == "ok"
                    && matches!(v["mode"].as_str(), Some("forward" | "router")) =>
            {
                println!("OK Portway forwarder: {url}")
            }
            Ok(_) => failures.push("health response is not a Portway forwarder".into()),
            Err(e) => failures.push(format!("listener health: {e}")),
        }
        match get_json(&url, "/__portway/stats").await {
            Ok(v) => {
                for name in ["anthropic", "codex"] {
                    if (name == "codex" && !args.client.codex())
                        || (name == "anthropic" && !args.client.claude())
                    {
                        continue;
                    }
                    if v["upstreams"].get(name).is_some() {
                        println!("OK upstream listed: {name}");
                    } else {
                        failures.push(format!(
                            "no {name} upstream in stats; check the server's [upstreams] mounts"
                        ));
                    }
                }
            }
            Err(e) => failures.push(format!("upstream stats: {e}")),
        }
    });
    println!(
        "Client settings checked at user scope. Project/managed settings and CLI overrides may change the effective connection."
    );
    println!(
        "No credentials or inference requests were sent. Verify login with a real CLI turn and check Portway's log; listed upstreams alone do not prove mount routing or compression."
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{}\nRun portway setup --help to configure clients, or check the listener's address and routes.",
            failures.join("\n")
        ))
    }
}
async fn get_json(url: &str, path: &str) -> Result<Value, String> {
    let request = async {
        let upstream = Arc::new(crate::pool::Upstream::new(url, None)?);
        let clock = Arc::new(crate::clock::PhaseClock::new());
        let request = http::Request::builder()
            .uri(path)
            .header("Host", &upstream.authority)
            .header("Connection", "close")
            .body(crate::body::TimedBody::empty(Arc::clone(&clock)))
            .map_err(|e| e.to_string())?;
        let (response, _lease) = upstream
            .send(request, clock)
            .await
            .map_err(|e| e.to_string())?;
        if response.status() != http::StatusCode::OK {
            return Err(format!("{path}: HTTP {}", response.status()));
        }
        let body = Limited::new(response.into_body(), 256 * 1024)
            .collect()
            .await
            .map_err(|e| e.to_string())?;
        serde_json::from_slice(&body.to_bytes()).map_err(|_| format!("{path}: expected JSON"))
    };
    tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .map_err(|_| format!("{path}: timed out after 5 seconds"))?
}
