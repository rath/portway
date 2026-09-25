// Per-browser conveniences. Storage can be missing or refuse (a private
// window, blocked site data); the page works the same without it.

const PREFIX = "portway.";

export function get(key, fallback) {
  try {
    const value = window.localStorage.getItem(PREFIX + key);
    return value === null ? fallback : value;
  } catch {
    return fallback;
  }
}

export function set(key, value) {
  try {
    window.localStorage.setItem(PREFIX + key, String(value));
  } catch {
    // Kept for this page only.
  }
}

export function flag(key, fallback) {
  return get(key, fallback ? "1" : "0") === "1";
}

export function setFlag(key, value) {
  set(key, value ? "1" : "0");
}
