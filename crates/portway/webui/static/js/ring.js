// The events the page holds, oldest first, capped like the TUI's pane, with
// the subset the current filter admits kept alongside so that scrolling and
// counting never rescan the whole backlog.

export const CAPACITY = 10000;

export class EventStore {
  constructor(capacity = CAPACITY) {
    this.capacity = capacity;
    /** Events by position; `seq` only grows. */
    this.items = [];
    this.bySeq = new Map();
    /** Seqs the filter admits, ascending. */
    this.filtered = [];
    this.accepts = () => true;
    /** Bumped on every change, so views redraw only when needed. */
    this.version = 0;
    /** Bumped when everything is replaced, so measurements start over. */
    this.epoch = 0;
  }

  get newest() {
    return this.items.length ? this.items[this.items.length - 1].seq : 0;
  }

  get oldest() {
    return this.items.length ? this.items[0].seq : 0;
  }

  get size() {
    return this.items.length;
  }

  get(seq) {
    return this.bySeq.get(seq);
  }

  /** Append one event; one already held (a resumed stream) is ignored. */
  push(event) {
    if (event.seq <= this.newest) return false;
    this.items.push(event);
    this.bySeq.set(event.seq, event);
    if (this.accepts(event)) this.filtered.push(event.seq);
    this.evict();
    this.version++;
    return true;
  }

  /** Older events, from `/api/events`, in front of what is held. */
  prepend(events) {
    const older = events.filter((event) => event.seq < this.oldest || this.items.length === 0);
    if (!older.length) return 0;
    const room = Math.max(0, this.capacity - this.items.length);
    const kept = older.slice(Math.max(0, older.length - room));
    this.items = kept.concat(this.items);
    for (const event of kept) this.bySeq.set(event.seq, event);
    this.filtered = kept.filter((event) => this.accepts(event)).map((event) => event.seq).concat(this.filtered);
    this.version++;
    return kept.length;
  }

  /** Replace everything: a snapshot after a reset. */
  reset(events) {
    this.epoch++;
    this.items = [];
    this.bySeq.clear();
    this.filtered = [];
    for (const event of events) {
      this.items.push(event);
      this.bySeq.set(event.seq, event);
      if (this.accepts(event)) this.filtered.push(event.seq);
    }
    this.evict();
    this.version++;
  }

  setFilter(accepts) {
    this.accepts = accepts;
    this.filtered = this.items.filter(accepts).map((event) => event.seq);
    this.version++;
  }

  evict() {
    const excess = this.items.length - this.capacity;
    if (excess <= 0) return;
    const gone = this.items.splice(0, excess);
    for (const event of gone) this.bySeq.delete(event.seq);
    const last = gone[gone.length - 1].seq;
    let drop = 0;
    while (drop < this.filtered.length && this.filtered[drop] <= last) drop++;
    if (drop) this.filtered.splice(0, drop);
  }

  /** Where `seq` sits in the filtered list, clamped (tui::State::position). */
  position(seq) {
    let low = 0;
    let high = this.filtered.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (this.filtered[mid] < seq) low = mid + 1;
      else high = mid;
    }
    return Math.min(low, Math.max(0, this.filtered.length - 1));
  }

  /** The filtered events, oldest first. */
  *matching() {
    for (const seq of this.filtered) yield this.bySeq.get(seq);
  }
}
