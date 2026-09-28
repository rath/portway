# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor
version may contain breaking changes.

## [Unreleased]

### Fixed

- `--reload` publishes the price table with the routes it rebuilds, so editing a
  rate in `portway.toml` reprices a running console — including for records it
  has already written — instead of waiting for a restart. An attached viewer
  still shows the rates it started with; restart it to pick up its own config.

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

[Unreleased]: https://github.com/rath/portway/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/rath/portway/releases/tag/v0.1.0
