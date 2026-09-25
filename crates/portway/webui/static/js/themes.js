// The palettes. `css/tokens.css` holds the same values as `[data-theme]`
// blocks (a test keeps the two identical); this copy is what the gallery
// lists and what the contrast tests measure.

export const TOKENS = [
  "bg", "surface", "surface-2", "border", "grid", "text", "text-dim",
  "accent", "on-accent", "focus", "raw", "wire", "good", "warn", "bad", "model",
];

const theme = (id, name, scheme, values, note = "") => ({
  id,
  name,
  scheme,
  note,
  tokens: Object.fromEntries(TOKENS.map((token, at) => [token, values[at]])),
});

//                                bg         surface    surface-2  border     grid       text       text-dim   accent     on-accent  focus      raw        wire       good       warn       bad        model
export const THEMES = [
  theme("portway-light", "Portway Light", "light", ["#f5f6f8", "#ffffff", "#eef1f5", "#d5dae2", "#e6e9ee", "#16181d", "#555d6c", "#1d5fd0", "#ffffff", "#1d5fd0", "#2f5fc4", "#00766a", "#16732f", "#8a5a00", "#c42129", "#8a3fb0"]),
  theme("portway-dark", "Portway Dark", "dark", ["#0e1014", "#15181e", "#1d2129", "#2c323d", "#232833", "#e6e9ef", "#9aa3b2", "#78a9ff", "#0b1020", "#8ab8ff", "#6b98f0", "#27bfa0", "#4fc46e", "#e2b342", "#ff6b6b", "#d08cf5"]),
  theme("midnight", "Midnight (OLED)", "dark", ["#000000", "#000000", "#0b0b0e", "#1f1f24", "#141418", "#e8e8ea", "#8e8e98", "#7aa7ff", "#000000", "#7aa7ff", "#6b98f0", "#18b894", "#3fbf5f", "#e0b040", "#ff5c5c", "#c77dff"]),
  theme("solarized-light", "Solarized Light", "light", ["#fdf6e3", "#fdf6e3", "#eee8d5", "#d9d2bf", "#e9e2cf", "#073642", "#4f6369", "#1f6aa3", "#fdf6e3", "#268bd2", "#2f5fc4", "#00705e", "#5b6a00", "#7a5800", "#c02b28", "#b02a6c"]),
  theme("solarized-dark", "Solarized Dark", "dark", ["#002b36", "#002b36", "#073642", "#0f4a58", "#0a3c48", "#eee8d5", "#a3b1b1", "#5cb1ef", "#002b36", "#5cb1ef", "#6d9be8", "#34c2a0", "#a3b81a", "#d9a400", "#ff7b70", "#f57ab4"]),
  theme("nord", "Nord", "dark", ["#2e3440", "#2e3440", "#3b4252", "#4c566a", "#3b4252", "#eceff4", "#c0c8d6", "#9fd3e0", "#2e3440", "#88c0d0", "#8fb0f0", "#62d0a8", "#b4cf9c", "#ebcb8b", "#f0a0a6", "#d8b6d3"]),
  theme("dracula", "Dracula", "dark", ["#282a36", "#21222c", "#343746", "#44475a", "#343746", "#f8f8f2", "#b4bbdc", "#bd93f9", "#21222c", "#ff79c6", "#9d9bff", "#2cc5d6", "#50fa7b", "#f1fa8c", "#ff7a7a", "#ff79c6"]),
  theme("gruvbox", "Gruvbox", "dark", ["#282828", "#282828", "#32302f", "#504945", "#3c3836", "#ebdbb2", "#bdae93", "#8ec07c", "#282828", "#fabd2f", "#83a5f0", "#4fbfa8", "#b8bb26", "#fabd2f", "#ff6f5c", "#e29bb0"]),
  theme("tokyo-night", "Tokyo Night", "dark", ["#16161e", "#1a1b26", "#24283b", "#2f3549", "#232433", "#c0caf5", "#9aa5ce", "#7aa2f7", "#16161e", "#7dcfff", "#7a95f5", "#35c7ad", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7"]),
  theme("catppuccin-latte", "Catppuccin Latte", "light", ["#e6e9ef", "#eff1f5", "#e6e9ef", "#ccd0da", "#dce0e8", "#4c4f69", "#50536b", "#1a55cc", "#ffffff", "#5b6ee0", "#2f5fc4", "#006b5c", "#276b1a", "#7f4800", "#b80d33", "#7a2fd6"]),
  theme("catppuccin-mocha", "Catppuccin Mocha", "dark", ["#181825", "#1e1e2e", "#313244", "#45475a", "#313244", "#cdd6f4", "#a6adc8", "#89b4fa", "#11111b", "#b4befe", "#7f9af5", "#4fd0b4", "#a6e3a1", "#f9e2af", "#f38ba8", "#cba6f7"]),
  theme("high-contrast", "High Contrast", "dark", ["#000000", "#000000", "#101010", "#9a9a9a", "#3a3a3a", "#ffffff", "#d4d4d4", "#7fd7ff", "#000000", "#ffd400", "#7aa2ff", "#00d7e0", "#3dff7a", "#ffd400", "#ff7373", "#ff8cff"]),
  theme("phosphor", "Phosphor (CRT)", "dark", ["#020a04", "#051208", "#0a1f0f", "#164d24", "#0e2a15", "#8dffa6", "#52c46e", "#c4ffd2", "#020a04", "#c4ffd2", "#3fb85e", "#9dffb6", "#5cff85", "#e4ff5c", "#ff6a55", "#7affd0"], "one hue, told apart by lightness and hatching"),
  theme("amber", "Amber CRT", "dark", ["#0c0600", "#130a00", "#1e1100", "#4a2e00", "#2a1800", "#ffc15a", "#c98f32", "#ffe0a3", "#0c0600", "#ffe0a3", "#c7801a", "#ffd88f", "#d8f07a", "#fff04d", "#ff6a55", "#ffa27a"], "one hue, told apart by lightness and hatching"),
  theme("paper", "Paper", "light", ["#f3eee3", "#faf7f0", "#ece5d5", "#d8cfbc", "#e6dfcf", "#2a251f", "#5f5446", "#1f4e8c", "#ffffff", "#1f4e8c", "#2f5fc4", "#00695a", "#2e6a1e", "#7d5000", "#a4201a", "#7a2e6e"]),
];

/** Ordinal one-hue themes: raw and wire differ by lightness, not hue. */
export const MONOCHROME = new Set(["phosphor", "amber"]);

export const SYSTEM = { id: "system", name: "System", scheme: "auto", note: "follows the OS: Portway Light or Dark" };

export function byId(id) {
  return THEMES.find((entry) => entry.id === id);
}

/** The theme `system` resolves to right now. */
export function resolve(id, prefersDark) {
  if (id === "system" || !byId(id)) return prefersDark ? "portway-dark" : "portway-light";
  return id;
}

// ------------------------------------------------------------- measurement

function channels(hex) {
  const value = parseInt(hex.slice(1), 16);
  return [(value >> 16) & 255, (value >> 8) & 255, value & 255].map((channel) => channel / 255);
}

function linear(channel) {
  return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
}

/** WCAG 2 contrast ratio of two `#rrggbb` colors. */
export function contrast(first, second) {
  const luminance = (hex) => {
    const [r, g, b] = channels(hex).map(linear);
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  };
  const [light, dark] = [luminance(first), luminance(second)].sort((a, b) => b - a);
  return (light + 0.05) / (dark + 0.05);
}

/** OKLab of a `#rrggbb` color. */
export function oklab(hex) {
  const [r, g, b] = channels(hex).map(linear);
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
  ];
}

/** Perceptual distance, OKLab ΔE × 100. */
export function deltaE(first, second) {
  const [a, b] = [oklab(first), oklab(second)];
  return Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]) * 100;
}

/** The whole `tokens.css` body for the palettes, as this module holds them. */
export function css() {
  const block = (selector, entry) =>
    `${selector} {\n  color-scheme: ${entry.scheme};\n${TOKENS.map((token) => `  --${token}: ${entry.tokens[token]};`).join("\n")}\n}\n`;
  return THEMES.map((entry) => block(`[data-theme="${entry.id}"]`, entry)).join("\n");
}
