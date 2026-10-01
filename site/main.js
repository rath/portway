import { SESSION } from "./session.js";
import { strings } from "./i18n.js?v=5c19a0daadbd";

const TURNS = SESSION.body.length;
const KIB = 1024;
const MIB = 1024 * 1024;
const PLAY_MS = 4200;

// The page declares its language; the sentences this script writes and the
// number format follow it. Grouping is the same in all four, but a reader who
// asks for another locale should get its digits and dates too.
const LANG = document.documentElement.lang || "en";
const S = strings(LANG);
const fill = (template, values) => template.replace(/\{(\w+)\}/g, (_, key) => values[key] ?? `{${key}}`);

const cumulative = (xs) => xs.reduce((acc, x, i) => (acc.push((acc[i - 1] ?? 0) + x), acc), []);
const TOTAL = {
  body: cumulative(SESSION.body),
  zstd: cumulative(SESSION.zstd),
  wire: cumulative(SESSION.wire),
};

const fmt = new Intl.NumberFormat(LANG);
const bytes = (n) => `${fmt.format(n)} B`;
const short = (n) =>
  n >= MIB ? `${(n / MIB).toFixed(1)} MiB` : n >= KIB ? `${Math.round(n / KIB)} KiB` : `${n} B`;

const monitor = document.getElementById("monitor");
const plot = monitor.querySelector("[data-plot]");
const range = monitor.querySelector("#turn");
const out = monitor.querySelector("#turn-out");
const note = monitor.querySelector("[data-turn-note]");
const counters = {
  body: monitor.querySelector('[data-counter="body"]'),
  zstd: monitor.querySelector('[data-counter="zstd"]'),
  wire: monitor.querySelector('[data-counter="wire"]'),
};
const viewButtons = [...monitor.querySelectorAll("[data-view]")];
const replayButton = monitor.querySelector("[data-replay]");
const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

let view = "total";
let position = TURNS; // fractional turn, 1..TURNS
let frame = 0;
let geometry = null;

// Round an axis maximum up to a step that gives three to five gridlines.
function axis(max, unit) {
  const raw = max / unit;
  const steps = [1, 2, 2.5, 4, 5, 10, 20, 25, 50, 100, 200, 250, 500];
  const step = steps.find((s) => raw / s <= 4.2) ?? steps[steps.length - 1];
  const top = Math.ceil(raw / step) * step;
  const ticks = [];
  for (let v = 0; v <= top + 1e-9; v += step) ticks.push(v * unit);
  return { top: top * unit, ticks, unit };
}

function build() {
  const width = plot.clientWidth;
  const height = plot.clientHeight;
  if (!width || !height) return;

  const narrow = width < 520;
  const pad = { top: 14, right: narrow ? 8 : 20, bottom: 26, left: narrow ? 50 : 64 };
  const w = width - pad.left - pad.right;
  const h = height - pad.top - pad.bottom;

  const series = view === "total" ? TOTAL : SESSION;
  const max = Math.max(...series.body);
  const unit = view === "total" ? MIB : 100 * KIB;
  const a = axis(max, unit);
  const unitName = view === "total" ? "MiB" : "KiB";
  const label = (v) => (view === "total" ? `${v / MIB} ${unitName}` : `${Math.round(v / KIB)} ${unitName}`);

  const x = (turn) => pad.left + ((turn - 1) / (TURNS - 1)) * w;
  const y = (v) => pad.top + h - (v / a.top) * h;
  const line = (xs) => xs.map((v, i) => `${i ? "L" : "M"}${x(i + 1).toFixed(1)},${y(v).toFixed(1)}`).join("");

  const grid = a.ticks
    .map((v) => `<line x1="${pad.left}" x2="${pad.left + w}" y1="${y(v)}" y2="${y(v)}"/>`)
    .join("");
  const yTicks = a.ticks
    .map((v) => `<text class="tick" x="${pad.left - 10}" y="${y(v) + 4}" text-anchor="end">${v ? label(v) : "0"}</text>`)
    .join("");
  const xEvery = narrow ? 13 : 5;
  const xTicks = Array.from({ length: TURNS }, (_, i) => i + 1)
    .filter((t) => t === 1 || t % xEvery === 0 || t === TURNS)
    .filter((t, i, all) => t === TURNS || all[i + 1] - t >= (narrow ? 6 : 3))
    .map((t) => `<text class="tick" x="${x(t)}" y="${pad.top + h + 18}" text-anchor="middle">${t}</text>`)
    .join("");

  let raw;
  if (view === "total") {
    const area = `${line(series.body)}L${x(TURNS)},${y(0)}L${x(1)},${y(0)}Z`;
    raw = `<path class="raw-area" d="${area}"/><path class="raw-line" d="${line(series.body)}"/>`;
  } else {
    const band = w / (TURNS - 1);
    const bw = Math.max(2, band * 0.62);
    raw = series.body
      .map((v, i) => `<rect class="raw-bar" x="${(x(i + 1) - bw / 2).toFixed(1)}" y="${y(v).toFixed(1)}" width="${bw.toFixed(1)}" height="${(y(0) - y(v)).toFixed(1)}"/>`)
      .join("");
  }

  plot.innerHTML = `
<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" aria-hidden="true" focusable="false">
  <defs><clipPath id="reveal"><rect x="0" y="0" height="${height}" width="${width}"/></clipPath></defs>
  <g class="grid">${grid}</g>
  <g>${yTicks}${xTicks}</g>
  <g clip-path="url(#reveal)">
    ${raw}
    <path class="zstd-line" d="${line(series.zstd)}"/>
    <path class="wire-line" d="${line(series.wire)}"/>
  </g>
  <line class="cursor" y1="${pad.top}" y2="${pad.top + h}"/>
  <circle class="wire-dot" r="5"/>
  <text class="wire-label" text-anchor="end"></text>
</svg>`;

  const svg = plot.firstElementChild;
  geometry = {
    x, y, pad, w, series,
    band: w / (TURNS - 1),
    clip: svg.querySelector("#reveal rect"),
    cursor: svg.querySelector(".cursor"),
    dot: svg.querySelector(".wire-dot"),
    tag: svg.querySelector(".wire-label"),
  };
  paint();
}

function paint() {
  if (!geometry) return;
  const { x, y, series, band, clip, cursor, dot, tag } = geometry;
  const turn = Math.max(1, Math.min(TURNS, Math.floor(position + 1e-6)));
  const edge = x(position) + (view === "turn" ? band / 2 : 0);

  clip.setAttribute("width", Math.max(0, edge + 1));
  cursor.setAttribute("x1", x(turn));
  cursor.setAttribute("x2", x(turn));
  dot.setAttribute("cx", x(turn));
  dot.setAttribute("cy", y(series.wire[turn - 1]));
  tag.setAttribute("x", Math.max(x(turn) - 10, geometry.pad.left + 60));
  tag.setAttribute("y", y(series.wire[turn - 1]) - 12);
  tag.textContent = short(series.wire[turn - 1]);

  const values = view === "total" ? TOTAL : SESSION;
  counters.body.textContent = bytes(values.body[turn - 1]);
  counters.zstd.textContent = bytes(values.zstd[turn - 1]);
  counters.wire.textContent = bytes(values.wire[turn - 1]);

  range.value = String(turn);
  out.textContent = String(turn);
  note.innerHTML =
    turn === 1
      ? fill(S.seed, { body: fmt.format(SESSION.body[0]), wire: fmt.format(SESSION.wire[0]) })
      : fill(S.turn, {
          turn: String(turn),
          body: fmt.format(SESSION.body[turn - 1]),
          wire: fmt.format(SESSION.wire[turn - 1]),
        });
}

function stop() {
  cancelAnimationFrame(frame);
  frame = 0;
  replayButton.textContent = S.replay;
}

function play() {
  stop();
  if (reduceMotion.matches) {
    position = TURNS;
    paint();
    return;
  }
  replayButton.textContent = S.stop;
  const start = performance.now();
  const tick = (now) => {
    const p = Math.min(1, (now - start) / PLAY_MS);
    position = 1 + p * (TURNS - 1);
    paint();
    if (p < 1) frame = requestAnimationFrame(tick);
    else stop();
  };
  frame = requestAnimationFrame(tick);
}

function setView(next) {
  view = next;
  for (const b of viewButtons) b.setAttribute("aria-pressed", String(b.dataset.view === next));
  build();
}

viewButtons.forEach((b) => b.addEventListener("click", () => setView(b.dataset.view)));
replayButton.addEventListener("click", () => (frame ? stop() : play()));
range.addEventListener("input", () => {
  stop();
  position = Number(range.value);
  paint();
});
plot.addEventListener("pointermove", (e) => {
  // Scrolling under a still pointer also fires pointermove, with no movement.
  if (e.pointerType !== "mouse" || !geometry || (e.movementX === 0 && e.movementY === 0)) return;
  const rect = plot.getBoundingClientRect();
  const t = 1 + Math.round(((e.clientX - rect.left - geometry.pad.left) / geometry.w) * (TURNS - 1));
  if (t < 1 || t > TURNS) return;
  stop();
  position = t;
  paint();
});

let resizeTimer = 0;
new ResizeObserver(() => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(build, 60);
}).observe(plot);

build();

// Show the complete comparison on arrival. Replay is an explicit action so
// the first viewport explains the result without waiting for an animation.
