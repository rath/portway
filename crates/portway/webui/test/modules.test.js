import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import { ALL_COLUMNS, detailFields, eventLine, lineText, parseColumns } from "../static/js/eventline.js";
import { compile, modeAccepts, nextMode, tokenize } from "../static/js/filter.js";
import { EventStore } from "../static/js/ring.js";
import { Bars, Traffic, rate } from "../static/js/series.js";
import { Flights, describe } from "../static/js/flights.js";
import { cell, toCsv } from "../static/js/export.js";
import { MONOCHROME, THEMES, TOKENS, contrast, css, deltaE, resolve } from "../static/js/themes.js";

const request = (overrides = {}) => ({
  seq: 1, kind: "request", ts: 1727000000, stamp: "12:34:56",
  model: "model-alpha", method: "POST", path: "/v1/chat/completions",
  route: "POST ../completions", route_known: true, status: 200,
  dns: null, tcp: null, tls: null, reused: true, handshake: null,
  body_len: 471859, wire_len: 113246, coding: "zstd", upload: 0.012, ttfb: 0.84,
  received: 2048, received_wire: 900, received_agent: 2048,
  upstream_encoding: "gzip", agent_encoding: null, download: 1.5, complete: true,
  usage: { prompt: 91234, cached: 91100, completion: 891, reasoning: null },
  trouble: false, flight: 7,
  ...overrides,
});

const log = (overrides = {}) => ({
  seq: 2, kind: "log", ts: 1727000001, stamp: "12:34:57", level: "WARNING",
  message: "origin 415: retrying identity once", trouble: true, ...overrides,
});

const every = new Set(ALL_COLUMNS);

test("a request line reads like the terminal's", () => {
  assert.equal(
    lineText(eventLine(request(), every)),
    "12:34:56 200 model-alpha POST ../completions 461KB→111KB -76% 12ms ttfb 840ms down 2KB 1.50s tok 91.2K(91.1K cached)→891",
  );
});

test("marks take a separator's place, and off fields close their gap", () => {
  const cut = request({ complete: false, handshake: 0.03, dns: 0.01, tcp: 0.01, tls: 0.01, reused: false });
  assert.equal(
    lineText(eventLine(cut, new Set(["time", "status", "cut", "model", "route"]))),
    "12:34:56 200✂model-alpha POST ../completions.",
  );
  assert.equal(lineText(eventLine(request(), new Set(["status", "ttfb"]))), "200 ttfb 840ms");
  const bare = request({ body_len: 0, wire_len: 0, coding: null, usage: null });
  assert.equal(lineText(eventLine(bare, new Set(["sizes", "tokens", "status"]))), "200");
});

test("a re-encoded download shows what the agent got", () => {
  const line = lineText(eventLine(request({ received_agent: 512, download: null }), new Set(["down"])));
  assert.equal(line, "down 2KB→0.5KB -75%");
});

test("a log line names the level from WARNING up", () => {
  assert.equal(lineText(eventLine(log(), every)), "12:34:57 WARNING origin 415: retrying identity once");
  assert.equal(lineText(eventLine(log({ level: "INFO", message: "hi" }), every)), "12:34:57 hi");
});

test("the detail popup's fields are the terminal's", () => {
  const fields = Object.fromEntries(detailFields(request()));
  assert.equal(fields.connection, "reused from the pool");
  assert.equal(fields.upload, "461KB -> 111KB (zstd, -76%)");
  assert.equal(fields.tokens, "91234 in (91100 cached) -> 891 out");
  assert.equal(fields.ended, "upstream body finished");
});

test("column lists are validated", () => {
  assert.deepEqual([...parseColumns(" time , route ")], ["time", "route"]);
  assert.throws(() => parseColumns("time,uri"), /uri/);
});

test("search terms, phrases and negation", () => {
  assert.deepEqual(tokenize('a -b "c d"').map((t) => [t.negated, t.text, t.phrase]),
    [[false, "a", false], [true, "b", false], [false, "c d", true]]);
  const events = [request(), request({ seq: 3, model: "beta", status: 502, trouble: true }), log()];
  const run = (query) => events.filter(compile(query).test).map((e) => e.seq);
  assert.deepEqual(run(""), [1, 3, 2]);
  assert.deepEqual(run("status:5xx"), [3]);
  assert.deepEqual(run("status:>=400"), [3]);
  assert.deepEqual(run("model:alpha"), [1]);
  assert.deepEqual(run("-model:alpha is:request"), [3]);
  assert.deepEqual(run("is:trouble"), [3, 2]);
  assert.deepEqual(run("level:warning"), [2]);
  assert.deepEqual(run('"retrying identity"'), [2]);
  assert.deepEqual(run("ttfb:>500ms size:>400KB"), [1, 3]);
  assert.deepEqual(run("tok:>90K"), [1, 3]);
  assert.deepEqual(run("completions"), [1, 3]);
  assert.deepEqual(compile("status:abc").errors.length, 1);
  assert.deepEqual(run('route:"POST /v1/chat"'), [1, 3]);
  assert.deepEqual(tokenize('route:"a b" http://x').map((t) => t.text), ["route:a b", "http://x"]);
});

test("the cycle filter walks all, trouble, then each model", () => {
  assert.equal(nextMode("all", ["a", "b"]), "trouble");
  assert.equal(nextMode("trouble", ["a", "b"]), "a");
  assert.equal(nextMode("a", ["a", "b"]), "b");
  assert.equal(nextMode("b", ["a", "b"]), "all");
  assert.equal(nextMode("trouble", []), "all");
  assert.ok(modeAccepts("model-alpha", request()));
  assert.ok(!modeAccepts("model-alpha", log()));
});

test("the store evicts, filters and resumes without duplicates", () => {
  const store = new EventStore(3);
  for (let seq = 1; seq <= 5; seq++) store.push(request({ seq, trouble: seq % 2 === 0 }));
  assert.equal(store.push(request({ seq: 4 })), false);
  assert.deepEqual(store.items.map((e) => e.seq), [3, 4, 5]);
  store.setFilter((e) => e.trouble);
  assert.deepEqual(store.filtered, [4]);
  store.push(request({ seq: 6, trouble: true }));
  assert.deepEqual(store.filtered, [4, 6]);
  store.push(request({ seq: 7 }));
  store.push(request({ seq: 8 }));
  assert.deepEqual(store.filtered, [6]);
  assert.equal(store.position(7), 0);
  const big = new EventStore(10);
  big.reset([request({ seq: 5 }), request({ seq: 6 })]);
  assert.equal(big.prepend([request({ seq: 3 }), request({ seq: 4 }), request({ seq: 6 })]), 2);
  assert.deepEqual(big.items.map((e) => e.seq), [3, 4, 5, 6]);
});

test("bars append only what this page has not counted", () => {
  const bars = new Bars();
  bars.load([[10, 5], [20, 5]], 2);
  bars.push([[20, 5], [30, 5]], 3);
  assert.deepEqual(bars.items, [[10, 5], [20, 5], [30, 5]]);
  bars.push([], 3);
  assert.equal(bars.items.length, 3);
});

test("traffic tails overwrite, append and zero-fill by second", () => {
  const traffic = new Traffic();
  traffic.load({ end: 10, up: [1, 2, 3], down: [0, 0, 0] });
  traffic.merge({ end: 12, up: [30, 4, 5], down: [0, 0, 0] });
  assert.deepEqual(traffic.up, [1, 2, 30, 4, 5]);
  traffic.merge({ end: 15, up: [9], down: [1] });
  assert.deepEqual(traffic.up, [1, 2, 30, 4, 5, 0, 0, 9]);
  assert.equal(traffic.end, 15);
});

test("rate is the terminal's: right-aligned integer means", () => {
  assert.deepEqual(rate([10, 20, 30], 1, 3), [10, 20, 30]);
  assert.deepEqual(rate([40], 1, 3), [0, 0, 40]);
  assert.deepEqual(rate([1, 2, 3, 4, 5], 2, 4), [0, 1, 3, 2]);
  assert.deepEqual(rate([1, 2], 0, 4), []);
});

test("flights age locally and leave when their record arrives", () => {
  const flights = new Flights();
  flights.load({ at_unix: 100, total: 2, more: 0, list: [
    { id: 1, phase: "prefill", age_s: 31, idle_s: 1, status: null, ttfb: null, received: 0, retries: 0 },
    { id: 2, phase: "stream", age_s: 5, idle_s: 61, status: 200, ttfb: 0.5, received: 4096, retries: 1 },
  ] });
  const [slow, stuck] = flights.list;
  assert.equal(describe(slow, flights.times(slow, 102)).warn, "slow prefill");
  assert.equal(flights.times(slow, 102).age, 33);
  const stream = describe(stuck, flights.times(stuck, 100));
  assert.equal(stream.text, "stream 200 ttfb 500ms ↓4KB 5.00s · retried 1×");
  assert.equal(stream.warn, "stalled");
  assert.ok(flights.finish(1));
  assert.equal(flights.total, 1);
  flights.load(null);
  assert.equal(flights.available, false);
});

test("CSV quotes, and keeps spreadsheets from running cells", () => {
  assert.equal(cell("a,b"), '"a,b"');
  assert.equal(cell('say "hi"'), '"say ""hi"""');
  assert.equal(cell("=1+1"), "'=1+1");
  assert.equal(cell("-cmd"), "'-cmd");
  assert.equal(cell(-5), "-5");
  const csv = toCsv([request(), log()]);
  assert.ok(csv.startsWith("seq,time,kind,"));
  assert.equal(csv.split("\r\n").length, 4);
});

test("tokens.css holds exactly the palettes themes.js lists", () => {
  const sheet = readFileSync(new URL("../static/css/tokens.css", import.meta.url), "utf8");
  assert.ok(sheet.endsWith(css()), "regenerate tokens.css from themes.js");
  assert.equal(new Set(THEMES.map((t) => t.id)).size, THEMES.length);
  assert.ok(THEMES.length >= 15);
});

test("every palette is readable", () => {
  for (const theme of THEMES) {
    const color = theme.tokens;
    for (const token of TOKENS) assert.match(color[token], /^#[0-9a-f]{6}$/, `${theme.id} ${token}`);
    for (const ground of ["bg", "surface", "surface-2"]) {
      for (const ink of ["text", "text-dim", "good", "warn", "bad", "model", "raw", "wire", "accent"]) {
        const ratio = contrast(color[ink], color[ground]);
        assert.ok(ratio >= 4.5, `${theme.id}: ${ink} on ${ground} is ${ratio.toFixed(2)}:1`);
      }
    }
    for (const ground of ["bg", "surface"]) {
      assert.ok(contrast(color.focus, color[ground]) >= 3, `${theme.id}: focus ring`);
    }
    assert.ok(contrast(color["on-accent"], color.accent) >= 4.5, `${theme.id}: on-accent`);
    if (!MONOCHROME.has(theme.id)) {
      assert.ok(deltaE(color.raw, color.wire) >= 15, `${theme.id}: raw and wire too alike`);
    }
  }
});

test("system follows the OS between the two Portway themes", () => {
  assert.equal(resolve("system", true), "portway-dark");
  assert.equal(resolve("system", false), "portway-light");
  assert.equal(resolve("nord", false), "nord");
  assert.equal(resolve("gone", true), "portway-dark");
});
