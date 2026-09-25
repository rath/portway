// The event list as a table. Each column the TUI line has becomes one or more
// cells with a header, every row's cells line up, and the route is spelled
// out (method and full path) where the terminal shortens it to fit. Pure, so
// it can be tested without a page.

import { statusTone } from "./eventline.js";
import { human, humanCount, humanTime, ratio } from "./format.js";

const optional = (value) => (value == null ? "not measured" : humanTime(value));

/**
 * The cells, in draw order. `column` is the picker column (eventline.js)
 * that shows or hides the cell; `read` gives `{ text, tone, title }` or null
 * for a blank cell; `cap` bounds the width in characters (longer text ends
 * in an ellipsis and keeps its full value in the tooltip). A cell that is
 * not `always` filled takes no room until some event fills it, the way the
 * terminal line leaves out a field it has nothing for.
 */
export const CELLS = [
  {
    key: "time", always: true, column: "time", label: "time", note: "when the relay ended",
    read: (e) => ({ text: e.stamp, tone: "dim" }),
  },
  {
    key: "status", always: true, column: "status", label: "status", numeric: true, note: "the code the agent got",
    read: (e) => ({ text: String(e.status), tone: statusTone(e.status) }),
  },
  {
    key: "cut", column: "cut", label: "cut", note: "✂: cut short (agent abort, error or read timeout)",
    read: (e) => (e.complete ? null : { text: "✂", tone: "time", title: "cut short (agent abort, error or read timeout)" }),
  },
  {
    key: "model", always: true, column: "model", label: "model", cap: 48, note: "the upstream the turn went to",
    read: (e) => ({ text: e.model, tone: "model", title: e.model }),
  },
  {
    key: "method", always: true, column: "route", label: "method", note: "the request method",
    read: (e) => ({ text: e.method, tone: "dim" }),
  },
  {
    key: "path", always: true, column: "route", label: "path", cap: 120, note: "the full path; bold when it is not a registered OpenAI route",
    read: (e) => ({ text: e.path, tone: e.route_known ? null : "bold", title: `${e.method} ${e.path}` }),
  },
  {
    key: "dial", column: "route", label: "dial", numeric: true, note: "handshake of a fresh connection; blank when a pooled one was reused",
    read: (e) => (e.handshake == null ? null : {
      text: humanTime(e.handshake),
      tone: "time",
      title: `fresh connection: dns ${optional(e.dns)}, tcp ${optional(e.tcp)}, tls ${optional(e.tls)}`,
    }),
  },
  {
    key: "raw", column: "sizes", label: "raw", numeric: true, note: "request body as the agent sent it",
    read: (e) => (e.body_len === 0 ? null : { text: human(e.body_len), tone: "raw" }),
  },
  {
    key: "wire", column: "sizes", label: "wire", numeric: true, note: "request body as it went upstream",
    read: (e) => (e.body_len === 0 ? null : { text: human(e.wire_len), tone: "wire", title: e.coding ?? "identity" }),
  },
  {
    key: "saved", column: "sizes", label: "saved", numeric: true, note: "upload saved by the coding",
    read: (e) => (e.body_len === 0 || e.coding == null ? null : { text: ratio(e.body_len, e.wire_len), tone: "good", title: e.coding }),
  },
  {
    key: "upload", column: "sizes", label: "↑ time", numeric: true, note: "until the upstream acknowledged the body",
    read: (e) => (e.body_len === 0 || e.upload == null ? null : { text: humanTime(e.upload), tone: "time" }),
  },
  {
    key: "ttfb", always: true, column: "ttfb", label: "ttfb", numeric: true, note: "first byte of the answer",
    read: (e) => ({ text: humanTime(e.ttfb), tone: "time" }),
  },
  {
    key: "down", always: true, column: "down", label: "down", numeric: true, note: "response bytes, decoded",
    read: (e) => ({ text: human(e.received), tone: "raw", title: `${human(e.received_wire)} on the wire (${e.upstream_encoding})` }),
  },
  {
    key: "agent", column: "down", label: "to agent", numeric: true, note: "what the agent got, when it differs",
    read: (e) => (e.received_agent > 0 && e.received_agent !== e.received
      ? { text: human(e.received_agent), tone: "wire", title: `${ratio(e.received, e.received_agent)} (${e.agent_encoding ?? "identity"})` }
      : null),
  },
  {
    key: "download", column: "down", label: "↓ time", numeric: true, note: "first byte to the last",
    read: (e) => (e.download == null ? null : { text: humanTime(e.download), tone: "time" }),
  },
  {
    key: "prompt", column: "tokens", label: "tok in", numeric: true, note: "prompt tokens the engine counted",
    read: (e) => (e.usage ? { text: humanCount(e.usage.prompt), tone: "raw", title: String(e.usage.prompt) } : null),
  },
  {
    key: "cached", column: "tokens", label: "cached", numeric: true, note: "of those, read from the cache",
    read: (e) => (e.usage?.cached == null ? null : { text: humanCount(e.usage.cached), tone: "dim", title: String(e.usage.cached) }),
  },
  {
    key: "completion", column: "tokens", label: "tok out", numeric: true, note: "completion tokens, reasoning included",
    read: (e) => (e.usage ? {
      text: humanCount(e.usage.completion),
      tone: "wire",
      title: e.usage.reasoning != null ? `${e.usage.completion} (${e.usage.reasoning} reasoning)` : String(e.usage.completion),
    } : null),
  },
];

/** Characters, not UTF-16 units: `✂` is one. */
function length(text) {
  let count = 0;
  for (const _ of text) count++;
  return count;
}

/**
 * The widest text each cell has held, in characters. It only grows, so a
 * column never jumps back while the list scrolls or old rows are evicted.
 */
export class Widths {
  constructor() {
    this.chars = Object.fromEntries(CELLS.map((cell) => [cell.key, length(cell.label)]));
    /** Cells some event has filled, and those that always are. */
    this.seen = new Set(CELLS.filter((cell) => cell.always).map((cell) => cell.key));
  }

  /** Measure one event; true when a column got wider or first appeared. */
  add(event) {
    if (event.kind !== "request") return false;
    let grew = false;
    for (const cell of CELLS) {
      const value = cell.read(event);
      if (!value) continue;
      if (!this.seen.has(cell.key)) {
        this.seen.add(cell.key);
        grew = true;
      }
      const chars = Math.min(length(value.text), cell.cap ?? Infinity);
      if (chars > this.chars[cell.key]) {
        this.chars[cell.key] = chars;
        grew = true;
      }
    }
    return grew;
  }
}

/** The cells `columns` (a Set of picker names) shows and some event filled, in draw order. */
export function shown(columns, widths) {
  return CELLS.filter((cell) => columns.has(cell.column) && widths.seen.has(cell.key));
}

/**
 * The path gives up room down to this many characters before the list
 * scrolls sideways: enough for the usual routes whole, even beside every
 * other column.
 */
export const PATH_FLOOR = 32;

/**
 * `grid-template-columns` for those cells, one track each, and the fewest
 * characters the tracks can take. Every track is fixed but the path's, which
 * takes what the list has room for up to its widest.
 */
export function template(cells, widths) {
  let least = 0;
  const tracks = cells.map((cell) => {
    const chars = widths.chars[cell.key];
    if (cell.key !== "path") {
      least += chars;
      return `${chars}ch`;
    }
    const floor = Math.min(chars, PATH_FLOOR);
    least += floor;
    return `minmax(${floor}ch, ${chars}ch)`;
  });
  return { tracks: tracks.join(" "), least };
}

/** A request's cells as `{ key, text, tone, title, numeric }`, blanks included. */
export function rowCells(event, cells) {
  return cells.map((cell) => {
    const value = cell.read(event);
    return {
      key: cell.key,
      text: value ? value.text : "",
      tone: value ? value.tone : null,
      title: value ? value.title ?? null : null,
      numeric: Boolean(cell.numeric),
    };
  });
}
