# Portway

Portway is a local HTTP proxy that sits between a coding agent and the LLM API
it talks to. When the other end runs `portway receive`, each request crosses
the network as the difference from the previous one instead of as the whole
conversation again. On a 1 MiB context, a turn that would upload about 1 MiB
uploads about 1 KiB.

**Both ends must take part.** The agent-side Portway compresses only when its
destination advertises the Portway protocol. That destination is `portway
receive`, run on a host you control in front of your own service or of a hosted
provider API, or any server implementing the [protocol](docs/protocol.md).
Pointed straight at a provider that runs neither, Portway forwards the request
unchanged and uncompressed.

## Who it is for

A coding agent resends its whole conversation on every turn: the system prompt,
every earlier message, and every tool result. Turn N is turn N-1 plus a few
kilobytes, yet the entire body is uploaded again, so the upload volume of a
session grows with the square of the number of turns. Portway helps where that
upload is what you wait or pay for:

- a self-hosted model server, or a gateway you run, in another region;
- a remote development machine, or a laptop on a slow or metered link;
- any long agent session whose requests grow append-only.

Short single-shot requests gain little, because nothing earlier can serve as a
base. If you cannot run a receiver anywhere on the path to your provider, read
[the FAQ on hosted provider APIs](docs/faq.md#does-portway-compress-requests-to-a-hosted-provider-api)
before going further.

## How it works

Four terms recur below. The agent-side process is the **sender**; `portway
receive` is the **receiver**; the service behind the receiver is the
**origin**. A request body the receiver has confirmed it stored is the
**dictionary** for the next request.

```text
            agent             Portway sender                    portway receive           app
turn 1  ─ 258 KiB ─▶  zstd, 59 KiB, X-Dict-Store: 1     ──▶  decode, keep body    ─ 258 KiB ─▶
                      ◀── 200, X-Dict-Stored: <SHA-256 of turn 1>
turn 2  ─ 262 KiB ─▶  dcz, 1 KiB, names turn 1 by hash  ──▶  restore from turn 1  ─ 262 KiB ─▶
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
- **Recovery.** A receiver refuses a request it cannot restore before the
  application runs, and marks the refusal. A lost dictionary (412) or a refused
  dictionary frame (400 or 415) makes the sender resend as plain zstd; a refused
  zstd body (415) makes it resend uncompressed. A request is sent at most three
  times, and application errors are never retried. See
  [errors and replay](docs/protocol.md#errors-and-replay).

Forwarding destinations come from your configuration; credentials and prices
are never supplied by Portway. The optional `setup --local` command writes
Claude Code and Codex upstreams into a config file you can inspect. The workspace
contains `portway-core`, an embeddable Rust library, and `portway`, a CLI with
recording, daemon control, reports, and an optional terminal dashboard.

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
sources. Runs are reproducible byte for byte.

### Measure it yourself

The script needs Python 3.9 or later and nothing outside its standard library.
It starts a local origin, a receiver, and one sender per configuration, checks
every restored body against the SHA-256 of what was sent, and exits nonzero
when a request fails or a dictionary is never used. Without options it runs 8
turns on a 256 KiB context; each table row is one 20-turn run:

```sh
cargo build --release --locked
python3 scripts/bench.py
# One row of the table; repeat with 128, 256, and 512 for the others:
python3 scripts/bench.py --turns 20 --context-kb 1024
```

For your own traffic, run an agent session through a sender whose destination
has a receiver, then read the sender's counters. In the stats, `saved_bytes`
is `body_bytes` minus `wire_bytes`, `down_saved_bytes` is the same for the
answers on their way back (`down_bytes` minus `down_wire_bytes`), and
`dict_hits` counts requests sent as a delta. The report shows the same
comparison as `up raw`, `up wire`, `up saved` and `down`, `down wire`,
`down saved`.

```sh
curl --fail-with-body http://127.0.0.1:8787/__portway/stats
portway --report --since 24h
```

See [checking compression](docs/operations.md#check-compression) if the
numbers stay flat.

## Before you rely on it

- **It saves bytes, not tokens.** The receiver restores the exact request, so
  the model receives the identical input and bills the same tokens.
  Provider-side prompt caching cuts the cost of processing a repeated prefix;
  Portway cuts the bytes and time of sending it. The two complement each other.
- **Dictionaries need warm-up.** The first request of a conversation travels
  without a dictionary, and by default the receiver does not store bodies under
  32 KiB as one ([`min_dictionary_bytes`](docs/configuration.md#receiving)).
  Dictionaries live only in memory on both sides, are lost on restart, and are
  kept apart per credential.
- **The receiver-to-origin hop is a separate, optional policy.** A receiver can
  try gzip or zstd toward its origin, learn from responses, and remember
  refusals. Enable
  [`[receiver.origin_compression] mode = "auto"`](docs/configuration.md#receiver-to-origin-upload-compression)
  for that; support varies by provider.

## Start here

| You want to… | Read |
| --- | --- |
| Put Claude Code or Codex behind Portway | [Claude Code and Codex](docs/cli-setup.md) |
| Install Portway, create a TOML file, and send a first request | [Getting started](docs/getting-started.md) |
| Put a compression receiver in front of a service you run | [Receiver setup](docs/getting-started.md#add-a-compression-receiver) |
| Know whether Portway fits your setup | [FAQ](docs/faq.md) |
| Register models, choose URLs, or adjust compression | [Configuration](docs/configuration.md) |
| Send API keys or connect routes with different credentials | [Authentication](docs/authentication.md) |
| Run in the background, use the dashboard, or diagnose errors | [Operations](docs/operations.md) |
| Implement a compatible receiver | [Request compression protocol](docs/protocol.md) |
| Understand what Portway stores and how to report a vulnerability | [Security](SECURITY.md) |

## Guided setup

The [Portway plugin](docs/plugins.md) walks you through connecting Codex or
Claude Code and checking the result. It uses `portway setup` to preview and
apply client settings with backups, and `portway doctor` to diagnose the
connection. Install Portway 0.2.5 or later, then add the plugin for your CLI:

```sh
brew install rath/tap/portway
# Already installed? Run: brew update && brew upgrade portway

# Codex
codex plugin marketplace add rath/portway
codex plugin add portway@portway

# Claude Code
claude plugin marketplace add rath/portway
claude plugin install portway@portway
```

Start a new session, then ask Codex to use the Portway plugin for setup, or run
`/portway:setup` in Claude Code. See [guided setup](docs/plugins.md) for binary
installation, existing-server and local options, and how to disconnect.
Plugin installation does not install or upgrade the Portway binary. The manual
quick start below is also available.

## Quick start

Install with [Homebrew](https://github.com/rath/homebrew-tap) on macOS Apple
silicon or Linux x86_64/ARM64 (glibc 2.28 or later). The package includes the
terminal dashboard and the browser console:

```sh
brew install rath/tap/portway
```

Or download a release binary with the same features. Checksums are in each
release's `SHA256SUMS`:

```sh
# macOS on Apple silicon. On Linux use x86_64-unknown-linux-gnu or
# aarch64-unknown-linux-gnu (glibc 2.28 or later).
target=aarch64-apple-darwin
curl -fsSL "https://github.com/rath/portway/releases/latest/download/portway-$target.tar.gz" | tar -xz
install -m 755 "portway-$target/portway" ~/.local/bin/   # any directory on your PATH
```

Or build from source: Rust 1.97 or later and a C build toolchain are required,
on macOS or Linux. From the repository root:

```sh
cargo install --path crates/portway --locked
# Add --features tui, web, or tui,web for the terminal dashboard, the browser
# console, or both.
```

That installs `portway` in `~/.cargo/bin`, which must be on your `PATH`.
Create `portway.toml` in a directory of your choice. For Claude Code and
Codex, mount each vendor's API under a name:

```toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"
```

Start Portway in that directory:

```sh
portway --config ./portway.toml
```

Leave Portway running and open a second terminal. Use a CLI you have already
installed and signed in to. This setup forwards directly to the providers;
[add a receiver](docs/cli-setup.md#across-the-network) to enable delta compression.

**Claude Code:** set the base URL for one invocation (bash, zsh, or fish):

```sh
env ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic claude
```

**Codex:** save this as `~/.codex/portway.config.toml` (or in `CODEX_HOME` if
you set it), using your existing ChatGPT login from `codex login`:

```toml
model_provider = "portway"

[model_providers.portway]
name = "portway"
base_url = "http://127.0.0.1:8787/codex"
requires_openai_auth = true
wire_api = "responses"
```

Then run from your project directory:

```sh
codex -p portway
```

The profile inherits your usual model and preferences. Plain `codex` uses
your usual provider. Each client keeps its credentials; Portway needs no key
of its own and no vendor model entries. End the base URLs at `/anthropic` or
`/codex`, without adding `/v1`. If Portway runs on another machine, replace
`127.0.0.1:8787` with that server's reachable address.

Send a short message and confirm the request appears in Portway's log or
browser console. A health response alone does not verify authentication.
[Claude Code and Codex](docs/cli-setup.md) covers persistent shell settings,
older Codex profile migration, API-key authentication, and troubleshooting.

For an OpenAI-compatible service, a single `upstream = "https://api.example.com"`
sends everything to it: set the client's API base URL to
`http://127.0.0.1:8787/v1` and keep its key and model name. The upstream URL
is a prefix: a request to `/v1/chat/completions` reaches
`https://api.example.com/v1/chat/completions`, so do not put `/v1` in both the
upstream URL and the client path. See
[a complete curl example](docs/getting-started.md#send-a-request). This mode
forwards any HTTP API with finite request bodies; it does not require JSON or
a `model` field. To send different model names to different services from one
base URL, use a `[models]` table; see
[model registration](docs/configuration.md#register-models) and the annotated
files in [examples/](examples/).

To keep it running after you close the terminal:

```sh
portway --config ./portway.toml --daemon
portway --status
portway --report --since 7d
portway --stop
```

Builds with `--features tui` or `web` add `--tui` and `--web`, dashboards that
attach to a running daemon or host the forwarder themselves. Use
`portway --tui --attach https://console.example/portway/` to view a remote
web console from your local terminal; see [remote attach](docs/operations.md#remote-terminal-dashboard)
for token entry and saved sessions. The CLI records request metadata in SQLite,
never bodies, credentials, cookies, or dictionary contents. See [operations](docs/operations.md) for the data directory, reload,
health checks, and the web console.

## Embed the core

```rust
use portway_core::{Forwarder, ForwarderConfig};
use bytes::Bytes;
use http::Request;

async fn example() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

The caller owns the Tokio runtime, configuration, authentication, and listener.
The core has no CLI, SQLite, or terminal UI dependencies and performs no
implicit recording. Inject `Telemetry` to observe an instance; counters and
dictionary state do not leak between instances. `DictionaryScope` can partition
dictionaries by application identity instead of the default
Authorization/Cookie fingerprint.

On the receiving side, `Receiver::decode` returns the restored request and an
acknowledgement; attach it with `Acknowledgement::finish` after calling your
authenticated application's handler. Buildable examples:
[forwarding](crates/portway-core/examples/embedded.rs) and
[receiving](crates/portway-core/examples/receiving.rs).

## Limits and verification

Requests are buffered with a configurable 256 MiB default limit; response bodies
stream incrementally. WebSockets, CONNECT, HTTP/2, inbound TLS, and unbounded
streaming uploads are not supported. When the agent disconnects, Portway waits
up to 2 s for the upstream answer to end, so an answer the agent stopped reading
at its last event is still recorded whole; an answer still generating, or still
open after that, is cancelled and its upstream HTTP/1.1 connection closed.
Dictionary storage is memory-only and bounded: 128 MiB on the receiver, 64 MiB
per route on the sender; see the
[FAQ](docs/faq.md#how-much-memory-do-dictionaries-use).

`bash scripts/check.sh` runs formatting, clippy, and the tests for the
default, `tui`, `web`, and `tui,web` builds, then the dependency-boundary and
web console checks listed in [CONTRIBUTING.md](CONTRIBUTING.md#checks). The
tests cover real TCP forwarding, sender/receiver interoperability, retry
safety, dictionary limits and isolation, stream cancellation, daemon lifecycle,
recording, and dashboard rendering.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and
[SECURITY.md](SECURITY.md) for the threat model and private vulnerability
reporting. Changes are listed in [CHANGELOG.md](CHANGELOG.md).

Licensed under [MIT](LICENSE).

Written by Jang-Ho Hwang &lt;rath@xrath.com&gt;.
