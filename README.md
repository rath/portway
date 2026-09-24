# Portway

**Send only what changed.** Portway is an HTTP forwarder for LLM agents whose
context grows with every turn. With a Portway receiver on the other side, each
request travels as a small delta against the one before it.

## Why

A coding agent resends its whole conversation on every turn: the system
prompt, every earlier message, and every tool result. Turn N is turn N-1 plus
a few kilobytes, yet the entire body is uploaded again. Across a session the
upload volume grows with the square of the number of turns, and a slow or
metered link pays for it on every request.

Portway keeps the last request body that the receiver confirmed it stored and
compresses the next request against it. What crosses the network is the new
part, zstd-compressed, plus a 40-byte header naming the previous body.

## Measured

[`scripts/bench.py`](scripts/bench.py) sends one synthetic, append-only chat
conversation through the real binaries three ways. Each turn appends 4 KiB.
The table shows bytes on the wire per turn, after the first turn, for
`python3 scripts/bench.py --turns 20 --context-kb N`:

| Context at turn 1 | Uncompressed | zstd only | zstd + previous turn | Saved per turn | Saved over 20 turns |
| --- | ---: | ---: | ---: | ---: | ---: |
| 128 KiB | 175,405 | 41,962 | 1,104 | 99.4% | 98.5% |
| 256 KiB | 306,503 | 69,608 | 1,058 | 99.7% | 98.7% |
| 512 KiB | 570,438 | 122,089 | 1,034 | 99.8% | 98.8% |
| 1 MiB | 1,094,276 | 222,937 | 1,048 | 99.9% | 98.9% |

A turn costs about as much as what it added, however large the context has
grown. The first request carries the whole context, zstd-compressed, to seed
the dictionary; the last column includes it. The synthetic text is tuned so
that plain zstd saves about as much as it does on this repository's own
sources. Runs are reproducible byte for byte. To check your own traffic, see
[measure it yourself](#measure-it-yourself).

## Before you rely on it

- **The other end must cooperate.** Portway compresses a request only when the
  destination advertises support. It ships that side as `portway receive`,
  which runs in front of a service you control, such as a self-hosted model
  server or your own gateway. Any server implementing the
  [protocol](docs/protocol.md) works too.
- **Hosted APIs get plain forwarding.** Pointed directly at a provider's
  public API, Portway sends requests uncompressed, because the provider
  advertises no support. Model routing and recording still work.
- **It saves bytes, not tokens.** The model receives the identical request and
  bills the same tokens. Provider-side prompt caching cuts the cost of
  processing a repeated prefix; Portway cuts the bytes and time of sending it.
  The two complement each other.
- **Dictionaries need warm-up.** The first request of a conversation travels
  without a dictionary, and bodies under 32 KiB are never stored as one.
  Dictionaries live only in memory on both sides and are kept apart per
  credential.

## How it works

```text
            agent             Portway sender                    portway receive           app
turn 1   ─ 264 KB ─▶  zstd, 61 KB, X-Dict-Store: 1      ──▶  decode, keep body     ─ 264 KB ─▶
                      ◀── 200, X-Dict-Stored: <SHA-256 of turn 1>
turn 2   ─ 268 KB ─▶  dcz, 1 KB, names turn 1 by hash   ──▶  restore from turn 1   ─ 268 KB ─▶
```

- **Base selection.** The sender keeps up to eight confirmed bodies per route
  and picks the one sharing the longest prefix with the new request. For an
  append-only conversation, that is the previous turn.
- **Frame format.** A dictionary-compressed request uses DCZ
  (Dictionary-Compressed Zstandard) from
  [RFC 9842](https://www.rfc-editor.org/rfc/rfc9842.html#section-5): a 40-byte
  header carrying the dictionary's SHA-256, then a checksummed zstd frame.
- **Confirmation.** The receiver answers with the SHA-256 of the bytes it
  actually decoded and stored. The sender uses only a body whose hash matches.
- **Recovery.** If the receiver has lost a dictionary, it answers 412 before
  the application runs, and the sender resends once as plain zstd. Application
  errors are never retried.

No provider, model endpoint, credential, or price is built in. The workspace
contains `portway-core`, an embeddable Rust library, and `portway`, a CLI with
recording, daemon control, reports, and an optional terminal dashboard.

## Start here

| You want to… | Read |
| --- | --- |
| Install Portway, create a TOML file, and send a first request | [Getting started](docs/getting-started.md) |
| Put a compression receiver in front of a service you run | [Receiver setup](docs/getting-started.md#add-a-compression-receiver) |
| Know whether Portway fits your setup | [FAQ](docs/faq.md) |
| Register models, choose URLs, or adjust compression | [Configuration](docs/configuration.md) |
| Send API keys or connect routes with different credentials | [Authentication](docs/authentication.md) |
| Run in the background, use the dashboard, or diagnose errors | [Operations](docs/operations.md) |
| Implement a compatible receiver | [Request compression protocol](docs/protocol.md) |
| Understand what Portway stores and how to report a vulnerability | [Security](SECURITY.md) |

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
Without `--config`, the binary reads `./portway.toml` first, then
`portway.toml` under that runtime directory, so a bare `portway --tui` attaches
to the same file the daemon is using. Use `--data-dir` consistently for
separate instances.

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

## Measure it yourself

Reproduce the table above from a checkout. The script needs Python 3.9 or
later and nothing outside its standard library:

```sh
cargo build --release --locked
python3 scripts/bench.py
python3 scripts/bench.py --context-kb 1024 --turns 20
```

It starts a local origin, a receiver, and one sender per configuration, then
prints a per-turn table and a Markdown summary. Every restored body is checked
against the SHA-256 of what was sent. The script exits nonzero when a request
fails or a dictionary is never used, so CI runs it too. Pass `--keep` to keep
the logs and databases; `--help` lists the payload options.

For your own traffic, run an agent session through a sender whose destination
has a receiver, then read the sender's counters:

```sh
curl --fail-with-body http://127.0.0.1:8787/__portway/stats
portway --report --since 24h
```

In the stats, compare `body_bytes` with `wire_bytes`; `saved_bytes` is the
difference, and `dict_hits` counts requests sent as a delta. The report shows
the same comparison as `up raw`, `up wire`, and `saved`. See
[checking compression](docs/operations.md#check-compression) if the numbers
stay flat.

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

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and
[SECURITY.md](SECURITY.md) for the threat model and private vulnerability
reporting. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

Licensed under [MIT](LICENSE).

Written by Jang-Ho Hwang &lt;rath@xrath.com&gt;.
