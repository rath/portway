// The popups: request detail (a drawer that follows a flight into its record),
// keys, columns, stop, costs, and the command palette.

import { $, copy, dialogFrame, fill, h, segments } from "../dom.js";
import { COLUMNS, detailFields, eventLine, lineText } from "../eventline.js";
import { cellsOf } from "../eventtable.js";
import { describe } from "../flights.js";
import { dollars, human, humanTime, label, maybeTime } from "../format.js";

function open(dialog) {
  if (!dialog.open) dialog.showModal();
}

// ------------------------------------------------------------------ detail

export function renderDetail(ctx) {
  const dialog = $("#detail");
  const target = ctx.state.detail;
  if (!target) {
    if (dialog.open) dialog.close();
    return;
  }
  if (target.type === "flight") {
    const flight = ctx.state.flights.get(target.id);
    if (!flight) {
      fill(dialog);
      dialogFrame(dialog, "Request in flight", h("p", { class: "note", text: "This request has finished; its record will appear in the list." }));
      open(dialog);
      return;
    }
    const times = ctx.state.flights.times(flight, ctx.now());
    const described = describe(flight, times);
    const fields = [
      ["flight", `#${flight.id}`],
      ["model", flight.model],
      ["request", `${flight.method} ${flight.path}`],
      ["phase", described.text],
      ["warning", described.warn ?? "none"],
      ["age", humanTime(times.age)],
      ["quiet for", humanTime(times.idle)],
      ["upload", `${human(flight.body_len)} -> ${human(flight.wire_len)} (${flight.coding ?? "identity"})`],
      ["upload acked in", maybeTime(flight.upload_s, "not yet")],
      ["status", flight.status == null ? "waiting for headers" : String(flight.status)],
      ["ttfb", maybeTime(flight.ttfb, "not yet")],
      ["received", `${human(flight.received_wire)} on the wire -> ${human(flight.received)} decoded`],
      ["retries", String(flight.retries)],
    ];
    dialogFrame(dialog, "Request in flight", fieldList(fields), [
      h("span", { class: "note", text: "live · becomes the finished record when it ends" }),
    ]);
    open(dialog);
    return;
  }
  const event = ctx.state.events.get(target.seq);
  if (!event) {
    ctx.state.detail = null;
    if (dialog.open) dialog.close();
    return;
  }
  if (event.kind !== "request") {
    dialogFrame(dialog, "Log record", fieldList([["when", event.stamp], ["level", event.level], ["message", event.message]]));
    open(dialog);
    return;
  }
  const line = lineText(eventLine(event, new Set(COLUMNS.map((column) => column.name))));
  dialogFrame(dialog, "Request", [
    fieldList(detailFields(event)),
    h("pre", { class: "trouble-list", "aria-label": "The line as printed" }, segments(eventLine(event, ctx.state.columns))),
  ], [
    h("button", { type: "button", onclick: async () => ctx.toast(await copy(JSON.stringify(event, null, 1)) ? "copied as JSON" : "the clipboard refused", "good"), text: "Copy JSON" }),
    h("button", { type: "button", onclick: async () => ctx.toast(await copy(line) ? "copied the line" : "the clipboard refused", "good"), text: "Copy line" }),
    h("button", {
      type: "button",
      onclick: () => {
        ctx.closeDetail();
        ctx.setQuery(`model:${event.model} route:"${event.method} ${event.path}"`);
      },
      text: "Show similar",
    }),
  ]);
  open(dialog);
}

function fieldList(fields) {
  return h("dl", { class: "fields" }, fields.flatMap(([name, value]) => [h("dt", { text: name }), h("dd", { text: value })]));
}

// -------------------------------------------------------------------- help

const HELP = [
  ["q", "stop the forwarder (asks first; q again confirms)"],
  ["j / ↓ / k / ↑", "move the cursor; the wheel scrolls too"],
  ["PgDn / Space / PgUp", "move by a page"],
  ["g / Home · G / End", "oldest line · back to following"],
  ["Enter", "details of the highlighted request"],
  ["e", "only 4xx/5xx, cut streams and warnings"],
  ["m", "cycle the model filter"],
  ["u", "tokens and cost per model, by window (←→ window, p costs)"],
  ["t", "1s / 10s / 60s traffic buckets"],
  ["c", "choose what a request line shows"],
  ["/", "search: words, -word, \"phrase\", status:5xx, model:, upstream:, route:, is:cut, is:catalog, ttfb:>2s, size:>1MB, tok:>50K"],
  ["H · I · A", "history · insights · appearance"],
  ["r", "reload the configuration"],
  ["⌘K / Ctrl-K", "every command, including every theme"],
  ["Esc", "close whatever is open"],
  ["?", "this"],
];

export function openHelp() {
  const dialog = $("#help");
  dialogFrame(dialog, "Keys", h("dl", { class: "keylist" }, HELP.flatMap(([key, what]) => [h("dt", { text: key }), h("dd", { text: what })])),
    [h("span", { class: "note", text: "Single-key shortcuts can be turned off under Appearance." })]);
  open(dialog);
}

// ----------------------------------------------------------------- columns

export function openColumns(ctx) {
  const dialog = $("#columns");
  const picks = COLUMNS.map((column) => {
    // A column of several cells names them as the table heads them, read
    // from the table itself, so a cell added there is listed here too.
    const cells = cellsOf(column.name);
    return h("label", { class: "pick" },
      h("input", {
        type: "checkbox",
        checked: ctx.state.columns.has(column.name),
        onchange: (event) => ctx.toggleColumn(column.name, event.target.checked),
      }),
      h("span", { class: "name", text: column.name }),
      h("span", { class: "what" },
        cells.length > 1
          ? h("span", { class: "cells" }, cells.flatMap((cell, at) => [
            at > 0 ? h("span", { class: "t-dim", text: "·" }) : null,
            h("span", { text: cell.label, title: cell.note }),
          ]))
          : null,
        h("span", { class: "t-dim", text: column.note })));
  });
  dialogFrame(dialog, "Columns", picks, [h("span", { class: "note", text: "kept for this browser" })]);
  open(dialog);
}

// -------------------------------------------------------------------- stop

export function openStop(ctx) {
  const dialog = $("#stop-dialog");
  const { state } = ctx;
  const attached = state.header.mode === "attached";
  const flights = state.flights;
  const body = [];
  if (attached) {
    body.push(h("p", { text: `Stop the daemon serving ${state.header.listen}? This console stays up and keeps showing what was recorded.` }));
  } else {
    body.push(h("p", { text: `Stop the forwarder on ${state.header.listen}? The recorder is flushed first, as on SIGTERM.` }));
    const live = flights.available ? flights.total : state.totals.in_flight;
    if (live > 0) {
      body.push(h("p", { class: "t-bad", text: `${live} request(s) in flight will be cut off:` }));
      if (flights.available) {
        body.push(h("ul", {}, flights.list.slice(0, 12).map((flight) =>
          h("li", {}, h("span", { class: "t-model", text: flight.model || flight.upstream }), ` ${flight.route} · `,
            describe(flight, flights.times(flight, ctx.now())).text))));
      }
    }
  }
  body.push(h("p", { class: "note" }, "Press ", h("kbd", { text: "q" }), " again to stop, any other key to cancel."));
  const cancel = h("button", { type: "button", autofocus: true, onclick: () => dialog.close(), text: "Cancel" });
  const confirm = h("button", {
    type: "button",
    class: "danger solid",
    onclick: () => {
      dialog.close();
      ctx.stop();
    },
    text: attached ? "Stop the daemon" : "Stop",
  });
  dialogFrame(dialog, attached ? "Stop the daemon" : "Stop the forwarder", body, [cancel, confirm]);
  dialog.onkeydown = (event) => {
    if (event.key === "q" && !event.metaKey && !event.ctrlKey) {
      event.preventDefault();
      confirm.click();
    } else if (event.key.length === 1 && !event.metaKey && !event.ctrlKey) {
      event.preventDefault();
      dialog.close();
    }
  };
  open(dialog);
  cancel.focus();
}

// ------------------------------------------------------------------- costs

export function openCosts(table) {
  const dialog = $("#costs");
  const money = (charge, part) => (charge ? dollars(charge[part]) : "-");
  const rows = table.rows.map((row) => h("tr", {},
    h("td", { class: row.charge ? "t-model" : "t-dim", text: label(row.model, row.tier) }),
    ["input", "cache_read", "output", "total"].map((part) => h("td", { class: row.charge ? "t-good" : "t-dim", text: money(row.charge, part) }))));
  if (table.total.charge) {
    rows.push(h("tr", { class: "total" },
      h("td", { class: "t-good", text: "total" }),
      ["input", "cache_read", "output", "total"].map((part) => h("td", { class: "t-good", text: money(table.total.charge, part) }))));
  }
  dialogFrame(dialog, "Costs", [
    h("p", { class: "note", text: "cost by source: what the window cost, not the rates that produced it" }),
    h("div", { class: "table-wrap" }, h("table", { class: "data" },
      h("thead", {}, h("tr", {}, ["model", "prompt", "cached", "output", "total$"].map((name) => h("th", { text: name })))),
      h("tbody", {}, rows))),
  ]);
  open(dialog);
}

// ----------------------------------------------------------------- palette

/** `commands` is `[{ name, hint, run }]`, rebuilt each time it opens. */
export function openPalette(commands) {
  const dialog = $("#palette");
  let matches = commands;
  let at = 0;
  const list = h("ul", { role: "listbox", id: "palette-list", "aria-label": "Commands" });
  const input = h("input", {
    type: "text",
    placeholder: "type a command, a theme, a model…",
    "aria-controls": "palette-list",
    "aria-label": "Command",
    autocomplete: "off",
    spellcheck: "false",
  });
  const draw = () => {
    fill(list, matches.slice(0, 60).map((command, index) => h("li", {
      role: "option",
      id: `palette-${index}`,
      "aria-selected": index === at ? "true" : "false",
      onmousedown: (event) => {
        event.preventDefault();
        run(index);
      },
    }, h("span", { text: command.name }), h("span", { class: "hint", text: command.hint || "" }))));
    input.setAttribute("aria-activedescendant", matches.length ? `palette-${at}` : "");
    list.children[at]?.scrollIntoView({ block: "nearest" });
  };
  const run = (index) => {
    const command = matches[index];
    if (!command) return;
    dialog.close();
    command.run();
  };
  input.addEventListener("input", () => {
    const words = input.value.toLowerCase().split(/\s+/).filter(Boolean);
    matches = commands.filter((command) => {
      const text = `${command.name} ${command.hint || ""}`.toLowerCase();
      return words.every((word) => text.includes(word));
    });
    at = 0;
    draw();
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      at = Math.min(at + 1, Math.max(0, Math.min(matches.length, 60) - 1));
      draw();
      event.preventDefault();
    } else if (event.key === "ArrowUp") {
      at = Math.max(at - 1, 0);
      draw();
      event.preventDefault();
    } else if (event.key === "Enter") {
      run(at);
      event.preventDefault();
    }
  });
  fill(dialog, input, list);
  draw();
  open(dialog);
  input.focus();
}
