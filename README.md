# Portway

**A compression-first HTTP forwarder.**

Portway compresses request bodies with zstd or gzip, reuses upstream connections,
and relays streaming responses. With a cooperating receiver, DCZ compresses a
request against a previously acknowledged body, making repeated context cheap
to send. No provider, model endpoint, credential, or price is built in.

The workspace contains `portway-core`, an embeddable Rust library, and `portway`,
a CLI with recording, daemon control, reports, and an optional terminal dashboard.

## Start here

| You want to… | Read |
| --- | --- |
| Install Portway, create a TOML file, and send a first request | [Getting started](docs/getting-started.md) |
| Register models, choose URLs, or adjust compression | [Configuration](docs/configuration.md) |
| Send API keys or connect routes with different credentials | [Authentication](docs/authentication.md) |
| Run in the background, use the dashboard, or diagnose errors | [Operations](docs/operations.md) |
| Add request decompression in front of an existing service | [Receiver setup](docs/getting-started.md#add-a-compression-receiver) |
| Implement a compatible receiver | [Request compression protocol](docs/protocol.md) |

## Install from this checkout

Rust 1.97 or later and a C build toolchain are required. Supported platforms are
macOS and Linux. Run from the repository root:

```sh
cargo install --path crates/portway --locked
# Or include the optional terminal dashboard:
cargo install --path crates/portway --locked --features tui
```

Choose one command. Installation normally puts `portway` in `~/.cargo/bin`, which
must be on your `PATH`. The default build has no terminal UI dependencies. To build
without installing, use `cargo build --release --locked` (optionally with
`--features tui`) and run `./target/release/portway` instead.

## First configuration

Create a file named `portway.toml` in a directory of your choice. For an
OpenAI-compatible service, replace the example address with its origin:

```toml
upstream = "https://api.example.com"
host = "127.0.0.1"
port = 8787
```

Start Portway in that directory:

```sh
portway --config ./portway.toml
```

In your client, set the API base URL to `http://127.0.0.1:8787/v1` and keep the
upstream service's API key and model name. Portway forwards the request's
`Authorization` header; it has no separate API key and does not read one from
TOML or environment variables. See [a complete curl example](docs/getting-started.md#send-a-request).

The upstream URL is a prefix: a request to `/v1/chat/completions` with the URL
above reaches `https://api.example.com/v1/chat/completions`. Putting `/v1` in both
the upstream URL and client path produces `/v1/v1/chat/completions`.

Single-upstream mode also supports ordinary HTTP APIs, arbitrary finite request
bodies, and paths unrelated to `/v1`. It does not require JSON or a `model` field.
Compression is negotiated automatically; services without a compatible
advertisement receive uncompressed requests.

## Route several models

Replace the `upstream` line with a `[models]` table. Put root settings before the
first table, and quote model names so dots and slashes remain part of the name:

```toml
host = "127.0.0.1"
port = 8787

[models]
"model-a" = "https://first.example.com"
"model-b.1" = "https://second.example.com"
"team/model-c" = "https://second.example.com"
```

The JSON `model` field selects a destination by exact name. Model names are sent
upstream unchanged, so each name must also be accepted by its destination.
`GET /v1/models` lists the configured names; it does not discover or download
models. Use either `upstream` or `[models]`, never both.

See [model registration and routing rules](docs/configuration.md#register-models)
for adding routes, bodyless requests, and optional cost estimates. Annotated
starting files are in [examples/](examples/): [single upstream](examples/forward.toml),
[models](examples/models.toml), and [receiver](examples/receive.toml).

## Observe and operate

After stopping a foreground instance with Ctrl-C, you can run it in the background:

```sh
portway --config ./portway.toml --daemon
portway --status
portway --report --since 7d
# Requires installation with --features tui:
portway --config ./portway.toml --tui
# In another terminal, when ready to stop:
portway --stop
```

A dashboard attached to a daemon can be closed without stopping forwarding.
Configuration changes require a restart; `--reload` only reopens the log and
renegotiates compression. Daemon mode does not install a boot or login service.

The CLI records request metadata in SQLite, never bodies, credentials, cookies,
or dictionary contents. The runtime directory defaults to
`$XDG_CONFIG_HOME/portway` (when absolute) or `~/.config/portway`.
**That directory is not a config search path:** the standalone binary reads
`--config PATH` or `./portway.toml` in the current working directory.
Use `--data-dir` consistently for separate instances.

For health checks, use `GET /__portway/health`; ordinary `/health` is forwarded to
the upstream. See [operations and troubleshooting](docs/operations.md).

## Embed the core

```rust,no_run
use portway_core::{Forwarder, ForwarderConfig};
use bytes::Bytes;
use http::Request;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let forwarder = Forwarder::from_url(
    "api", "https://api.example.com", &ForwarderConfig::default(),
)?;
forwarder.negotiate().await;
let response = forwarder.forward(Request::builder()
    .method("POST")
    .uri("/items")
    .body(Bytes::from_static(b"arbitrary request bytes"))?
).await;
// Consume or stream response.into_body(); dropping it cancels the upstream.
# Ok(())
# }
```

The caller owns the Tokio runtime, configuration, authentication, and listener.
The core has no CLI, SQLite, or terminal UI dependencies and performs no implicit
recording. Inject `Telemetry` to observe an instance; counters and dictionary
state do not leak between instances. `DictionaryScope` can partition dictionaries
by application identity instead of the default Authorization/Cookie fingerprint.

`Receiver::decode` returns the restored request and an acknowledgement; attach it
with `Acknowledgement::finish` after calling your authenticated application's
handler. This works with any response body type. A convenience `Receiver::handle`
adapter supports Portway's streaming response type.

Buildable examples: [forwarding](crates/portway-core/examples/embedded.rs) and
[receiving](crates/portway-core/examples/receiving.rs).

## Limits and verification

Requests are buffered with a configurable 256MiB default limit; response bodies
stream incrementally. WebSockets, CONNECT, HTTP/2, inbound TLS, and unbounded
streaming uploads are not supported. Cancellation closes the upstream HTTP/1.1
connection. Dictionary storage is memory-only and bounded.

```sh
bash scripts/check.sh
```

Checks cover both default and TUI builds, formatting, clippy, real TCP forwarding,
sender/receiver interoperability, retry safety, dictionary limits and isolation,
stream cancellation, daemon lifecycle, recording, and dashboard rendering.

Licensed under [MIT](LICENSE).
