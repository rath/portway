#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets --no-default-features
cargo clippy --locked --workspace --all-targets --no-default-features -- -D warnings
cargo test --locked --workspace --all-targets --features tui
cargo clippy --locked --workspace --all-targets --features tui -- -D warnings
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
