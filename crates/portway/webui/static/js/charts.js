// Canvas drawing. Colors are read from the palette in force at draw time,
// so a theme switch needs nothing but a redraw.

import { token } from "./dom.js";
import { human } from "./format.js";

/** Size the backing store to the element and the device, and clear it. */
export function surface(canvas) {
  const ratio = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.floor(canvas.clientWidth));
  const height = Math.max(1, Math.floor(canvas.clientHeight));
  if (canvas.width !== Math.round(width * ratio) || canvas.height !== Math.round(height * ratio)) {
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
  }
  const context = canvas.getContext("2d");
  context.setTransform(ratio, 0, 0, ratio, 0, 0);
  context.clearRect(0, 0, width, height);
  return { context, width, height };
}

/** A diagonal hatch in `color`, for themes whose two series share a hue. */
function hatch(context, color) {
  const tile = document.createElement("canvas");
  tile.width = 6;
  tile.height = 6;
  const pen = tile.getContext("2d");
  pen.strokeStyle = color;
  pen.lineWidth = 1.5;
  pen.beginPath();
  pen.moveTo(0, 6);
  pen.lineTo(6, 0);
  pen.stroke();
  return context.createPattern(tile, "repeat");
}

/**
 * One column per turn, newest on the right: the raw body full height, the
 * wire part of it over the bottom (tui::chart::TwoToneBars). Returns how many
 * turns fit.
 */
export function drawBars(canvas, bars, monochrome) {
  const { context, width, height } = surface(canvas);
  const slot = 5;
  const count = Math.max(1, Math.floor(width / slot));
  const shown = bars.slice(Math.max(0, bars.length - count));
  const peak = shown.reduce((max, [raw]) => Math.max(max, raw), 0);
  context.fillStyle = token("grid");
  context.fillRect(0, height - 1, width, 1);
  if (!peak) return shown;
  const raw = token("raw");
  const wire = monochrome ? hatch(context, token("wire")) : token("wire");
  const start = width - shown.length * slot;
  shown.forEach(([rawBytes, wireBytes], at) => {
    const x = start + at * slot;
    const tall = Math.max(1, (rawBytes / peak) * (height - 4));
    const low = Math.max(wireBytes ? 1 : 0, (wireBytes / peak) * (height - 4));
    context.fillStyle = raw;
    context.fillRect(x, height - tall, slot - 1, tall - low);
    context.fillStyle = wire;
    context.fillRect(x, height - low, slot - 1, low);
  });
  return shown;
}

/** Two sparklines, up over down, each with its peak. */
export function drawTraffic(canvas, series) {
  const { context, width, height } = surface(canvas);
  const label = 92;
  const half = height / 2;
  const rows = [
    { data: series.up, color: token("wire"), name: "↑ up", top: 0 },
    { data: series.down, color: token("raw"), name: "↓ down", top: half },
  ];
  context.font = `11px ${getComputedStyle(document.body).getPropertyValue("--mono") || "monospace"}`;
  context.textBaseline = "bottom";
  for (const row of rows) {
    const peak = row.data.reduce((max, value) => Math.max(max, value), 0);
    const plot = width - label;
    const step = row.data.length ? plot / row.data.length : plot;
    context.fillStyle = row.color;
    context.fillText(row.name, 0, row.top + half - 4);
    context.fillStyle = token("text-dim");
    context.fillText(human(peak), 44, row.top + half - 4);
    context.fillStyle = token("grid");
    context.fillRect(label, row.top + half - 1, plot, 1);
    if (!peak) continue;
    context.fillStyle = row.color;
    row.data.forEach((value, at) => {
      const tall = (value / peak) * (half - 6);
      if (tall <= 0) return;
      context.fillRect(label + at * step, row.top + half - 1 - tall, Math.max(1, step - 1), tall);
    });
  }
  return label;
}

/**
 * Lines over time: `series` is `[{ name, values, color, dash }]` over shared
 * `times` (unix seconds). `format` prints a y value. `hover` is an index to
 * mark, or null. `ceiling` fixes the top of the axis (100 for a percentage).
 * Returns the geometry the pointer handler needs.
 */
export function drawLines(canvas, times, series, format, hover, ceiling = null) {
  const { context, width, height } = surface(canvas);
  const left = 64;
  const bottom = 18;
  const plotWidth = Math.max(1, width - left - 8);
  const plotHeight = Math.max(1, height - bottom - 8);
  const values = series.flatMap((line) => line.values.filter((value) => value != null));
  const peak = values.length ? Math.max(...values) : 0;
  const top = ceiling ?? (peak > 0 ? peak * 1.1 : 1);
  context.font = `11px ${getComputedStyle(document.body).getPropertyValue("--mono") || "monospace"}`;
  context.fillStyle = token("text-dim");
  context.strokeStyle = token("grid");
  context.lineWidth = 1;
  for (let at = 0; at <= 3; at++) {
    const y = 8 + plotHeight - (plotHeight * at) / 3;
    context.beginPath();
    context.moveTo(left, Math.round(y) + 0.5);
    context.lineTo(left + plotWidth, Math.round(y) + 0.5);
    context.stroke();
    context.textBaseline = "middle";
    context.fillText(format((top * at) / 3), 0, y);
  }
  const x = (index) => left + (times.length > 1 ? (plotWidth * index) / (times.length - 1) : plotWidth);
  const y = (value) => 8 + plotHeight - (value / top) * plotHeight;
  for (const line of series) {
    context.strokeStyle = line.color;
    context.lineWidth = 2;
    context.setLineDash(line.dash ? [5, 4] : []);
    context.beginPath();
    let drawing = false;
    line.values.forEach((value, index) => {
      if (value == null) {
        drawing = false;
        return;
      }
      if (drawing) context.lineTo(x(index), y(value));
      else context.moveTo(x(index), y(value));
      drawing = true;
    });
    context.stroke();
  }
  context.setLineDash([]);
  if (hover != null && times.length) {
    context.strokeStyle = token("text-dim");
    context.beginPath();
    context.moveTo(Math.round(x(hover)) + 0.5, 8);
    context.lineTo(Math.round(x(hover)) + 0.5, 8 + plotHeight);
    context.stroke();
    for (const line of series) {
      const value = line.values[hover];
      if (value == null) continue;
      context.fillStyle = line.color;
      context.beginPath();
      context.arc(x(hover), y(value), 3.5, 0, Math.PI * 2);
      context.fill();
    }
  }
  return { left, plotWidth, count: times.length };
}
