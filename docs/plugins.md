# Guided CLI setup

The Portway plugin helps you connect Codex or Claude Code without editing
configuration files yourself. It asks which client and server to use, runs
Portway's setup command, and checks the connection. Your existing login and
model preferences are kept.

## Install Portway and the plugin

`portway setup` and `portway doctor` are included in **Portway 0.2.5 and later**.
Install with Homebrew on macOS Apple silicon or Linux x86_64/ARM64:

```sh
brew install rath/tap/portway
```

If Portway is already installed, update it:

```sh
brew update
brew upgrade portway
```

Confirm that your shell finds the updated binary:

```sh
portway --version
portway setup --help
```

If an older Cargo or manually installed binary comes first on PATH, use the
Homebrew binary explicitly and adjust your PATH before continuing:

```sh
"$(brew --prefix portway)/bin/portway" --version
```

Without Homebrew, [download a release binary](getting-started.md#install).
Source builds are optional. Plugin installation does not install or upgrade
the Portway binary, and upgrading it does not restart an existing daemon.

Register the public repository as a marketplace and install the plugin for
the CLI you use. You can run these commands from any directory:

```sh
# Codex:
codex plugin marketplace add rath/portway
codex plugin add portway@portway

# Claude Code:
claude plugin marketplace add rath/portway
claude plugin install portway@portway
```

No repository checkout or public plugin-directory listing is required. This
setup was checked with Codex 0.162.0 and Claude Code 2.1.295.

Start a new CLI session. In Codex, ask **“Use the Portway plugin to set up my
connection.”** In Claude Code, run **`/portway:setup`**. Choose either a new
local forwarder or an existing server; for an existing server, supply its API
listener URL, such as `http://localhost:8787`, not its web-console URL.

The skill previews the files and fields to change before applying them. It
uses the same deterministic commands described below. After setup, start
Codex with `codex -p portway`, or start a new `claude` session. A plugin cannot
switch the API connection of an already-running client reliably.

## Use the commands without a plugin

For an existing Portway server, preview the changes:

```sh
portway setup --client codex --url http://localhost:8787
```

Apply them, creating a private backup of each existing file that changes:

```sh
portway setup --client codex --url http://localhost:8787 --apply
portway doctor --client codex --url http://localhost:8787
```

Use `--client claude` or `--client both` instead when appropriate. The URL is
the API listener's origin: `http://` or `https://`, a host, and an optional
port. Paths (including `/codex`, `/anthropic`, `/v1`, and web-console prefixes),
credentials, queries, and fragments are refused. Setup adds the mount names.
Custom mount names or reverse-proxy API prefixes need [manual configuration](cli-setup.md).

To prepare a new local forwarder for both clients:

```sh
portway setup --client both --local
portway setup --client both --local --apply
```

This creates `portway.toml` in the normal Portway data directory and prints
an explicit command to start it. Run that command in another terminal, or
append `--daemon`. Setup itself does not start, stop, or restart a process.
Use `--port 8789 --data-dir /path/to/separate-directory` with `--local` if you
need a separate instance. A different existing listener or upstream config is
refused, rather than overwritten; use `--url` to connect to that instance.

The generated local config points directly at Anthropic and the ChatGPT Codex
backend. It records requests but does not provide delta compression. For
compression, [add a receiver](cli-setup.md#across-the-network).

## What changes

| Client | File | Changes |
| --- | --- | --- |
| Codex | `$CODEX_HOME/portway.config.toml`, normally `~/.codex/portway.config.toml` | Selects the `portway` provider and its `/codex` URL, existing ChatGPT authentication, Responses API, and HTTP streaming instead of WebSockets |
| Claude Code | `$CLAUDE_CONFIG_DIR/settings.json`, normally `~/.claude/settings.json` | Sets only `env.ANTHROPIC_BASE_URL` to the `/anthropic` URL |

Codex's base `config.toml` and login store are untouched. Existing profile
preferences and other providers are retained, including TOML comments outside
replaced fields. If the Portway provider already declares an API-key source or
credential helper, setup refuses the conflict: this automatic path is for
ChatGPT login. Use the [manual API-key variant](cli-setup.md#optional-use-an-openai-api-key)
for a different authentication setup.

Claude's other JSON settings, including any existing credential settings, are
preserved; JSON whitespace may be reformatted. Its user-level setting applies
across projects. Project or managed settings can override it. Neither setup
nor doctor reads credential stores or checks whether you are signed in.

All selected files are parsed before applying changes. Existing files receive
an adjacent `.bak-…` copy before replacement. New/replaced files and backups
have mode `0600`; replacement is atomic per file. Repeating an unchanged setup
creates no additional backups. Symlinked files and malformed configurations
are refused for manual resolution. An I/O error stops further writes; files
already reported as backed up can be restored from those backups.

## Diagnose or disconnect

Run `portway doctor --client both --url http://localhost:8787`, or ask the
plugin to diagnose your connection (`/portway:doctor` in Claude Code). Doctor
checks the selected user settings, the forwarder's health response, and the
expected upstream names in its stats. Requests are bounded, credentials are
not sent, and redirects are not followed. A failed check exits nonzero.

These checks do not establish provider authentication, effective project/CLI
overrides, mount routing, or compression savings. Send a short message with
the client and confirm it appears in Portway's log or console to verify the
actual route. See [CLI troubleshooting](cli-setup.md#if-it-does-not-work).

Uninstalling the plugin removes the skills, **not the settings it created**:

- Codex: run `codex` without `-p portway` to return to your usual provider.
- Claude Code: remove only `env.ANTHROPIC_BASE_URL` if it still points at the
  Portway instance you are disconnecting, then start a new session. Check for
  shell or project settings of the same variable too.
- Restore an adjacent backup only if doing so will not discard later changes.
  Stopping a locally running forwarder is a separate operation.

## Plugin maintenance

When developing from a checkout, build with
`cargo install --path crates/portway --locked --features tui,web`, and use `.`
in place of `rath/portway` in the marketplace-add command from the repository
root. This loads the local marketplace for testing.

Both clients use the same `plugins/portway/skills/` directory. The portable
manifest carries Codex's onboarding metadata, while the Claude manifest and
two marketplace catalogs supply each CLI's distribution format. No MCP server,
startup hook, model credentials, or platform binary is bundled in the plugin.

To validate changes without installing into your daily CLI settings:

```sh
claude plugin validate plugins/portway
claude plugin validate .
```

For installation smoke tests, point `CODEX_HOME` or `CLAUDE_CONFIG_DIR` at a
fresh temporary directory before adding the local marketplace and installing.
Check that the installed plugin exposes `setup` and `doctor`. Keep both plugin
manifest versions in sync when publishing an update; plugin and binary
versions are independent.
