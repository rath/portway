# Portway documentation

Portway is a local HTTP proxy that sends each request of a coding agent's
conversation as the difference from the previous one, when the far end runs
`portway receive`. The [project README](../README.md) explains what it does and
shows measured savings; the pages below cover setting it up and running it.

## Set up

- [Guided CLI setup](plugins.md): install the Codex or Claude Code plugin, or
  use `portway setup` and `portway doctor` directly (Portway 0.2.5 or later).

- [Claude Code and Codex](cli-setup.md): mount each vendor's API under a name,
  connect Claude Code with an environment variable or Codex with a separate
  profile file, keep your login, and verify the first request; persistent
  settings, troubleshooting, the two-hop layout, and prices.
- [Getting started](getting-started.md): install Portway, write a first TOML
  file, send a request, route several models, and
  [add a compression receiver](getting-started.md#add-a-compression-receiver).
- [Configuration](configuration.md): how files are loaded, how a request
  picks its upstream, and every setting, including
  [compression](configuration.md#compression) and
  [receiving](configuration.md#receiving).
- [Vendor prices](prices.md): OpenAI's and Anthropic's published list prices,
  tiers included, as `[prices]` tables to copy, and what they leave out.
- [Authentication](authentication.md): how API keys pass through Portway,
  routes with different credentials, and how receivers keep dictionaries apart
  per caller.

## Run

- [Operations](operations.md): recording and reports, the daemon, reloading
  configuration, the terminal dashboard and web console,
  [remote TUI attach](operations.md#remote-terminal-dashboard), health checks,
  [checking compression](operations.md#check-compression), and
  [troubleshooting](operations.md#troubleshooting).
- [FAQ](faq.md): short answers on whether Portway fits your setup, how
  dictionaries behave, and day-to-day operation.

## Build on it

- [Request compression protocol](protocol.md): capability discovery, the DCZ
  dictionary transport, and [errors and replay](protocol.md#errors-and-replay),
  for anyone implementing a compatible receiver or sender.
- [Embedding the core](../README.md#embed-the-core): using `portway-core` as a
  Rust library.

## Project

- [Contributing](../CONTRIBUTING.md): layout, checks, and the benchmark.
- [Security](../SECURITY.md): what Portway stores, the threat model, and how
  to report a vulnerability.
- [Changelog](../CHANGELOG.md)
