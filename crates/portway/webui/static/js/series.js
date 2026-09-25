// The two charts' data: per-turn body bars and per-second socket bytes,
// kept the way the server sends them and read back the way the TUI draws.

export const BARS = 1024;
export const TRAFFIC_SECONDS = 3600;
export const SCALES = [1, 10, 60];

/** `(raw, wire)` of every request that carried a body, newest last. */
export class Bars {
  constructor() {
    this.items = [];
    this.total = 0;
  }

  load(bars, total) {
    this.items = bars.slice(-BARS);
    this.total = total;
  }

  /** A tick's `bars_push`: only what this page has not counted yet. */
  push(bars, total) {
    const fresh = Math.min(Math.max(0, total - this.total), bars.length);
    if (fresh > 0) this.items.push(...bars.slice(bars.length - fresh));
    if (this.items.length > BARS) this.items.splice(0, this.items.length - BARS);
    this.total = total;
  }

  /** The last `count` bars. */
  last(count) {
    return this.items.slice(Math.max(0, this.items.length - count));
  }
}

/** Socket bytes in one-second buckets, keyed by the server's second index. */
export class Traffic {
  constructor() {
    this.up = [];
    this.down = [];
    this.end = 0;
  }

  load({ end, up, down }) {
    this.up = up.slice(-TRAFFIC_SECONDS);
    this.down = down.slice(-TRAFFIC_SECONDS);
    this.end = end;
  }

  /**
   * A tick's tail: overwrite the seconds already held (the newest was still
   * filling), append new ones, zero-fill any second the page missed.
   */
  merge({ end, up, down }) {
    const start = end - up.length + 1;
    if (this.up.length === 0 || end < this.end || start > this.end + TRAFFIC_SECONDS) {
      // Nothing held yet, or a clock this page cannot line up with.
      this.load({ end, up, down });
      return;
    }
    while (this.end + 1 < start) {
      this.up.push(0);
      this.down.push(0);
      this.end++;
    }
    for (let at = 0; at < up.length; at++) {
      const second = start + at;
      if (second <= this.end) {
        const offset = this.up.length - 1 - (this.end - second);
        if (offset >= 0) {
          this.up[offset] = up[at];
          this.down[offset] = down[at];
        }
      } else {
        this.up.push(up[at]);
        this.down.push(down[at]);
        this.end = second;
      }
    }
    const excess = this.up.length - TRAFFIC_SECONDS;
    if (excess > 0) {
      this.up.splice(0, excess);
      this.down.splice(0, excess);
    }
  }

  /** The last `width` columns of `scale`-second buckets, as a rate. */
  series(scale, width) {
    return { up: rate(this.up, scale, width), down: rate(this.down, scale, width) };
  }
}

/**
 * `tui::state::rate`, exactly: right-aligned, so a short history leaves the
 * left columns empty; each column is the integer mean of its bucket.
 */
export function rate(buckets, scale, width) {
  if (width === 0 || scale === 0) return [];
  const wanted = width * scale;
  const tail = buckets.slice(Math.max(0, buckets.length - wanted));
  const columns = Math.ceil(tail.length / scale);
  const out = new Array(Math.max(0, width - columns)).fill(0);
  for (let at = 0; at < tail.length; at += scale) {
    const chunk = tail.slice(at, at + scale);
    out.push(Math.floor(chunk.reduce((sum, value) => sum + value, 0) / scale));
  }
  return out;
}

/** A rolling series of numbers sampled once per tick, for the insight charts. */
export class History {
  constructor(capacity = 900) {
    this.capacity = capacity;
    this.points = [];
  }

  add(at, values) {
    this.points.push({ at, ...values });
    if (this.points.length > this.capacity) this.points.shift();
  }

  clear() {
    this.points = [];
  }
}
