# Operations

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

No reported usage, missing cache detail, and an unpriced model are distinct:
unknown values do not become zero. Cost estimates use optional configured rates,
with reasoning tokens included in output tokens rather than charged twice.

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

`--reload` sends SIGHUP, reopening the log and rereading upstream capabilities.
It does not reload TOML routes, prices, or limits. Restart to apply those changes.
`--stop` sends SIGTERM and waits up to ten seconds. The process flushes recording
and removes its pid file. Controls do not require upstream configuration.

## Optional terminal dashboard

```sh
cargo build --release --features tui
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
