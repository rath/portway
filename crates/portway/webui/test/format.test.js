import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  dollars, fixedEven, human, humanCount, humanTime, label, percent, ratio, share, span, uptime,
} from "../static/js/format.js";

const fixture = JSON.parse(readFileSync(new URL("./fixtures/format.json", import.meta.url), "utf8"));

const cases = {
  human: (input) => human(input),
  human_time: (input) => humanTime(input),
  human_count: (input) => humanCount(input),
  span: (input) => span(input),
  uptime: (input) => uptime(input),
  ratio: ([raw, wire]) => ratio(raw, wire),
  share: ([part, whole]) => share(part, whole),
  dollars: (input) => dollars(input),
  label: ([model, tier]) => label(model, tier),
};

for (const [name, format] of Object.entries(cases)) {
  test(`${name} prints what the Rust side prints`, () => {
    assert.ok(fixture[name].length > 3, `fixture has ${name}`);
    for (const [input, expected] of fixture[name]) {
      assert.equal(format(input), expected, `${name}(${JSON.stringify(input)})`);
    }
  });
}

test("ties go to even, everything else to the nearest", () => {
  assert.equal(fixedEven(2.5, 0), "2");
  assert.equal(fixedEven(3.5, 0), "4");
  assert.equal(fixedEven(1.125, 2), "1.12");
  assert.equal(fixedEven(1.375, 2), "1.38");
  assert.equal(fixedEven(0.35, 1), "0.3");
  assert.equal(fixedEven(0.45, 1), "0.5");
  assert.equal(fixedEven(9.5, 0), "10");
  assert.equal(fixedEven(0, 2), "0.00");
});

test("a hit rate keeps one decimal", () => {
  assert.equal(percent(0.9734), "97.3%");
  assert.equal(percent(null), "n/a");
});
