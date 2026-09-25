// The dashboard: header chips, the four HUD tiles, the model table, the two
// charts, the event list and the in-flight strip. Each part renders from
// `ctx.state` when the scheduler says it is dirty.

import { $, fill, h } from "../dom.js";
import { drawBars, drawTraffic } from "../charts.js";
import { Widths, handWidth, parseHand, rowCells, shown, template } from "../eventtable.js";
import { describe } from "../flights.js";
import { human, humanTime, maybeTime, ratio, span, uptime } from "../format.js";
import * as prefs from "../prefs.js";
import { SCALES } from "../series.js";
import { MONOCHROME } from "../themes.js";

// ------------------------------------------------------------------ header

export function renderHeader(ctx) {
  const { state } = ctx;
  const header = state.header;
  $("#listen").textContent = header.listen || "—";
  const chip = (label, value, tone) =>
    h("span", { class: "chip" }, h("span", { class: "label", text: label }), h("span", { class: tone ? `t-${tone}` : "", text: value }));
  const chips = [
    chip("mode", header.mode === "attached" ? "attached" : header.mode, header.mode === "attached" ? "time" : "good"),
    chip("up", uptime(ctx.uptime()), "time"),
    header.window_s != null
      ? chip("watching", span(header.window_s), "good")
      : chip("coding", header.coding || "-", "good"),
    chip("models", String(state.models.length)),
  ];
  if (state.generation > 0) chips.push(chip("reloads", String(state.generation), "wire"));
  fill($("#chips"), chips);
  const control = header.control || {};
  $("#reload").hidden = !control.reload;
  $("#stop").hidden = !control.stop;
  // A stopped forwarder has nothing left to reload or stop.
  const gone = state.stopped && header.mode !== "attached";
  $("#reload").disabled = gone;
  $("#stop").disabled = gone;
  $("#stop").title = header.mode === "attached" ? "Stop the daemon this console is watching (q)" : "Stop the forwarder (q)";
}

// --------------------------------------------------------------------- hud

function stat(label, value, tone, small) {
  return h("div", { class: "stat" },
    h("span", { class: "label", text: label }),
    h("span", { class: `value${tone ? ` t-${tone}` : ""}` }, value, small ? h("small", { text: ` ${small}` }) : null));
}

function tile(title, lead, stats) {
  return h("section", { class: "panel tile", "aria-label": title },
    h("h2", { text: title }),
    lead,
    h("div", { class: "stats" }, stats));
}

export function renderHud(ctx) {
  const { totals: t, counts: c, latency: l } = ctx.state;
  const reuse = c.seen ? `${Math.floor((c.reused * 100) / c.seen)}%` : "-";
  const quantile = (pair) => `${maybeTime(pair?.p50)} / ${maybeTime(pair?.p95)}`;
  const saved = Math.max(0, t.body_bytes - t.wire_bytes);
  const lead = (label, value, tone, small) => {
    const element = stat(label, value, tone, small);
    element.classList.add("lead");
    return element;
  };
  fill($("#hud"),
    tile("requests", lead("total", String(t.requests), null, `${t.in_flight} live`), [
      stat("2xx", String(c.ok), "good"),
      stat("4xx", String(c.client_errors), c.client_errors > 0 ? "time" : "dim"),
      stat("5xx", String(c.server_errors), c.server_errors > 0 ? "bad" : "dim"),
      stat("cut", String(c.truncated), "dim"),
      stat("aborts", String(t.aborts), "dim"),
      stat("up-err", String(t.upstream_errors), t.upstream_errors > 0 ? "bad" : "dim"),
      stat("reuse", reuse, "good"),
    ]),
    tile("upload", lead("saved", human(saved), "good", ratio(t.body_bytes, t.wire_bytes)), [
      stat("raw", human(t.body_bytes), "raw"),
      stat("wire", human(t.wire_bytes), "wire"),
      stat("encoded", `${t.encoded}/${t.requests}`),
      stat("415-retry", String(t.retried_identity), t.retried_identity > 0 ? "time" : "dim"),
    ]),
    tile("download", lead("decoded", human(t.down_bytes), "raw", ratio(t.down_bytes, t.down_wire_bytes)), [
      stat("wire", human(t.down_wire_bytes), "wire"),
      t.agent_bytes > 0 ? stat("agent", human(t.agent_bytes), "good", ratio(t.down_bytes, t.agent_bytes)) : null,
      stat("idle conns", String(t.idle_conns)),
    ]),
    tile("latency", lead("ttfb p50 / p95", quantile(l.ttfb), "time"), [
      stat("upload p50 / p95", quantile(l.upload), "time"),
      stat("handshake avg", maybeTime(l.handshake_mean), "time"),
      stat("samples", String(l.ttfb?.n ?? 0), "dim"),
    ]),
  );
}

// ------------------------------------------------------------------ models

const MODEL_COLUMNS = [
  { name: "model", left: true },
  { name: "coding", left: true },
  { name: "reqs" },
  { name: "live" },
  { name: "raw" },
  { name: "wire" },
  { name: "saved" },
  { name: "down", roomy: true },
  { name: "idle", roomy: true },
  { name: "err", roomy: true },
  { name: "abort", roomy: true },
  { name: "compression status", status: true, left: true },
];

function columnClass(column) {
  return [column.left ? "left" : "", column.roomy ? "roomy" : "", column.status ? "status-col" : ""].filter(Boolean).join(" ");
}

export function renderModels(ctx) {
  const { state } = ctx;
  const table = $("#models");
  table.className = "data";
  const panel = table.closest(".models");
  panel.hidden = state.models.length === 0;
  const head = h("thead", {}, h("tr", {}, MODEL_COLUMNS.map((column) => h("th", { class: columnClass(column), scope: "col", text: column.name }))));
  const rows = state.models.map((model) => {
    const cells = [
      [model.name, "model"],
      [model.coding_label, model.coding ? "good" : "dim"],
      [String(model.requests)],
      [String(model.in_flight), model.in_flight > 0 ? "good" : "dim"],
      [human(model.body), "raw"],
      [human(model.wire), "wire"],
      [human(model.saved), "good"],
      [human(model.down), "raw"],
      [String(model.idle)],
      [String(model.errors), model.errors > 0 ? "bad" : "dim"],
      [String(model.aborts), "dim"],
      [model.status, "time"],
    ];
    const selected = state.filterMode === model.name;
    return h("tr", {
      class: "clickable",
      "aria-selected": selected ? "true" : "false",
      title: selected ? "Show every model" : `Show only ${model.name}`,
      onclick: () => ctx.setMode(selected ? "all" : model.name),
    }, cells.map(([text, tone], at) => h("td", {
      class: [columnClass(MODEL_COLUMNS[at]), tone ? `t-${tone}` : ""].filter(Boolean).join(" "),
      "data-label": MODEL_COLUMNS[at].name,
      text,
    })));
  });
  fill(table, head, h("tbody", {}, rows));
}

// ------------------------------------------------------------------ charts

export function renderCharts(ctx) {
  const { state } = ctx;
  const barsCanvas = $("#bars-chart");
  if (barsCanvas.offsetParent === null) return;
  const monochrome = MONOCHROME.has(document.documentElement.dataset.theme);
  const shown = drawBars(barsCanvas, state.bars.items, monochrome);
  const window = shown.reduce((sum, [raw, wire]) => [sum[0] + raw, sum[1] + wire], [0, 0]);
  $("#bars-note").textContent = `${shown.length} turns · ${ratio(window[0], window[1])}`;
  barsCanvas.setAttribute("aria-label", `Request body per turn: ${shown.length} turns, ${human(window[0])} raw, ${human(window[1])} on the wire`);

  const trafficCanvas = $("#traffic-chart");
  const columns = Math.max(1, Math.floor((trafficCanvas.clientWidth - 92) / 3));
  const series = state.traffic.series(state.scale, columns);
  drawTraffic(trafficCanvas, series);
  const peak = (data) => human(data.reduce((max, value) => Math.max(max, value), 0));
  trafficCanvas.setAttribute("aria-label", `Socket bytes per second in ${state.scale}s buckets: peak up ${peak(series.up)}, peak down ${peak(series.down)}`);
  fill($("#scale"), SCALES.map((scale) => h("button", {
    type: "button",
    "aria-pressed": state.scale === scale ? "true" : "false",
    onclick: () => ctx.setScale(scale),
    text: `${scale}s`,
  })));
}

// ------------------------------------------------------------------ events

/** A cell's classes: its key, alignment and tone. */
function cellClass(key, numeric, tone) {
  return `cell c-${key}${numeric ? " num" : ""}${tone ? ` t-${tone}` : ""}`;
}

/**
 * The virtualized list, as a table under a sticky header row. Follow keeps
 * the newest line in view; any move up leaves it, and walking off the bottom
 * resumes it (tui::State::scroll). The header is one row tall, so row `at`
 * sits one row below where the scroll offset alone would put it. Its grips
 * set a column's width by hand, kept for this browser.
 */
export class EventList {
  constructor(ctx) {
    this.ctx = ctx;
    this.list = $("#events-list");
    this.head = $("#events-head");
    this.spacer = $("#events-spacer");
    this.rows = $("#events-rows");
    this.ignoreScrollUntil = 0;
    this.widths = new Widths();
    this.epoch = -1;
    this.measured = { low: Infinity, high: -Infinity };
    this.hand = parseHand(prefs.get("widths", null));
    this.keys = null;
    this.tracks = null;
    // A run of zeros in the list's font: what one `ch` is in pixels.
    this.probe = h("span", { class: "probe", "aria-hidden": "true", text: "0".repeat(20) });
    this.list.append(this.probe);
    this.bindResize();
    this.list.addEventListener("scroll", () => this.onScroll(), { passive: true });
    this.list.addEventListener("click", (event) => {
      const row = event.target.closest(".row");
      if (row) this.select(Number(row.dataset.seq));
    });
    this.list.addEventListener("dblclick", (event) => {
      const row = event.target.closest(".row");
      if (row) ctx.openDetail({ type: "event", seq: Number(row.dataset.seq) });
    });
  }

  get store() {
    return this.ctx.state.events;
  }

  rowHeight() {
    const value = parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--row"));
    return Number.isFinite(value) && value > 0 ? value : 24;
  }

  /** Rows the list has room for under its header: what a page moves by. */
  viewport() {
    const height = this.rowHeight();
    return Math.max(1, Math.floor((this.list.clientHeight - height) / height));
  }

  cursorIndex() {
    const { state } = this.ctx;
    const length = this.store.filtered.length;
    if (state.follow || state.cursor == null) return length - 1;
    return this.store.position(state.cursor);
  }

  scroll(delta) {
    const { state } = this.ctx;
    const length = this.store.filtered.length;
    if (!length) return;
    const next = this.cursorIndex() + delta;
    if (next >= length - 1 && delta > 0) {
      this.follow();
      return;
    }
    const at = Math.min(Math.max(next, 0), length - 1);
    state.follow = false;
    state.cursor = this.store.filtered[at];
    this.reveal(at);
    this.ctx.invalidate("events");
  }

  page(pages) {
    this.scroll(pages * this.viewport());
  }

  oldest() {
    if (!this.store.filtered.length) return;
    this.ctx.state.follow = false;
    this.ctx.state.cursor = this.store.filtered[0];
    this.reveal(0);
    this.ctx.invalidate("events");
  }

  follow() {
    this.ctx.state.follow = true;
    this.ctx.state.cursor = null;
    this.ctx.invalidate("events");
  }

  select(seq) {
    this.ctx.state.follow = false;
    this.ctx.state.cursor = seq;
    this.ctx.invalidate("events");
  }

  /** Bring filtered row `at` into view, below the header, moving no more than needed. */
  reveal(at) {
    const height = this.rowHeight();
    const top = at * height;
    let scroll = this.list.scrollTop;
    if (top < scroll) scroll = top;
    else if (top + 2 * height > scroll + this.list.clientHeight) scroll = top + 2 * height - this.list.clientHeight;
    this.setScroll(scroll);
  }

  setScroll(value) {
    if (Math.abs(this.list.scrollTop - value) < 1) return;
    this.ignoreScrollUntil = performance.now() + 80;
    this.list.scrollTop = value;
  }

  onScroll() {
    if (performance.now() < this.ignoreScrollUntil) {
      this.draw();
      return;
    }
    const atBottom = this.list.scrollTop + this.list.clientHeight >= this.list.scrollHeight - 2;
    const { state } = this.ctx;
    if (atBottom && !state.follow) {
      state.follow = true;
      state.cursor = null;
    } else if (!atBottom && state.follow) {
      state.follow = false;
    }
    this.ctx.invalidate("events");
  }

  /**
   * Widen the columns for events not measured yet: the new ones at the end,
   * older ones loaded in front. A reset of the store measures from scratch.
   */
  measure() {
    const items = this.store.items;
    if (this.store.epoch !== this.epoch) {
      this.epoch = this.store.epoch;
      this.widths = new Widths();
      this.measured = { low: Infinity, high: -Infinity };
    }
    if (!items.length) return;
    let at = items.length - 1;
    while (at >= 0 && items[at].seq > this.measured.high) this.widths.add(items[at--]);
    for (let from = 0; from <= at && items[from].seq < this.measured.low; from++) this.widths.add(items[from]);
    this.measured = { low: Math.min(this.measured.low, items[0].seq), high: items[items.length - 1].seq };
  }

  /**
   * The header row, rebuilt only when the cells change (never mid-drag), and
   * the column tracks, set whenever a width does.
   */
  columns() {
    const cells = shown(this.ctx.state.columns, this.widths);
    const keys = cells.map((cell) => cell.key).join(",");
    if (keys !== this.keys) {
      this.keys = keys;
      fill(this.head, cells.map((cell) => h("span", { class: cellClass(cell.key, cell.numeric, null), title: cell.note },
        h("span", { class: "label", text: cell.label }),
        h("span", { class: "grip", dataset: { key: cell.key }, title: "Drag to resize; double-click to fit the content" }))));
    }
    const { tracks, least, gaps } = template(cells, this.widths, this.hand);
    if (tracks !== this.tracks) {
      this.tracks = tracks;
      this.list.style.setProperty("--cells", tracks);
      this.list.style.setProperty("--cells-chars", String(least));
      this.list.style.setProperty("--cells-gaps", String(gaps));
    }
    return cells;
  }

  /** Drag a header grip to set that column's width; double-click it to fit the content again. */
  bindResize() {
    this.head.addEventListener("pointerdown", (event) => {
      const grip = event.target.closest(".grip");
      if (!grip || event.button !== 0) return;
      event.preventDefault();
      const key = grip.dataset.key;
      const unit = this.probe.getBoundingClientRect().width / 20 || 8;
      const from = event.clientX;
      const start = grip.parentElement.getBoundingClientRect().width / unit;
      document.body.classList.add("resizing");
      const move = (moved) => {
        this.hand[key] = handWidth(start + (moved.clientX - from) / unit);
        this.columns();
      };
      const end = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", end);
        window.removeEventListener("pointercancel", end);
        document.body.classList.remove("resizing");
        this.saveHand();
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", end);
      window.addEventListener("pointercancel", end);
    });
    this.head.addEventListener("dblclick", (event) => {
      const grip = event.target.closest(".grip");
      if (!grip) return;
      delete this.hand[grip.dataset.key];
      this.saveHand();
      this.columns();
    });
  }

  saveHand() {
    prefs.set("widths", JSON.stringify(this.hand));
  }

  /** Forget every width set by hand; how many there were. */
  resetWidths() {
    const count = Object.keys(this.hand).length;
    this.hand = {};
    this.saveHand();
    this.columns();
    return count;
  }

  render() {
    const { state } = this.ctx;
    const height = this.rowHeight();
    const length = this.store.filtered.length;
    this.measure();
    this.spacer.style.height = `${length * height}px`;
    if (state.follow) this.setScroll(Math.max(0, (length + 1) * height - this.list.clientHeight));
    this.draw();
  }

  /** One request's cells, or a log record's stamp and message across the rest. */
  cells(event, cells) {
    if (event.kind === "request") {
      return rowCells(event, cells).map((cell) => h("span", {
        class: cellClass(cell.key, cell.numeric, cell.tone),
        title: cell.title,
        text: cell.text,
      }));
    }
    const tone = event.level === "ERROR" ? "bad" : event.level === "WARNING" ? "time" : null;
    const stamped = this.ctx.state.columns.has("time");
    const message = tone ? `${event.level} ${event.message}` : event.message;
    return [
      stamped ? h("span", { class: cellClass("time", false, "dim"), text: event.stamp }) : null,
      h("span", { class: `${cellClass("message", false, tone)}${stamped ? "" : " whole"}`, title: event.message, text: message }),
    ];
  }

  draw() {
    const { state } = this.ctx;
    const height = this.rowHeight();
    const filtered = this.store.filtered;
    const length = filtered.length;
    const cells = this.columns();
    const first = Math.max(0, Math.floor(this.list.scrollTop / height));
    const count = Math.ceil(Math.max(0, this.list.clientHeight - height) / height) + 1;
    const last = Math.min(length, first + count);
    this.rows.style.transform = `translateY(${first * height}px)`;
    const selected = state.follow ? null : state.cursor;
    const rows = [];
    for (let at = first; at < last; at++) {
      const event = this.store.get(filtered[at]);
      if (!event) continue;
      rows.push(h("div", {
        // Stripes follow the row's place in the list, not in the drawn slice.
        class: `row ${event.kind}${at % 2 ? " alt" : ""}`,
        id: `ev-${event.seq}`,
        role: "option",
        "aria-selected": event.seq === selected ? "true" : "false",
        dataset: { seq: String(event.seq) },
      }, this.cells(event, cells)));
    }
    if (!length) {
      rows.push(h("div", {
        class: "empty",
        text: this.store.size ? "nothing matches this filter" : "no events yet: requests appear here as they finish",
      }));
    }
    fill(this.rows, rows);
    if (selected != null) this.list.setAttribute("aria-activedescendant", `ev-${selected}`);
    else this.list.removeAttribute("aria-activedescendant");

    // Lines hidden under the viewport: what scrolling back down would reveal.
    const below = state.follow ? 0 : Math.max(0, length - last);
    const position = $("#position");
    fill(position, state.follow
      ? h("span", { class: "follow", text: "FOLLOW" })
      : h("span", { class: "paused", text: below > 0 ? `PAUSED · ${below} below` : "PAUSED" }));
    const mode = state.filterMode === "all" ? "all" : state.filterMode;
    $("#counts").textContent = `${length} lines · filter ${mode}${state.query ? ` · “${state.query}”` : ""}`;
  }
}

// ----------------------------------------------------------------- flights

export function renderFlights(ctx) {
  const { state } = ctx;
  const flights = state.flights;
  const rows = $("#flights-rows");
  const now = ctx.now();
  const open = state.flightsOpen;
  $("#flights-toggle").setAttribute("aria-expanded", open ? "true" : "false");
  rows.hidden = !open;
  if (!flights.available) {
    $("#flights-title").textContent = "IN FLIGHT · n/a";
    fill(rows, h("div", {
      class: "note",
      text: "In-flight requests are visible only in the console of the process serving them: run --web on the forwarder itself, or --daemon --web.",
    }));
    return;
  }
  const oldest = flights.list.length ? ` · oldest ${humanTime(flights.oldest(now))}` : "";
  const more = flights.more ? ` (+${flights.more} not listed)` : "";
  $("#flights-title").textContent = `IN FLIGHT · ${flights.total}${more}${oldest}`;
  if (!open) return;
  if (!flights.list.length) {
    fill(rows, h("div", { class: "note", text: "nothing in flight" }));
    return;
  }
  fill(rows, flights.list.map((flight) => {
    const described = describe(flight, flights.times(flight, now));
    return h("div", {
      class: "flight",
      role: "button",
      tabindex: "0",
      title: "Follow this request",
      onclick: () => ctx.openDetail({ type: "flight", id: flight.id }),
      onkeydown: (event) => {
        if (event.key === "Enter") ctx.openDetail({ type: "flight", id: flight.id });
      },
    },
    h("span", { class: "flight-model t-model", text: flight.model }),
    h("span", { class: "flight-method t-dim", text: flight.method }),
    h("span", { class: `flight-path${flight.route_known ? "" : " t-bold"}`, title: `${flight.method} ${flight.path}`, text: flight.path }),
    h("span", { class: flight.phase === "stream" ? "t-good" : "t-time", text: described.text }),
    described.warn ? h("span", { class: `badge ${described.warn === "stalled" ? "bad" : "warn"}`, text: described.warn }) : h("span"));
  }));
}

// ---------------------------------------------------------------- key strip

const KEYS = [
  ["q", "stop"], ["↑↓/jk", "scroll"], ["g/G", "top/live"], ["↵", "detail"], ["e", "trouble"],
  ["m", "model"], ["u", "usage"], ["t", "buckets"], ["c", "columns"], ["/", "search"], ["⌘K", "commands"], ["?", "keys"],
];

export function renderKeys(ctx) {
  const keys = ctx.state.shortcuts ? KEYS : [["⌘K", "commands"], ["esc", "close"]];
  fill($("#keys"), keys.map(([key, what]) => h("span", {}, h("kbd", { text: key }), ` ${what}`)));
}
