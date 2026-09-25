# Configuration

Start with [getting started](getting-started.md) for a complete first-run example.
This page explains how files are loaded, how requests select an upstream, and
what each setting controls. All names, domains, and prices below are examples.

## Create and load a file

Save configuration as plain UTF-8 text with a `.toml` extension. From the
repository root, you can copy one of the annotated examples to a new file:

```sh
cp examples/models.toml ./portway.toml
# Edit the copied file to set your actual model names and URLs.
portway --config ./portway.toml
```

Use [forward.toml](../examples/forward.toml) for one destination,
[models.toml](../examples/models.toml) for a model routing table, or
[receive.toml](../examples/receive.toml) for a receiver.

The standalone binary chooses its configuration in this order:

1. If supplied, read exactly `--config PATH`. A missing file is an error.
2. Otherwise, try `portway.toml` in the **current working directory**.
3. If that file does not exist, try `portway.toml` under the runtime data
   directory — `$XDG_CONFIG_HOME/portway` when that variable is absolute, or
   `~/.config/portway`. A file there is the same one the daemon writes its
   database beside, so a bare `portway --tui` attaches to it by default.
4. If neither exists, use defaults and explicit command-line settings. Serving
   still requires a destination, such as `--upstream URL`.

The data directory's TOML is read no matter which directory the process is
launched from. A stable absolute path skips the search entirely:

```sh
portway --config "$HOME/.config/portway/portway.toml" --daemon
```

Create that file yourself first. Portway does not generate one automatically and
has no `init` or `--check-config` command. Start it in the foreground to see parsing
and startup errors. A separate port and `--data-dir` allow a trial instance without
sharing a running instance's database.

Explicit CLI values override the loaded settings; omitted flags retain file
values. For example:

```sh
portway --config ./portway.toml --port 8789 --level 3
```

This changes the listening port and compression level for that invocation.
`--upstream URL` also clears a file's `[models]` table and selects single-upstream
mode. There is no `--models` registration flag; edit the table in TOML.
A file must still parse successfully before CLI overrides can be applied.

Relative config paths are read before daemonization. TOML strings do not expand
shell variables or `~`; expansion in the shell command above happens before
Portway sees its arguments. Unknown fields and invalid settings produce errors.
After editing an active daemon configuration, [reload or restart as
appropriate](operations.md#apply-configuration-changes).

## TOML basics that matter here

Root settings such as `host`, `port`, and `upstream` go **before** the first table
header. After `[models]`, assignments belong to that table until the next header.
Use `#` for comments, quotes for string values, integers for ports and byte counts,
and quoted model keys for literal names containing punctuation.

```toml
host = "127.0.0.1"
port = 8787

[models]
"model-b.1" = "https://second.example.com"
"team/model-c" = "https://third.example.com"

[compression]
level = 3
```

An unquoted `model-b.1` is a TOML dotted key, not one literal model name. Quote the
name in price table headers too: `[prices."model-b.1"]`. Tables cannot be declared
twice, and duplicate model keys are errors. To add a route, put a new entry inside
the existing `[models]` section, before the next table.

## Choose the routing mode

| Requirement | Configuration |
| --- | --- |
| One HTTP service, including arbitrary non-JSON requests | `upstream = "https://api.example.com"` |
| Several destinations selected by a JSON `model` field | `[models]` table |
| Decode compressed requests in front of an application | `receive` CLI mode with one `upstream` |

Use exactly one of `upstream` or a nonempty `[models]` table. Even a one-entry
`[models]` table requires a matching model in requests; it is not an implicit
default destination.

Single-upstream mode sends application requests to the one configured service,
including `/v1/models`. It neither inspects nor restricts the JSON `model` field.
Use it when a single upstream already handles multiple models, multipart uploads,
or other payload formats that model routing cannot inspect.

## Choose the upstream URL

The upstream URL is the destination origin plus an optional **path prefix**.
Portway appends the client's path and preserves the client's query string:

| Configured URL | Incoming path | Upstream path |
| --- | --- | --- |
| `https://api.example.com` | `/v1/chat/completions` | `/v1/chat/completions` |
| `https://api.example.com/api` | `/v1/chat/completions` | `/api/v1/chat/completions` |
| `https://api.example.com/v1` | `/v1/chat/completions` | `/v1/v1/chat/completions` |
| `https://api.example.com/api` | `/items?q=1` | `/api/items?q=1` |

For a service whose chat endpoint is `https://api.example.com/v1/chat/completions`,
configure `https://api.example.com` and point your client's API base URL at
`http://127.0.0.1:8787/v1`. Do not use the full chat endpoint as the upstream URL.

URLs must use explicit `http://` or `https://`, with no embedded credentials,
query, or fragment. HTTPS upstream certificates are verified. These are reverse
proxy destinations; they are not HTTP CONNECT proxy settings.

## Register models

Register each model by adding a name-to-URL entry:

```toml
host = "127.0.0.1"
port = 8787

[models]
"model-a" = "https://first.example.com"
"model-b.1" = "https://second.example.com"
"team/model-c" = "https://second.example.com"
```

Here `model-b.1` and `team/model-c` share a destination. The names are exact,
case-sensitive matches for the request's JSON `model` string. They must be the
names accepted by the selected service, because Portway forwards them unchanged.
Registering `"fast"` does not rename an upstream model to `fast`.

Registration is local routing configuration. It does not create, download, load,
or discover models on a server. There is no live registration endpoint. Add or
remove entries, reload Portway, then inspect the configured names:

```sh
curl --fail-with-body http://127.0.0.1:8787/v1/models
```

This returns an OpenAI-shaped list with your configured names as `id` values. It
is generated locally and does not test upstream availability or credentials.
For readiness, send a small real request with the appropriate key.

Routing rules are:

| Request | Behavior in model-routing mode |
| --- | --- |
| Nonempty JSON object with a string `model` | Select that exact name |
| Nonempty body without a supported `model` | 400; no default model |
| Nonempty non-JSON body | 400 |
| Empty body with `?model=model-a` | Select the query's model |
| Nonempty body plus query `model` | The body determines the model, even if missing or invalid |
| Precompressed request body | 415; routing requires an uncompressed body |
| `GET /v1/models` | Return the local model list |

For a bodyless upstream health request:

```sh
curl --fail-with-body 'http://127.0.0.1:8787/health?model=model-a'
```

The query remains present on the forwarded request. Ordinary `/health` without a
model cannot choose a route. Use `/__portway/health` for Portway's local liveness.
After route selection, Portway can compress the request body before forwarding.
It does not rewrite model names, JSON fields, or API formats, and provides no
automatic failover between model destinations.

Keys are sent by the client, not registered in the routing table. See
[authentication](authentication.md) for services with different credentials.

## Root settings and prices

| Root setting | Default | Meaning |
| --- | --- | --- |
| `host` | `"127.0.0.1"` | Listening address |
| `port` | `8787` | Listening port |
| `upstream` | unset | Single upstream HTTP(S) URL |
| `[models]` | empty | Model name → upstream URL; exclusive with `upstream` |
| `[prices.NAME]` | absent | Optional `input`, `output`, `cache_read` rates in USD per million tokens |

Prices are optional inputs to the TUI's usage and cost estimates. They do not
change requests, charge a balance, or discover provider rates. For example:

```toml
[models]
"model-b.1" = "https://second.example.com"

# Illustrative numbers only; replace with your own comparison rates.
[prices."model-b.1"]
input = 1.0
output = 2.0
cache_read = 0.1
```

All three price fields are required together and must be finite, nonnegative
numbers. Names match **recorded route names**. In single-upstream mode the route
is always `upstream`, regardless of any JSON `model`, so use `[prices.upstream]`.
A price entry can also describe a historical route no longer being served.

Missing prices remain unpriced. Token usage also depends on what the upstream
reports; a price table cannot fill in missing usage. TUI estimates use the
configuration loaded by that TUI invocation, including for historical records.
An attached viewer should receive the same `--config` if you want the same prices.
The text `--report` command reports traffic and timing; it does not load TOML
prices or calculate these cost estimates.

## Compression

Omit this entire section to use defaults. To make your choices explicit:

```toml
[compression]
coding = "auto"
dict = "auto"
level = 11
min_bytes = 1024
```

| `[compression]` setting | Default | Meaning |
| --- | --- | --- |
| `coding` | `"auto"` | `auto`, `zstd`, `gzip`, or `off`; preferences require advertisement |
| `dict` | `"auto"` | `auto` or `off`; DCZ requires negotiated zstd |
| `level` | `11` | 1–19; gzip is capped at 9 |
| `min_bytes` | `1024` | Smaller request bodies remain unchanged |
| `max_body_bytes` | `268435456` | Maximum collected request size (256MiB) |
| `probe_path` | `/health` | Absolute fallback origin path, without query |

`auto` prefers zstd, then gzip, and otherwise sends identity (uncompressed).
`coding = "zstd"` is a preference that still requires advertised support; it does
not force an incompatible service to accept zstd. Bodies are compressed only
when eligible and when the encoded representation is smaller.

Use `dict = "off"` to disable dictionaries while retaining ordinary compression.
Use `coding = "off"` to disable Portway's request compression altogether.
Already encoded bodies pass through in single-upstream mode. These settings are
for request compression; response negotiation is handled separately.

Negotiation tries `/__portway/capabilities`, then `probe_path` if no codecs are
advertised. For a legacy service advertising support at a custom path:

```toml
[compression]
probe_path = "/compression-info"
```

Probe paths are relative to the origin, not the configured upstream path prefix.
The probe reads `X-Request-Encodings` or the `request_encodings` JSON array and
separately reads `X-Request-Dictionary`. It sends no API key. Unavailable probes
start with identity; a later failed probe retains the last known capabilities.

Capabilities refresh lazily after 60 seconds. Requests do not wait for background
refresh. Explicit decoder refusals suppress the refused codec for ten minutes.
The pool holds up to eight idle connections per route, for up to 300 seconds.

CLI overrides: `--host`, `--port`, `--upstream`, `--coding`, `--dict`, `--level`,
`--min-bytes`, `--max-body-bytes`, `--probe-path`.

See [checking compression](operations.md#check-compression) if ordinary requests
work but compression or dictionary reuse is absent.

## Receiving

Start with `portway receive --config ./receive.toml`. Receiver mode requires one
`upstream`. It does not perform model routing or recompress restored requests
before sending them to the application. `[receiver]` configures this mode; it
does not activate the mode by itself.

| `[receiver]` setting | Default |
| --- | --- |
| `dictionary_bytes` | `268435456` (256MiB); zero disables dictionaries |
| `dictionary_ttl_seconds` | `3600` idle seconds |
| `min_dictionary_bytes` | `32768` (32KiB) |
| `max_dictionary_bytes` | `33554432` (32MiB) |
| `max_body_bytes` | `268435456` (256MiB), for encoded and decoded bodies |
| `max_window_bytes` | `134217728` (128MiB) |

Dictionary thresholds must be positive and ordered, with the maximum no larger
than the body limit. The zstd window limit is a power of two between 1KiB and
128MiB. Expiry is lazy: dictionary lookups and insertions remove expired entries.
The forwarding body limit in `[compression]` also applies to the restored request;
raise both body limits if you intend to accept larger decoded bodies.

Dictionaries are memory-only. They are isolated by the receiving instance and
authentication context. Application middleware can supply a `DictionaryScope`
extension after authenticating its caller. Standalone sender and receiver derive
that context from the exact Authorization and Cookie headers; changing either
starts a different context. A gateway that strips identity headers must isolate
receiver instances per trust context or use middleware with an explicit scope.
A scope partitions storage; it is not authentication.

See the [receiver walkthrough](getting-started.md#add-a-compression-receiver) and
[authentication guidance](authentication.md#receivers-and-dictionary-isolation)
when putting the receiver behind a gateway.

## Runtime files and command-only options

`--data-dir PATH` overrides `$XDG_CONFIG_HOME/portway` (when XDG_CONFIG_HOME is
absolute) or `$HOME/.config/portway`. Newly created directories have mode 0700.
The runtime directory contains `db.sqlite3`, `portway.pid` in daemon mode,
`portway.log` for daemon output, and `portway.tui` for saved dashboard settings.
Old runtime directories are not discovered or automatically migrated.

`--config` chooses TOML; `--data-dir` chooses storage. Neither implies the other.
`data_dir`, `api_key`, `mode`, `tui`, and `retention_days` are not TOML root fields.
Choose mode and operations with CLI arguments:

| CLI option | Purpose |
| --- | --- |
| `receive` | Decode compressed requests before forwarding |
| `--data-dir PATH` | Select runtime files for this instance |
| `--daemon` | Start in the background |
| `--status`, `--reload`, `--stop` | Control the daemon in the selected data directory |
| `--tui` | Start a dashboard or attach to an existing local instance; optional build feature |
| `--report --since SPAN` | Read a report; default window is 24h |
| `--model NAME` | Filter a report by recorded route name |
| `--retention-days N` | Delete older rows at startup and daily; default `0` keeps all rows |

`SPAN` accepts a positive integer followed by `s`, `m`, `h`, or `d`, such as
`90s`, `30m`, `24h`, or `7d`.
Daemon operations, `--report`, and `--tui` are mutually exclusive in one invocation.
See [operations](operations.md) for complete command sequences.
