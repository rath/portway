# Claude Code and Codex

Both clients speak their vendor's own API and let you set one thing: the base
URL. Portway mounts each vendor's API under a name, and the client's base URL
ends in that name. Nothing else changes. The client keeps its own login or key,
which Portway passes through unchanged, and its model names, which Portway
never needs to know.

## One file for both

```toml
# portway.toml
host = "127.0.0.1"
port = 8787

[upstreams]
anthropic = "https://api.anthropic.com"
codex = "https://chatgpt.com/backend-api/codex"
```

```sh
portway --config ./portway.toml
```

Each name answers under `/<name>/`: Portway removes that segment and forwards
the rest of the path, with its query, to the URL. The choice is made from the
path alone, so a request without a body routes like any other, and a model the
file has never heard of needs no entry. [Mounts](configuration.md#mount-upstreams-at-path-prefixes)
has the rules; this page has the client side.

## Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787/anthropic
claude
```

Claude Code sends `POST /anthropic/v1/messages`; Portway forwards
`/v1/messages` to `api.anthropic.com` with the request's own credentials —
the login Claude Code already has, or `ANTHROPIC_API_KEY` if you use one.
Model names travel in the body and are recorded, not matched: a model works
the day it ships.

## Codex

In `~/.codex/config.toml`, a provider whose base URL is the mount:

```toml
model_provider = "portway"

[model_providers.portway]
name = "portway"
base_url = "http://127.0.0.1:8787/codex"
wire_api = "responses"
requires_openai_auth = true
```

`requires_openai_auth = true` makes Codex sign the requests with the ChatGPT
login it already has (`codex login`), the way its built-in provider does, and
the mount sends them on to `chatgpt.com/backend-api/codex`. To keep your usual
provider as the default, put `model_provider` in a profile instead and start
Codex with `codex -p portway`:

```toml
[profiles.portway]
model_provider = "portway"
```

Codex also fetches its model catalog with a bodiless `GET /codex/models`
before the first turn. The mount forwards it like any other request, which is
why routing by path matters here: there is no model in that request to route
by.

For an API key instead of the ChatGPT login, use the settings of Codex's
built-in `openai` provider with the base URL moved to the mount: mount
`codex = "https://api.openai.com/v1"` and replace `requires_openai_auth = true`
with `env_key = "OPENAI_API_KEY"`.

The base URL ends at the mount. `http://127.0.0.1:8787/codex/v1` would make
Codex ask for `/codex/v1/responses`, and the vendor answers `404 Not Found` to
`/v1/responses` under its own prefix.

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

Prices are keyed by the model names the clients send, whatever mount the
request went through:

```toml
[prices."claude-opus-5-5"]
input = 5.0
output = 25.0
cache_read = 0.5
```

The dashboards list `anthropic` and `codex` as upstreams and each request
under its model; `portway --report --model claude-opus-5-5` narrows to one.
A catalog request names no model and shows `-`.

Codex asks for no response compression — its HTTP client sends no
`Accept-Encoding` — so its answers reach it as they were decoded, and the
console's "to agent" column stays empty for it. Claude Code accepts a
compressed answer and shows the smaller figure there.

## If it does not work

| Symptom | Cause |
| --- | --- |
| Codex: `404 Not Found: {"detail":"Not Found"}` from the vendor at `…/v1/responses` | The base URL has `/v1` after the mount; end it at `/codex` |
| Portway: `No upstream is mounted at "/v1/messages"` | The base URL does not end in a mount name; the message lists them |
| `401` from the vendor | The request carried no credential the vendor accepts; Portway adds none. `codex login`, or Claude Code's own login or `ANTHROPIC_API_KEY` |
| Codex logs `failed to refresh available models` | The mount's upstream does not serve Codex's catalog at `/models`; Codex continues with its built-in list |
