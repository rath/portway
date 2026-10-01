import { test } from "node:test";
import assert from "node:assert/strict";
import { Connection } from "../static/js/connection.js";

const settled = () => new Promise((resolve) => setImmediate(resolve));

function harness(connect) {
  const pending = new Map();
  const states = [];
  let id = 0;
  const connection = new Connection({
    connect,
    connected: (snapshot) => states.push(snapshot),
    retrying: () => states.push("reconnecting"),
    expired: () => states.push("sign in"),
    setTimer: (callback, delay) => { pending.set(++id, { callback, delay }); return id; },
    clearTimer: (id) => pending.delete(id),
  });
  return {
    connection, pending, states,
    async advance(delay) {
      assert.equal(pending.size, 1, "only one retry can be scheduled");
      const [id, timer] = pending.entries().next().value;
      assert.equal(timer.delay, delay);
      pending.delete(id);
      timer.callback();
      await settled();
    },
  };
}

test("a redeploy reconnects with a fresh snapshot, using one bounded retry loop", async () => {
  let online = true;
  let epoch = 1;
  const h = harness(async () => {
    if (!online) throw new Error("unavailable");
    return { epoch, events: [epoch] };
  });
  h.connection.start();
  await settled();
  assert.deepEqual(h.states[0], { epoch: 1, events: [1] });
  online = false;
  h.connection.retry(); // server stopping
  h.connection.retry(); // stream closed too
  for (const delay of [2000, 4000, 8000, 16000, 30000, 30000]) await h.advance(delay);
  online = true;
  epoch = 2;
  await h.advance(30000);
  assert.deepEqual(h.states.at(-1), { epoch: 2, events: [2] });
  assert.equal(h.pending.size, 0);
  h.connection.retry();
  await h.advance(2000); // a successful recovery resets the backoff
});

test("opening the page during an outage recovers without reloading it", async () => {
  let online = false;
  const h = harness(async () => {
    if (!online) throw new Error("offline");
    return "live";
  });
  h.connection.start();
  await settled();
  online = true;
  await h.advance(2000);
  assert.deepEqual(h.states, ["reconnecting", "live"]);
});

test("reset credentials stop retries and request sign-in", async () => {
  const h = harness(async () => { throw Object.assign(new Error("reset"), { status: 401 }); });
  h.connection.start();
  await settled();
  h.connection.retry();
  assert.deepEqual(h.states, ["sign in"]);
  assert.equal(h.pending.size, 0);
  assert.equal(h.connection.active, false);
});

test("explicit Stop cancels recovery and ignores an in-flight snapshot", async () => {
  let reply;
  const h = harness(() => new Promise((resolve) => { reply = resolve; }));
  h.connection.start();
  h.connection.stop();
  reply("old snapshot");
  await settled();
  h.connection.retry();
  assert.deepEqual(h.states, []);
  assert.equal(h.pending.size, 0);
  h.connection.start();
  reply("live");
  await settled();
  h.connection.retry();
  assert.equal(h.pending.size, 1);
  h.connection.stop();
  assert.equal(h.pending.size, 0);
});

test("a replacement connection ignores failure from an older request", async () => {
  let rejectOld;
  let calls = 0;
  const h = harness(() => ++calls === 1
    ? new Promise((_, reject) => { rejectOld = reject; })
    : Promise.resolve("new snapshot"));
  h.connection.start();
  h.connection.start();
  await settled();
  rejectOld({ status: 401 });
  await settled();
  assert.deepEqual(h.states, ["new snapshot"]);
  assert.equal(h.connection.active, true);
});

test("native browser timers are called without a Connection receiver", async () => {
  const originalSet = globalThis.setTimeout;
  const originalClear = globalThis.clearTimeout;
  let scheduled = false;
  let cancelled = false;
  globalThis.setTimeout = function (callback, delay) {
    assert.ok(!(this instanceof Connection), "Window.setTimeout requires its native receiver");
    assert.equal(typeof callback, "function");
    assert.equal(delay, 2000);
    scheduled = true;
    return 17;
  };
  globalThis.clearTimeout = function (timer) {
    assert.ok(!(this instanceof Connection), "Window.clearTimeout requires its native receiver");
    assert.equal(timer, 17);
    cancelled = true;
  };
  try {
    const connection = new Connection({
      connect: async () => "live", connected() {}, retrying() {}, expired() {},
    });
    connection.start();
    await settled();
    connection.retry();
    connection.stop();
    assert.ok(scheduled && cancelled);
  } finally {
    globalThis.setTimeout = originalSet;
    globalThis.clearTimeout = originalClear;
  }
});
