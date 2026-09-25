# API keys and authentication

Portway forwards credentials supplied by the client to the selected upstream.
It does not issue keys, validate inbound API keys, or replace one provider's key
with another. There is no `api_key` TOML field, `--api-key` flag, or automatic
API-key environment variable lookup.

```text
Client: Authorization: Bearer <upstream key>
  → Portway: same Authorization header
  → Selected upstream: validates the key
```

Keep the existing key when changing a client's base URL to
`http://127.0.0.1:8787/v1`. A made-up “local Portway key” will reach the upstream
and fail if that upstream expects a real credential.

## Send a bearer key from curl

Set `UPSTREAM_API_KEY` in the terminal running curl using your normal secret
management method, then send it as a request header:

```sh
curl --fail-with-body --no-buffer \
  http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer ${UPSTREAM_API_KEY:?Set UPSTREAM_API_KEY first}" \
  -H 'Content-Type: application/json' \
  --data '{"model":"model-a","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

The shell expands this variable for curl. Exporting a key in the terminal that
starts Portway does **not** add that key to forwarded requests. The same applies
to names such as `OPENAI_API_KEY`: a client may read them, but Portway does not.

For an API using `X-API-Key`, use its header instead:

```sh
curl --fail-with-body http://127.0.0.1:8787/your/path \
  -H "X-API-Key: ${UPSTREAM_API_KEY:?Set UPSTREAM_API_KEY first}"
```

This path example uses single-upstream mode. Cookies and ordinary application
headers are also forwarded. Hop-by-hop headers are removed according to the
[HTTP forwarding rules](protocol.md#streaming-and-http-boundaries).

For an unauthenticated upstream, no credential is needed. If a client insists on
an API key even then, any placeholder it sends will still be forwarded; whether
that is accepted depends on the upstream.

## Different models with different keys

A routing table contains destinations only:

```toml
[models]
"model-a" = "https://first.example.com"
"model-b.1" = "https://second.example.com"
```

| Requested model | Destination | Credential to send |
| --- | --- | --- |
| `model-a` | `first.example.com` | A key accepted by the first service |
| `model-b.1` | `second.example.com` | A key accepted by the second service |

When one key is valid at both destinations, the client can use it for either
model. Otherwise configure separate client profiles or clients, each with the
same local base URL but the appropriate model and key. Portway makes no
model-to-key lookup and does not fall back to another credential after a 401.

A client limited to one fixed key cannot switch between differently authenticated
services through this routing table alone. Use an existing authentication gateway
that maps its client credential to the correct upstream key, or an embedding
application that supplies that behavior. Running two Portway instances separates
routes and ports, but does not itself inject credentials.

Choose destinations you intend to trust with the headers your clients send;
a route change also changes where those credentials are delivered.

## Where keys belong

Keep keys in the client or its secret store and send them in the service's
expected header. TOML is for destinations, compression, and optional prices.
An `api_key` field is rejected as unknown. `${NAME}` and `~` are not expanded in
TOML strings; this is configuration parsing, not shell execution. Embedded URL
credentials such as `https://user:password@example.com` are rejected.

The CLI recorder stores request metadata, not header values or bodies. API keys,
cookies, and dictionary contents are not written to its database or request log.
Paths and operational errors are recorded, so do not put secrets in URL paths.

## Inbound access and probes

The default listener is `127.0.0.1`. Portway has no inbound authentication layer;
binding it to a reachable interface exposes its listener and local management
endpoints to that network. For a shared or public deployment, place it behind
your existing access-control and TLS gateway.

`GET /__portway/health` checks local liveness without a key. In model-routing
mode, `GET /v1/models` lists configuration without contacting or authenticating
with upstreams. Neither response proves that an API key is valid. Test an actual
application request to verify credentials.

Compression probes intentionally contain no caller Authorization or Cookie
headers. They try `/__portway/capabilities`, then `probe_path` (`/health` by
default). If every capability endpoint requires authentication, initial requests
remain uncompressed; authenticated application calls can still succeed.
Do not make the application API public merely to enable compression: expose only
the receiver's read-only capability endpoint through your gateway.

## Receivers and dictionary isolation

A receiving Portway must sit behind authentication so unauthenticated callers
cannot read or use an application's dictionary context. By default, sender and
receiver partition dictionaries using the exact Authorization and Cookie header
values. A different key or cookie creates a separate context; rotating a key may
therefore require new dictionary warm-up.

A gateway that removes or replaces caller credentials with one shared value can
collapse those contexts. Preserve distinct identity headers, isolate receiver
instances per trust context, or integrate `portway-core` and set a
`DictionaryScope` extension after authenticating the caller. A custom `X-API-Key`
header is forwarded, but it is **not** part of the default dictionary scope;
use explicit scopes or separate receivers when it distinguishes tenants.

Dictionary scope separates storage; it does not authenticate a request.
See [receiver configuration](configuration.md#receiving) and the
[protocol](protocol.md#dictionary-transport) for the full contract.

## If authentication fails

A 401 or 403 from a forwarded application request is an upstream or gateway
response. Verify the client's header, key permissions, and selected destination.
Try the same path, payload, and credential directly against that destination to
separate upstream authentication from local routing. Avoid verbose request dumps
that print Authorization headers. Portway does not repair, refresh, or retry keys.
