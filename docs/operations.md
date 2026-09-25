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
router, and renegotiates upstream capabilities. The new router is published only
after it has been built and probed.
`--stop` sends SIGTERM and waits up to ten seconds. The process flushes recording
and removes its pid file. Controls do not require upstream configuration.

Daemon mode detaches the process; it does not install a system service or arrange
automatic startup after a reboot. `--status` uses the daemon's pid file; a
foreground server has no daemon pid file, so check its HTTP health instead.

## Apply configuration changes

Model additions, destination changes, and compression settings can be applied
with `--reload` in daemon mode. Listener host/port changes still require a
restart because the socket is already bound.
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
SQLite history stays in the same data directory. Reopen attached dashboards with
the updated config to load changed prices, host, or port. For a foreground
instance, use Ctrl-C and rerun its command.

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
window. A viewer neither owns the listener nor writes the database. Its
configuration supplies any prices used for estimates. To attach with default
host, port and data directory, `portway --tui` needs no upstream configuration.

The dashboard displays request/byte totals, route counters, body sizes, socket
throughput, latency, and recent events. Live counters are per running instance;
a viewer reconstructs its window from recorded rows. Usage and costs are read
from the database for today, yesterday, seven days, or thirty days.

| Key | Action |
| --- | --- |
| `q` | Quit; a live server asks before interrupting active relays |
| `↑` / `↓`, `j` / `k`, mouse wheel | Scroll events |
| `PgUp` / `PgDn`, `g` / `G` | Page, oldest, or live end |
| `Enter` | Request details |
| `e` / `m` | Trouble-only filter / model filter |
| `t` | Throughput bucket width |
| `u` | Usage and cost view |
| `←` / `→` in usage | Change the date window |
| `p` in usage | Cost breakdown |
| `c` | Event column picker |
| `?` / `Esc` | Help / close popup |

`--event-columns time,route,tokens` sets columns for one invocation. The picker
persists interactive choices in `portway.tui`. Narrow terminals reduce detail
rather than truncating every field. Ctrl-C, SIGTERM and terminal closure restore
the terminal. Quitting a viewer leaves the serving process alive.

Both `--tui` and `--event-columns` are absent in builds without the `tui` feature.

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

The `models` object is keyed by route name (`upstream` in single-upstream mode).
Useful fields include:

| Field | Interpretation |
| --- | --- |
| `coding` | Currently negotiated request coding; `null` means identity (no request compression) |
| `encoded_requests` | Requests actually sent with compression |
| `body_bytes`, `wire_bytes`, `saved_bytes` | Original body size, sent body size, and savings counters |
| `dict` | Whether dictionary support is enabled for the route |
| `dict_hits`, `dict_misses` | Dictionary reuse and misses reported during forwarding |
| `identity_reason` | Why request compression is off: `not_negotiated`, `configured_off`, `probe_failed`, `no_supported_coding`, or `encoding_refused`; `null` when compression is enabled |
| `identity_backoff_secs` | Remaining backoff after a marked encoding 415, rounded up to seconds |
| `dict_backoff_reason`, `dict_backoff_secs` | Dictionary suspension cause (`dictionary_refused`, `hash_mismatch`, or `encoding_refused`) and remaining backoff; the cause clears when DCZ is enabled again |
| `last_probe_ok`, `probe_failures` | Result of the last capability probe (`null` before probing) and cumulative failed probes; a failed background probe preserves the last negotiated coding |
| `dict_hash_mismatches` | Cumulative storage acknowledgements whose hash differs from the sent body |
| `in_flight` | Active relays; useful when choosing a restart time |
| `upstream_errors`, `client_aborts` | Upstream failures and clients that disconnected |

Refusal backoffs last 600 seconds. A zero remaining time means the backoff has
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
| `configure either upstream or [models], exclusively` | Configure exactly one routing mode. If neither was intended to be empty, check the current directory or pass `--config`. |
| TOML complains about a table or type for a model | Quote names containing dots or slashes, including in `[prices."name"]`. |
| Unknown field such as `api_key`, `data_dir`, or `mode` | Keys belong in client headers; runtime directory and mode are CLI options. |
| Address already in use | Stop the process owning the port, choose another port, or use `--tui` to view an existing Portway. |
| `--status` says stopped but requests succeed | Check the selected data directory and whether this is a foreground instance. |
| 400 asks for a supported model | Match a registered name exactly and put it in the JSON object; there is no implicit default. |
| 415 says routing needs an uncompressed body | Disable client-side request compression in model mode, or use single-upstream mode. |
| 404 from the upstream | Check path composition, especially a duplicated `/v1` prefix. |
| 401 or 403 from the upstream | Send the destination's actual key from the client; see [authentication](authentication.md#if-authentication-fails). |
| Edited routes do not appear | Run `--reload` with the serving daemon's data directory and check the log for `configuration reloaded`; restart if the listener host or port changed. |
| `--tui` is an unrecognized argument | Rebuild or install with `--features tui`, and check which executable your shell finds. |
| Dashboard needs a terminal | Run `--tui` directly in a terminal; use `--report` for redirected output. |
| Dashboard has no prices or an empty history | Pass its price-bearing config and the serving instance's data directory; also check the selected date window. |
| Health is OK but requests fail | Local health does not check upstream reachability, model availability, or credentials. Test a real request. |
| Compression or DCZ is absent | Follow [compression checks](#check-compression); a working API alone does not imply support. |
