// Requests the forwarder has counted and not finished relaying: the part of
// the traffic the terminal dashboard never sees. The server sends the list
// when it changes; ages move on here, from the moment it was read.

import { human, humanTime } from "./format.js";

/** A prefill this long, or a stream this quiet, is worth a second look. */
export const SLOW_PREFILL = 30;
export const STALLED = 60;

export class Flights {
  constructor() {
    this.available = false;
    this.list = [];
    this.total = 0;
    this.more = 0;
    this.at = 0;
    this.version = 0;
  }

  /** A `flights` frame, or the snapshot's (null when the console is attached). */
  load(frame) {
    this.available = frame != null;
    this.list = frame ? frame.list : [];
    this.total = frame ? frame.total : 0;
    this.more = frame ? frame.more : 0;
    this.at = frame ? frame.at_unix : 0;
    this.version++;
  }

  /** The record that ended flight `id` arrived: the row goes at once. */
  finish(id) {
    const at = this.list.findIndex((flight) => flight.id === id);
    if (at < 0) return false;
    this.list.splice(at, 1);
    this.total = Math.max(0, this.total - 1);
    this.version++;
    return true;
  }

  get(id) {
    return this.list.find((flight) => flight.id === id);
  }

  /** Age and idle time as of `now` (unix seconds). */
  times(flight, now) {
    const since = Math.max(0, now - this.at);
    return { age: flight.age_s + since, idle: flight.idle_s + since };
  }

  /** The oldest flight's age, for the strip's title. */
  oldest(now) {
    return this.list.length ? this.times(this.list[0], now).age : 0;
  }
}

/** What the row says, and whether it is worth a warning. */
export function describe(flight, times) {
  const { age, idle } = times;
  let text;
  let warn = null;
  switch (flight.phase) {
    case "upload":
      text = `upload ${humanTime(age)}`;
      break;
    case "prefill":
      text = `prefill ${humanTime(age)}`;
      if (age > SLOW_PREFILL) warn = "slow prefill";
      break;
    default:
      text = `stream ${flight.status} ttfb ${humanTime(flight.ttfb ?? 0)} ↓${human(flight.received)} ${humanTime(age)}`;
      if (idle > STALLED) warn = "stalled";
      break;
  }
  if (flight.retries > 0) text += ` · retried ${flight.retries}×`;
  return { text, warn };
}
