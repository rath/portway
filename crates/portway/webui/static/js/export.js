// The filtered events, as a file: CSV (RFC 4180) or JSON.

const FIELDS = [
  ["seq", (e) => e.seq],
  ["time", (e) => new Date(e.ts * 1000).toISOString()],
  ["kind", (e) => e.kind],
  ["level", (e) => e.level ?? ""],
  ["status", (e) => e.status ?? ""],
  ["model", (e) => e.model ?? ""],
  ["method", (e) => e.method ?? ""],
  ["path", (e) => e.path ?? ""],
  ["body_len", (e) => e.body_len ?? ""],
  ["wire_len", (e) => e.wire_len ?? ""],
  ["coding", (e) => e.coding ?? ""],
  ["upload_s", (e) => e.upload ?? ""],
  ["ttfb_s", (e) => e.ttfb ?? ""],
  ["received", (e) => e.received ?? ""],
  ["received_wire", (e) => e.received_wire ?? ""],
  ["received_agent", (e) => e.received_agent ?? ""],
  ["download_s", (e) => e.download ?? ""],
  ["complete", (e) => (e.kind === "request" ? e.complete : "")],
  ["handshake_s", (e) => e.handshake ?? ""],
  ["prompt_tokens", (e) => e.usage?.prompt ?? ""],
  ["cached_tokens", (e) => e.usage?.cached ?? ""],
  ["completion_tokens", (e) => e.usage?.completion ?? ""],
  ["reasoning_tokens", (e) => e.usage?.reasoning ?? ""],
  ["message", (e) => e.message ?? ""],
];

/**
 * One cell. Text a spreadsheet would run as a formula is prefixed with an
 * apostrophe; anything with a quote, comma or line break is quoted.
 */
export function cell(value) {
  let text = String(value);
  if (typeof value === "string" && /^[=+\-@\t\r]/.test(text)) text = `'${text}`;
  return /[",\r\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text;
}

export function toCsv(events) {
  const lines = [FIELDS.map(([name]) => name).join(",")];
  for (const event of events) {
    lines.push(FIELDS.map(([, read]) => cell(read(event))).join(","));
  }
  return `${lines.join("\r\n")}\r\n`;
}

export function toJson(events) {
  return `${JSON.stringify(events, null, 1)}\n`;
}

/** Rows of strings (a history table) as CSV. */
export function tableCsv(header, rows) {
  return `${[header, ...rows].map((row) => row.map(cell).join(",")).join("\r\n")}\r\n`;
}
