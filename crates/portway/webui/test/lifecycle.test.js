import test from "node:test";
import assert from "node:assert/strict";
import { ViewLifecycle } from "../static/js/views/lifecycle.js";
import { REFRESH_MS, UsageView } from "../static/js/views/usage.js";

const settled = () => new Promise((resolve) => setImmediate(resolve));

function harness(t, load = async (range) => ({ range, rows: [] })) {
  const originalDocument = globalThis.document;
  const originalSet = globalThis.setInterval;
  const originalClear = globalThis.clearInterval;
  const gate = { hidden: true };
  const timers = new Map();
  const calls = [];
  let nextTimer = 0;
  globalThis.document = {
    hidden: false,
    querySelector: (selector) => selector === "#gate" ? gate : {},
  };
  globalThis.setInterval = (callback, delay) => {
    timers.set(++nextTimer, { callback, delay });
    return nextTimer;
  };
  globalThis.clearInterval = (id) => timers.delete(id);

  const lifecycle = new ViewLifecycle();
  const usage = new UsageView({
    api: { usage: (range) => { calls.push(range); return load(range); } },
    expired: () => {
      lifecycle.hide();
      gate.hidden = false;
    },
  });
  usage.render = () => {};
  t.after(() => {
    lifecycle.hide();
    globalThis.document = originalDocument;
    globalThis.setInterval = originalSet;
    globalThis.clearInterval = originalClear;
  });
  return {
    lifecycle, usage, timers, calls, gate,
    async tick() {
      assert.equal(timers.size, 1, "only one usage refresh timer is active");
      const timer = timers.values().next().value;
      assert.equal(timer.delay, REFRESH_MS);
      timer.callback();
      await settled();
    },
  };
}

test("rerouting to active usage preserves its polling schedule", async (t) => {
  const h = harness(t);
  h.lifecycle.show(h.usage);
  await settled();
  const timer = h.usage.timer;
  h.lifecycle.show(h.usage);
  h.lifecycle.show(h.usage);
  await settled();
  assert.deepEqual(h.calls, ["today"]);
  assert.equal(h.usage.timer, timer);
  await h.tick();
  assert.deepEqual(h.calls, ["today", "today"]);
});

test("usage resumes polling on the same route after signing in again", async (t) => {
  let authorized = true;
  const h = harness(t, async (range) => {
    if (!authorized) throw Object.assign(new Error("session expired"), { status: 401 });
    return { range, rows: [] };
  });
  h.lifecycle.show(h.usage);
  await settled();

  authorized = false;
  await h.tick();
  assert.equal(h.gate.hidden, false);
  assert.equal(h.timers.size, 0);
  assert.equal(h.usage.timer, 0);

  authorized = true;
  h.gate.hidden = true;
  h.lifecycle.show(h.usage);
  await settled();
  assert.deepEqual(h.calls, ["today", "today", "today"]);
  assert.equal(h.usage.error, null);
  assert.equal(h.usage.table.range, "today");
  await h.tick();
  assert.equal(h.calls.length, 4);
});

test("leaving usage stops polling and returning refreshes it", async (t) => {
  const h = harness(t);
  // Dashboard has no view object; the other tabs do.
  for (const next of [{ show() {}, hide() {} }, undefined]) {
    h.lifecycle.show(h.usage);
    await settled();
    assert.equal(h.timers.size, 1);
    h.lifecycle.show(next);
    assert.equal(h.timers.size, 0);
    assert.equal(h.usage.timer, 0);
  }
  assert.deepEqual(h.calls, ["today", "today"]);
});
