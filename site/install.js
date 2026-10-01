import { strings } from "./i18n.js?v=5c19a0daadbd";

const S = strings(document.documentElement.lang);

// The command stays selectable without JavaScript or clipboard permission.
for (const block of document.querySelectorAll("[data-install]")) {
  const code = block.querySelector("code");
  const button = block.querySelector("[data-copy]");
  const label = button.querySelector("[data-copy-label]");
  const status = block.querySelector('[role="status"]');
  const originalLabel = label.textContent;
  let resetTimer;
  let copying = false;

  button.hidden = false;
  button.addEventListener("click", async () => {
    if (copying) return;
    copying = true;
    clearTimeout(resetTimer);
    button.setAttribute("aria-busy", "true");
    status.textContent = "";
    status.classList.add("sr-only");
    try {
      await navigator.clipboard.writeText(code.textContent.trim());
      label.textContent = S.copied;
      status.textContent = S.copySuccess;
      resetTimer = setTimeout(() => { label.textContent = originalLabel; }, 2000);
    } catch {
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(code);
      selection?.removeAllRanges();
      selection?.addRange(range);
      label.textContent = originalLabel;
      status.classList.remove("sr-only");
      status.textContent = S.copyError;
    } finally {
      copying = false;
      button.removeAttribute("aria-busy");
    }
  });
}
