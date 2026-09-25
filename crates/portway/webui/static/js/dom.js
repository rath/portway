// Element building without markup strings: every piece of text the server
// sends reaches the page through textContent, never through the parser.

/**
 * `h("td", { class: "x", onclick }, "text", child)`. Attributes named `on*`
 * become listeners, `dataset` is merged, `text` sets textContent, `hidden`
 * and other booleans are set as properties; everything else is an attribute.
 */
export function h(tag, attrs = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (name.startsWith("on") && typeof value === "function") {
      element.addEventListener(name.slice(2), value);
    } else if (name === "class") {
      element.className = value;
    } else if (name === "text") {
      element.textContent = value;
    } else if (name === "dataset") {
      Object.assign(element.dataset, value);
    } else if (value === true) {
      element.setAttribute(name, "");
    } else {
      element.setAttribute(name, String(value));
    }
  }
  append(element, children);
  return element;
}

function append(element, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    element.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

/** Replace every child of `element`. */
export function fill(element, ...children) {
  element.replaceChildren();
  append(element, children);
  return element;
}

/** Toned segments (eventline.js) as spans. */
export function segments(list) {
  const fragment = document.createDocumentFragment();
  for (const segment of list) {
    if (!segment.tone) {
      fragment.append(document.createTextNode(segment.text));
    } else {
      const span = document.createElement("span");
      span.className = `t-${segment.tone}`;
      span.textContent = segment.text;
      fragment.append(span);
    }
  }
  return fragment;
}

export function $(selector, root = document) {
  return root.querySelector(selector);
}

/** A value from the palette in force. */
export function token(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(`--${name}`).trim();
}

/** Save `text` as a file named `name`. */
export function download(name, text, type) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = h("a", { href: url, download: name });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** A dialog's frame: head with title and close, body, optional foot. */
export function dialogFrame(dialog, title, body, foot) {
  const titleId = `${dialog.id}-title`;
  fill(
    dialog,
    h("div", { class: "dialog-head" },
      h("h2", { id: titleId, text: title }),
      h("button", { type: "button", class: "icon", "aria-label": "Close", onclick: () => dialog.close(), text: "✕" })),
    h("div", { class: "dialog-body" }, body),
    foot ? h("div", { class: "dialog-foot" }, foot) : null,
  );
}
