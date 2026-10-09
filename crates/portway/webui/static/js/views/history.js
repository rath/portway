// `--report` in the browser: any window the recorder still holds, per model,
// with the trouble lines, as tables and as the exact text.

import { displayModel } from "../modelnames.js";
import { $, copy, download, fill, h } from "../dom.js";
import { tableCsv } from "../export.js";
import { human, maybeTime } from "../format.js";

const PRESETS = ["1h", "6h", "24h", "7d", "30d"];
const SPAN = /^[1-9][0-9]*[smhd]$/;

export class HistoryView {
  constructor(ctx) {
    this.ctx = ctx;
    this.root = $("#view-history");
    this.since = "24h";
    this.model = "";
    this.report = null;
    this.error = null;
  }

  show() {
    if (!this.report && !this.error) this.load();
    else this.render();
  }

  hide() {}

  async load() {
    this.error = null;
    this.renderControls("reading…");
    try {
      this.report = await this.ctx.api.report(this.since, this.model, true);
    } catch (err) {
      this.report = null;
      this.error = err.message;
      if (err.status === 401) this.ctx.expired();
    }
    this.render();
  }

  controls(status) {
    const custom = h("input", {
      type: "text",
      value: this.since,
      "aria-label": "Window, like 90s, 30m, 24h or 7d",
      size: "6",
      spellcheck: "false",
      onkeydown: (event) => {
        if (event.key === "Enter") apply();
      },
    });
    const models = new Set(this.ctx.state.models.map((model) => model.name));
    for (const row of this.report?.rows ?? []) models.add(row.model);
    if (this.model) models.add(this.model);
    const select = h("select", { "aria-label": "Model", onchange: (event) => {
      this.model = event.target.value;
      this.load();
    } }, h("option", { value: "", text: "all models" }), [...models].sort().map((name) =>
      h("option", { value: name, selected: name === this.model, title: name, text: displayModel(name) })));
    const apply = () => {
      const value = custom.value.trim();
      if (!SPAN.test(value)) {
        this.ctx.toast("a window looks like 90s, 30m, 24h or 7d", "bad");
        return;
      }
      this.since = value;
      this.load();
    };
    return h("div", { class: "controls" },
      h("div", { class: "segmented", role: "group", "aria-label": "Window" }, PRESETS.map((preset) => h("button", {
        type: "button",
        "aria-pressed": preset === this.since ? "true" : "false",
        onclick: () => {
          this.since = preset;
          this.load();
        },
        text: preset,
      }))),
      custom,
      h("button", { type: "button", onclick: apply, text: "Read" }),
      select,
      h("div", { class: "grow" }),
      status ? h("span", { class: "note", text: status }) : null,
      h("button", {
        type: "button",
        class: "ghost",
        disabled: !this.report,
        onclick: async () => this.ctx.toast(await copy(this.report.text) ? "copied the --report text" : "the clipboard refused", "good"),
        text: "Copy as --report",
      }));
  }

  renderControls(status) {
    fill(this.root, h("section", { class: "panel" }, this.controls(status)));
  }

  render() {
    if (this.error) {
      fill(this.root, h("section", { class: "panel" }, this.controls(), h("p", { class: "error-box", text: this.error })));
      return;
    }
    const report = this.report;
    if (!report) {
      this.renderControls();
      return;
    }
    const rows = report.total ? [...report.rows, report.total] : [];
    const volumeHead = ["model", "reqs", "2xx", "3xx", "4xx", "5xx", "trunc", "reused"];
    const volume = rows.map((row) => [row.model, row.requests, row.ok, row.redirect, row.client, row.server, row.truncated, row.reused].map(String));
    const timingHead = ["model", "up raw", "up wire", "up saved", "down", "down wire", "down saved", "ttfb p50", "ttfb p95", "up p50", "up p95", "conn mean"];
    const timing = rows.map((row) => [
      row.model, human(row.body), human(row.wire), row.saved, human(row.received), human(row.received_wire), row.down_saved,
      maybeTime(row.ttfb_p50), maybeTime(row.ttfb_p95), maybeTime(row.up_p50), maybeTime(row.up_p95), maybeTime(row.conn_mean),
    ]);
    const table = (title, head, body, file) => h("section", { class: "panel" },
      h("div", { class: "controls" },
        h("h2", { text: title }),
        h("div", { class: "grow" }),
        h("button", { type: "button", class: "ghost", onclick: () => download(file, tableCsv(head, body), "text/csv"), text: "CSV" })),
      h("div", { class: "table-wrap" }, h("table", { class: "data" },
        h("thead", {}, h("tr", {}, head.map((name) => h("th", { scope: "col", text: name })))),
        h("tbody", {}, body.map((cells, at) => h("tr", { class: at === body.length - 1 ? "total" : "" },
          cells.map((text, column) => h("td", { class: column === 0 ? (at === body.length - 1 ? "t-good" : "t-model") : "", title: column === 0 ? text : undefined, text: column === 0 && at < report.rows.length ? displayModel(text) : text }))))))));
    const scope = displayModel(report.model) ?? "all models";
    fill(this.root,
      h("section", { class: "panel" },
        this.controls(),
        h("p", { class: "window-title", text: `portway — ${report.db}` }),
        h("p", { class: "window-title", text: `window  ${report.window}  (${report.span}, ${scope})` })),
      rows.length
        ? [table("Volume", volumeHead, volume, "portway-volume.csv"), table("Timing", timingHead, timing, "portway-timing.csv")]
        : h("section", { class: "panel" }, h("p", { class: "note", text: report.model ? `no requests for model ${displayModel(report.model)} in this window` : "no requests recorded in this window" })),
      h("section", { class: "panel" },
        h("h2", { class: "panel-head", text: "Trouble, last 20 in the window" }),
        report.trouble.length
          ? h("pre", { class: "trouble-list" }, report.trouble.map((line) => h("div", {
            class: line.kind === "log" ? (line.level === "ERROR" ? "t-bad" : "t-time") : line.status >= 500 ? "t-bad" : "t-time",
            title: line.kind === "request" ? line.model : undefined,
            text: line.kind === "request" && line.model ? line.text.replace(`  ${line.status}  ${line.model}`, () => `  ${line.status}  ${displayModel(line.model)}`) : line.text,
          })))
          : h("p", { class: "note", text: "none" })));
  }
}
