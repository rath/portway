// What the HUD's numbers did over the time this page has been open: latency
// percentiles and the upload saving per tick, and each model's share.

import { $, fill, h, token } from "../dom.js";
import { drawLines } from "../charts.js";
import { clock, human, humanTime } from "../format.js";

const METRICS = [
  ["requests", "requests", (model) => model.requests, String],
  ["raw", "raw upload", (model) => model.body, human],
  ["wire", "wire upload", (model) => model.wire, human],
  ["down", "download", (model) => model.down, human],
];

export class InsightsView {
  constructor(ctx) {
    this.ctx = ctx;
    this.root = $("#view-insights");
    this.metric = "requests";
    this.hover = { latency: null, saved: null };
    this.built = false;
    this.tooltip = h("div", { class: "tooltip", hidden: true, role: "presentation" });
    document.body.append(this.tooltip);
  }

  show() {
    this.build();
    this.render();
  }

  hide() {
    this.tooltip.hidden = true;
  }

  build() {
    if (this.built) return;
    this.built = true;
    this.latency = h("canvas", { tabindex: "0", role: "img", "aria-label": "time to first byte" });
    this.saved = h("canvas", { tabindex: "0", role: "img", "aria-label": "upload saved" });
    this.share = h("div", { class: "share" });
    this.shareControls = h("div", { class: "segmented", role: "group", "aria-label": "Measure" });
    const panel = (title, legend, body) => h("section", { class: "panel" },
      h("div", { class: "controls" }, h("h2", { text: title }), h("div", { class: "grow" }), legend), body);
    fill(this.root,
      h("p", { class: "note", text: "Collected by this page once a second since it opened; a reload of the page starts over." }),
      h("div", { class: "insights" },
        panel("TTFB p50 / p95", h("span", { class: "legend" }, "p50 ", h("i", { class: "swatch raw" }), " p95 (dashed)"), this.latency),
        panel("Upload saved", h("span", { class: "legend", text: "share of raw bytes that did not go on the wire" }), this.saved),
        panel("Share by model", this.shareControls, this.share),
        panel("About", null, h("p", { class: "note", text: "Percentiles are the dashboard's rolling window (the last 512 samples, nearest rank); History has the recorder's exact ones for any window." }))));
    this.bind(this.latency, "latency");
    this.bind(this.saved, "saved");
  }

  /** Crosshair and tooltip, by pointer or by ←/→ on the focused chart. */
  bind(canvas, name) {
    const points = () => this.ctx.state.history.points;
    const place = (index, x, y) => {
      this.hover[name] = index;
      this.render();
      const point = points()[index];
      if (!point) {
        this.tooltip.hidden = true;
        return;
      }
      this.tooltip.textContent = name === "latency"
        ? `${clock(point.at)}  p50 ${point.p50 == null ? "-" : humanTime(point.p50)}  p95 ${point.p95 == null ? "-" : humanTime(point.p95)}`
        : `${clock(point.at)}  saved ${point.saved == null ? "-" : `${point.saved.toFixed(1)}%`}`;
      this.tooltip.hidden = false;
      this.tooltip.style.left = `${Math.min(x + 12, window.innerWidth - 260)}px`;
      this.tooltip.style.top = `${y + 12}px`;
    };
    canvas.addEventListener("pointermove", (event) => {
      const count = points().length;
      if (!count) return;
      const rect = canvas.getBoundingClientRect();
      const left = 64;
      const width = Math.max(1, rect.width - left - 8);
      const index = Math.round(((event.clientX - rect.left - left) / width) * (count - 1));
      place(Math.min(Math.max(index, 0), count - 1), event.clientX, event.clientY);
    });
    canvas.addEventListener("pointerleave", () => {
      this.hover[name] = null;
      this.tooltip.hidden = true;
      this.render();
    });
    canvas.addEventListener("keydown", (event) => {
      const count = points().length;
      if (!count || (event.key !== "ArrowLeft" && event.key !== "ArrowRight")) return;
      event.preventDefault();
      event.stopPropagation();
      const current = this.hover[name] ?? count - 1;
      const next = Math.min(Math.max(current + (event.key === "ArrowLeft" ? -1 : 1), 0), count - 1);
      const rect = canvas.getBoundingClientRect();
      place(next, rect.left + 64 + ((rect.width - 72) * next) / Math.max(1, count - 1), rect.top + 20);
    });
    canvas.addEventListener("blur", () => {
      this.hover[name] = null;
      this.tooltip.hidden = true;
    });
  }

  render() {
    if (!this.built || this.root.hidden) return;
    const points = this.ctx.state.history.points;
    const times = points.map((point) => point.at);
    drawLines(this.latency, times, [
      { values: points.map((point) => point.p50), color: token("raw") },
      { values: points.map((point) => point.p95), color: token("raw"), dash: true },
    ], (value) => humanTime(value), this.hover.latency);
    drawLines(this.saved, times, [
      { values: points.map((point) => point.saved), color: token("good") },
    ], (value) => `${value.toFixed(0)}%`, this.hover.saved, 100);
    const last = points[points.length - 1];
    this.latency.setAttribute("aria-label", last
      ? `time to first byte over ${points.length} seconds: now p50 ${last.p50 == null ? "none" : humanTime(last.p50)}, p95 ${last.p95 == null ? "none" : humanTime(last.p95)}`
      : "time to first byte: no samples yet");
    this.saved.setAttribute("aria-label", last?.saved != null
      ? `upload saved: now ${last.saved.toFixed(1)} percent`
      : "upload saved: nothing uploaded yet");
    this.renderShare();
  }

  renderShare() {
    const metric = METRICS.find(([key]) => key === this.metric);
    fill(this.shareControls, METRICS.map(([key, label]) => h("button", {
      type: "button",
      "aria-pressed": key === this.metric ? "true" : "false",
      onclick: () => {
        this.metric = key;
        this.renderShare();
      },
      text: label,
    })));
    const models = this.ctx.state.models;
    const values = models.map((model) => metric[2](model));
    const total = values.reduce((sum, value) => sum + value, 0);
    if (!models.length || !total) {
      fill(this.share, h("p", { class: "note", text: "nothing counted yet" }));
      return;
    }
    const order = models.map((model, at) => [model, values[at]]).sort((a, b) => b[1] - a[1]);
    fill(this.share, order.map(([model, value]) => {
      const fill_ = h("div", { class: "fill" });
      fill_.style.transform = `scaleX(${value / total})`;
      return h("div", { class: "bar" },
        h("span", { class: "t-model", text: model.name }),
        h("div", { class: "track", role: "img", "aria-label": `${model.name}: ${((value / total) * 100).toFixed(1)} percent` }, fill_),
        h("span", { class: "mono", text: `${metric[3](value)} · ${((value / total) * 100).toFixed(0)}%` }));
    }));
  }
}
