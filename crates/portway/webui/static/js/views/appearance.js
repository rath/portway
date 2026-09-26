// Themes, density, type and motion, all per browser. Every card previews its
// own palette by carrying its own `data-theme`.

import { $, fill, h } from "../dom.js";
import { SYSTEM, THEMES } from "../themes.js";

export class AppearanceView {
  constructor(ctx) {
    this.ctx = ctx;
    this.root = $("#view-appearance");
    this.before = null;
  }

  show() {
    this.before = this.ctx.theme();
    this.render();
  }

  hide() {}

  card(entry, index, all) {
    const chosen = this.ctx.theme() === entry.id;
    const previewTheme = entry.id === "system" ? this.ctx.resolved() : entry.id;
    const card = h("button", {
      type: "button",
      class: "card",
      role: "radio",
      "aria-checked": chosen ? "true" : "false",
      tabindex: chosen ? "0" : "-1",
      "data-theme": previewTheme,
      "data-index": String(index),
      onclick: () => this.pick(entry.id),
      onkeydown: (event) => {
        const columns = Math.max(1, Math.round(this.root.querySelector(".gallery").clientWidth / 222));
        const moves = { ArrowRight: 1, ArrowDown: columns, ArrowLeft: -1, ArrowUp: -columns };
        if (!(event.key in moves)) return;
        event.preventDefault();
        event.stopPropagation();
        const next = Math.min(Math.max(index + moves[event.key], 0), all.length - 1);
        this.pick(all[next].id);
        this.root.querySelector(`[data-index="${next}"]`)?.focus();
      },
    },
    h("span", { class: "preview", "aria-hidden": "true" },
      h("span", { class: "surface" },
        h("span", { class: "t-dim", text: "12:34:56 " }), h("span", { class: "t-good", text: "200 " }),
        h("span", { class: "t-model", text: "alpha " }), h("span", { class: "t-raw", text: "461KB" }),
        h("span", { class: "t-dim", text: "→" }), h("span", { class: "t-wire", text: "111KB" }), h("br"),
        h("span", { class: "t-time", text: "ttfb 840ms " }), h("span", { class: "t-bad", text: "502 " }),
        h("span", { class: "t-time", text: "WARNING" })),
      h("span", { class: "bars" }, [9, 5, 12, 6, 7, 3, 11, 4].map(() => h("i")))),
    h("span", { class: "caption" }, h("span", { text: entry.name }), h("small", { text: entry.scheme })));
    // Bar heights are data, set through the CSSOM rather than markup.
    [...card.querySelectorAll(".bars i")].forEach((bar, at) => {
      bar.style.height = `${[90, 45, 100, 50, 70, 30, 95, 40][at]}%`;
    });
    return card;
  }

  pick(id) {
    this.ctx.setTheme(id);
    this.render();
    this.root.querySelector('[aria-checked="true"]')?.focus();
  }

  option(label, key, choices) {
    const current = document.documentElement.dataset[key];
    return [
      h("span", { text: label }),
      h("div", { class: "segmented", role: "group", "aria-label": label }, choices.map(([value, text]) => h("button", {
        type: "button",
        "aria-pressed": current === value ? "true" : "false",
        onclick: () => {
          this.ctx.setLook(key, value);
          this.render();
        },
        text,
      }))),
    ];
  }

  render() {
    const all = [SYSTEM, ...THEMES];
    const { state } = this.ctx;
    const notify = h("input", {
      type: "checkbox",
      id: "notify-toggle",
      checked: state.notify,
      onchange: (event) => this.ctx.setNotify(event.target.checked),
    });
    const shortcuts = h("input", {
      type: "checkbox",
      id: "shortcut-toggle",
      checked: state.shortcuts,
      onchange: (event) => this.ctx.setShortcuts(event.target.checked),
    });
    fill(this.root,
      h("section", { class: "panel" },
        h("div", { class: "controls" },
          h("h2", { text: "Theme" }),
          h("span", { class: "count", text: String(all.length) }),
          h("div", { class: "grow" }),
          h("button", {
            type: "button",
            class: "ghost",
            disabled: this.before === this.ctx.theme(),
            onclick: () => this.pick(this.before),
            text: "Revert",
          })),
        h("p", { class: "note", text: "Arrow keys move through the gallery and apply as they go. Phosphor and Amber are one-hue themes: raw and wire differ by lightness and a hatch." }),
        h("div", { class: "gallery", role: "radiogroup", "aria-label": "Theme" }, all.map((entry, index) => this.card(entry, index, all)))),
      h("section", { class: "panel" },
        h("h2", { class: "panel-head", text: "Layout" }),
        h("div", { class: "options" },
          this.option("Density", "density", [["comfortable", "Comfortable"], ["compact", "Compact"]]),
          this.option("Type", "font", [["sans", "Sans, mono figures"], ["mono", "Monospace throughout"]]),
          this.option("Motion", "motion", [["system", "Follow the system"], ["reduce", "Reduced"]]),
          h("label", { for: "shortcut-toggle", text: "Single-key shortcuts" }),
          h("span", {}, shortcuts, " j/k, e, m, q… (⌘K and Esc always work)"),
          h("label", { for: "notify-toggle", text: "Desktop notifications" }),
          h("span", {}, notify, " trouble while this tab is in the background, at most one every 30s"))));
  }
}
