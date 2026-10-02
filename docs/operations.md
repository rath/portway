# Operations

The commands below assume `portway` is installed on your `PATH`. See
[getting started](getting-started.md) for installation and the first config file.

## Recording and reports

The CLI writes completed-request metadata and log events to `db.sqlite3`, in
foreground, daemon, and live TUI modes. It records timings, route, method, path,
status, sizes, codecs, completion status, and any reported OpenAI token usage.
It does not record request/response bodies, headers, keys, or dictionary contents.
An unavailable data directory or database prevents startup.

The recorder uses a bounded queue so disk I/O does not block forwarding. A full
queue drops observations and emits periodic warnings. Shutdown flushes the queue.
The data directory is private (0700); see [configuration](configuration.md) for
its location and retention settings.

```sh
portway --report
portway --report --since 7d --model model-a
portway --data-dir /path/to/runtime --report --since 30m
```

The text report shows request totals, transferred bytes, timings, and recent
trouble. It does not load TOML or calculate prices. Use the TUI's usage view for
token and cost estimates, passing the configuration containing your prices.

In that view, no reported usage, missing cache detail, and an unpriced model are
distinct: unknown values do not become zero. Cost estimates use optional configured
rates, with reasoning tokens included in output tokens rather than charged twice.
Changing those configured rates also changes estimates for historical records.

## Daemon lifecycle

```sh
portway --config examples/forward.toml --daemon
portway --status
portway --reload
portway --stop
```

Use the same `--data-dir` for each command when it is customized. A locked pid
file prevents two daemons from owning one runtime directory. Startup reports its
pid and log path. The process forks before creating a runtime or recorder thread.

`--reload` sends SIGHUP, reopens the log, rereads the config file, rebuilds the
router and the price table, and renegotiates upstream capabilities. Routes and
rates are published only after the new router has been built and probed, so the
console a daemon hosts reprices on the reload itself: an edited rate needs no
restart. A locally attached TUI reads its own `--config`; restart that viewer
to load changed prices. A remote TUI (`--tui --attach URL`) uses the console's
prices and picks up the daemon's reload without restarting the viewer.
`--stop` sends SIGTERM and waits up to ten seconds. The process flushes recording
and removes its pid file. Controls do not require upstream configuration.

Daemon mode detaches the process; it does not install a system service or arrange
automatic startup after a reboot. `--status` uses the daemon's pid file; a
foreground server has no daemon pid file, so check its HTTP health instead.

## Apply configuration changes

Model additions, destination changes, price changes, and compression settings
can be applied with `--reload` in daemon mode. Listener host/port changes still
require a restart because the socket is already bound.
For a daemon using the default runtime directory:

```sh
# Edit your existing portway.toml first.
portway --stop
portway --config ./portway.toml --daemon
portway --status
curl --fail-with-body http://127.0.0.1:8787/__portway/health
# Model-routing mode only: confirm the new registered names.
curl --fail-with-body http://127.0.0.1:8787/v1/models
```

Stopping terminates forwarding, so choose a quiet moment and let active requests
finish before a restart. Runtime counters and in-memory dictionaries reset; the
SQLite history stays in the same data directory. Reopen locally attached
dashboards with the updated config to load changed prices, host, or port.
Remote TUI viewers reconnect with their saved session after a daemon restart;
they do not load local forwarding configuration. For a foreground instance, use
Ctrl-C and rerun its command.

## Separate instances

Give each instance a different listening port **and** data directory. Keep using
the same data directory for that instance's controls and reports:

```sh
portway --config ./portway.toml --port 8789 --data-dir ./trial-data --daemon
portway --data-dir ./trial-data --status
portway --data-dir ./trial-data --report --since 30m
# Optional dashboard; use the same config, port, and directory.
portway --config ./portway.toml --port 8789 --data-dir ./trial-data --tui
# Run after closing the viewer:
portway --data-dir ./trial-data --stop
```

These relative paths assume you stay in the same directory. Use absolute paths
when invoking commands from elsewhere. A different `--config` alone does not
select a different daemon; controls select their pid file using `--data-dir`.

## Optional terminal dashboard

```sh
cargo build --release --locked --features tui
./target/release/portway --config examples/models.toml --tui
```

With a free port, the dashboard starts and observes its own forwarder. With an
existing Portway on that port, it watches the same data directory's recorded
window and reads current requests from its local Unix socket. A viewer neither
owns the listener nor writes the database. Its configuration supplies any prices used for estimates. To attach with default
host, port and data directory, `portway --tui` needs no upstream configuration.

The dashboard displays request/byte totals, route counters, body sizes, socket
throughput, latency, and recent events. Live counters are per running instance;
a viewer reconstructs its window from recorded rows. Usage and costs are read
from the database for today, yesterday, seven days, or thirty days.

The model table shows at most three recently used models, prioritizing models
with requests in flight, then the latest completed requests. Unused configured
models are hidden. The title shows the displayed and total model counts; HUD
totals and event filters still cover every model. This also applies to remote
attach. Press `f` to see all in-flight requests or `u` for per-model usage.

Press `f` for the **in flight** dialog, which refreshes every 250ms, both when
the dashboard owns the forwarder and when it attaches to a local server: model,
phase (upload, prefill, stream), elapsed time and received bytes. From about 70
columns the method and path appear; from about 100, the status, time to first
byte, idle time and retry count. The dialog is as wide as those columns, not
the terminal: a long model name or route widens it into the spare room, and one
that still does not fit ends in `…`. Prefill over 30s is marked `slow prefill`;
a stream idle for over 60s is marked `stalled`. In an owning dashboard,
completed requests leave as their event arrives. In an attached viewer,
completed or cancelled requests leave on the next successful snapshot.

The dialog lists requests oldest first, as many as fit in two thirds of the
terminal's height, and counts the rest in its title. It is placed as though it
held eight rows, just above the middle of the screen, so up to eight requests
can arrive and leave without moving its title; past that it is centred. The
dashboard layout never changes with the number of active requests. While it is
open it takes the keyboard: `f`, `Esc` or `q` close it. With it closed, the
HUD's `live` count flags stalled and slow requests, for example
`live 3 (1 stalled)`. Event filters and scrolling do not hide active requests.
Local attach keeps historical request counts, bytes, charts and events sourced from
SQLite; live snapshots supplement the HUD and existing model rows without
changing the database schema.

Every CLI build serves `<data-dir>/portway.live.sock`, with mode `0600` and
same-UID peer checks. Each connection returns one versioned JSON snapshot with
a process instance ID, the actual listening address, total and per-model counts,
and at most the oldest 200 requests. The table's additional count includes
requests beyond that limit. No bodies, headers or query strings are sent.
Attach checks the server address/port and replaces the entire snapshot, including
after a server restart; it never matches live IDs to historical DB records.

Socket queries run separately from terminal rendering and database reads. They
do not overlap, time out after 500ms, and retry every second after failure. A
failure clears the previous list and shows `in flight unavailable`, distinct
from a successful `live 0`; history and keyboard input remain available.
Responses are limited to 1MiB and the server permits eight concurrent clients,
each with a 500ms deadline. Socket setup failures warn without stopping
forwarding. The permanent `portway.live.lock` file controls socket ownership;
do not remove it while a server is running. Only its lock holder cleans up a
stale socket.

Use the same `--data-dir`, host and port as the server. Older servers still
provide recorded history; restart them with the new binary to enable live
attach. An attached viewer's `q` only closes the viewer, even during active
requests. Web attach continues to show recorded history only.

### Remote terminal dashboard

Run the TUI locally and point it at the remote daemon's **web console URL**:

```sh
portway --tui --attach https://console.example/portway/
```

The remote daemon must have `--web` enabled; the local binary only needs the
`tui` feature. HTTP and HTTPS are supported, including a console published
under `--web-base-path`. Use the console address, not the forwarding port.
An existing reverse proxy keeps its path and Host/Origin rewrites unchanged.
HTTPS uses the same certificate verification as Portway's upstream client;
redirects are not followed.

On the first connection, paste just the console token at the hidden prompt.
Use the current token from the remote console's launch URL or its private
`portway.web` file. Do not put a token in the `--attach` argument. The viewer
exchanges it for a session and stores **only the session cookie**, with mode
`0600`, in `<data-dir>/remote-sessions.json`. Sessions are isolated by scheme,
host, port, and URL prefix. Later connections to that URL need no prompt.

Console tokens and sessions survive restarts when the server keeps its data
directory, console port and base path. An open viewer reconnects automatically;
starting it again reuses its saved session. If console access was explicitly
reset, an open viewer restores the terminal and exits with a sign-in message,
removing its invalid saved session. To forget saved sessions locally, remove
`remote-sessions.json` while no remote viewer is running.

Counters, latency, charts and active requests come from the remote console.
The event pane loads up to the latest 10,000 events the console still retains
in memory, then follows its event stream; this is not an export of its SQLite
history. Usage (`u`) and cost breakdown (`p`) query the remote database through
the console API, using the remote server's prices and calendar boundaries.
A console that itself runs in attached mode cannot supply in-flight requests;
the TUI displays them as unavailable.

On a network interruption, the viewer marks its retained data as stale, clears
its active-request list, and reconnects with a delay from 2 to 30 seconds.
A fresh snapshot restores counters, charts and events without adding the
same requests twice. `q` and Ctrl-C only close the viewer; they never stop the
remote daemon or interrupt its requests.

`--data-dir` chooses where the viewer stores its session and display settings,
and `--event-columns` and `--theme` still work. Remote attach does not load the local
`portway.toml`, read or create a local database, probe a local daemon, or bind a
forwarding port. Server options such as `--config`, `--host`, `--port`, and
`--upstream` cannot be combined with `--attach`. A shell alias or wrapper that
adds `--config` automatically must omit it for remote attach.

### Dashboard keys and display settings

| Key | Action |
| --- | --- |
| `q` | Quit; a live server asks before interrupting active relays |
| `↑` / `↓`, `j` / `k`, mouse wheel | Scroll events |
| `PgUp` / `PgDn`, `g` / `G` | Page, oldest, or live end |
| `Enter` | Request details |
| `f` | Requests in flight |
| `e` / `m` | Trouble-only filter / upstream filter |
| `t` | Throughput bucket width |
| `u` | Usage and cost view |
| `←` / `→` in usage | Change the date window |
| `p` in usage | Cost breakdown |
| `c` | Event column picker |
| `T` | Color theme picker |
| `?` / `Esc` | Help / close popup |

`--event-columns time,route,tokens` sets columns for one invocation. The picker
persists interactive choices in `portway.tui`. Narrow terminals reduce detail
rather than truncating every field. Ctrl-C, SIGTERM and terminal closure restore
the terminal. Quitting a viewer leaves the serving process alive.

`T` chooses the dashboard's colors: `terminal`, the sixteen colors of whatever
theme the terminal already has, or one of the web console's palettes under the
same names (`portway-dark`, `catppuccin-mocha`, `catppuccin-latte`,
`tokyo-night`, `nord`, `dracula`, `gruvbox`), which paint their own background.
Moving the cursor previews each one on the whole dashboard; `Enter` keeps it in
`portway.tui` and `Esc` puts the previous one back. `--theme catppuccin-latte`
sets one for a single invocation. With no theme saved, the dashboard starts in
Catppuccin Mocha when `COLORTERM` is `truecolor` or `24bit`, and in `terminal`
otherwise. The palettes need 24-bit color: on a terminal without it, or over an
SSH session that does not pass `COLORTERM` along, choose `terminal` or set
`COLORTERM`.

`--tui`, `--attach`, `--event-columns` and `--theme` are absent in builds
without the `tui` feature.

## Optional web console

```sh
cargo build --release --locked --features web
./target/release/portway --config examples/models.toml --web
# portway: console at http://127.0.0.1:8790/#token=…
```

`--web` runs the forwarder as usual and serves a dashboard in the browser on a
second listener, `--web-host` (default `127.0.0.1`) and `--web-port` (default
8790; `0` picks a free port). It opens in the default browser when the link is
printed to a terminal, outside an SSH session, and (other than on macOS) with
a display; otherwise, or with `--no-open`, open the printed link. The link's
token is exchanged once for a session cookie and removed from the address bar;
the page never stores it. The browser the console opens is handed a one-time
launch code instead of the token, since a command line is visible to other
local users. Like `--tui`, a console started on a port another Portway already serves
attaches to that instance's data directory instead of starting a forwarder,
and a bare `portway --web` needs no upstream configuration to attach.

To reach the console from another machine, bind it to every interface with
`--web-host 0.0.0.0` (or `::`). The console then prints one link per address:
loopback first, then each address of an interface that is up (link-local ones
left out), then each name given with `--web-allow-host`. The console answers
only to `localhost`, IP addresses, a `--web-host` that is a name, and the names
`--web-allow-host` lists (repeat it or separate names with commas), such as the
machine's host name or its Tailscale MagicDNS name; any other name in the
`Host` header is refused, which is what keeps DNS rebinding out.

```sh
portway --daemon --web --web-host 0.0.0.0 --web-allow-host sender-host
# portway: console at http://127.0.0.1:8790/#token=…
# portway: console at http://192.168.0.10:8790/#token=…
# portway: console at http://sender-host:8790/#token=…
```

To publish the console through a reverse proxy at a subpath, pass the path
with `--web-base-path`, such as `--web-base-path /portway`. The console then
emits its assets and API calls under that prefix and answers 404 outside it,
so the proxy forwards the path unchanged instead of stripping it. The `Host`
the proxy forwards must be one the console answers to: rewrite it, or name the
public host with `--web-allow-host`. The console accepts a request only from
its own origin, `http://` plus that `Host`, so the proxy must also rewrite
`Origin` to that value, even when the browser reached the proxy over HTTPS.
Without the `Origin` rewrite every page loads but signing in fails with 403.

`--daemon --web` hosts the console in the daemon without opening a browser. The
launcher prints the links next to the log path, `--status` prints them again while the console runs, and they
are kept in `portway.web` (mode 0600) in the data directory. The log only ever
contains the address without the token.

The token and a separate session secret are saved in `web-auth.json` (mode
0600), keyed by console port and base path. Keep this file across deployments;
`portway.web` remains the temporary discovery file for the running console.
Browser and remote TUI logins survive server restarts. The browser uses an
HttpOnly, SameSite=Strict persistent cookie with a rolling 400-day storage
lifetime, renewed by authenticated API responses and daily while a tab stays
open. The server does not expire sessions on a timer. Clearing browser cookies
or the TUI's local cache requires signing in again.

An open browser reconnects after a deployment, fetching a fresh snapshot so
counters and events reflect the new process. During the outage it marks the
retained data as stale and clears in-flight requests. Clicking Stop in that
browser deliberately leaves it stopped; reload the page after starting Portway
again. Authentication failures ask for a sign-in link rather than retrying.

To **reset all console access** for a data directory, stop every console using
that directory, delete only `web-auth.json`, then restart them. This revokes
all previous tokens and sessions and generates new ones. Do not delete the file
while a console is running: it still has its credentials in memory. A corrupt
or incorrectly permissioned auth file is refused, never silently replaced;
restore it or use this explicit reset procedure. Initial upgrading from a
version with per-run authentication requires one final sign-in in each browser
and remote TUI. Later deployments need none.

The console shows everything the terminal dashboard shows, computed by the
same code, with these views and additions:

| View | What it adds |
| --- | --- |
| Dashboard | Requests in flight, with their phase (upload, prefill, stream), age and progress; a slow prefill (over 30s) or a stalled stream (no bytes for 60s) is flagged. A console attached to another instance cannot see that instance's flights. |
| Events | An aligned table with a header per field and each request's full method and path, where the terminal shortens known routes to fit; a field no event has filled yet takes no room. Downloads read like uploads: `down` is the answer decoded, `↓ wire` and `↓ saved` what the upstream hop carried and kept off the wire (behind a receiver, its download saving), and `to agent` the leg to the agent when the agent asked for a coding. Drag the edge of a header to set a column's width; double-click it, or run "Reset column widths", to fit the content again. Widths are kept per browser. Search (`status:5xx`, `model:`, `upstream:`, `route:`, `is:cut`, `ttfb:>2s`, `size:>1MB`, `tok:>50K`, `-word`, `"phrase"`), CSV and JSON export of the filtered lines, and older lines on request. A model catalog fetch (`GET …/models`, which Codex sends each time it starts) counts in every total but is not listed unless it failed; `is:catalog` lists them. |
| Usage | The terminal's usage screen, refreshed every 5s. |
| History | `--report` for any window, per model, as tables with CSV, or as the exact text. |
| Insights | ttfb percentiles and upload savings since the page opened, and each model's share. |
| Appearance | 15 themes plus one that follows the OS, density, type and motion; kept per browser. |

Reload rereads the configuration the way `--reload` does. Stop ends a live
console's forwarder the way SIGTERM does, after listing the requests it would
cut; an attached console signals the daemon it is watching and stays open.
The keys match the terminal dashboard (`q` asks before stopping; a second `q`
confirms), with `/` for search, `?` for the key list and ⌘K or Ctrl-K for every
command. Desktop notifications for trouble in a background tab are opt-in.

`--web`, `--web-host`, `--web-port`, `--web-allow-host`, `--web-base-path` and
`--no-open` are absent in builds without
the `web` feature. See [security](../SECURITY.md#web-console) before binding the
console to anything other than loopback.

## Health, model lists, and logs

| Check | What it tells you |
| --- | --- |
| `portway --status` | Whether the daemon recorded in this data directory is running |
| `GET /__portway/health` | Whether the local listener is serving; no upstream health test |
| `GET /v1/models` in model mode | Which names are configured locally; no upstream discovery |
| `GET /__portway/stats` | Counters and negotiated compression for each route since process start |
| A real authenticated application request | Whether routing, credentials, and that upstream operation work |

The `/__portway/` paths are reserved local management paths. Ordinary `/health`
is an upstream request; in model mode it needs a `?model=...` selector.
Receiver mode also exposes `/__portway/capabilities` and adds decoder/dictionary
counters to stats.

For the default runtime directory, read the daemon log with:

```sh
tail -n 50 "$HOME/.config/portway/portway.log"
```

If you set `--data-dir` or an absolute `XDG_CONFIG_HOME`, use that directory
instead. Foreground logs go to the terminal. The database records completed
requests; an active stream may not appear until it finishes. Locally generated
model lists, health responses, and route-validation errors do not represent
forwarded requests and are not recorded as such.

## Check compression

To confirm that your build compresses at all, independent of your network and
gateway, run `python3 scripts/bench.py` from the checkout. It starts a local
sender, receiver, and origin and exits nonzero if dictionaries are not used;
see [measure it yourself](../README.md#measure-it-yourself).

Inspect sender statistics after sending application requests:

```sh
curl --fail-with-body http://127.0.0.1:8787/__portway/stats
```

The `upstreams` object is keyed by route name: a mount's name, a model's name,
or `upstream` in single-upstream mode. Useful fields include:

| Field | Interpretation |
| --- | --- |
| `coding` | Sender-negotiated request coding; `null` means identity. Origin auto selects per context; see below. |
| `encoded_requests` | Requests actually sent with compression |
| `body_bytes`, `wire_bytes`, `saved_bytes` | Original body size, sent body size, and savings counters |
| `down_bytes`, `down_wire_bytes`, `down_saved_bytes` | Answers decoded, as they crossed the upstream hop, and the difference: behind a receiver, its download saving. It is counted whatever the agent asked for; `agent_bytes` is the separate leg to the agent, re-encoded only when the agent offered a coding |
| `dict` | Whether dictionary support is enabled for the route |
| `dict_hits`, `dict_misses` | Dictionary reuse and misses reported during forwarding |
| `identity_reason` | Why request compression is off: `not_negotiated`, `configured_off`, `probe_failed`, `no_supported_coding`, or `encoding_refused`; `null` when compression is enabled |
| `identity_backoff_secs` | Remaining backoff after a marked encoding 415, rounded up to seconds |
| `dict_backoff_reason`, `dict_backoff_secs` | Dictionary suspension cause (`dictionary_refused`, `hash_mismatch`, or `encoding_refused`) and remaining backoff; the cause clears when DCZ is enabled again |
| `last_probe_ok`, `probe_failures` | Result of the last capability probe (`null` before probing) and cumulative failed probes; a failed background probe preserves the last negotiated coding |
| `dict_hash_mismatches` | Cumulative storage acknowledgements whose hash differs from the sent body |
| `in_flight` | Active relays; useful when choosing a restart time |
| `upstream_errors`, `client_aborts` | Upstream failures and clients that disconnected |

For sender capability negotiation, refusal backoffs last 600 seconds. A zero remaining time means the backoff has
expired, not that compression has recovered: requests trigger background probes,
and a successful capability advertisement must enable the coding again. A
dictionary-only refusal or hash mismatch keeps ordinary zstd compression enabled.
The backoff cause remains available while waiting for recovery. An unmarked
application or gateway 415 does not trigger a backoff.

At terminal widths of at least 136 columns, the live TUI's model table includes a
`compression status` column showing the cause and remaining backoff, or `probe due`
after expiry. An attached viewer reconstructs historical requests and does not
have these live diagnostics; use the serving instance's stats endpoint instead.

A negotiated codec alone does not mean every request will be compressed. Small
bodies below `min_bytes`, already encoded bodies, and bodies that do not shrink
are sent without additional compression. DCZ also needs an acknowledged base
body, a matching authentication context, and a subsequent suitable request. The
receiver's default minimum dictionary size is 32KiB. A short “Hello” request
usually cannot demonstrate dictionary reuse.

If `coding` stays `null`, inspect the receiver or gateway's public capability
endpoint without a key:

```sh
curl --fail-with-body -i https://gateway.example.com/__portway/capabilities
```

A compatible receiver advertises `zstd`/`gzip` and, when enabled, `dcz`. A 404, an
authentication challenge, or an ordinary health response with no advertisement
cannot enable compression. Setting `--coding zstd` does not bypass negotiation.
After fixing discovery, `--reload` on the sender daemon rereads the config and
requests fresh probes.

Dictionary storage is memory-only, so restarts, expiry, eviction, or rotated
credentials may require warm-up again. A recognized dictionary miss falls back
to ordinary compression according to the [retry protocol](protocol.md#errors-and-replay).

## Troubleshooting

| Symptom | Check or action |
| --- | --- |
| `configure an upstream, an [upstreams] table or a [models] table` | The file names no destination. If it was meant to, check the current directory or pass `--config`. |
| `upstream is the one destination for everything` | Remove `upstream` to route by `[upstreams]` or `[models]`, or remove the tables to send everything to it. |
| 404 says no upstream is mounted at the path | The client's base URL must end in a mount name from `[upstreams]`, such as `http://127.0.0.1:8787/anthropic`; the message lists the mounts. |
| TOML complains about a table or type for a model | Quote names containing dots or slashes, including in `[prices."name"]`. |
| Unknown field such as `api_key`, `data_dir`, or `mode` | Keys belong in client headers; runtime directory and mode are CLI options. |
| Address already in use | Stop the process owning the port, choose another port, or use `--tui` or `--web` to view an existing Portway. For `--web ...: Address already in use`, choose another `--web-port`. |
| `--status` says stopped but requests succeed | Check the selected data directory and whether this is a foreground instance. |
| 400 asks for a supported model | Match a registered name exactly and put it in the JSON object; there is no implicit default. |
| 415 says routing needs an uncompressed body | Disable client-side request compression in model mode, or use single-upstream mode. |
| 404 from the upstream | Check path composition, especially a duplicated `/v1` prefix. |
| 401 or 403 from the upstream | Send the destination's actual key from the client; see [authentication](authentication.md#if-authentication-fails). |
| Edited routes or rates do not appear | Run `--reload` with the serving daemon's data directory and check the log for `configuration reloaded`; restart if the listener host or port changed, or if the viewer is an attached one reading its own config. |
| `--tui` is an unrecognized argument | Rebuild or install with `--features tui`, and check which executable your shell finds. |
| Dashboard needs a terminal | Run `--tui` directly in a terminal; use `--report` for redirected output. |
| `--web` is an unrecognized argument | Rebuild or install with `--features web`. |
| The console asks for the printed address | The session belongs to one run: open the link the current run printed, or `portway --status --data-dir …` for a daemon's. |
| The web console shows no requests in flight | Web attach reads another instance's database; use an attached TUI or the serving process's own console for live flights. |
| TUI says `in flight unavailable` | Check the same data directory, host/port and user; restart an older server with the new binary and check its log for socket setup warnings. Recorded history remains available. |
| Dashboard has no prices or an empty history | Pass its price-bearing config and the serving instance's data directory; also check the selected date window. |
| Health is OK but requests fail | Local health does not check upstream reachability, model availability, or credentials. Test a real request. |
| Compression or DCZ is absent | Follow [compression checks](#check-compression); a working API alone does not imply support. |

## Check receiver-to-origin compression

With `[receiver.origin_compression] mode = "auto"`, read the **receiver's**
`/__portway/stats`. Its `receiver` object counts decoding of the incoming hop;
`upstreams.upstream` describes forwarding to the origin (replace the second
`upstream` with the route's name when the receiver has a table). The origin
codec varies by request context, so the route-level negotiated `coding` is not
an aggregate origin capability. Use `upstreams.upstream.origin_compression`
instead:

| Field | Meaning |
| --- | --- |
| `mode` | `auto` or `off`; only the mode is present when disabled |
| `attempts`, `encoded_attempts` | Origin send attempts, including identity retries, and attempts with an encoding |
| `wire_bytes` | Request-body bytes handed to HTTP across all attempts, including refusals and failed/cancelled requests |
| `retried_identity`, `refusals` | Identity retries and cached compression refusals |
| `cache_entries` | Number of retained request contexts, up to 1024 |
| `gzip_entries`, `zstd_entries` | Unexpired contexts learned to use each codec |
| `backoff_entries`, `probing_entries` | Contexts temporarily suspended or currently trialling compression |

These origin counters and learned entries survive a compatible reload; ordinary
route counters reset when the router is rebuilt. All state resets on restart.
Wire counts exclude HTTP/TLS framing and TCP retransmissions and measure bytes
handed to the transport, not a guarantee that the peer received them.

The receiver's request log reports the final upload coding (`gzip`, `zstd`, or
`identity`). Its upload byte count includes a rejected attempt before identity
fallback; the failed trial can therefore produce negative savings. The warning
`origin 415: retrying identity once` explains the retry. A preceding rejection
warning names the attempted coding and the 600-second suspension. A 400 produces
only the suspension warning and returns the original error.

Existing `upstreams.upstream.wire_bytes` includes refused attempts for requests that
obtained a final response. The nested origin `wire_bytes` also includes bytes
handed to HTTP before a transport error or cancellation. No origin URL, request
body, or authentication value is exposed by the capability cache diagnostics.
