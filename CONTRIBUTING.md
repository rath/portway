# Contributing

Thank you for helping improve Portway. This page covers the development setup,
the checks every change must pass, and the conventions the repository follows.
To report a vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening
an issue.

## Development setup

You need:

- macOS or Linux; the code uses Unix-only APIs.
- Rust 1.97 or later with the `rustfmt` and `clippy` components.
- A C compiler and CMake, for bundled SQLite, zstd, and the TLS crypto
  provider.
- Python 3.9 or later, only for the benchmark script.
- Node.js 18 or later, only for the web console's JavaScript tests. There are
  no packages to install; `check.sh` skips them with a warning when `node` is
  missing and fails without it in CI.

Build and run everything the CI runs:

```sh
bash scripts/check.sh
```

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/portway-core` | The embeddable library: forwarding, compression negotiation, dictionaries, the receiver, and telemetry |
| `crates/portway` | The `portway` CLI: configuration, daemon control, SQLite recording, reports, the optional `tui` dashboard and the optional `web` console server |
| `crates/portway/webui` | The web console's page: `static/` is embedded in the binary as is (no build step), `test/` runs under `node --test` |
| `crates/portway-core/tests`, `crates/portway/tests` | Integration tests over real TCP, including sender and receiver interoperability |
| `crates/portway-core/examples` | Buildable embedding examples |
| `examples/` | Annotated TOML files referenced by the documentation |
| `docs/` | User documentation; `docs/protocol.md` is the wire contract |
| `scripts/` | `check.sh` for CI checks, `bench.py` for the compression benchmark |
| `site/` | The project page at portway.told.md, static files with no build step, published by `.github/workflows/pages.yml` |

## Checks

`scripts/check.sh` must pass before a change is merged. It runs, in order:

1. `cargo fmt --check`.
2. Tests and `clippy -D warnings` for the default build.
3. Tests and `clippy -D warnings` with the `tui`, `web`, and `tui,web`
   features.
4. Documentation tests.
5. Three dependency boundary checks. `portway-core` must never depend on
   `clap`, `rusqlite`, `ratatui`, or `crossterm`, and neither the default CLI
   build nor the `web` build may depend on `ratatui` or `crossterm`.
6. The web console's JavaScript tests, `node --test` in `crates/portway/webui`.
   They assert the formatters against `test/fixtures/format.json`, which the
   Rust tests assert too, and every theme's contrast.

A file added under `crates/portway/webui/static` also needs a line in
`crates/portway/src/web/assets.rs`; a test compares the table to the directory.

Behavior changes need a test. Prefer integration tests that exercise real
sockets over mocks of Portway's own types.

## Benchmark

`scripts/bench.py` drives the real binaries through a local origin, receiver,
and sender. It also runs in CI as a smoke test and exits nonzero if
dictionaries stop being used:

```sh
cargo build --release --locked
python3 scripts/bench.py
```

The payload is deterministic, so the output is identical across machines for
the same seed, level, and `Cargo.lock`. If a change alters compressed sizes,
regenerate the README's measured table with the commands it names, and say so
in the pull request. The project page carries the same table in `site/index.html`
and a 40-turn run (`--turns 40 --context-kb 256`) in `site/session.js`; update
them together.

## Documentation

- Use placeholder destinations such as `https://api.example.com` and model
  names such as `model-a`. Do not present a real provider's endpoints, models,
  or prices as facts.
- Keep claims about savings conditional. State that compression needs a
  cooperating receiver, and that it saves bytes on the wire, not tokens.
- `docs/protocol.md` is the contract for other implementations. Change it in
  the same pull request as any change to headers, status codes, or framing.

## Commits and pull requests

Commit messages use [Conventional Commits](https://www.conventionalcommits.org/)
in English: a `type(scope): description` title, a blank line, then bullet
points describing each change. Common types are `feat`, `fix`, `refactor`,
`perf`, `test`, `docs`, and `chore`; common scopes are `core`, `cli`, and
`scripts`.

```text
fix(core): handle IPv6 origins and discard failed response connections

- Separate bracketed HTTP authorities from IPv6 DNS and TLS host names
- Return connections to the pool only after a response is fully consumed
```

Keep each pull request to one logical change. Add a line under
`## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything a user or
embedder would notice.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
