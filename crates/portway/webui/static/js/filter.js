// The event filter: the TUI's all / trouble / model cycle, and a search
// query on top of it.
//
//   words          every word must appear in the line (case-insensitive)
//   -word          and this one must not
//   "a phrase"     an exact run of words; key:"a value" quotes one too
//   status:5xx     a class; status:404, status:>=400, status:<300
//   model:alpha    the model the turn named (substring)
//   upstream:codex the upstream the turn went to (substring)
//   route:chat     method + path (substring)
//   level:warning  log level, WARNING and up with level:>=warning
//   is:cut is:trouble is:log is:request is:fresh is:reused is:coded
//   is:catalog     model catalog fetches (GET …/models), which are otherwise
//                  left out unless they failed (board::catalog)
//   ttfb:>2s  gap:>20s  size:>1MB  down:>10KB  tok:>50K   compare a number

import { lineText, requestLine, logLine, ALL_COLUMNS } from "./eventline.js";

const EVERY_COLUMN = new Set(ALL_COLUMNS);
const LEVELS = { INFO: 0, WARNING: 1, ERROR: 2 };

/** `2s`, `150ms`, `1m` → seconds. */
function seconds(text) {
  const match = /^(\d+(?:\.\d+)?)(ms|s|m)?$/i.exec(text);
  if (!match) return null;
  const value = Number(match[1]);
  switch ((match[2] || "s").toLowerCase()) {
    case "ms": return value / 1000;
    case "m": return value * 60;
    default: return value;
  }
}

/** `1MB`, `10KB`, `512` → bytes (1024 steps, like the sizes printed). */
function bytes(text) {
  const match = /^(\d+(?:\.\d+)?)(b|kb|k|mb|m|gb|g)?$/i.exec(text);
  if (!match) return null;
  const scale = { b: 1, k: 1024, kb: 1024, m: 1048576, mb: 1048576, g: 1073741824, gb: 1073741824 };
  return Number(match[1]) * scale[(match[2] || "b").toLowerCase()];
}

/** `50K`, `1.2M`, `900` → a count (1000 steps, like the counts printed). */
function count(text) {
  const match = /^(\d+(?:\.\d+)?)(k|m)?$/i.exec(text);
  if (!match) return null;
  return Number(match[1]) * { "": 1, k: 1e3, m: 1e6 }[(match[2] || "").toLowerCase()];
}

/** `>=2s` → [">=", "2s"]; a bare value compares for equality. */
function comparison(text) {
  const match = /^(>=|<=|>|<|=)?(.*)$/.exec(text);
  return [match[1] || "=", match[2]];
}

function compare(op, left, right) {
  switch (op) {
    case ">": return left > right;
    case ">=": return left >= right;
    case "<": return left < right;
    case "<=": return left <= right;
    default: return left === right;
  }
}

/** Split a query into terms, keeping quoted phrases whole. */
export function tokenize(query) {
  const terms = [];
  // An optional `key:` prefix may be followed by a quoted value.
  const pattern = /(-?)([a-z]+:)?(?:"([^"]*)"|(\S+))/gi;
  let match;
  while ((match = pattern.exec(query)) !== null) {
    const negated = match[1] === "-";
    const key = match[2] || "";
    const value = match[3] !== undefined ? match[3] : match[4];
    if (value === undefined || (value === "" && !key)) continue;
    terms.push({ negated, text: key + value, phrase: match[3] !== undefined && !key });
  }
  return terms;
}

/** One term → a predicate over an event, or an error string. */
function term({ text, phrase }) {
  const colon = phrase ? -1 : text.indexOf(":");
  if (colon > 0) {
    const key = text.slice(0, colon).toLowerCase();
    const raw = text.slice(colon + 1);
    const [op, value] = comparison(raw);
    const request = (test) => (event) => event.kind === "request" && test(event);
    switch (key) {
      case "status": {
        const cls = /^([1-5])xx$/i.exec(value);
        if (cls && op === "=") {
          const low = Number(cls[1]) * 100;
          return request((event) => event.status >= low && event.status < low + 100);
        }
        const code = Number(value);
        if (!Number.isInteger(code)) return `status: wants 5xx, 404 or >=400, not "${raw}"`;
        return request((event) => compare(op, event.status, code));
      }
      case "model": {
        const needle = value.toLowerCase();
        return request((event) => event.model.toLowerCase().includes(needle));
      }
      case "upstream": {
        const needle = value.toLowerCase();
        return request((event) => event.upstream.toLowerCase().includes(needle));
      }
      case "route": {
        const needle = value.toLowerCase();
        return request((event) => `${event.method} ${event.path}`.toLowerCase().includes(needle));
      }
      case "level": {
        const level = LEVELS[value.toUpperCase()];
        if (level === undefined) return `level: wants info, warning or error, not "${raw}"`;
        return (event) => event.kind === "log" && compare(op, LEVELS[event.level], level);
      }
      case "is":
        switch (value.toLowerCase()) {
          case "cut": return request((event) => !event.complete);
          case "trouble": return (event) => event.trouble;
          case "log": return (event) => event.kind === "log";
          case "request": return (event) => event.kind === "request";
          case "fresh": return request((event) => event.handshake != null);
          case "reused": return request((event) => event.reused);
          case "coded": return request((event) => event.coding != null);
          case "catalog": return request((event) => event.catalog === true);
          default: return `is: wants cut, trouble, log, request, fresh, reused, coded or catalog, not "${value}"`;
        }
      case "ttfb": {
        const limit = seconds(value);
        if (limit === null) return `ttfb: wants a time like >2s or <300ms, not "${raw}"`;
        return request((event) => compare(op === "=" ? ">=" : op, event.ttfb, limit));
      }
      case "gap": {
        const limit = seconds(value);
        if (limit === null) return `gap: wants a time like >20s or <1s, not "${raw}"`;
        return request((event) => event.max_gap != null &&
          compare(op === "=" ? ">=" : op, event.max_gap, limit));
      }
      case "size": {
        const limit = bytes(value);
        if (limit === null) return `size: wants a size like >1MB, not "${raw}"`;
        return request((event) => compare(op === "=" ? ">=" : op, event.body_len, limit));
      }
      case "down": {
        const limit = bytes(value);
        if (limit === null) return `down: wants a size like >10KB, not "${raw}"`;
        return request((event) => compare(op === "=" ? ">=" : op, event.received, limit));
      }
      case "tok": {
        const limit = count(value);
        if (limit === null) return `tok: wants a count like >50K, not "${raw}"`;
        return request((event) => event.usage != null &&
          compare(op === "=" ? ">=" : op, event.usage.prompt + event.usage.completion, limit));
      }
      default:
        break;
    }
  }
  const needle = text.toLowerCase();
  return (event) => searchText(event).includes(needle);
}

const TEXT = new WeakMap();

/** The line as printed with every column, plus the full path: what words match. */
export function searchText(event) {
  let text = TEXT.get(event);
  if (text === undefined) {
    const line = event.kind === "request" ? requestLine(event, EVERY_COLUMN) : logLine(event);
    text = `${lineText(line)} ${event.kind === "request" ? event.path : ""}`.toLowerCase();
    TEXT.set(event, text);
  }
  return text;
}

/** A model catalog fetch answered with a 2xx in full: counted, but not a line. */
const quiet = (event) => event.catalog === true && event.status >= 200 && event.status < 300 && event.complete;

/**
 * A query compiled once: `{ test, errors }`. An empty query accepts every
 * line, which leaves out a successful catalog fetch unless the query asks for
 * `is:catalog`; a malformed term is reported and left out.
 */
export function compile(query) {
  const errors = [];
  const tests = [];
  let catalog = false;
  for (const parsed of tokenize(query || "")) {
    const predicate = term(parsed);
    if (typeof predicate === "string") {
      errors.push(predicate);
      continue;
    }
    if (!parsed.negated && parsed.text.toLowerCase() === "is:catalog") catalog = true;
    tests.push(parsed.negated ? (event) => !predicate(event) : predicate);
  }
  return {
    test: (event) => (catalog || !quiet(event)) && tests.every((predicate) => predicate(event)),
    errors,
    empty: tests.length === 0,
  };
}

/** The TUI's cycle filter: "all", "trouble", or an upstream's name. */
export function modeAccepts(mode, event) {
  if (mode === "all") return true;
  if (mode === "trouble") return event.trouble;
  return event.kind === "request" && event.upstream === mode;
}

/**
 * The upstreams worth a row and a filter: those that have carried a request or
 * have one in flight. A route nothing has used yet only lengthens the table;
 * the terminal's upstream table leaves it out for the same reason
 * (tui::State::recent_models).
 */
export function inUse(models) {
  return models.filter((model) => model.requests > 0 || model.in_flight > 0);
}

/** `all -> trouble` and then once through the upstreams (tui::State::cycle_filter). */
export function nextMode(mode, models) {
  if (mode === "all") return "trouble";
  if (mode === "trouble") return models[0] ?? "all";
  const at = models.indexOf(mode);
  return at >= 0 && at + 1 < models.length ? models[at + 1] : "all";
}
