(() => {
  const pad = (value) => String(value).padStart(2, "0");

  function zuluTime(date) {
    return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}Z`;
  }

  function zuluDate(date) {
    return `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())}`;
  }

  // Update the clock just after each second ticks over.
  function startClock(clock, dateElement, environment = globalThis) {
    function tick() {
      const now = new environment.Date();
      clock.textContent = zuluTime(now);
      if (dateElement) dateElement.textContent = zuluDate(now);
      environment.setTimeout(tick, 1000 - (now.getTime() % 1000));
    }
    tick();
  }

  // Show the charts whose names contain every word typed, opening the groups with matches.
  function filterCharts(container, query) {
    const words = query.toUpperCase().split(/\s+/).filter(Boolean);
    let total = 0;
    for (const group of container.querySelectorAll("[data-chart-group]")) {
      let matches = 0;
      for (const item of group.querySelectorAll("[data-chart-name]")) {
        const name = item.dataset.chartName.toUpperCase();
        const match = words.every((word) => name.includes(word));
        item.hidden = !match;
        if (match) matches++;
      }
      group.hidden = matches === 0;
      group.open = words.length > 0 && matches > 0;
      total += matches;
    }
    const empty = container.querySelector("[data-chart-empty]");
    if (empty) empty.hidden = total > 0;
  }

  function setUp(document) {
    const clock = document.querySelector("#ids-clock");
    if (clock) startClock(clock, document.querySelector("#ids-date"));

    // lookups explain their own errors, so show those instead of dropping them
    document.addEventListener("htmx:beforeSwap", (event) => {
      if (event.detail.target.closest("#ids-tools") && event.detail.xhr.status >= 400) {
        event.detail.shouldSwap = true;
        event.detail.isError = false;
      }
    });

    document.addEventListener("input", (event) => {
      if (event.target.matches("[data-chart-filter]")) {
        filterCharts(document.querySelector("#ids-charts-result"), event.target.value);
      }
    });

    // the charts button in each airport row
    document.addEventListener("click", (event) => {
      const row = event.target.closest("[data-action='charts']")?.closest("[data-key]");
      const form = document.querySelector("#ids-charts-form");
      if (!row || !form) return;
      form.elements.icao.value = row.dataset.key;
      form.requestSubmit();
      form.scrollIntoView({ block: "nearest", behavior: "smooth" });
    });
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = { zuluTime, zuluDate, startClock, filterCharts };
  } else {
    setUp(document);
  }
})();
