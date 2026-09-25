# Security

## Reporting a vulnerability

Report vulnerabilities privately through GitHub: open the repository's
**Security** tab and choose **Report a vulnerability**. Do not open a public
issue for a suspected vulnerability. Include the version or commit, the mode
(sender, receiver, or embedded library), and the smallest reproduction you
have. You should receive an acknowledgement within a week.

Portway is pre-1.0. Fixes land on `main` and in the next release; older
releases are not patched.

## What Portway is trusted with

Portway sits in the request path, so it handles everything a client sends. The
rules below describe what it does with that material. They hold for the CLI;
an application embedding `portway-core` owns its own listener, logging, and
storage.

### Credentials

- Request headers, including `Authorization`, `Cookie`, and custom key
  headers, are forwarded unchanged to the selected destination. A route change
  therefore changes where credentials go.
- Portway has no credential of its own, reads no API key from configuration or
  the environment, and rejects destination URLs with embedded credentials.
- Capability probes are sent without the caller's `Authorization` or `Cookie`.
- HTTPS destinations are verified against the bundled Mozilla root
  certificates (`webpki-roots`). The operating system's trust store is not
  consulted, so a destination using a private certificate authority fails
  verification.

### Request bodies and dictionaries

- Each request body is buffered in memory while it is forwarded, up to a
  configurable limit (256 MiB by default).
- A dictionary is a complete earlier request body. The sender and the receiver
  each hold dictionaries in memory only. They are never written to disk and
  are dropped on restart, after one idle hour on the receiver by default, or
  on eviction under the memory budget.
- Dictionaries are partitioned by a SHA-256 fingerprint of the exact
  `Authorization` and `Cookie` values, so a caller can only compress against,
  or have decoded against, bodies sent with the same credentials. Only the
  fingerprint is kept as the key. An embedding application can replace the
  partition with an explicit `DictionaryScope` after authenticating the caller.
- The receiver confirms a stored dictionary with the SHA-256 of the bytes it
  decoded. The sender uses a dictionary only when that hash matches its own,
  and disables dictionaries for the route when it does not.
- The receiver bounds both the encoded and the decoded body size and the zstd
  window (128 MiB by default), and rejects dictionary-compressed frames that
  carry no checksum. A compressed body that expands past the limit is refused
  with 413 before the application runs.

### What is recorded

The CLI records completed requests in `db.sqlite3` in its data directory.
Each row holds the time, route name, method, URL path without the query
string, status, connection and transfer timings, body and wire sizes, codings,
completion state, and any token counts the upstream reported. Log records
hold a level and a message. Log messages can include upstream host names and
error text.

Portway never records request or response bodies, header values, credentials,
cookies, query strings, or dictionary contents. A URL path is recorded as
sent, so do not put secrets or personal identifiers in paths.

When Portway creates the data directory, it creates it with mode 0700. A
serving process creates the database file with mode 0600, or narrows an
existing one to 0600, even inside a directory it did not create; the WAL
sidecar files share that mode. The daemon's pid and log files are created with
mode 0600, and so is `portway.web`, which holds the web console's address and
token while it runs.

## Deployment boundaries

- **No inbound authentication.** Anyone who can reach the listener can send
  requests through it and read `/__portway/stats` and `/__portway/health`.
  The default bind address is `127.0.0.1`. Binding to another interface
  exposes the listener to that network.
- **Receivers belong behind authentication.** A receiver stores what callers
  send and decodes against it. Put it behind a gateway that terminates TLS and
  authenticates every application request. Only
  `GET /__portway/capabilities` needs to be reachable without a key; it
  returns codec names and nothing else.
- **Gateways must preserve identity.** A gateway that replaces every caller's
  credentials with one shared value merges their dictionary partitions.
  Preserve distinct identity headers, run separate receivers per trust
  context, or set an explicit `DictionaryScope`. See
  [authentication](docs/authentication.md#receivers-and-dictionary-isolation).
- **Not supported:** inbound TLS, HTTP/2, WebSockets, and CONNECT tunnels.
  Portway refuses tunnels and protocol upgrades.

## Web console

The optional `--web` console (the `web` build feature) can read every recorded
request and stop or reload the forwarder, so it is guarded even on loopback:

- **A token per run.** The launch link carries 256 random bits after `#`, which
  browsers do not send to servers or put in referrers. The page removes it from
  the address bar and trades it once for an `HttpOnly`, `SameSite=Strict`
  session cookie whose value is a second, independent secret. Only the
  launching terminal, the daemon's launcher and `portway.web` see the token;
  the log records the address without it.
- **A launch code for the browser it opens.** A command line is visible to
  other local users, so the browser a foreground `--web` opens is handed a
  separate code, not the token. It opens one session and expires after two
  minutes; if someone else uses it first, the page asks for the printed link.
- **Requests from other sites are refused.** The `Host` header must name the
  console's port on `localhost` or an IP literal, which defeats DNS rebinding;
  a present `Origin` must be the console's own; every `POST` must carry an
  `x-portway-console` header, which a cross-site form cannot send.
- **The page cannot be turned against itself.** It is served with a
  Content-Security-Policy that allows only its own embedded files, no framing,
  no inline script, and no inline styles. Every text it shows is set as text,
  never parsed as markup.
- **No TLS.** The console speaks plain HTTP. Binding `--web-host` to a
  non-loopback address prints a warning: the token keeps strangers out, but
  anyone on the path can read the traffic. Put it behind a TLS-terminating
  proxy that preserves `Host`, or use an SSH tunnel instead.

## Replay safety

Portway resends a request only when the receiver's compression layer rejected
it before the application ran, which the receiver marks explicitly. The
receiver removes those markers from application responses, so an application
response cannot trigger a replay. See
[errors and replay](docs/protocol.md#errors-and-replay).
