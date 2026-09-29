// Progressive enhancement for the masthead's language menu. A <details> opens
// and closes on its own — with the keyboard too — so nothing here is required
// to switch languages. What the element does not do is close when the reader
// clicks somewhere else, or when they press Escape, and a disclosure that stays
// open behind a click is a nuisance. Ten lines, no dependencies.

const menu = document.querySelector("details.lang");

if (menu) {
  const summary = menu.querySelector("summary");

  document.addEventListener("pointerdown", (event) => {
    if (menu.open && !menu.contains(event.target)) menu.open = false;
  });

  menu.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !menu.open) return;
    menu.open = false;
    summary.focus();
  });
}
