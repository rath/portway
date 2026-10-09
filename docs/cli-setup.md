# Claude Code and Codex

Use your existing Claude Code or Codex login through Portway. Each client
connects to a named path, called a **mount**: `/anthropic` for Claude Code and
`/codex` for Codex. Portway forwards the client's credentials and model name;
you do not need a Portway API key or a list of vendor models in its config.

## Before you start

Install the CLI you want to use and confirm it works directly with your
account first. For Codex, use `codex login` to sign in with ChatGPT. For Claude
Code, use its normal sign-in flow. Keep that login; do not add an API key just
to use Portway. Install Portway with `brew install rath/tap/portway`, or use
[another installation method](getting-started.md#install).

The first setup below runs Portway on your own machine and forwards directly
to the providers. It records traffic, but **delta compression needs a receiver**;
add one with [Across the network](#across-the-network) after the client works.

## Start Portway

Save this as `portway.toml` in a directory of your choice:

```toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"
```

From that directory, start Portway and leave the terminal open:

```sh
portway --config ./portway.toml
```

Add `--web` for the browser console if your build includes it (Homebrew does).
Open a second terminal for your CLI. If you already have a Portway server,
skip starting another one and replace `127.0.0.1:8787` in the client settings
and health check below with its reachable host and port. Keep the mount name.

Portway removes the mount name and appends the rest of the path to the
configured upstream URL. For example, `/anthropic/v1/messages` reaches
`https://api.anthropic.com/v1/messages`. Routing uses the path, so even a
request without a body works. See [mounts](configuration.md#mount-upstreams-at-path-prefixes)
for the full rules.

## Claude Code

Run this in the second terminal to use Portway for one invocation. `env` works
in bash, zsh, and fish:

```sh
env ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic claude
```

The base URL ends at `/anthropic`, without `/v1`. Claude Code adds
`/v1/messages` itself and uses its existing credentials. If you already use
`ANTHROPIC_API_KEY` instead of a subscription login, keep that credential;
Portway does not require you to switch authentication methods.

### Keep the setting

For bash or zsh, add this to `~/.bashrc` or `~/.zshrc` respectively:

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic
```

For fish, add this to `~/.config/fish/config.fish`:

```fish
set -gx ANTHROPIC_BASE_URL http://127.0.0.1:8787/anthropic
```

Open a new terminal, then run `claude`. To go back to a direct connection,
remove the line and unset the variable in your current terminal with
`unset ANTHROPIC_BASE_URL` (bash/zsh) or `set -e ANTHROPIC_BASE_URL` (fish).
If your Claude settings also define this variable, update that setting too.

Changing the base URL can affect Claude Code features: MCP tool search is
disabled by default on non-first-party hosts, and Remote Control is disabled.
See the [official environment variable reference](https://code.claude.com/docs/en/env-vars)
for current behavior and the tool-search opt-in.

## Codex

### Create a Portway profile

Create `~/.codex/portway.config.toml` with the complete contents below. If the
file already exists, back it up and edit it rather than overwriting settings
you want to keep. If you set `CODEX_HOME`, use that directory instead of
`~/.codex`.

```toml
model_provider = "portway"

[model_providers.portway]
name = "portway"
base_url = "http://127.0.0.1:8787/codex"
requires_openai_auth = true
wire_api = "responses"
```

Start Codex from your project directory:

```sh
codex -p portway
```

`-p portway` loads `portway.config.toml` on top of your usual `config.toml`.
The profile only changes the connection; model, reasoning effort, speed, and
other preferences come from your normal configuration or Codex defaults.
Run `codex` without `-p portway` to use your usual provider again.

| Setting | Purpose |
| --- | --- |
| `model_provider` | Selects the provider defined in this file |
| `name` | Display name for that provider |
| `base_url` | Portway's address including the `/codex` mount; do not append `/v1` |
| `requires_openai_auth = true` | Uses Codex's existing OpenAI authentication; this example uses a ChatGPT login |
| `wire_api = "responses"` | Explicitly selects the Responses API transport |

No `env_key` is needed for the ChatGPT-login setup. In particular,
`INFERENCE_API_KEY` is not a Portway requirement; a key for another inference
service does not authenticate requests to the ChatGPT Codex backend.

**Migrating an older profile:** Codex 0.134.0 and later use separate profile
files. Move the settings from `[profiles.portway]` in `~/.codex/config.toml`
to the top level of `~/.codex/portway.config.toml`, and put the provider table
there as shown above. Remove the old profile table and any obsolete top-level
`profile = "portway"` selector. If you previously set a global
`model_provider = "portway"`, remove it to restore the default provider for
plain `codex`. See [official Codex profiles](https://learn.chatgpt.com/docs/config-file/config-advanced#profiles).

Codex also fetches its model catalog with a bodiless `GET /codex/models`.
The mount routes that request too, without needing a model name in Portway.

### Optional: use an OpenAI API key

This is a different authentication setup from the ChatGPT-login example.
Change the Portway upstream to `codex = "https://api.openai.com/v1"` and
replace `requires_openai_auth = true` in the Codex provider with
`env_key = "OPENAI_API_KEY"`. Supply your own OpenAI API key through that
environment variable. Keep the client base URL ending at `/codex`: the
upstream already contains `/v1`. Restart a foreground Portway process after
editing its config, or [reload a daemon](operations.md#apply-configuration-changes).

## Check the connection

Check the Portway listener:

```sh
curl --fail-with-body http://127.0.0.1:8787/__portway/health
```

Expect `{"status":"ok","mode":"forward"}`. This only checks the listener;
it does not verify provider credentials or model access.

Send a short message in the CLI you connected and confirm both that it answers
and that a request appears in Portway's terminal log or browser console under
`anthropic` or `codex`. Successful model-catalog requests count in totals but
are hidden from the console's default event list, so use a real conversation
turn for this check. A `401` or `404` means the full connection is not working
even when health is OK.

With direct provider upstreams, zero delta savings are expected. Once you add
a receiver, use [checking compression](operations.md#check-compression) to
verify the compressed hop; short first messages need not produce dictionary hits.

## Across the network

Compression needs a receiver on the far side, so the layout for two vendors is
one sender near the agents and one receiver near the providers, with the same
names on both:

```text
Claude Code ─┐                                            ┌─ api.anthropic.com
             ├─ portway ── compressed ──▶ portway receive ─┤
Codex ───────┘  /anthropic, /codex        /anthropic, /codex └─ chatgpt.com/backend-api/codex
```

```toml
# sender, next to the agents
[upstreams]
anthropic = "https://gateway.example.com/anthropic"
codex = "https://gateway.example.com/codex"
```

```toml
# receiver, behind the gateway
host = "127.0.0.1"
port = 8788

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"

[receiver.origin_compression]
mode = "auto"
```

The sender removes `/anthropic` and appends the rest to its URL, whose own
path carries the name on; the receiver removes it again. One receiver, one
dictionary store, both vendors. The gateway in between terminates TLS and
authenticates, as in [receiver setup](getting-started.md#add-a-compression-receiver)
and [authentication](authentication.md#receivers-and-dictionary-isolation).

## Prices and what you will see

Claude Code sends fast mode as `"speed": "fast"`, recorded as the tier `fast`.
Codex sends its `fast` setting as `"priority"`, `ultrafast` as `"ultrafast"`,
and no tier for standard speed. Portway prices the tier the request names.

Prices are keyed by the model names the clients send, whatever mount the
request went through, and by the tier each request names.
[Vendor prices](prices.md) has both vendors' list prices as tables to copy,
fast and ultrafast tiers included:

```toml
[prices."claude-opus-5-5"]
input = 4.0
output = 20.0
cache_read = 0.2

[prices."claude-opus-5-5".tiers.fast]
input = 8.0
output = 40.0
cache_read = 0.4
```

The dashboards list `anthropic` and `codex` as upstreams and each request
under its model; `portway --report --model claude-opus-5-5` narrows to one.
A catalog request names no model and shows `-`.

Both clients' answers are compressed across the receiver hop, and the
console shows that saving as `↓ wire` and `↓ saved` for each request (and
`↓ saved` per upstream). The `to agent` column is the last leg, from the
sender to the client: Claude Code accepts a compressed answer there, while
Codex asks for none (its HTTP client sends no `Accept-Encoding`), so for Codex
that column stays empty. Next to the agent that leg is loopback or a LAN and
costs nothing worth saving.

## If it does not work

| Symptom | Cause |
| --- | --- |
| Codex cannot find the `portway` profile | Create `portway.config.toml` in `CODEX_HOME` (normally `~/.codex`); current Codex does not read `[profiles.portway]` |
| Connection refused | Start Portway, check the host and port, and keep the serving terminal open |
| Requests work but nothing appears in Portway | Start a new Claude process with the base URL set, or launch Codex with `-p portway`; check for overriding client settings |
| Codex: `404 Not Found: {"detail":"Not Found"}` from the vendor at `…/v1/responses` | The base URL has `/v1` after the mount; end it at `/codex` |
| Portway: `No upstream is mounted at "/v1/messages"` | The base URL does not end in a mount name; the message lists them |
| `401` from the vendor | The request carried no credential the vendor accepts; Portway adds none. `codex login`, or Claude Code's own login or `ANTHROPIC_API_KEY` |
| Codex logs `failed to refresh available models` | The mount's upstream does not serve Codex's catalog at `/models`; Codex continues with its built-in list |
