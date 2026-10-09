---
name: setup
description: Connect Codex or Claude Code to Portway, using an existing server or a new local forwarder. Use when the user wants Portway CLI setup or to change its server address.
---

Use Portway's deterministic setup command to configure the client. The plugin
contains no proxy, login credentials, MCP server, or startup hooks.

1. Establish the client (`codex`, `claude`, or `both`) and whether the user
   wants an existing server or a local forwarder. Infer these from the request
   and current client where clear; ask only for missing choices. For an existing
   server, get its reachable **API listener** URL, not its web-console URL.
   Do not invent a remote server. A direct local forwarder records traffic;
   delta compression additionally needs a receiver on the remote hop.
2. Run `portway --version` and `portway setup --help`. If the executable is
   missing, use the project's documented installation method (`brew install
   rath/tap/portway` where Homebrew is available). Installing the plugin alone
   does not install the binary. If the installed version lacks `setup`, stop
   before editing any client files and explain that a build containing the
   setup feature is required. For a user working in a Portway source checkout,
   build with `cargo build --locked -p portway` and use the absolute path to
   `target/debug/portway` throughout instead. Do not substitute an unrelated
   binary on PATH or fetch an unrequested development build.
3. Preview the selected operation:
   - Existing server: `portway setup --client <client> --url <listener-url>`.
   - Local: `portway setup --client <client> --local`. Use `--port <port>` and
     `--data-dir <directory>` only when this installation needs a separate
     listener or data directory. Do not replace an existing Portway config.
   Quote argument values for the host shell. `--url` excludes mount paths,
   query strings, fragments, and credentials. The command adds the client
   mount names itself. It honors `CODEX_HOME` and `CLAUDE_CONFIG_DIR`.
4. Explain the preview: which files and connection fields change, and that
   Claude's user settings apply across projects while Codex uses an opt-in
   profile. Apply the same command with `--apply` when the user's setup request
   authorizes these changes. Ask only if the preview exposes a scope conflict
   or a choice the user has not made. Existing files receive private backups;
   model preferences, unrelated settings, and login stores are preserved.
   On a parse, authentication-conflict, or filesystem error, report it and
   stop applying; do not overwrite the file or invent credentials to continue.
5. Run `portway doctor --client <client> --url <listener-url>`. For a newly
   prepared local forwarder that is not running, launch the exact `--config`
   and `--data-dir` command printed by setup, adding `--daemon`, then recheck.
   Check whether the port is already occupied first. Never stop or restart
   an existing process to make room; diagnose it or choose a separate port.
   For a remote server, report reachability failures without deploying to it.
6. Tell the user how to start their next session: `codex -p portway` or a new
   `claude` process. Existing sessions need not adopt the new connection.
   Use the CLI's existing ChatGPT/Claude login; never read or copy `auth.json`
   or replace it with an inference-service key. Have the user send a short
   message and confirm it appears in Portway's log/console. Doctor checks
   user settings and management endpoints, not vendor authentication, actual
   inference, or achieved compression.

Removing this plugin does not undo client settings. To disconnect Codex, omit
`-p portway`. For Claude, remove only `env.ANTHROPIC_BASE_URL` if it still
points at the Portway server being disconnected; preserve all other settings
and account for any shell/project override. Use the backups for recovery,
without overwriting unrelated changes made since setup.
