# Configuration

`--config PATH` loads a TOML file. Otherwise Portway tries `./portway.toml`.
Explicit CLI values win over TOML. CLI defaults do not overwrite file settings.
Unknown fields, malformed prices, conflicting route modes, and invalid URLs fail
before serving. Relative config paths are read before daemonization.

| Root setting | Default | Meaning |
| --- | --- | --- |
| `host` | `127.0.0.1` | Listening address |
| `port` | `8787` | Listening port |
| `upstream` | unset | Single upstream HTTP(S) URL |
| `[models]` | empty | Model name → upstream URL; exclusive with `upstream` |
| `[prices.NAME]` | absent | Optional `input`, `output`, `cache_read` rates in USD/M tokens |

Price fields are required together and must be finite, nonnegative numbers.
Receiver mode requires one `upstream`. It does not perform model routing or
recompress restored requests before sending them to the application.

## Compression

| `[compression]` setting | Default | Meaning |
| --- | --- | --- |
| `coding` | `"auto"` | `auto`, `zstd`, `gzip`, or `off`; preferences require advertisement |
| `dict` | `"auto"` | `auto` or `off`; DCZ requires negotiated zstd |
| `level` | `11` | 1–19; gzip is capped at 9 |
| `min_bytes` | `1024` | Smaller request bodies remain unchanged |
| `max_body_bytes` | `268435456` | Maximum collected request size |
| `probe_path` | automatic | Absolute origin path, without query |

Automatic negotiation tries `/__portway/capabilities`, then `/health` if no codecs
are advertised. Explicit `probe_path` disables that fallback. Probe paths are
relative to the origin, not the configured upstream path prefix. The probe reads
`X-Request-Encodings` or the `request_encodings` JSON array and separately reads
`X-Request-Dictionary`. Unavailable probes start with identity; a later failed
probe retains the last known capabilities.

Capabilities refresh lazily after 60 seconds. Requests do not wait for background
refresh. Explicit decoder refusals suppress the refused codec for ten minutes.
The pool holds up to eight idle connections per route, for up to 300 seconds.

CLI overrides: `--host`, `--port`, `--upstream`, `--coding`, `--dict`, `--level`,
`--min-bytes`, `--max-body-bytes`, `--probe-path`.

## Receiving

| `[receiver]` setting | Default |
| --- | --- |
| `dictionary_bytes` | `268435456` (256MiB); zero disables dictionaries |
| `dictionary_ttl_seconds` | `3600` idle seconds |
| `min_dictionary_bytes` | `32768` (32KiB) |
| `max_dictionary_bytes` | `33554432` (32MiB) |
| `max_body_bytes` | `268435456` (256MiB), for encoded and decoded bodies |
| `max_window_bytes` | `134217728` (128MiB) |

Dictionary thresholds must be positive and ordered, with the maximum no larger
than the body limit. The zstd window limit is a power of two between 1KiB and
128MiB. Expiry is lazy: dictionary lookups and insertions remove expired entries.

Dictionaries are isolated by the receiving instance and authentication context.
Application middleware can supply a `DictionaryScope` extension after authenticating
its caller. Standalone sender and receiver derive that context from the exact
Authorization and Cookie headers; changing either starts a different context.
A gateway that strips identity headers must isolate receiver instances per trust
context or use middleware with an explicit scope. A scope partitions storage; it
is not authentication.

## Runtime files

`--data-dir PATH` overrides `$XDG_CONFIG_HOME/portway` or
`$HOME/.config/portway`. The directory is created with mode 0700. It contains
`db.sqlite3`, `portway.pid`, `portway.log`, and, with TUI settings, `portway.tui`.
Old runtime directories are not discovered or automatically migrated.

`--retention-days N` deletes older rows at startup and daily; zero keeps all rows.
`--report --since 90s|30m|24h|7d` defaults to 24h. `--model NAME` filters a report
by recorded route name. These are CLI runtime options, not TOML fields.
