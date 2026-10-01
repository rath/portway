# Getting started

This guide starts with one upstream service, then shows how to connect Claude
Code and Codex, register several models, and add a compression receiver.
Example domains and model names are placeholders: substitute the URL and model
accepted by your own service.

## Install

With [Homebrew](https://github.com/rath/homebrew-tap), install on macOS Apple
silicon or Linux x86_64/ARM64 (glibc 2.28 or later):

```sh
brew install rath/tap/portway
```

The package includes the terminal dashboard and the browser console. Update it
with `brew update` followed by `brew upgrade portway`, then restart any running
Portway process to use the new binary.

Each [release](https://github.com/rath/portway/releases) also provides the same
binaries for manual installation:

```sh
# macOS on Apple silicon. On Linux use x86_64-unknown-linux-gnu or
# aarch64-unknown-linux-gnu (glibc 2.28 or later).
target=aarch64-apple-darwin
curl -fsSL "https://github.com/rath/portway/releases/latest/download/portway-$target.tar.gz" | tar -xz
install -m 755 "portway-$target/portway" ~/.local/bin/   # any directory on your PATH
```

To build from source instead, from the root of this checkout, choose one
installation:

```sh
# CLI, daemon control, recording, and reports:
cargo install --path crates/portway --locked

# The same features, plus the terminal dashboard:
cargo install --path crates/portway --locked --features tui
```

Building needs Rust 1.97 or later and a C build toolchain on macOS or Linux. If your
shell cannot find the installed command, add Cargo's bin directory to `PATH`
(normally `~/.cargo/bin`) or run it using its full path. Confirm installation:

```sh
portway --version
portway --help
```

These instructions describe the standalone binary. A locally installed wrapper
may supply default flags; an explicit `--config` makes your intended file clear.

## Create your first TOML file

Create a new working directory for this example:

```sh
mkdir -p ~/portway-demo
cd ~/portway-demo
```

Save the following as `portway.toml` using your editor. Replace the upstream
address with a service you can already call directly:

```toml
# The destination service, including any required path prefix.
upstream = "https://api.example.com"

# Your client connects here.
host = "127.0.0.1"
port = 8787
```

You can also copy [examples/forward.toml](../examples/forward.toml) from the
checkout and edit its `upstream`. Compression defaults are already enabled;
you do not need a `[compression]` section to get started.

Start the forwarder and leave this terminal open:

```sh
portway --config ./portway.toml
```

Open a second terminal and check the local process:

```sh
curl --fail-with-body http://127.0.0.1:8787/__portway/health
```

Expect `{"status":"ok","mode":"forward"}`. This verifies Portway's listener,
not the upstream service or its credentials. Startup negotiates compression with
the upstream before serving requests, so an unreachable upstream may delay this
first check. Use Ctrl-C in the serving terminal to stop it.

If port 8787 is already in use, choose another port in the file or start with
`--port 8789`; use that same port in every client and health check.

## Send a request

For an OpenAI-compatible chat endpoint, set `UPSTREAM_API_KEY` in your client
terminal to the real key. For example, replace the placeholder below, or load
the variable from your existing secret manager:

```sh
export UPSTREAM_API_KEY='<your-upstream-key>'
```

The variable name here is just for curl; Portway does not read it. Replace
`model-a` in the request below with the service's actual model name:

```sh
curl --fail-with-body --no-buffer \
  http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer ${UPSTREAM_API_KEY:?Set UPSTREAM_API_KEY first}" \
  -H 'Content-Type: application/json' \
  --data '{
    "model": "model-a",
    "messages": [{"role": "user", "content": "Say hello."}],
    "stream": true
  }'
```

If the upstream requires no authentication, omit the Authorization header. If it
uses a different header such as `X-API-Key`, supply that instead. Portway relays
the service's streaming response as it arrives. `--no-buffer` lets curl display
it immediately. The endpoint and JSON fields must be supported by your upstream;
Portway does not translate API formats.

For a client with an OpenAI-compatible configuration screen, use:

| Client setting | Value |
| --- | --- |
| API base URL | `http://127.0.0.1:8787/v1` |
| API key | The key accepted by the destination service |
| Model | The model name accepted by that service |

A client asking for a **full endpoint** instead of a base URL needs
`http://127.0.0.1:8787/v1/chat/completions`. For an ordinary HTTP application,
use its normal method, path, headers, and body against `http://127.0.0.1:8787`.

If the upstream base URL you were given ends in `/v1`, see
[URL composition](configuration.md#choose-the-upstream-url) before copying it
into TOML. The client path and upstream prefix are appended, not deduplicated.

## Connect Claude Code or Codex

A vendor's own client speaks that vendor's API and lets you set its base URL.
Mount the vendor's API under a name and end the base URL in that name:

```toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"
```

Restart Portway, then:

| Client | Setting |
| --- | --- |
| Claude Code | `ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic` |
| Codex | `base_url = "http://127.0.0.1:8787/codex"` in its provider entry, with `wire_api = "responses"` and `requires_openai_auth = true` |

Portway removes the name and forwards the rest of the path to the URL, so
`/anthropic/v1/messages` reaches `https://api.anthropic.com/v1/messages`. The
client keeps its own login and model names; nothing in the file lists them.
[Claude Code and Codex](agents.md) has both clients' settings in full, the
API-key variant for Codex, and the two-hop layout.

## Add a second model destination

Stop the foreground instance with Ctrl-C. Replace the file with this layout,
substituting actual model names and destinations:

```toml
host = "127.0.0.1"
port = 8787

[models]
"model-a" = "https://first.example.com"
"model-b.1" = "https://second.example.com"
```

The `upstream` setting is removed because `[models]` takes its place. Restart:

```sh
portway --config ./portway.toml
```

List the registered names:

```sh
curl --fail-with-body http://127.0.0.1:8787/v1/models
```

Send the earlier chat request with `"model": "model-b.1"` and the key accepted by
`second.example.com`. The name selects the URL; Portway forwards that same name
and the supplied key. There is no implicit default route, model alias rewriting,
or per-model API key storage. See [routing](configuration.md#register-models) and
[using different keys](authentication.md#different-models-with-different-keys).

## Run in the background

Stop the foreground instance first, then run these commands from the directory
containing your configuration:

```sh
portway --config ./portway.toml --daemon
portway --status
portway --report --since 24h
```

If you installed the TUI feature, attach a dashboard:

```sh
portway --config ./portway.toml --tui
```

Press `q` to leave this attached viewer; the daemon keeps running. Stop the daemon
with `portway --stop`. Use [operations](operations.md) for custom runtime
directories, configuration restarts, logs, and dashboard keys.

To keep your TOML at `~/.config/portway/portway.toml` instead, move or create it
there yourself. A bare invocation finds it after `./portway.toml`:

```sh
portway --daemon
portway --config "$HOME/.config/portway/portway.toml" --daemon  # or be explicit
```

## Add a compression receiver

Forwarding works even when your upstream does not understand compressed requests.
To gain zstd/gzip and dictionary compression, the receiving side must advertise
and decode them. A Portway receiver supplies this layer in front of your own
application or a remote provider API. Upload compression toward that final origin
is a separate, optional policy; provider support varies.

```text
Client → local Portway → HTTPS/authentication gateway → Portway receive → application
```

On the server hosting your application, create a separate `receive.toml`:

```toml
upstream = "http://127.0.0.1:8000"
host = "127.0.0.1"
port = 8788
```

Then run:

```sh
portway receive --config ./receive.toml --data-dir ./receiver-data
```

Configure your existing gateway to forward to `http://127.0.0.1:8788`. It must
terminate TLS and authenticate application requests **before** forwarding them
to the receiver. Allow only `GET /__portway/capabilities` through without a key
for capability discovery; other management paths can stay private. This endpoint
returns codec names, not application data or dictionary contents. The standalone
receiver itself does not validate API keys or terminate TLS.

Set the **sending** Portway's `upstream` (or model destination) to the gateway's
public origin, for example `https://gateway.example.com`. Client settings remain
pointed at the local sender. Preserve caller identity through the gateway as
explained in [authentication and dictionary isolation](authentication.md#receivers-and-dictionary-isolation).

The first eligible compressed request can establish a dictionary; subsequent
similar requests can reuse it after acknowledgement. With default settings,
bodies below 32KiB cannot seed receiver dictionaries, so a tiny chat request is
not a useful DCZ test. See [checking compression](operations.md#check-compression)
and [receiver settings](configuration.md#receiving).

The `[receiver]` table only changes receiver settings. It does not select the
mode: the `receive` positional argument is required. A receiver accepts one
`upstream`, an `[upstreams]` table that mounts each origin under its own path,
or a `[models]` table that routes each request to the origin configured for
its JSON `model` field.

### Compress the receiver's upload to a provider

For a receiver on a remote host such as receiver-host, the final origin can itself be
HTTPS. The sender continues pointing at the receiver through your existing tunnel:

```text
Claude Code → sender (sender-host) → SSH tunnel → receiver (receiver-host) → HTTPS origin
```

Example receiver configuration:

```toml
upstream = "https://api.anthropic.com"
host = "127.0.0.1"
port = 8788 # use the receiver port targeted by your SSH tunnel

[receiver.origin_compression]
mode = "auto"
```

Keep your existing origin URL and listener port if they differ. Replace the
receiver binary and restart it once to install this version. Later configuration
changes can use `portway --reload --data-dir ./receiver-data` in daemon mode.
No sender change is needed to enable compression on the receiver's origin leg.

The receiver first tries gzip and learns supported codings from origin responses.
A 415 triggers one identity retry and a ten-minute suspension; a 400 suspends
compression without replaying that request. Enabling `auto` does not guarantee
that any particular provider accepts compressed uploads. See the complete
[origin policy](configuration.md#receiver-to-origin-upload-compression) and
[diagnostics](operations.md#check-receiver-to-origin-compression).
