// One event as the terminal prints it: the same fields in the same order,
// the same marks glued in the same places, as a list of toned segments the
// view turns into spans. Pure, so the line can be tested without a page.

import { human, humanCount, humanTime, label, ratio, share } from "./format.js";

/**
 * Every field a request line can carry, in draw order (tui::state::COLUMNS).
 * The notes describe the cells the page's table shows for each.
 */
export const COLUMNS = [
  { name: "time", note: "when the relay ended" },
  { name: "status", note: "the code the agent got" },
  { name: "cut", note: "✂ on a relay cut short" },
  { name: "model", note: "the model the turn named (- when none), and the upstream when it differs" },
  { name: "route", note: "method, full path, and the dial time of a fresh connection" },
  { name: "sizes", note: "request body raw and on the wire, saved, ↑ time" },
  { name: "ttfb", note: "first byte of the answer" },
  { name: "down", note: "response bytes decoded and on the upstream hop, saved, what the agent got, ↓ time, ↓ gap" },
  { name: "tokens", note: "in, the share of it cached, and out, as the engine counted" },
];

export const ALL_COLUMNS = COLUMNS.map((column) => column.name);

/** A set of column names from a comma list; unknown names are an error. */
export function parseColumns(list) {
  const names = list.split(",").map((name) => name.trim()).filter(Boolean);
  for (const name of names) {
    if (!ALL_COLUMNS.includes(name)) {
      throw new Error(`unknown column "${name}": ${ALL_COLUMNS.join(",")}`);
    }
  }
  return new Set(names);
}

/**
 * The size the answer crossed a hop as, for the line's download pair: the
 * upstream hop when it was coded, else the agent leg when the hop recoded it
 * (tui::view::down_wire).
 */
export function downWire(event) {
  if (event.upstream_encoding !== "identity" && event.received_wire > 0) return event.received_wire;
  if (event.received_agent > 0 && event.received_agent !== event.received) return event.received_agent;
  return null;
}

/** The tone a status code is drawn in. */
export function statusTone(status) {
  if (status < 300) return "good";
  if (status < 400) return "wire";
  if (status < 500) return "time";
  return "bad";
}

/**
 * Builds a line field by field: a field that is off closes its gap, and the
 * two one-cell marks take the place of a separator (tui::view::Fields).
 */
class Fields {
  constructor() {
    this.segments = [];
    this.gap = false;
    this.held = false;
  }

  word(text, tone) {
    const held = this.held;
    this.held = false;
    if (this.gap && !held) this.segments.push({ text: " ", tone: null });
    this.segments.push({ text, tone });
    this.gap = true;
  }

  bare(text, tone) {
    this.segments.push({ text, tone });
    this.gap = true;
    this.held = false;
  }

  glue(text, tone) {
    this.segments.push({ text, tone });
    this.gap = true;
    this.held = true;
  }
}

/** A request event's line, restricted to `columns` (a Set of names). */
export function requestLine(event, columns) {
  const line = new Fields();
  for (const name of ALL_COLUMNS) {
    if (!columns.has(name)) continue;
    switch (name) {
      case "time":
        line.word(event.stamp, "dim");
        break;
      case "status":
        line.word(String(event.status), statusTone(event.status));
        break;
      case "cut":
        if (!event.complete) line.glue("✂", "time");
        break;
      case "model":
        line.word(event.model || "-", "model");
        break;
      case "route":
        line.word(event.route, event.route_known ? "dim" : "bold");
        if (event.handshake != null) line.bare(".", "time");
        break;
      case "sizes":
        if (event.body_len === 0) break;
        line.word(human(event.body_len), "raw");
        line.glue("→", "dim");
        line.word(human(event.wire_len), "wire");
        if (event.coding != null) line.word(ratio(event.body_len, event.wire_len), "good");
        if (event.upload != null) line.word(humanTime(event.upload), "time");
        break;
      case "ttfb":
        line.word("ttfb", "dim");
        line.word(humanTime(event.ttfb), "time");
        break;
      case "down": {
        line.word("down", "dim");
        line.word(human(event.received), "raw");
        const wire = downWire(event);
        if (wire != null) {
          line.glue("→", "dim");
          line.word(human(wire), "wire");
          line.word(ratio(event.received, wire), "good");
        }
        if (event.download != null) line.word(humanTime(event.download), "time");
        break;
      }
      case "tokens": {
        const usage = event.usage;
        if (!usage) break;
        line.word("tok", "dim");
        line.word(humanCount(usage.prompt), "raw");
        const cached = usage.cached == null ? null : share(usage.cached, usage.prompt);
        if (cached != null) line.glue(`(${cached} cached)`, "dim");
        line.glue("→", "dim");
        line.word(humanCount(usage.completion), "wire");
        break;
      }
    }
  }
  return line.segments;
}

/** A log event's line: stamp, the level when it is WARNING or worse, message. */
export function logLine(event) {
  const tone = event.level === "ERROR" ? "bad" : event.level === "WARNING" ? "time" : null;
  const segments = [{ text: event.stamp, tone: "dim" }, { text: " ", tone: null }];
  if (tone) segments.push({ text: `${event.level} `, tone });
  segments.push({ text: event.message, tone });
  return segments;
}

export function eventLine(event, columns) {
  return event.kind === "request" ? requestLine(event, columns) : logLine(event);
}

/** The line as plain text, the way it is copied. */
export function lineText(segments) {
  return segments.map((segment) => segment.text).join("");
}

/** The detail popup's fields, word for word (tui::view::detail_lines). */
export function detailFields(event) {
  const optional = (value) => (value == null ? "not measured" : humanTime(value));
  const agent = event.received_agent > 0 && event.received_agent !== event.received
    ? ` -> ${human(event.received_agent)} sent to the agent (${ratio(event.received, event.received_agent)})`
    : "";
  const usage = event.usage;
  return [
    ["when", event.stamp],
    ["upstream", event.upstream],
    ["model", label(event.model || "-", event.tier)],
    ["request", `${event.method} ${event.path} -> ${event.status}`],
    ["connection", event.handshake == null
      ? "reused from the pool"
      : `dialed in ${humanTime(event.handshake)} (dns ${optional(event.dns)}, tcp ${optional(event.tcp)}, tls ${optional(event.tls)})`],
    ["upload", `${human(event.body_len)} -> ${human(event.wire_len)} (${event.coding ?? "identity"}, ${ratio(event.body_len, event.wire_len)})`],
    ["upload acked in", optional(event.upload)],
    ["ttfb", humanTime(event.ttfb)],
    ["download", `${human(event.received_wire)} on the wire -> ${human(event.received)} decoded (${event.upstream_encoding}${event.upstream_encoding === "identity" ? "" : `, ${ratio(event.received, event.received_wire)}`})${agent}`],
    ["download took", optional(event.download)],
    ["longest gap", optional(event.max_gap)],
    ["tokens", usage
      ? `${usage.prompt} in${usage.cached != null ? ` (${usage.cached} cached)` : ""} -> ${usage.completion} out${usage.reasoning != null ? ` (${usage.reasoning} reasoning)` : ""}`
      : "not reported"],
    ["ended", event.complete
      ? "upstream body finished"
      : "cut short (agent abort, error or read timeout)"],
  ];
}
