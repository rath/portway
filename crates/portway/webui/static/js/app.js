// The console: sign in, take a snapshot, then follow the stream. State lives
// here; views render from it when the scheduler flushes what changed, once
// per animation frame (once a second while the tab is hidden).

import { displayModel, setModelAliases } from "./modelnames.js";
import { ApiError, api, signIn } from "./api.js";
import { Connection } from "./connection.js";
import { $, download, fill, h } from "./dom.js";
import { ALL_COLUMNS, COLUMNS, eventLine, lineText } from "./eventline.js";
import { toCsv, toJson } from "./export.js";
import { compile, inUse, modeAccepts, nextMode } from "./filter.js";
import { Flights } from "./flights.js";
import * as prefs from "./prefs.js";
import { EventStore } from "./ring.js";
import { Bars, History, SCALES, Traffic } from "./series.js";
import { SYSTEM, THEMES, byId, resolve } from "./themes.js";
import { AppearanceView } from "./views/appearance.js";
import { EventList, renderCharts, renderFlights, renderHeader, renderHud, renderModels } from "./views/dashboard.js";
import { openColumns, openHelp, openPalette, openStop, renderDetail } from "./views/dialogs.js";
import { HistoryView } from "./views/history.js";
import { InsightsView } from "./views/insights.js";
import { UsageView } from "./views/usage.js";

/** A stream that has sent nothing for this long is called stalled. */
const STALLED_MS = 5000;
/** Desktop notifications, at most one per this long. */
const NOTIFY_EVERY_MS = 30000;

function savedColumns() {
  const saved = prefs.get("columns", null);
  if (saved === null) return new Set(ALL_COLUMNS);
  return new Set(saved.split(",").filter((name) => ALL_COLUMNS.includes(name)));
}

const state = {
  header: { listen: "", coding: "", mode: "live", control: {} },
  generation: 0,
  totals: {
    requests: 0, encoded: 0, in_flight: 0, body_bytes: 0, wire_bytes: 0, down_bytes: 0,
    down_wire_bytes: 0, agent_bytes: 0, retried_identity: 0, aborts: 0, upstream_errors: 0, idle_conns: 0,
  },
  counts: { seen: 0, ok: 0, redirected: 0, client_errors: 0, server_errors: 0, reused: 0, truncated: 0 },
  latency: { ttfb: {}, upload: {}, handshake_mean: null },
  models: [],
  uptime: { value: 0, at: performance.now() },
  bars: new Bars(),
  traffic: new Traffic(),
  scale: SCALES.includes(Number(prefs.get("scale", 1))) ? Number(prefs.get("scale", 1)) : 1,
  events: new EventStore(),
  flights: new Flights(),
  filterMode: "all",
  query: "",
  columns: savedColumns(),
  follow: true,
  cursor: null,
  conn: "connecting",
  lastFrame: 0,
  history: new History(),
  view: "dashboard",
  detail: null,
  flightsOpen: prefs.flag("flights", true),
  shortcuts: prefs.flag("shortcuts", true),
  notify: prefs.flag("notify", false),
  stopped: false,
  noOlder: false,
  unread: 0,
};

// --------------------------------------------------------------- scheduler

const dirty = new Set();
let pending = false;

/**
 * Mark parts dirty and flush them on the next animation frame, or, while the
 * tab is hidden (when frames stop), once a second: a background tab still
 * shows current numbers the moment it is looked at, or screenshotted.
 */
function invalidate(...parts) {
  for (const part of parts) dirty.add(part);
  if (pending) return;
  pending = true;
  if (document.hidden) setTimeout(flush, 1000);
  else requestAnimationFrame(flush);
}

function flush() {
  if (!pending) return;
  pending = false;
  const parts = new Set(dirty);
  dirty.clear();
  const dashboard = state.view === "dashboard";
  if (parts.has("header")) renderHeader(ctx);
  if (parts.has("hud") && dashboard) renderHud(ctx);
  if (parts.has("models") && dashboard) renderModels(ctx);
  if (parts.has("charts") && dashboard) renderCharts(ctx);
  if (parts.has("events") && dashboard) list.render();
  if (parts.has("flights") && dashboard) renderFlights(ctx);
  if (parts.has("detail")) renderDetail(ctx);
  if (parts.has("insights") && state.view === "insights") views.insights.render();
  if (parts.has("title")) renderTitle();
}

function renderAll() {
  invalidate("header", "hud", "models", "charts", "events", "flights", "detail", "insights", "title");
}

// ------------------------------------------------------------------ context

const ctx = {
  state,
  api,
  invalidate,
  now: () => Date.now() / 1000,
  uptime() {
    const live = state.header.mode !== "attached" && !state.stopped;
    return state.uptime.value + (live ? (performance.now() - state.uptime.at) / 1000 : 0);
  },
  toast,
  expired,
  openDetail(target) {
    state.detail = target;
    invalidate("detail");
  },
  closeDetail() {
    state.detail = null;
    if ($("#detail").open) $("#detail").close();
  },
  setMode(mode) {
    state.filterMode = mode;
    applyFilter();
  },
  setQuery(text) {
    state.query = text;
    $("#query").value = text;
    applyFilter();
    go("dashboard");
  },
  setScale(scale) {
    state.scale = scale;
    prefs.set("scale", scale);
    invalidate("charts");
  },
  toggleColumn(name, on) {
    if (on) state.columns.add(name);
    else state.columns.delete(name);
    prefs.set("columns", ALL_COLUMNS.filter((column) => state.columns.has(column)).join(","));
    invalidate("events", "detail");
  },
  stop: () => stopForwarder(),
  theme: () => document.documentElement.dataset.themeChoice || "system",
  resolved: () => resolve(ctx.theme(), darkQuery.matches),
  setTheme(id) {
    const choice = id === "system" || byId(id) ? id : "system";
    document.documentElement.dataset.themeChoice = choice;
    document.documentElement.dataset.theme = resolve(choice, darkQuery.matches);
    prefs.set("theme", choice);
    renderAll();
  },
  setLook(key, value) {
    document.documentElement.dataset[key] = value;
    prefs.set(key, value);
    renderAll();
  },
  setShortcuts(on) {
    state.shortcuts = on;
    prefs.setFlag("shortcuts", on);
  },
  async setNotify(on) {
    if (on && "Notification" in window && Notification.permission === "default") {
      await Notification.requestPermission();
    }
    const granted = "Notification" in window && Notification.permission === "granted";
    state.notify = on && granted;
    prefs.setFlag("notify", state.notify);
    if (on && !granted) toast("the browser did not allow notifications", "bad");
    if (state.view === "appearance") views.appearance.render();
  },
};

const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");
darkQuery.addEventListener("change", () => {
  if (ctx.theme() === "system") ctx.setTheme("system");
});

const list = new EventList(ctx);
const views = {
  usage: new UsageView(ctx),
  history: new HistoryView(ctx),
  insights: new InsightsView(ctx),
  appearance: new AppearanceView(ctx),
};

// ------------------------------------------------------------------- toasts

function toast(message, tone = "") {
  const element = h("div", { class: `toast ${tone}`, text: message });
  $("#toasts").append(element);
  setTimeout(() => element.remove(), 4000);
}

function banner(text, bad = false) {
  const element = $("#banner");
  element.hidden = !text;
  element.classList.toggle("bad", bad);
  element.textContent = text || "";
}

function gate(text) {
  $("#gate").hidden = false;
  $("#gate-text").textContent = text;
}

function expired() {
  setConn("expired");
  source?.close();
  connection.stop();
  banner("");
  gate("Sign in with the console link ending in #token=… (available from portway --status on the server). " +
    "This browser will stay signed in across restarts. If access was reset, use the new link.");
}

// ------------------------------------------------------------------ filters

function applyFilter() {
  const compiled = compile(state.query);
  fill($("#query-errors"), compiled.errors.join(" · "));
  state.events.setFilter((event) => modeAccepts(state.filterMode, event) && compiled.test(event));
  state.follow = true;
  state.cursor = null;
  renderModes();
  invalidate("events", "models");
}

function renderModes() {
  const modes = ["all", "trouble", ...inUse(state.models).map((model) => model.name)];
  if (!modes.includes(state.filterMode)) modes.push(state.filterMode);
  fill($("#modes"), modes.map((mode) => h("button", {
    type: "button",
    "aria-pressed": state.filterMode === mode ? "true" : "false",
    onclick: () => ctx.setMode(mode),
    title: mode,
    text: mode === "all" || mode === "trouble" ? mode : displayModel(mode),
  })));
}

// ----------------------------------------------------------------- snapshot

function applyTotals(frame) {
  if (setModelAliases(frame.model_aliases ?? {})) {
    applyFilter();
    renderAll();
    if (state.view === "usage") views.usage.render();
    if (state.view === "history") views.history.render();
    const costs = $("#costs");
    if (costs?.open) views.usage.costs();
  }
  state.totals = frame.totals;
  state.counts = frame.counts;
  state.latency = frame.latency;
  // A route's first request adds its filter, so the names in use are compared.
  const used = () => inUse(state.models).map((model) => model.name).join();
  const names = used();
  state.models = frame.models;
  if (names !== used()) renderModes();
  state.uptime = { value: frame.uptime_s ?? frame.header?.uptime_s ?? 0, at: performance.now() };
  const saved = frame.totals.body_bytes > 0
    ? ((frame.totals.body_bytes - frame.totals.wire_bytes) * 100) / frame.totals.body_bytes
    : null;
  state.history.add(Date.now() / 1000, { p50: frame.latency.ttfb?.p50 ?? null, p95: frame.latency.ttfb?.p95 ?? null, saved });
}

function applySnapshot(snapshot) {
  state.header = snapshot.header;
  state.generation = snapshot.generation;
  applyTotals({ ...snapshot, uptime_s: snapshot.header.uptime_s });
  state.bars.load(snapshot.bars, snapshot.bars_total);
  state.traffic.load(snapshot.traffic);
  state.flights.load(snapshot.flights);
  state.events.reset(snapshot.events);
  state.noOlder = snapshot.oldest === 0 || (snapshot.events[0]?.seq ?? 0) <= snapshot.oldest;
  $("#load-older").disabled = state.noOlder;
  document.title = "portway";
  renderModes();
  if (state.header.mode === "attached") {
    banner(`Attached to the forwarder on ${state.header.listen}: reading its database, last ${Math.round((state.header.window_s ?? 3600) / 60)} minutes. Reload and Stop signal its daemon.`);
  } else if (!state.stopped) {
    banner("");
  }
  renderAll();
}

function resync() {
  source?.close();
  connection.start();
}

// ------------------------------------------------------------------- stream

let source = null;

function setConn(conn) {
  if (state.conn === conn) return;
  state.conn = conn;
  const pill = $("#conn");
  pill.dataset.state = conn;
  pill.textContent = conn;
}

function openStream() {
  source?.close();
  source = new EventSource(api.streamUrl(state.events.newest));
  source.addEventListener("open", () => {
    state.lastFrame = performance.now();
    if (!state.stopped) setConn("live");
  });
  source.addEventListener("ev", (message) => onEvent(JSON.parse(message.data)));
  source.addEventListener("tick", (message) => onTick(JSON.parse(message.data)));
  source.addEventListener("flights", (message) => {
    state.lastFrame = performance.now();
    state.flights.load(JSON.parse(message.data));
    invalidate("flights", "detail");
  });
  source.addEventListener("control", (message) => onControl(JSON.parse(message.data)));
  source.addEventListener("reset", () => {
    source.close();
    resync();
  });
  source.addEventListener("error", () => onStreamError());
}

function onStreamError() {
  source?.close();
  if (state.stopped) return;
  connection.retry();
}

function onEvent(event) {
  state.lastFrame = performance.now();
  if (!state.events.push(event)) return;
  if (event.kind === "request" && event.flight != null) {
    state.flights.finish(event.flight);
    // A drawer following that flight follows it into its record.
    if (state.detail?.type === "flight" && state.detail.id === event.flight) {
      state.detail = { type: "event", seq: event.seq };
      invalidate("detail");
    }
  }
  if (event.trouble) noticeTrouble(event);
  invalidate("events", "flights");
}

function onTick(tick) {
  state.lastFrame = performance.now();
  if (state.conn !== "live" && !state.stopped) setConn("live");
  applyTotals(tick);
  state.bars.push(tick.bars_push, tick.bars_total);
  state.traffic.merge(tick.traffic_tail);
  if (tick.generation !== state.generation) state.generation = tick.generation;
  invalidate("header", "hud", "models", "charts", "insights");
  if (state.flights.list.length) invalidate("flights", "detail");
}

function onControl(control) {
  if (control.event === "reloaded") {
    state.generation = control.generation;
    toast(`configuration reloaded: ${control.routes} route(s); per-model counters restart`, "good");
    invalidate("header");
  } else if (control.event === "stopping") {
    if (!state.stopped) onStreamError();
  }
}

setInterval(() => {
  if (state.conn === "live" && performance.now() - state.lastFrame > STALLED_MS) setConn("stalled");
  if (state.view === "dashboard") invalidate("header");
}, 1000);

// ------------------------------------------------------------ notifications

let lastNotified = 0;
let notifyTimer = 0;
let pendingTrouble = null;

function noticeTrouble(event) {
  if (!document.hidden) return;
  state.unread++;
  invalidate("title");
  renderTitle();
  if (!state.notify || !("Notification" in window) || Notification.permission !== "granted") return;
  pendingTrouble = event;
  const wait = lastNotified + NOTIFY_EVERY_MS - Date.now();
  if (wait <= 0) sendNotification();
  else if (!notifyTimer) notifyTimer = setTimeout(sendNotification, wait);
}

function sendNotification() {
  notifyTimer = 0;
  const event = pendingTrouble;
  if (!event || !document.hidden) return;
  lastNotified = Date.now();
  pendingTrouble = null;
  const note = new Notification(`portway: ${state.unread} trouble event(s)`, {
    body: lineText(eventLine(event, new Set(ALL_COLUMNS))),
    tag: "portway-trouble",
  });
  note.onclick = () => {
    window.focus();
    go("dashboard");
    ctx.openDetail({ type: "event", seq: event.seq });
    note.close();
  };
}

function renderTitle() {
  const favicon = $("#favicon");
  if (state.unread > 0 && document.hidden) {
    document.title = `(${state.unread}) portway`;
    favicon.href = "/favicon-alert.svg";
  } else {
    document.title = "portway";
    favicon.href = "/favicon.svg";
  }
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    state.unread = 0;
    renderTitle();
    renderAll();
    flush();
  }
});

// ------------------------------------------------------------------ actions

async function reloadConfig() {
  try {
    const answer = await api.reload();
    toast(answer.message, "good");
  } catch (err) {
    toast(err.message, "bad");
    if (err.status === 401) expired();
  }
}

async function stopForwarder() {
  const stopsConsole = state.header.mode !== "attached";
  // Set intent before awaiting: the stopping frame can arrive before the reply.
  if (stopsConsole) {
    state.stopped = true;
    connection.stop();
    source?.close();
    setConn("stopped");
  }
  try {
    const answer = await api.stop();
    toast(answer.message, "good");
    if (stopsConsole) banner("The forwarder is stopped. Reload this page after starting it again.");
  } catch (err) {
    toast(err.message, "bad");
    if (err.status === 401) expired();
    else if (stopsConsole) {
      state.stopped = false;
      resync();
    }
  }
}

async function loadOlder() {
  if (state.noOlder) return;
  try {
    const answer = await api.events(state.events.oldest || undefined, 1000);
    const added = state.events.prepend(answer.events);
    state.noOlder = added === 0 || answer.events.length === 0 || (answer.events[0]?.seq ?? 0) <= answer.oldest;
    $("#load-older").disabled = state.noOlder;
    toast(added ? `${added} older event(s)` : "nothing older is held", added ? "good" : "");
    invalidate("events");
  } catch (err) {
    toast(err.message, "bad");
  }
}

function exportEvents(kind) {
  const events = [...state.events.matching()];
  const stamp = new Date().toISOString().replaceAll(":", "-").slice(0, 19);
  if (kind === "csv") download(`portway-events-${stamp}.csv`, toCsv(events), "text/csv");
  else download(`portway-events-${stamp}.json`, toJson(events), "application/json");
  toast(`${events.length} event(s) exported`, "good");
}

// ------------------------------------------------------------------ routing

const VIEWS = ["dashboard", "usage", "history", "insights", "appearance"];

function go(view) {
  const hash = view === "dashboard" ? "#/" : `#/${view}`;
  if (location.hash !== hash) history.replaceState(null, "", hash);
  route();
}

function route() {
  const name = location.hash.replace(/^#\/?/, "") || "dashboard";
  const view = VIEWS.includes(name) ? name : "dashboard";
  if (state.view !== view) views[state.view]?.hide();
  state.view = view;
  for (const other of VIEWS) $(`#view-${other}`).hidden = other !== view;
  for (const link of document.querySelectorAll(".tabs a")) {
    if (link.dataset.view === view) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  views[view]?.show();
  renderAll();
}

// A new launch link pasted into this tab changes only the hash: sign in again.
window.addEventListener("hashchange", () => {
  if (/(?:^#|&)token=/.test(location.hash)) boot();
  else route();
});

// ----------------------------------------------------------------- keyboard

function paletteCommands() {
  const commands = [
    ...VIEWS.map((view) => ({ name: `Go to ${view}`, hint: "view", run: () => go(view) })),
    { name: "Filter: all", hint: "e / m", run: () => ctx.setMode("all") },
    { name: "Filter: trouble", hint: "e", run: () => ctx.setMode("trouble") },
    ...inUse(state.models).map((model) => ({ name: `Filter: ${displayModel(model.name)}`, hint: model.name, run: () => ctx.setMode(model.name) })),
    { name: "Search events", hint: "/", run: () => focusSearch() },
    { name: "Clear the search", hint: "search", run: () => ctx.setQuery("") },
    ...COLUMNS.map((column) => ({
      name: `${state.columns.has(column.name) ? "Hide" : "Show"} column: ${column.name}`,
      hint: column.note,
      run: () => ctx.toggleColumn(column.name, !state.columns.has(column.name)),
    })),
    ...SCALES.map((scale) => ({ name: `Traffic buckets: ${scale}s`, hint: "t", run: () => ctx.setScale(scale) })),
    ...[SYSTEM, ...THEMES].map((entry) => ({ name: `Theme: ${entry.name}`, hint: entry.scheme, run: () => ctx.setTheme(entry.id) })),
    { name: "Density: comfortable", hint: "appearance", run: () => ctx.setLook("density", "comfortable") },
    { name: "Density: compact", hint: "appearance", run: () => ctx.setLook("density", "compact") },
    { name: "Type: monospace", hint: "appearance", run: () => ctx.setLook("font", "mono") },
    { name: "Type: sans", hint: "appearance", run: () => ctx.setLook("font", "sans") },
    { name: "Export filtered events as CSV", hint: "file", run: () => exportEvents("csv") },
    { name: "Export filtered events as JSON", hint: "file", run: () => exportEvents("json") },
    { name: "Load older events", hint: "list", run: () => loadOlder() },
    { name: "Follow the newest line", hint: "G", run: () => list.follow() },
    { name: "Jump to the oldest line", hint: "g", run: () => list.oldest() },
    { name: state.flightsOpen ? "Collapse the in-flight list" : "Expand the in-flight list", hint: "flights", run: () => toggleFlights() },
    { name: `${state.notify ? "Turn off" : "Turn on"} desktop notifications`, hint: "notify", run: () => ctx.setNotify(!state.notify) },
    { name: `${state.shortcuts ? "Turn off" : "Turn on"} single-key shortcuts`, hint: "keys", run: () => ctx.setShortcuts(!state.shortcuts) },
    { name: "Choose columns", hint: "c", run: () => openColumns(ctx) },
    {
      name: "Reset column widths",
      hint: "events",
      run: () => {
        const count = list.resetWidths();
        toast(count ? `${count} column width(s) fit to their content again` : "no width was set by hand", "good");
      },
    },
    { name: "Keys", hint: "?", run: () => openHelp() },
  ];
  if (state.header.control?.reload) commands.push({ name: "Reload the configuration", hint: "r", run: () => reloadConfig() });
  if (state.header.control?.stop) commands.push({ name: state.header.mode === "attached" ? "Stop the daemon" : "Stop the forwarder", hint: "q", run: () => openStop(ctx) });
  return commands;
}

function focusSearch() {
  go("dashboard");
  const input = $("#query");
  input.focus();
  input.select();
}

function toggleFlights() {
  state.flightsOpen = !state.flightsOpen;
  prefs.setFlag("flights", state.flightsOpen);
  invalidate("flights");
}

function anyDialogOpen() {
  return [...document.querySelectorAll("dialog")].some((dialog) => dialog.open);
}

document.addEventListener("keydown", (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    openPalette(paletteCommands());
    return;
  }
  const target = event.target;
  const typing = target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target instanceof HTMLSelectElement;
  if (typing) {
    if (event.key === "Escape" && target.id === "query") {
      target.blur();
      list.list.focus();
    }
    return;
  }
  if (anyDialogOpen() || event.metaKey || event.ctrlKey || event.altKey || !state.shortcuts) return;
  // Enter and Space belong to whatever control has focus.
  const control = target instanceof HTMLButtonElement || target instanceof HTMLAnchorElement ||
    ["button", "radio"].includes(target.getAttribute?.("role"));
  if (control && (event.key === "Enter" || event.key === " ")) return;
  if (state.view === "usage") {
    const usage = views.usage;
    const keys = {
      ArrowLeft: () => usage.step(-1), h: () => usage.step(-1),
      ArrowRight: () => usage.step(1), l: () => usage.step(1),
      p: () => usage.costs(), u: () => go("dashboard"), Escape: () => go("dashboard"), q: () => go("dashboard"),
    };
    if (keys[event.key]) {
      event.preventDefault();
      keys[event.key]();
    }
    return;
  }
  const dashboard = state.view === "dashboard";
  const global = {
    u: () => go("usage"),
    H: () => go("history"),
    I: () => go("insights"),
    A: () => go("appearance"),
    "?": () => openHelp(),
    "/": () => focusSearch(),
    r: () => state.header.control?.reload && reloadConfig(),
    q: () => state.header.control?.stop && openStop(ctx),
    Escape: () => (state.view !== "dashboard" ? go("dashboard") : list.follow()),
  };
  const local = dashboard ? {
    j: () => list.scroll(1), ArrowDown: () => list.scroll(1),
    k: () => list.scroll(-1), ArrowUp: () => list.scroll(-1),
    PageDown: () => list.page(1), " ": () => list.page(1), PageUp: () => list.page(-1),
    g: () => list.oldest(), Home: () => list.oldest(),
    G: () => list.follow(), End: () => list.follow(),
    Enter: () => {
      const seq = state.follow ? null : state.cursor;
      const event = seq != null ? state.events.get(seq) : null;
      if (event?.kind === "request") ctx.openDetail({ type: "event", seq });
    },
    e: () => ctx.setMode(state.filterMode === "trouble" ? "all" : "trouble"),
    m: () => ctx.setMode(nextMode(state.filterMode, inUse(state.models).map((model) => model.name))),
    t: () => ctx.setScale(SCALES[(SCALES.indexOf(state.scale) + 1) % SCALES.length]),
    c: () => openColumns(ctx),
  } : {};
  const action = local[event.key] ?? global[event.key];
  if (!action) return;
  // Space and the arrows would also scroll the page under the list.
  event.preventDefault();
  action();
});

// ---------------------------------------------------------------- controls

$("#copy-listen").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(state.header.listen);
    toast("copied the forwarder address", "good");
  } catch {
    toast("the clipboard refused", "bad");
  }
});
$("#reload").addEventListener("click", () => reloadConfig());
$("#stop").addEventListener("click", () => openStop(ctx));
$("#open-palette").addEventListener("click", () => openPalette(paletteCommands()));
$("#open-help").addEventListener("click", () => openHelp());
$("#columns-button").addEventListener("click", () => openColumns(ctx));
$("#export-csv").addEventListener("click", () => exportEvents("csv"));
$("#export-json").addEventListener("click", () => exportEvents("json"));
$("#load-older").addEventListener("click", () => loadOlder());
$("#flights-toggle").addEventListener("click", () => toggleFlights());
let queryTimer = 0;
$("#query").addEventListener("input", (event) => {
  clearTimeout(queryTimer);
  queryTimer = setTimeout(() => {
    state.query = event.target.value.trim();
    applyFilter();
  }, 120);
});
$("#detail").addEventListener("close", () => {
  state.detail = null;
});
window.addEventListener("resize", () => invalidate("charts", "events", "insights"));

// -------------------------------------------------------------------- boot

const connection = new Connection({
  async connect() {
    if (!await signIn()) throw new ApiError(401, "Sign in required");
    return api.snapshot();
  },
  connected(snapshot) {
    $("#gate").hidden = true;
    applySnapshot(snapshot);
    route();
    openStream();
  },
  retrying() {
    source?.close();
    setConn("reconnecting");
    state.flights.load({ at_unix: Date.now() / 1000, total: 0, more: 0, list: [] });
    invalidate("flights", "detail");
    banner("Connection interrupted. Reconnecting automatically; displayed data may be out of date.");
  },
  expired,
});

function boot() {
  $("#gate").hidden = true;
  state.stopped = false;
  setConn("connecting");
  resync();
}

// Refresh the persistent cookie even when this tab only consumes an SSE stream.
setInterval(async () => {
  if (state.stopped || !connection.active) return;
  try {
    if (!(await api.health()).session) expired();
  } catch {
    onStreamError();
  }
}, 24 * 60 * 60 * 1000);

// The store starts with the empty query's filter, which already leaves a
// catalog fetch out, rather than accepting everything until a key is pressed.
applyFilter();
boot();
