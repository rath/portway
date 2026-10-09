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
[upstreams.toml](../examples/upstreams.toml) for vendor APIs mounted under
their own paths, [models.toml](../examples/models.toml) for a model routing
table, or [receive.toml](../examples/receive.toml) for a receiver.

Remote TUI attach (`--tui --attach URL`) skips local TOML loading and uses the
remote console's configuration and prices. Its `--data-dir` holds only viewer
sessions and display settings; see [remote attach](operations.md#remote-terminal-dashboard).

For forwarding and local dashboards, the standalone binary chooses its
configuration in this order:

1. If supplied, read exactly `--config PATH`. A missing file is an error.
2. Otherwise, try `portway.toml` in the **current working directory**.
3. If that file does not exist, try `portway.toml` in the runtime data
   directory: `--data-dir PATH` when given, otherwise `$XDG_CONFIG_HOME/portway`
   when that variable is absolute, or `~/.config/portway`. This is the directory
   that holds the daemon's database, so a dashboard started with the same
   `--data-dir` (or none) reads the same file as the daemon. An explicit
   `--data-dir` never falls back to the default directory's file.
4. If neither exists, use defaults and explicit command-line settings. Serving
   still requires a destination, such as `--upstream URL`.

The data directory's TOML is read no matter which directory the process is
launched from. Relative `--config` and `--data-dir` paths are resolved against
the launch directory once, at startup, so `--reload` rereads the same file after
the daemon has left that directory. A stable absolute path skips the search
entirely:

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
`--upstream URL` also clears a file's `[upstreams]` and `[models]` tables and
selects single-upstream mode. There is no flag that adds a table entry; edit
the TOML.
A file must still parse successfully before CLI overrides can be applied.

Relative config paths are read before daemonization. TOML strings do not expand
shell variables or `~`; expansion in the shell command above happens before
Portway sees its arguments. Unknown fields and invalid settings produce errors.
After editing an active daemon configuration, [reload or restart as
appropriate](operations.md#apply-configuration-changes).

## TOML basics that matter here

Root settings such as `host`, `port`, and `upstream` go **before** the first table
header. After `[upstreams]` or `[models]`, assignments belong to that table
until the next header. Use `#` for comments, quotes for string values, integers
for ports and byte counts, and quoted model keys for literal names containing
punctuation.

```toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"

[models]
"model-b.1" = "https://second.example.com"
"team/model-c" = "https://third.example.com"

[compression]
level = 3
```

An unquoted `model-b.1` is a TOML dotted key, not one literal model name. Quote the
name in price table headers too: `[prices."model-b.1"]`. Tables cannot be declared
twice, and duplicate keys are errors. To add a route, put a new entry inside
the existing table, before the next table header.

## Choose the routing mode

| Requirement | Configuration |
| --- | --- |
| One HTTP service, including arbitrary non-JSON requests | `upstream = "https://api.example.com"` |
| A vendor's API for that vendor's own client (Claude Code, Codex), or several such APIs on one listener, each under its own path | `[upstreams]` table |
| Several destinations selected by a JSON `model` field, for a client that sees one provider with many models | `[models]` table |
| Decode compressed requests in front of one or more applications | `receive` CLI mode with any of the above |

`upstream` stands alone: it is the one destination for everything. The two
tables combine on one listener: the path decides first, and a request outside
every mount is routed by its model. Even a one-entry `[models]` table requires
a matching model in requests; it is not an implicit default destination.

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

## Mount upstreams at path prefixes

A vendor's own client, such as Claude Code or Codex, speaks that vendor's API and lets
you set one thing: its base URL. Mount the vendor's API under a name, and set
the client's base URL to that mount:

```toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"
```

Each name answers under `/<name>/`. Portway removes that segment and forwards
the rest of the path, with its query, to the URL, appended to the URL's own
path prefix, as for any upstream:

| Configured | Incoming path | Upstream request |
| --- | --- | --- |
| `anthropic = "https://api.anthropic.com"` | `/anthropic/v1/messages` | `https://api.anthropic.com/v1/messages` |
| `codex = "https://chatgpt.com/backend-api/codex"` | `/codex/models?client_version=1` | `https://chatgpt.com/backend-api/codex/models?client_version=1` |
| `codex = "https://chatgpt.com/backend-api/codex"` | `/codex` | `https://chatgpt.com/backend-api/codex/` |

The choice is made from the path alone. The body is not read, so a request
without one (a model catalog, a health check) routes like any other, and a
model name the table has never heard of needs no entry: the client sends
whatever its vendor ships next. An already encoded body goes through a mount
as it came. For the client side, see [Claude Code and Codex](cli-setup.md).

Names are one path segment: letters, digits, `-`, `.`, `_` and `~`. The
segment `__portway` is reserved for management paths, and with a `[models]`
table on the same listener `v1` is refused, because it would hide the `/v1`
paths that table routes. A path outside every mount is a 404 that lists the
mounts, unless a `[models]` table routes it.

Each mount is one route, with its own connection pool, dictionaries and
counters, keyed by its name in `/__portway/stats`. Across a hop, mount the
same names on the receiver and let the sender's URL carry the name:

```toml
# sender
[upstreams]
anthropic = "https://gateway.example.com/anthropic"

# receiver
[upstreams]
anthropic = "https://api.anthropic.com"
```

The sender strips `/anthropic` and appends the rest to its URL's prefix, so the
receiver sees `/anthropic/v1/messages` and strips it again.

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

## Model display aliases

`[model_aliases]` gives long model IDs shorter names in the TUI and web
console, including events, in-flight requests, usage, history and model filters:

```toml
[model_aliases]
"vendor/model-a-long-name" = "model-a"
```

Keys match the original model ID exactly, including case. A matching route
label uses the same display name. Missing entries show the original ID; names
are substituted once, without alias chaining. Keys and names must be nonempty
and contain no control characters. Models do not have to appear in `[models]`:
aliases also work for mounted providers and historical records. Two IDs may
share a display name, but remain separate rows and filter selections.

This is presentation only. Requests still send the original ID, and routing,
`/v1/models`, stored records, pricing keys, CLI reports and CSV/JSON exports
keep it. Request details show both names when an alias applies; web model
labels expose the ID on hover. Web searches match either name.

A daemon's `--reload`, or the owning web console's Reload action, updates
aliases for existing as well as new events. Removing an entry restores the
original ID. Invalid configuration keeps the previous aliases and routes.
Attached local dashboards and remote TUIs receive the serving process's
aliases automatically; no duplicate client configuration is needed. Local
viewers fall back to their startup configuration if the server predates this
metadata; remote viewers of older servers show original IDs.

## Root settings and prices

| Root setting | Default | Meaning |
| --- | --- | --- |
| `host` | `"127.0.0.1"` | Listening address |
| `port` | `8787` | Listening port |
| `upstream` | unset | Single upstream HTTP(S) URL; exclusive with both tables |
| `[upstreams]` | empty | Name → upstream URL, mounted at `/<name>/` |
| `[models]` | empty | Model name → upstream URL, chosen by the JSON `model` field |
| `[model_aliases]` | empty | Original model ID → display name in dashboards |
| `[prices.NAME]` | absent | Optional `input`, `output`, `cache_read` rates in USD per million tokens |
| `[prices.NAME.tiers.TIER]` | absent | The same three rates for requests whose tier is `TIER` (their `speed`, else their `service_tier`) |
| `long_context = { above, input, output, cache_read }` | absent | In either table: the rates for a request whose prompt passes `above` tokens |

Prices are optional inputs to the TUI's usage and cost estimates. They do not
change requests, charge a balance, or discover provider rates. Tables for the
vendors' current list prices are in [Vendor prices](prices.md). For example:

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
numbers. Names match the **model the request named**, the string `model` of
its JSON body, whatever route it went through: under `upstream = "…"` or a
mount, a request for `claude-opus-5-5` is priced by `[prices."claude-opus-5-5"]`.
A request that names no model (a catalog, a health check) is recorded with an
empty model and stays unpriced. Each record also keeps the route it went
through (`upstream`, a mount's name, or a model's name), which is what the
dashboards' upstream tables and filters use; the two coincide only under a
`[models]` table. Rows recorded before this distinction carry their route name
as the model.

### Service tiers

Some vendors bill a faster or prioritized class of the same model at rates of
its own, chosen by a field in the request body: `service_tier` in OpenAI's
APIs, `speed` in Anthropic's. Portway records it as the request's tier and
gives each tier its own rates under the model's price:

```toml
# Illustrative numbers only; replace with your own comparison rates.
[prices."model-b.1"]            # requests that name no tier
input = 1.0
output = 2.0
cache_read = 0.1

[prices."model-b.1".tiers.tier-a]
input = 2.0
output = 4.0
cache_read = 0.2
```

The tier is the string `speed` of the request body, or else its string
`service_tier`, matched exactly. `speed` decides where a body names both,
because Anthropic prices by it while its `service_tier` asks for capacity, not
a price. Neither is always the name the client's settings use. The base
rates apply only to a request that names no tier; a request that names one
the table lacks, even `default` or `auto`, stays unpriced rather than being
billed at the base rates, and the usage screen counts it among the unpriced
rows. Each tier needs all three rates. The usage screens add up a model once
per tier its requests named, shown as `model-b.1 · tier-a`, and the request
detail names the tier beside the model.

The tier is taken from the request because a response does not reliably
report the class that served it. Rows recorded before Portway kept the tier
read as naming none and are priced at the base rates.

### Long context

A vendor may bill a request whose prompt passes a size at higher rates, for
all of that request's tokens rather than for the part past the line. Give a
set of rates its line as `long_context`, with the threshold in prompt tokens
(cache reads and writes included) and the rates beyond it:

```toml
# Illustrative numbers only; replace with your own comparison rates.
[prices."model-b.1"]
input = 1.0
output = 2.0
cache_read = 0.1
long_context = { above = 100000, input = 2.0, output = 3.0, cache_read = 0.2 }

[prices."model-b.1".tiers.tier-a]
input = 2.0
output = 4.0
cache_read = 0.2
long_context = { above = 100000, input = 4.0, output = 6.0, cache_read = 0.4 }
```

Each set of rates carries its own line, so a tier without one is priced at its
own rates whatever its size; there is no inheritance from the base rates. A
prompt of exactly `above` tokens is still short. The usage screens keep a
model's row whole and count the requests that passed the line in a note under
the table.

Missing prices remain unpriced. Token usage also depends on what the upstream
reports; a price table cannot fill in missing usage. TUI estimates use the
configuration loaded by that TUI invocation, including for historical records.
A daemon-hosted console rereads this table on `--reload`, the same signal that
rebuilds its routes. An attached viewer should receive the same `--config` if
you want the same prices; it keeps the ones it started with.
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

Start with `portway receive --config ./receive.toml`. Receiver mode requires an
`upstream`, an `[upstreams]` table or a `[models]` table. It restores incoming
compressed requests and forwards them to that application or, with a table,
to the origin [mounted under the request's first path segment](#mount-upstreams-at-path-prefixes)
or configured for its JSON `model` field, so one receiver can front several
providers that each speak their own API. By default the origin upload is
uncompressed; optional origin compression is described below. A path outside
every mount, or a model outside the table, is refused before any origin is
contacted.
`[receiver]` configures this mode; it does not activate the mode by itself.

| `[receiver]` setting | Default |
| --- | --- |
| `dictionary_bytes` | `134217728` (128MiB); zero disables dictionaries |
| `dictionaries_per_scope` | `16` bodies per authentication context |
| `dictionary_ttl_seconds` | `3600` idle seconds |
| `min_dictionary_bytes` | `32768` (32KiB) |
| `max_dictionary_bytes` | `33554432` (32MiB) |
| `max_body_bytes` | `268435456` (256MiB), for encoded and decoded bodies |
| `max_window_bytes` | `134217728` (128MiB) |

Every stored turn is a whole conversation, and a sender only ever names its own
last eight confirmed bodies per route, so a context's older bodies are evicted
first: `dictionaries_per_scope` keeps its newest sixteen, and `dictionary_bytes`
bounds the total across contexts, least recently used first. Raise the budget
for a receiver shared by many credentials, or the count when one credential
fronts several routes.

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

### Receiver-to-origin upload compression

The sender-to-receiver and receiver-to-origin legs negotiate independently.
To enable adaptive upload compression on the receiver's configured `upstream`:

```toml
[receiver.origin_compression]
mode = "auto" # default: "off"
```

`auto` tries gzip for an eligible request when support is unknown. It learns
from the origin's response `Accept-Encoding`, including on successful responses.
That response header advertises **request** content codings for subsequent
requests to the resource ([RFC 9110 §12.5.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-12.5.3)).
The request's `Accept-Encoding` still negotiates response compression separately.
No Portway capability or health probes are sent to the origin in this mode.

Supported origin codings are gzip and zstd. Quality weights, explicit exclusions,
and an empty advertisement are respected; gzip wins equal weights. Zstd requires
an explicit advertisement. Without an advertisement, a successful compressed
request establishes support for its codec. A missing response header alone does
not establish support or erase a previous advertisement.

Compression uses `[compression] level`, `min_bytes`, and `max_body_bytes`.
Bodies must shrink; empty bodies, GET/HEAD, `Cache-Control: no-transform`, and
requests carrying content digest or HTTP signature headers are not recompressed.
Portway dictionaries and their headers are confined to the sender-to-receiver leg.
`[compression] coding` and `dict` do not control this separate origin policy.

If a request Portway compressed receives **400 or 415**, the receiver remembers
the refusal and retries once with its original, uncompressed body. The client
receives the retry's response, including any error it returns. An explicit
response advertisement excluding identity prevents that retry. Uncompressed
requests, authentication errors, rate limits, 5xx responses, and transport
failures do not cause an identity retry. This policy treats 400 and 415 on
compressed uploads as possible encoding refusals; the retry is bounded to one
attempt, even if the origin rejects the original body too.

Refusals suspend compression for 600 seconds. After expiry, one request trials
gzip while concurrent requests use identity. The same single-trial rule applies
when support is initially unknown. Origin capability entries expire after 600
seconds without refresh. State is isolated by upstream, method, request target
(including query), authentication context (including `X-API-Key`), content type,
and Anthropic version/beta headers. At most 1024 hashed contexts are kept per
upstream; neither bodies nor credentials are retained in this cache.

`--reload` applies this policy immediately for new requests. Learned state survives
reload when the destination, policy, level, and body thresholds remain the same;
changing them starts fresh. Restarting the process also clears learned state.
See [origin compression diagnostics](operations.md#check-receiver-to-origin-compression)
for per-hop counters and refusal logs.

## Runtime files and command-only options

`--data-dir PATH` overrides `$XDG_CONFIG_HOME/portway` (when XDG_CONFIG_HOME is
absolute) or `$HOME/.config/portway`. Newly created directories have mode 0700.
The runtime directory contains `db.sqlite3`, `portway.pid` in daemon mode,
`portway.log` for daemon output, `portway.tui` for saved dashboard settings, and
`portway.web` (mode 0600) with the running web console's address and token.
Remote TUI viewers save console sessions in `remote-sessions.json` (mode 0600);
they do not create a local database. Console tokens and session secrets live
in `web-auth.json` (mode 0600), with creation serialized by `web-auth.lock`.
They are isolated by console port and base path and survive restarts in the
same data directory. See [resetting console access](operations.md#optional-web-console)
for explicit revocation.
Old runtime directories are not discovered or automatically migrated.

`--data-dir` chooses storage and, without `--config`, where the search looks
after the working directory. `--config` chooses TOML and never moves storage.
`data_dir`, `api_key`, `mode`, `tui`, `attach`, `web`, and `retention_days` are not TOML root fields.
Choose mode and operations with CLI arguments:

| CLI option | Purpose |
| --- | --- |
| `receive` | Decode compressed requests before forwarding |
| `--data-dir PATH` | Select runtime files for this instance |
| `--daemon` | Start in the background |
| `--status`, `--reload`, `--stop` | Control the daemon in the selected data directory |
| `--tui` | Start a dashboard or attach to an existing local instance; optional build feature |
| `--tui --attach URL` | View a remote web console over HTTP/HTTPS; prompts for its token once and remembers the session |
| `--web` | Serve the dashboard in a browser, or attach to an existing local instance; combines with `--daemon`; optional build feature |
| `--web-host HOST`, `--web-port PORT` | Where the console listens; default `127.0.0.1` and `8790`, `0` for a free port |
| `--web-allow-host NAME` | A name the console answers to besides `localhost` and IP addresses; repeat or separate with commas. `--web-host` counts when it is a name |
| `--no-open` | Print the console's link without opening it in the default browser |
| `--report --since SPAN` | Read a report; default window is 24h |
| `--model NAME` | Filter a report by the model requests named |
| `--retention-days N` | Delete older rows at startup and daily; default `0` keeps all rows |

`SPAN` accepts a positive integer followed by `s`, `m`, `h`, or `d`, such as
`90s`, `30m`, `24h`, or `7d`.
Daemon operations, `--report`, and `--tui` are mutually exclusive in one invocation;
`--web` excludes `--tui`, `--report` and the daemon controls but combines with `--daemon`.
See [operations](operations.md) for complete command sequences.
