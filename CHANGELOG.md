# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor
version may contain breaking changes.

## [Unreleased]

### Added

- Compression diagnostics in route statistics and the wide live TUI: refusal
  and dictionary suspension causes, remaining backoff, probe results, and
  cumulative probe failures and dictionary hash mismatches.
- Shared DCZ framing and default dictionary limits for embedded senders and
  receivers in `portway_core::dict`.

- Without `--config`, the CLI looks for `./portway.toml` in the working
  directory, then in the runtime data directory (`--data-dir` when given,
  otherwise `$XDG_CONFIG_HOME/portway` or `~/.config/portway`), before falling
  back to built-in defaults. An explicit `--data-dir` never falls back to the
  default directory's file. A bare `portway --tui` now attaches to the config
  the daemon is actually using. Relative `--config` and `--data-dir` paths are
  resolved once at startup, so a daemon keeps its files and `--reload` rereads
  the same TOML after the daemon leaves the launch directory.

- `portway-core`, an embeddable library for forwarding HTTP requests with
  zstd or gzip request compression, negotiated per destination.
- Dictionary compression of append-only request bodies: the previous confirmed
  body is used as a zstd prefix, sent in the DCZ frame format from RFC 9842.
- A receiver that decodes compressed requests in front of an existing HTTP
  service, with bounded memory, per-credential dictionary partitions, and
  explicit decoder errors that make retries safe.
- The `portway` CLI with single-upstream forwarding, model routing by the JSON
  `model` field, and `receive` mode.
- Daemon control, SQLite request recording, text reports, and an optional
  terminal dashboard behind the `tui` feature.
- `scripts/bench.py`, a reproducible end-to-end benchmark of request
  compression.
- A FAQ, a security policy, contribution guidelines, and continuous
  integration on Linux and macOS.

- `--web`: the dashboard in a browser, behind the optional `web` build feature.
  It shows what `--tui` shows, computed by the same code, with the events as
  an aligned table that spells out each request's path, plus requests still
  in flight, the `--report` history for any window, search, CSV and JSON
  export, insight charts, opt-in desktop notifications, a command palette and
  15 themes. It runs beside a forwarder, attaches to a running one like
  `--tui`, or is hosted by the daemon with `--daemon --web`; `--status` prints
  its address. Access needs a per-run token exchanged for a session cookie. A
  foreground `--web` opens itself in the default browser with a one-time
  launch code; `--no-open` only prints the link. Bound to a wildcard, it
  prints a link per interface address; `--web-allow-host` names the host
  names it answers to besides `localhost` and IP addresses.
- `portway_core::flights`: a registry of requests counted but not yet relayed,
  on `Telemetry::flights()`.

### Changed

- A request for an unconfigured model in router mode is still a 400 listing
  the supported models, but the message now names the model the request
  asked for when one was given.

- `RequestRecord` has a new `flight` field naming the in-flight entry the
  record ends (`None` for rows rebuilt from the database). Code that builds
  `RequestRecord` values must set it.
- The flight registry entry is removed before `Event::Request` is emitted, so
  a reader holding both never sees a request twice or not at all.

### Fixed

- `--report` failed with `Invalid column type Null` when a request had dialed
  a plain-HTTP upstream (no TLS phase); such a dial now counts as DNS plus TCP.
- Anthropic answers recorded only `input_tokens` as the prompt, so the cached
  count exceeded it (a hit rate above 100%) and fresh input was priced at
  zero. The prompt now adds the cache read and cache write counts Anthropic
  reports beside it. Rows recorded before the fix keep the old counts.
- `--stop` reported the daemon stopped as soon as its pid file was removed,
  while the daemon was still shutting down, so a script that went on to replace
  the binary or start another daemon could overlap the old one. It now waits
  until the daemon releases the pid file's lock, the last thing it does before
  exiting.

### Security

- The request database is created with mode 0600, and an existing database is
  narrowed to 0600, even when `--data-dir` names a directory Portway did not
  create.

