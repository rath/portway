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
  directory, then under the runtime data directory (`$XDG_CONFIG_HOME/portway`
  or `~/.config/portway`), before falling back to built-in defaults. A bare
  `portway --tui` now attaches to the config the daemon is actually using.

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

### Security

- The request database is created with mode 0600, and an existing database is
  narrowed to 0600, even when `--data-dir` names a directory Portway did not
  create.
