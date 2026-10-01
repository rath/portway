# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor
version may contain breaking changes.

## [Unreleased]

### Changed

- Download savings are shown wherever upload savings are. The event line pairs
  the answer with what the upstream hop carried (`down 128KB→11KB -91%`), the
  console's event table gains `↓ wire` and `↓ saved`, the upstream tables gain
  `↓ saved`, `--report` gains `down wire` and `down saved` (its `saved` column
  is now `up saved`), the log line reads `down 128KB <- 11KB (zstd, -91%)`, and
  `/__portway/stats` adds `down_saved_bytes`. Before, a receiver's download
  saving showed only as a tooltip, and an agent that asks for no response
  coding, such as Codex, looked as if nothing had been saved.

### Fixed

- A response Portway decodes or re-encodes keeps its `ETag`, weakened, instead
  of losing it. Codex keys its model catalog on that tag and compares it with
  the `X-Models-Etag` of every answer; without it, Codex downloaded the whole
  catalog (about 600 KB) again after every turn through Portway.
- `scripts/bench.py` reads the sender's counters under `upstreams`, the key
  `/__portway/stats` uses since 0.2.0. The 0.2.0 tag's copy still reads
  `models` and stops with a `KeyError`; the released binaries are unaffected.

## [0.2.0] - 2026-10-01

### Added

- An `[upstreams]` table mounts named upstreams at path prefixes: with
  `anthropic = "https://api.anthropic.com"`, a request to `/anthropic/v1/messages`
  reaches `https://api.anthropic.com/v1/messages`. The choice is made from the
  path alone, so a bodiless request such as Codex's model catalog routes like
  any other, and a new model name needs no configuration. A vendor's own client
  is connected by its base URL (`http://127.0.0.1:8787/anthropic`) with nothing
  else to set. Mounts combine with `[models]` on one listener and work in
  `receive` mode too, so one receiver can front several providers. The stats
  endpoint now keys its per-route counters under `upstreams` rather than
  `models`. `docs/agents.md` walks through Claude Code and Codex, and the
  README, getting-started guide and project page start from them.
- Each release publishes prebuilt binaries for macOS on Apple silicon and Linux
  on x86-64 and arm64, with the terminal dashboard and the browser console
  built in. The Linux builds need glibc 2.28 or later. `SHA256SUMS` lists the
  archives' checksums, and `releases/latest/download/portway-<target>.tar.gz`
  always names the newest one.
- `portway receive` accepts a `[models]` table as well as a single `upstream`,
  routing each request to the origin configured for its JSON `model` field. One
  receiver can therefore front several providers from one listener: its
  dictionary store and its credential partitions serve every route, and a model
  outside the table is refused with a 400 before any origin is contacted.
- `portway --tui --attach URL` views a remote web console over HTTP/HTTPS,
  including its live requests, recent events, charts, usage and costs. It
  remembers an owner-only session after hidden token entry, reconnects after
  network interruptions, and leaves the remote daemon running when closed.
- The project page at portway.told.me is published in Korean, Simplified Chinese
  and Japanese as well as English, at `/ko/`, `/zh/` and `/ja/`. Each language is
  a page of its own with a language switcher, `hreflang` alternates and an entry
  in the sitemap; the 404 picks the reader's language. English is unchanged at
  `/`.

### Changed

- Every recorded request keeps its route and its model apart: `upstream` is the
  route it went through (a mount's name, a model's name, or `upstream`), and
  `model` is what its JSON body asked for, empty when it asked for none. The
  dashboards' per-route tables and filters are therefore labelled "upstreams",
  the events show the model, and `--report --model` filters by the model. The
  database moves to schema 4, copying the old column into `upstream`; rows from
  before carry their route name as the model. The local live-flights feed is
  version 2, so an older attached viewer reports it unavailable rather than
  misreading it.

### Fixed

- An answer the agent stops reading at its last event is recorded whole. Codex
  closes the stream as soon as `response.completed` arrives, before the
  server's EOF, which made most of its turns cut ones: no token counts, and a
  connection that could not be pooled. The relay now reads the upstream on for
  at most 250 ms and 64 KiB after the agent has gone; an answer that ends
  within that grace keeps its usage and its connection, and one that does not
  is dropped as before, which is what stops the engine.
- Prices now apply under a single `upstream` and under a mount: a request for
  `claude-opus-5-5` through either is priced by `[prices."claude-opus-5-5"]`.
  Before, every request through a single upstream was recorded as the model
  `upstream`, so `[prices.upstream]` was the only key that matched and a
  report could not tell one model from another.
- The TUI model table shows at most three recently used models, prioritizes
  in-flight activity, and hides unused routes to leave more room for events.
- The TUI's in-flight dialog stands apart from the dashboard with a muted
  backdrop, double border, clearer title and close keys, and vertical padding.
- `--reload` publishes the price table with the routes it rebuilds, so editing a
  rate in `portway.toml` reprices a running console — including for records it
  has already written — instead of waiting for a restart. A locally attached
  TUI still shows the rates it started with; restart it to pick up its own
  config. Remote TUI attach uses the console's updated prices.

## [0.1.0] - 2026-09-28

The first public release.

### Added

- `portway`, a local HTTP proxy for a coding agent's API requests. It forwards
  to a single upstream, or routes each request by its JSON `model` field to the
  upstream configured for that model; a request for any other model is refused
  with a 400 that names it and lists the configured ones.
- Request compression with zstd or gzip, negotiated per destination: the sender
  compresses only when the destination advertises the Portway protocol, and
  forwards unchanged otherwise.
- Dictionary compression of append-only request bodies. The sender keeps up to
  eight bodies per route that the receiver confirmed by SHA-256, and sends each
  new request as a zstd frame against the one sharing the longest prefix, in
  the DCZ format from RFC 9842. A refused dictionary or zstd body is resent
  with less compression, at most three attempts in all; application errors are
  never retried.
- `portway receive`, a receiver that decodes compressed requests in front of an
  existing HTTP service, with bounded memory, dictionaries kept apart per
  credential, and a refusal before the application runs whenever a request
  cannot be restored.
- Adaptive compression of the receiver's upload to its origin
  (`[receiver.origin_compression] mode = "auto"`), learned from the origin's
  `Accept-Encoding`.
- TOML configuration. Without `--config`, Portway reads `portway.toml` from the
  working directory, then from the data directory (`--data-dir`, else
  `$XDG_CONFIG_HOME/portway` or `~/.config/portway`).
- Daemon control with `--daemon`, `--stop`, `--status`, and `--reload` (or
  SIGHUP), which rereads the configuration without dropping the listener.
- Request recording in SQLite, readable only by its owner, with `--report` for
  usage and spend over any window or model and `--retention-days` to prune it.
  Anthropic cache reads and writes count toward the prompt.
- `--tui`, a terminal dashboard, behind the `tui` build feature.
- `--web`, the same dashboard in a browser, behind the `web` build feature:
  requests in flight, an event table with search and CSV or JSON export,
  report history, insight charts, and themes. Access needs a per-run token.
  `--web-host`, `--web-allow-host`, and `--web-base-path` publish it beyond
  loopback or behind a reverse proxy.
- `portway-core`, the forwarding and receiving engine as an embeddable Rust
  library, including DCZ framing and dictionary limits in `portway_core::dict`,
  compression diagnostics in route statistics, and requests in flight on
  `Telemetry::flights()`.
- `scripts/bench.py`, a reproducible end-to-end benchmark of request
  compression.
- Guides to getting started, configuration, authentication, and operations; an
  FAQ; the request compression protocol; a security policy; and contribution
  guidelines.

[Unreleased]: https://github.com/rath/portway/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/rath/portway/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rath/portway/releases/tag/v0.1.0
