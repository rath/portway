#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets --no-default-features
cargo clippy --locked --workspace --all-targets --no-default-features -- -D warnings
for features in tui web tui,web; do
    cargo test --locked --workspace --all-targets --features "$features"
    cargo clippy --locked --workspace --all-targets --features "$features" -- -D warnings
done
cargo test --locked --workspace --doc
core_tree="$(cargo tree --locked -p portway-core --edges normal --prefix none)"
if grep -Eq '^(clap|rusqlite|ratatui|crossterm) ' <<< "$core_tree"; then
    echo 'portway-core acquired an application dependency' >&2
    exit 1
fi
cli_tree="$(cargo tree --locked -p portway --no-default-features --edges normal --prefix none)"
if grep -Eq '^(ratatui|crossterm) ' <<< "$cli_tree"; then
    echo 'the default CLI acquired a terminal UI dependency' >&2
    exit 1
fi
web_tree="$(cargo tree --locked -p portway --no-default-features --features web --edges normal --prefix none)"
if grep -Eq '^(ratatui|crossterm) ' <<< "$web_tree"; then
    echo 'the web console acquired a terminal UI dependency' >&2
    exit 1
fi
# The console's JavaScript: formatters, line parity, filters, palettes. CI
# must run it; a machine without node skips it with a warning.
if command -v node > /dev/null; then
    (cd crates/portway/webui && node --test test/*.test.js)
elif [ -n "${CI:-}" ]; then
    echo 'node is required in CI for the web console tests' >&2
    exit 1
else
    echo 'warning: node not found; skipping the web console tests' >&2
fi
