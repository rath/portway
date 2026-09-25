// The Rust formatters, ported digit for digit. `test/fixtures/format.json` is
// generated from the Rust side and asserted by both test suites, so a size,
// a duration or a ratio reads the same in the terminal and in the browser.

/**
 * `x` with `digits` decimals, rounded the way Rust's `{:.N}` rounds: on the
 * exact binary value, ties to even. `toFixed` also works on the exact value
 * but breaks ties upward, so an exact tie (2.5, 1.125) is settled here.
 */
export function fixedEven(x, digits) {
  if (!Number.isFinite(x)) return String(x);
  if (x < 0) {
    const text = fixedEven(-x, digits);
    return /[1-9]/.test(text) ? `-${text}` : text;
  }
  const rounded = x.toFixed(digits);
  if (x >= 1e21) return rounded;
  const exact = x.toFixed(100);
  const dot = exact.indexOf(".");
  const tail = exact.slice(dot + 1 + digits);
  if (!/^50*$/.test(tail)) return rounded;
  // An exact tie: toFixed went up. Keep the lower neighbour when it is even.
  const lower = digits === 0 ? exact.slice(0, dot) : exact.slice(0, dot + 1 + digits);
  const last = Number(lower[lower.length - 1]);
  return last % 2 === 0 ? lower : rounded;
}

/** `logfmt::human`: B under a tenth of a KB, then KB, then MB (1024 steps). */
export function human(size) {
  if (size < 1024) {
    const kb = size / 1024;
    return kb < 0.05 ? `${size}B` : `${fixedEven(kb, 1)}KB`;
  }
  const kb = size / 1024;
  return kb < 1024 ? `${fixedEven(kb, 0)}KB` : `${fixedEven(kb / 1024, 1)}MB`;
}

/** `logfmt::human_time`: ms below a second, then seconds, then minutes. */
export function humanTime(seconds) {
  if (seconds < 1) return `${fixedEven(seconds * 1000, 0)}ms`;
  if (seconds < 60) return `${fixedEven(seconds, 2)}s`;
  // `{:02.1}` never pads: one decimal is already three characters.
  return `${Math.floor(Math.trunc(seconds) / 60)}m${fixedEven(seconds % 60, 1)}s`;
}

/** `logfmt::human_count`: 891, 18.2K, 182K, 1.2M. */
export function humanCount(count) {
  if (count < 10000) return String(count);
  const [scaled, unit] = count < 1000000 ? [count / 1000, "K"] : [count / 1000000, "M"];
  return scaled < 100 ? `${fixedEven(scaled, 1)}${unit}` : `${fixedEven(scaled, 0)}${unit}`;
}

/** `logfmt::span`: the largest unit the width divides into whole. */
export function span(seconds) {
  const units = seconds < 2 * 86400
    ? [["h", 3600], ["m", 60]]
    : [["d", 86400], ["h", 3600], ["m", 60]];
  for (const [unit, scale] of units) {
    if (seconds % scale === 0 && seconds / scale >= 1) return `${seconds / scale}${unit}`;
  }
  return `${seconds}s`;
}

/** The HUD's `up`: hours keep counting past 99. */
export function uptime(seconds) {
  const whole = Math.floor(seconds);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(Math.floor(whole / 3600))}:${pad(Math.floor((whole % 3600) / 60))}:${pad(whole % 60)}`;
}

/** `-76%` for a raw/wire pair, truncated like the log line; `-` before traffic. */
export function ratio(raw, wire) {
  if (!raw || !wire) return "-";
  const saved = raw > wire ? BigInt(raw) - BigInt(wire) : 0n;
  return `-${(saved * 100n) / BigInt(raw)}%`;
}

/** More decimals the smaller the amount: a cent of cache is not `$0.00`. */
export function dollars(amount) {
  if (amount === 0) return "$0";
  if (amount >= 1) return `$${fixedEven(amount, 2)}`;
  if (amount >= 0.01) return `$${fixedEven(amount, 3)}`;
  return `$${fixedEven(amount, 4)}`;
}

/** A hit rate as the usage table prints it: `97.3%`. */
export function percent(rate) {
  return rate == null ? "n/a" : `${fixedEven(rate * 100, 1)}%`;
}

/** A duration that may not have been measured. */
export function maybeTime(seconds, missing = "-") {
  return seconds == null ? missing : humanTime(seconds);
}

/** Local `YYYY-MM-DD HH:MM:SS`, as the report prints a moment. */
export function datetime(unix) {
  const at = new Date(unix * 1000);
  const pad = (n) => String(n).padStart(2, "0");
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())} ` +
    `${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}`;
}

/** Local `HH:MM:SS`. */
export function clock(unix) {
  return datetime(unix).slice(11);
}

/** Bytes per second on a chart axis. */
export function rate(bytesPerSecond) {
  return `${human(Math.round(bytesPerSecond))}/s`;
}
