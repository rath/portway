# Getting started

This guide starts with one upstream service, then shows how to register several
models and add a compression receiver. Example domains and model names are
placeholders: substitute the URL and model accepted by your own service.

## Install

From the root of this checkout, choose one installation:

```sh
# CLI, daemon control, recording, and reports:
cargo install --path crates/portway --locked

# The same features, plus the terminal dashboard:
cargo install --path crates/portway --locked --features tui
```

You need Rust 1.97 or later and a C build toolchain on macOS or Linux. If your
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
export UPSTREAM_API_KEY='replace-with-your-upstream-key'
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
there yourself and pass its path explicitly:

```sh
portway --config "$HOME/.config/portway/portway.toml" --daemon
```

The standalone binary does not automatically load that location.

## Add a compression receiver

Forwarding works even when your upstream does not understand compressed requests.
To gain zstd/gzip and dictionary compression, the receiving side must advertise
and decode them. If you control the service, Portway can supply that layer:

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
`upstream`; it does not use a `[models]` routing table.
