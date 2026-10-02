# Request compression protocol

Portway uses standard zstd/gzip request content codings and the DCZ frame format
from [RFC 9842 section 5](https://www.rfc-editor.org/rfc/rfc9842.html#section-5).
The request-side advertisement, storage acknowledgements, and retry markers below
are Portway extensions, not the RFC's dictionary negotiation protocol. An arbitrary
HTTP service is not assumed to understand them: use a receiver or implement this
contract in an authenticated request layer.

## Capability discovery

A receiver serves `GET /__portway/capabilities`:

```http
X-Request-Encodings: zstd, gzip
X-Request-Dictionary: dcz
Content-Type: application/json

{"request_encodings":["zstd","gzip"]}
```

The dictionary header is absent when storage is disabled. A sender also accepts
an advertisement from `/health` when the primary endpoint advertises nothing, or
from an explicit configured origin path. It does not send request credentials in
capability probes. If a gateway protects those paths, requests initially remain
uncompressed unless a capability advertisement is reachable.

## Dictionary transport

| Header | Direction | Meaning |
| --- | --- | --- |
| `Content-Encoding: zstd`, `gzip`, or `dcz` | Request | Body coding |
| `X-Dict-Store: 1` | Request | Ask the receiver to retain the decoded body |
| `X-Dict-Stored: <64-character hex SHA-256>` | Response | Confirmation of the receiver's actual retained bytes |
| `X-Dict-Miss: 1` with 412 | Response | The named base is unavailable in this context |
| `X-Portway-Decode-Error: 1` | Response | The compression layer rejected the request before application execution |

A DCZ body consists of:

```text
5e 2a 4d 18 20 00 00 00 | SHA-256(dictionary), 32 bytes | zstd frame
```

The dictionary is the raw content of a previously confirmed request. The sender
uses a checksum-bearing zstd frame. Portway requires that checksum and verifies
its result. The receiver explicitly strips the first 40 bytes before decoding;
it does not hand the skippable dictionary header to the zstd frame decoder.

The sender retains at most eight acknowledged bases and 64MiB per route, across
all credential contexts. Only a base from the same context can be selected. The
candidate with the longest shared prefix wins. Bases larger than 32MiB, or a
base-plus-body larger than 128MiB, are not used. A response acknowledging a hash
other than the sender's actual body disables dictionaries for that route until
the refusal backoff expires and capabilities are renegotiated.

The receiver independently hashes the fully decoded body. It acknowledges only
compressed bodies explicitly opted into storage that fit both its per-entry and
total memory budgets. Identity requests do not seed dictionaries. Retained bodies
never go to disk. Restarting a receiver empties its store.

Authorization and Cookie values determine the default local storage partition;
only fingerprints are retained as scope keys. An application may override that
partition through the `DictionaryScope` request extension after authentication.
Authentication remains the gateway or embedding application's responsibility.
The scope is not transmitted as a new HTTP header.

## Errors and replay

The following decoder-marker contract governs the sender-to-receiver leg.

| Condition | Result |
| --- | --- |
| Missing dictionary | 412 + `X-Dict-Miss: 1`; no decompression or application invocation |
| Bad magic, truncation, checksum mismatch, trailing frame/member | 400 + decoder marker |
| Encoded or decoded body too large | 413 + decoder marker |
| Unknown/multiple coding, or DCZ disabled | 415 + decoder marker and `Accept-Encoding: zstd, gzip` |
| Application returns an error | Preserve the application's status/body; no decoder marker |

A marked DCZ 400, dictionary miss, or marked DCZ 415 causes one retry with plain
zstd. The missed base is discarded; a DCZ 415 disables dictionaries temporarily.
If that encoded retry itself gets a marked 415, one final identity attempt is
allowed. Plain zstd/gzip receiving a marked 415 goes directly to identity. Thus a
request makes at most three encoding attempts. Network errors and application
errors are not generally replayed; a stale pooled connection can be retried only
when the HTTP client returns its unsent request.

Earlier implementations advertising the same codec headers remain usable, but
unmarked 400/415 responses are returned unchanged. Safe replay requires the
explicit decoder marker. The receiver removes any such marker or dictionary-miss
header returned by the origin, so an application cannot accidentally trigger a
second execution. A storage acknowledgement does not imply application success.

### Optional receiver-to-origin policy

After restoring a request, a standalone receiver may recompress it for its origin
with `[receiver.origin_compression] mode = "auto"`. This leg uses ordinary gzip or
zstd without Portway dictionaries, advertisements, or retry markers. A server's
response `Accept-Encoding` advertises which request codings the resource accepts,
including on 2xx responses; it is distinct from the client's response preferences.
See [RFC 9110 §12.5.3](https://www.rfc-editor.org/rfc/rfc9110.html#section-12.5.3).

Unknown support permits one optimistic gzip trial. A compressed origin request
receiving 415 is resent as identity at most once unless identity was explicitly
excluded. This opt-in rule relies on the origin using 415 for rejection before
execution. It does not change the sender's marker requirement. A generic 400
suspends compression without replay; network failures and 5xx do not trigger a
replay or prove that compression is unsupported. The receiver continues stripping
origin-supplied decoder markers before returning the final response to its sender.

Refusals are remembered for ten minutes per request context. One trial after
expiry checks for recovery while concurrent requests remain uncompressed. For
cache scope, reload behavior, and eligibility, see
[receiver-to-origin configuration](configuration.md#receiver-to-origin-upload-compression).

## Streaming and HTTP boundaries

Request bodies are bounded and buffered before compression or restoration.
Responses are relayed incrementally, optionally decoded and re-encoded according
to the client's `Accept-Encoding`. When the client disconnects, the proxy waits
up to 2 s for the upstream answer to end: an answer that ends within that grace
is complete, with its usage, and its connection is pooled. An answer that sends
more than 1 KiB of itself meanwhile is still generating, and is cancelled at
once, as is one still open when the grace runs out: both proxy hops drop it and
close the active upstream HTTP/1.1 connection. Only fully consumed responses
return a connection to the pool. Upstream connections are HTTP/1.1 only, with
ALPN pinned, because that close is the cancellation signal: an HTTP/2 stream
cancel would leave the shared connection, and the generation, running (see the
[FAQ](faq.md#does-it-support-http2-websockets-streaming-uploads-or-inbound-tls)).

Hop-by-hop headers, including those named by `Connection`, are not forwarded.
Content length and encoding are recomputed after transformations. When the
response changes encoding, digests (`Content-MD5`, `Digest`, `Content-Digest`,
`Repr-Digest`) are removed, an `ETag` is kept but weakened (`"x"` becomes
`W/"x"`, a weak tag is unchanged), and `Vary` includes `Accept-Encoding`. HEAD, 204/304, partial responses, and `no-transform` responses
are not re-encoded. Authentication headers and application payload bytes retain
their meaning across the compression boundary.
