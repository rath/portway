# Portway

**A compression-first HTTP forwarder.**

Portway compresses request bodies with zstd or gzip, reuses upstream connections,
and relays streaming responses. With a cooperating receiver, DCZ compresses a
request against a previously acknowledged body, making repeated context cheap
to send. No provider, model endpoint, credential, or price is built in.

The workspace contains `portway-core`, an embeddable Rust library, and `portway`,
a CLI with recording, daemon control, reports, and an optional terminal dashboard.

## Build

Rust 1.97 or later and a C build toolchain are required. Supported platforms are
macOS and Linux. The committed lockfile fixes dependency versions.

```sh
cargo build --release --locked
# Optional dashboard; the default build has no terminal UI dependencies.
cargo build --release --locked --features tui
```

## Forward HTTP

```sh
./target/release/portway --upstream https://api.example.com
curl http://127.0.0.1:8787/your/path
```

The single-upstream mode forwards any supported HTTP method and arbitrary finite
request bodies. JSON and a `model` field are not required. Paths and queries are
preserved; a path in the upstream URL is a prefix (`https://example.com/api` plus
`/items?q=1` becomes `/api/items?q=1`). Already encoded request bodies pass through
unchanged. Request compression is attempted only when the upstream advertises
support, and only if it reduces the body size.

Authorization and cookies pass through unchanged. Upstream URLs must be explicit
HTTP(S) URLs without embedded credentials, queries, or fragments. TLS upstreams
use certificate verification. Default listening address is `127.0.0.1:8787`.

## Add a receiver to an existing HTTP service

On the server, put Portway behind your existing TLS/authentication gateway:

```sh
./target/release/portway receive --upstream http://127.0.0.1:8000 --port 8788
```

Point the sending Portway at the authenticated gateway's public URL. The receiver
advertises its codecs, restores compressed requests, and forwards ordinary HTTP
to the application. Application code needs no compression support. The gateway
must authenticate application requests **before** they reach the receiver. Expose
only `GET /__portway/capabilities` without credentials so senders can discover
compression support; it returns codec names, not application or dictionary data.
Portway does not
implement gateway authentication or inbound TLS termination.

DCZ requires both an advertisement and confirmation that the receiver stored the
base body. A missing dictionary causes a bounded retry with plain zstd. Decoder
errors are distinguishable from application errors; an application's 400 or 415
is returned without replaying the request. See [the protocol](docs/protocol.md).

## Configure routes and prices

```sh
./target/release/portway --config examples/models.toml
```

```toml
host = "127.0.0.1"
port = 8787

[models]
model-a = "https://first.example.com"
model-b = "https://second.example.com"

[compression]
coding = "auto"
level = 11
dict = "auto"
min_bytes = 1024

# Optional estimates, in USD per million tokens. No prices are supplied by default.
[prices.model-a]
input = 1.0
output = 2.0
cache_read = 0.1
```

Use either `upstream = "..."` or `[models]`. In model mode, `/v1/models` lists the
configured names. An uncompressed JSON body's `model` selects its upstream;
bodyless requests may use `?model=...`. The body's model takes precedence, and
an absent or unknown model is an error. No model names or payload fields are
rewritten. The single-upstream mode forwards `/v1/models` to the application.

Portway reads an explicit `--config` or the current directory's `portway.toml`.
An explicit missing file is an error. Explicit CLI values override file values;
`--upstream` replaces the file's routing table. Serving without any upstream
configuration fails. There are no provider-specific environment variables.

Prices apply to recorded route names. The single-upstream route is named
`upstream`. Missing prices remain unpriced, rather than being counted as zero.
The complete settings and defaults are in [configuration](docs/configuration.md).

## Observe and operate

```sh
./target/release/portway --config examples/forward.toml --daemon
./target/release/portway --status
./target/release/portway --reload
./target/release/portway --report --since 7d
./target/release/portway --stop

# Requires a build with --features tui:
./target/release/portway --config examples/forward.toml --tui
```

The CLI records request metadata in SQLite. Bodies, credentials, cookies, and
dictionary contents are never logged. `--data-dir` selects an isolated runtime
directory. Reload reopens the log and renegotiates codecs; changing configuration
requires a restart. A TUI can attach to an existing Portway on the selected port.
See [operations and dashboard keys](docs/operations.md).

Management paths are reserved under `/__portway/`: `health` is local liveness,
`stats` contains runtime counters, and `capabilities` advertises the receiver's
request codecs. Ordinary `/health` is forwarded. Receiver stats include separate
decoding and dictionary counters; its normal request log describes the final
hop to the application.

## Embed the core

```rust,no_run
use portway_core::{Forwarder, ForwarderConfig};
use bytes::Bytes;
use http::Request;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let forwarder = Forwarder::from_url(
    "api", "https://api.example.com", &ForwarderConfig::default(),
)?;
forwarder.negotiate().await;
let response = forwarder.forward(Request::builder()
    .method("POST")
    .uri("/items")
    .body(Bytes::from_static(b"arbitrary request bytes"))?
).await;
// Consume or stream response.into_body(); dropping it cancels the upstream.
# Ok(())
# }
```

The caller owns the Tokio runtime, configuration, authentication, and listener.
The core has no CLI, SQLite, or terminal UI dependencies and performs no implicit
recording. Inject `Telemetry` to observe an instance; counters and dictionary
state do not leak between instances. `DictionaryScope` can partition dictionaries
by application identity instead of the default Authorization/Cookie fingerprint.

`Receiver::decode` returns the restored request and an acknowledgement; attach it
with `Acknowledgement::finish` after calling your authenticated application's
handler. This works with any response body type. A convenience `Receiver::handle`
adapter supports Portway's streaming response type.

Buildable examples: [forwarding](crates/portway-core/examples/embedded.rs) and
[receiving](crates/portway-core/examples/receiving.rs).

## Limits and verification

Requests are buffered with a configurable 256MiB default limit; response bodies
stream incrementally. WebSockets, CONNECT, HTTP/2, inbound TLS, and unbounded
streaming uploads are not supported. Cancellation closes the upstream HTTP/1.1
connection. Dictionary storage is memory-only and bounded.

```sh
bash scripts/check.sh
```

Checks cover both default and TUI builds, formatting, clippy, real TCP forwarding,
sender/receiver interoperability, retry safety, dictionary limits and isolation,
stream cancellation, daemon lifecycle, recording, and dashboard rendering.

Licensed under [MIT](LICENSE).
