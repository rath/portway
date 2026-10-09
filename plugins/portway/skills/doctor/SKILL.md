---
name: doctor
description: Diagnose a Codex or Claude Code connection through Portway when requests fail, bypass the proxy, or show no compression savings.
---

Find the client and intended Portway API listener from the conversation or its
connection setting. Read only the relevant connection fields, not credential
stores or entire settings files containing secrets. If the address is unknown,
ask for it. The web console address is a different listener.

Run `portway --version`, then
`portway doctor --client <codex|claude|both> --url <listener-url>`.
This checks user-level client configuration and Portway's health/stats with
bounded requests and no credentials. It does not modify settings or send a
model request. If the binary lacks `doctor`, explain that a build with this
feature is needed; do not turn diagnosis into an upgrade without authorization.

Interpret the result within its limits:
- Missing or mismatched client configuration: offer the plugin's setup skill.
- Connection refused or timeout: check the configured host/port and whether
  that instance is running. Do not kill or restart an existing service.
- HTTP 401/403 or an unexpected health response: the gateway may require
  separate access or the URL may point at the web console. Do not send the
  client's vendor credentials to a management endpoint.
- Upstream missing from stats: inspect the server's named mounts. Their
  appearance in stats does not prove that a client request took that route.
- Doctor passes but requests fail: inspect project/managed settings, CLI
  overrides, the `-p portway` selection, and the vendor login. Never dump auth
  stores or tokens. A real CLI turn plus a matching Portway log entry verifies
  the route; health alone does not.
- No delta savings: direct provider forwarding does not negotiate the Portway
  protocol. A receiver is required; dictionaries also need warm-up and a
  sufficiently large request.

Report observed checks separately from anything that still requires a real
CLI request. Ask before making configuration changes that the user has not
requested.
