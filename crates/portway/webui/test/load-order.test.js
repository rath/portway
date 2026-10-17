import test from "node:test";
import assert from "node:assert/strict";
import { HistoryView } from "../static/js/views/history.js";
import { UsageView } from "../static/js/views/usage.js";

// These tests exercise async view state without browser layout or timers.
globalThis.document = { querySelector: () => ({}) };

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

test("history keeps the latest selection when older requests finish last", async () => {
  for (const staleError of [false, true]) {
    const pending = [deferred(), deferred()];
    let calls = 0, expired = 0;
    const view = new HistoryView({ api: { report: () => pending[calls++].promise }, expired: () => expired++ });
    view.render = view.renderControls = () => {};
    const old = view.load();
    view.since = "1h";
    const current = view.load();
    pending[1].resolve({ span: "1h" });
    await current;
    if (staleError) pending[0].reject(Object.assign(new Error("old request"), { status: 401 }));
    else pending[0].resolve({ span: "24h" });
    await old;
    assert.equal(view.report.span, "1h");
    assert.equal(view.error, null);
    assert.equal(expired, 0);
  }
});

test("usage loads a changed range while an earlier range is pending", async () => {
  const pending = [deferred(), deferred()];
  const ranges = [];
  const view = new UsageView({ api: { usage: (range) => { ranges.push(range); return pending[ranges.length - 1].promise; } } });
  view.render = () => {};
  const old = view.load();
  view.range = "month";
  const current = view.load();
  await view.load(); // A timer must not duplicate the same pending range.
  assert.deepEqual(ranges, ["today", "month"]);
  pending[1].resolve({ range: "month" });
  await current;
  pending[0].resolve({ range: "today" });
  await old;
  assert.equal(view.table.range, "month");
});
