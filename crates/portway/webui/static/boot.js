// Runs before the first paint, synchronously, so the page never flashes the
// wrong theme. Everything else waits for the module in js/app.js.
(function () {
  var root = document.documentElement;
  var read = function (key, fallback) {
    try {
      var value = window.localStorage.getItem("portway." + key);
      return value === null ? fallback : value;
    } catch (err) {
      return fallback;
    }
  };
  var choice = read("theme", "system");
  var dark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  var known = /^[a-z0-9-]+$/.test(choice) ? choice : "system";
  root.setAttribute("data-theme-choice", known);
  root.setAttribute("data-theme", known === "system" ? (dark ? "portway-dark" : "portway-light") : known);
  root.setAttribute("data-density", read("density", "comfortable") === "compact" ? "compact" : "comfortable");
  root.setAttribute("data-font", read("font", "mono") === "sans" ? "sans" : "mono");
  root.setAttribute("data-motion", read("motion", "system") === "reduce" ? "reduce" : "system");
})();
