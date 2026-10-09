// The `u` screen: tokens and cost per model over a local-day window, read
// from the recorder's database and re-read every 5s while it is up.

import { displayModel, displayNotes } from "../modelnames.js";
import { $, fill, h } from "../dom.js";
import { dollars, humanCount, label, percent } from "../format.js";
import { openCosts } from "./dialogs.js";

export const REFRESH_MS = 5000;
const RANGES = ["today", "yesterday", "week", "month"];

export class UsageView {
  constructor(ctx) {
    this.ctx = ctx;
    this.root = $("#view-usage");
    this.range = "today";
    this.table = null;
    this.error = null;
    this.timer = 0;
    this.loading = false;
  }

  show() {
    this.load();
    clearInterval(this.timer);
    this.timer = setInterval(() => {
      if (!document.hidden) this.load();
    }, REFRESH_MS);
  }

  hide() {
    clearInterval(this.timer);
    this.timer = 0;
  }

  step(delta) {
    const at = RANGES.indexOf(this.range);
    this.range = RANGES[(at + delta + RANGES.length) % RANGES.length];
    this.load();
  }

  costs() {
    if (this.table) openCosts(this.table);
  }

  async load() {
    if (this.loading) return;
    this.loading = true;
    try {
      this.table = await this.ctx.api.usage(this.range);
      this.error = null;
    } catch (err) {
      this.error = err.message;
      if (err.status === 401) this.ctx.expired();
    } finally {
      this.loading = false;
    }
    this.render();
  }

  render() {
    const table = this.table;
    const ranges = (table?.ranges ?? RANGES.map((key) => ({ key, label: key }))).map((range) =>
      h("button", {
        type: "button",
        "aria-pressed": range.key === this.range ? "true" : "false",
        onclick: () => {
          this.range = range.key;
          this.load();
        },
        text: range.label,
      }));
    const head = h("div", { class: "controls" },
      h("div", { class: "segmented", role: "group", "aria-label": "Window" }, ranges),
      h("span", { class: "note", text: "←→ window" }),
      h("div", { class: "grow" }),
      h("button", { type: "button", class: "ghost", onclick: () => this.costs(), disabled: !table, text: "Costs (p)" }));
    if (this.error) {
      fill(this.root, h("section", { class: "panel" }, head, h("p", { class: "error-box", text: this.error })));
      return;
    }
    if (!table) {
      fill(this.root, h("section", { class: "panel" }, head, h("p", { class: "note", text: "reading…" })));
      return;
    }
    const names = ["model", "reqs", "prompt", "cached", "hit", "output", "in$", "cache$", "out$", "total$"];
    const roomy = new Set(["in$", "cache$", "out$"]);
    const body = table.rows.length
      ? h("div", { class: "table-wrap models" }, h("table", { class: "data" },
        h("thead", {}, h("tr", {}, names.map((name) => h("th", { class: roomy.has(name) ? "roomy" : "", scope: "col", text: name })))),
        h("tbody", {}, table.rows.map((row) => usageRow(row, false)), usageRow(table.total, true))))
      : h("p", { class: "note", text: "no usage recorded in this window" });
    fill(this.root,
      h("section", { class: "panel" },
        head,
        h("h2", { class: "window-title", text: table.title }),
        body),
      h("section", { class: "panel" },
        h("h2", { class: "panel-head", text: "What these numbers are not" }),
        h("ul", { class: "notes" }, displayNotes(table).map((note) => h("li", { text: note }))),
        h("p", { class: "note", text: `re-read every ${REFRESH_MS / 1000}s while this view is open` })));
  }
}

function usageRow(row, total) {
  const unreported = !total && row.requests > 0 && row.unreported === row.requests;
  const hit = row.hit_rate;
  const part = (name) => (row.charge ? dollars(row.charge[name]) : "-");
  const cells = [
    [label(total ? row.model : displayModel(row.model), row.tier), total ? "good" : "model"],
    [String(row.requests)],
    [humanCount(row.prompt)],
    [unreported ? "-" : humanCount(row.cached), unreported ? "dim" : null],
    [percent(hit), hit == null ? "dim" : hit >= 0.5 ? "good" : "time"],
    [humanCount(row.completion)],
    [part("input"), null, "roomy"],
    [part("cache_read"), null, "roomy"],
    [part("output"), null, "roomy"],
    [row.cost == null ? "unpriced" : dollars(row.cost), "good t-bold"],
  ];
  return h("tr", { class: total ? "total" : "" }, cells.map(([text, tone, extra], at) => h("td", {
    class: [tone ? tone.split(" ").map((name) => (name.startsWith("t-") ? name : `t-${name}`)).join(" ") : "", extra || ""].filter(Boolean).join(" "),
    title: at === 0 && !total ? row.model : undefined,
    text,
  })));
}
