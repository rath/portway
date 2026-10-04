# Frequently asked questions

Short answers, each linked to the page that holds the full rules. For a first
run, start with [getting started](getting-started.md).

## Fit

### Does Portway compress requests to a hosted provider API?

It depends on the hop and the provider. The ordinary sender compresses only when
its destination advertises the Portway request-compression protocol; otherwise it
forwards uncompressed. A [receiver](getting-started.md#add-a-compression-receiver)
lets you compress that first hop even when the final provider does not implement
the Portway protocol.

The receiver can separately enable
[`[receiver.origin_compression] mode = "auto"`](configuration.md#receiver-to-origin-upload-compression).
It tries gzip, learns from response `Accept-Encoding`, and remembers refusals.
Provider support is determined at runtime; no provider is assumed to support it.

### Does it reduce tokens or my bill?

No. The receiver restores the exact request bytes, so the model sees the same
input and counts the same tokens. Portway reduces the bytes uploaded and the
time spent uploading them. Provider-side prompt caching is the tool for the
cost of processing a repeated prefix; the two address different costs and can
be used together.

### Which clients work with it?

Any client that lets you set the API base URL, for example
`http://127.0.0.1:8787/v1`, or any HTTP application you can point at a local
address. Portway forwards paths, headers, and bodies as they are and does not
translate between API formats. See [getting started](getting-started.md#send-a-request).

### Where does it help most?

Long agent sessions whose requests grow append-only, sent over a link where
upload size or time matters: a remote development machine, a laptop on a slow
or metered connection, or a self-hosted model server in another region. Short
single-shot requests gain little, because nothing earlier can serve as a
dictionary.

### Does compression add latency?

It trades CPU on the sender for fewer bytes on the wire. Whether that is a net
gain depends on your link and your body sizes, so measure it: every recorded
request has upload and time-to-first-byte timings, and `portway --report`
shows their percentiles. Compare a run with the default settings against one
with `--coding off`, and lower `[compression] level` (default 11) if the
sender is CPU-bound. See [compression settings](configuration.md#compression).

## Dictionaries

### Why was my first request sent in full?

A dictionary is a previous request body that the receiver confirmed it stored.
The first request of a conversation has no such body, so it goes out with plain
zstd and becomes the dictionary for the next one. The
[benchmark](../README.md#measured) counts this cost in its session column.

### Why do small requests never build a dictionary?

The sender does not compress bodies under `min_bytes` (1 KiB by default), and
the receiver does not store bodies under `min_dictionary_bytes` (32 KiB by
default) as dictionaries. A conversation whose requests stay small therefore
uses plain zstd at most. See [receiver settings](configuration.md#receiving).

### Why did dictionary hits stop after I changed my API key?

Dictionaries are partitioned by a fingerprint of the exact `Authorization` and
`Cookie` header values, so one caller can never decode against another's
bodies. A new key or cookie starts a new, empty partition, and the next large
request warms it up again. See
[dictionary isolation](authentication.md#receivers-and-dictionary-isolation).

### Why does a restart start over?

Dictionaries are held only in memory, on both sides, by design: stored request
bodies never reach disk. Restarting either process, one hour without use (the
receiver's default `dictionary_ttl_seconds`), or eviction under the memory
budget each require one full request to warm up again.

### How much memory do dictionaries use?

The sender keeps at most eight confirmed bodies and 64 MiB per route. The
receiver keeps the newest sixteen bodies per credential context and 64 MiB in
total, evicting the least recently used entries first, and keeps nothing
larger than 32 MiB. A coding agent's turns average about 1.4 MB, so one
active context holds roughly 22 MB. The limits are in
[receiver settings](configuration.md#receiving).

### What happens when the receiver no longer has the dictionary?

It answers 412 with `X-Dict-Miss: 1` before the application runs. The sender
discards that dictionary and resends the same request once with plain zstd. A
sender request makes at most three encoding attempts on this hop. The receiver
forwards only the successfully decoded attempt; optional origin retries follow
the separate policy below. See [errors and replay](protocol.md#errors-and-replay).

### Can a retry run my request twice?

On the sender-to-receiver leg, Portway replays only when the receiver's compression
layer rejected it before the application ran, which it marks with `X-Dict-Miss` or
`X-Portway-Decode-Error`. The receiver strips those headers from application
responses, so an application cannot trigger a replay. Application errors and
network errors after sending are returned as they are.

The optional receiver-to-origin `auto` policy has a separate rule: an origin 415
on a request Portway compressed permits one identity retry, unless identity is
explicitly excluded. This relies on the origin rejecting the request before
execution; it does not require a Portway marker. General 400, 5xx, and transport
errors are never replayed by this policy. A 400 or 415 suspends origin compression
for ten minutes. See [errors and replay](protocol.md#errors-and-replay).

### Is DCZ a standard?

The frame format is: DCZ is Dictionary-Compressed Zstandard from
[RFC 9842 section 5](https://www.rfc-editor.org/rfc/rfc9842.html#section-5).
The capability advertisement, the storage acknowledgement, and the retry
markers are Portway extensions, documented in the [protocol](protocol.md).

## Operation

### The stats show `coding: null`. What now?

The sender found no compression advertisement at the destination, so it sends
requests uncompressed. Check that the receiver's `/__portway/capabilities`
endpoint is reachable without a key through your gateway. See
[checking compression](operations.md#check-compression).

### How do I check that it is working?

`python3 scripts/bench.py` runs a known-good sender, receiver, and origin on
your machine and fails loudly if dictionaries are not used. For real traffic,
compare `body_bytes` with `wire_bytes` and watch `dict_hits` in
`/__portway/stats`. See [measure it yourself](../README.md#measure-it-yourself).

### What does Portway see, and what does it keep?

It holds each request body in memory while forwarding it, and dictionary bodies
in memory as described above. The CLI records request metadata such as sizes,
timings, status codes, paths, and token counts in SQLite; it never records
bodies, headers, credentials, or dictionary contents. See
[SECURITY.md](../SECURITY.md) for the full list and the threat model.

### Does it support HTTP/2, WebSockets, streaming uploads, or inbound TLS?

No. Portway speaks HTTP/1.1, buffers each request body up to a configurable
limit (256 MiB by default), and streams responses. Terminate TLS and
authenticate callers in a gateway in front of it. See
[limits](../README.md#limits-and-verification).

Each of these is a decision rather than a gap:

- **HTTP/2.** The one signal an engine honors when an agent gives up is its
  connection closing. HTTP/1.1 gives each request its own connection, so
  dropping the response closes it and the engine stops. HTTP/2 multiplexes
  requests over a shared connection: cancelling a stream leaves that
  connection up, and an engine that watches the connection keeps generating
  to `max_tokens`, every token of it paid for. Portway therefore pins ALPN to
  `http/1.1` and dials upstreams with an HTTP/1.1 client. The per-connection
  phase timings (DNS, TCP, TLS, first byte) rest on the same
  one-request-per-connection rule.
- **WebSockets and CONNECT.** Both turn the proxy into an opaque byte pipe.
  Everything Portway does — dictionary compression, usage capture, pricing —
  needs to read the request and the response, and an open tunnel would also
  let callers reach arbitrary hosts through it. Vendor LLM APIs are plain
  request/response with SSE streaming, so nothing is lost; such requests are
  refused with a 501 rather than passed through.
- **Streaming uploads.** The dictionary lookup, the compression and the
  identity replay after a 415 all need the whole body in hand, so uploads are
  buffered up to the limit rather than streamed.
- **Inbound TLS.** Certificates and caller authentication belong in one place,
  the gateway, as [authentication](authentication.md) describes.
