(() => {
  const POLL_INTERVAL = 60000;
  const REQUEST_TIMEOUT = 15000;

  function setText(element, value) {
    const text = String(value ?? "");
    if (element.textContent !== text) element.textContent = text;
  }

  function setHidden(element, hidden) {
    if (element.hidden !== hidden) element.hidden = hidden;
  }

  function setClass(element, className) {
    if (element.className !== className) element.className = className;
  }

  function setAttribute(element, name, value) {
    if (element.getAttribute(name) !== value) element.setAttribute(name, value);
  }

  function field(row, name) {
    return row.querySelector(`[data-field="${name}"]`);
  }

  function updateOverviewRow(row, data) {
    const link = field(row, "airport-link");
    const name = field(row, "airport-name");
    setText(link, data.is_split ? data.icao : "");
    setAttribute(link, "href", `/ids/${encodeURIComponent(data.icao)}`);
    setHidden(link, !data.is_split);
    setText(name, data.is_split ? "" : data.icao);
    setHidden(name, data.is_split);

    for (const key of ["dep_rwys", "arr_rwys", "wind", "altimeter", "raw_metar"]) {
      const element = field(row, key);
      setText(element, data[key] || "—");
      setClass(element, data[key] ? (key === "raw_metar" ? "font-monospace small" : "") : "text-muted");
    }
    setText(field(row, "atis_info"), data.atis_info);
    setText(field(row, "error"), data.error);
    setHidden(field(row, "error"), !data.error);
    const split = !data.error && data.is_split;
    setHidden(field(row, "split"), !split);
    setText(field(row, "dep_name"), split ? `D-ATIS: ${data.dep_name || "—"}` : "");
    setText(field(row, "arr_name"), split ? `A-ATIS: ${data.arr_name || "—"}` : "");
    const combined = !data.error && !data.is_split;
    setHidden(field(row, "flow_name"), !combined);
    setText(field(row, "flow_name"), combined ? data.flow_name || "—" : "");

    const conditions = field(row, "conditions");
    const classes = {
      VFR: "badge rounded-pill text-bg-success",
      MVFR: "badge rounded-pill text-bg-info",
      IFR: "badge rounded-pill text-bg-danger",
      LIFR: "badge rounded-pill",
    };
    const known = Object.hasOwn(classes, data.conditions);
    setText(conditions, known ? data.conditions : "—");
    setClass(conditions, known ? classes[data.conditions] : "text-muted");
    const background = data.conditions === "LIFR" ? "purple" : "";
    if (conditions.style.backgroundColor !== background) conditions.style.backgroundColor = background;
  }

  function updateDepartureRow(row, data) {
    setText(field(row, "direction"), data.direction);
    setText(field(row, "rwy"), data.rwy);
    setText(field(row, "gates"), data.gates.length ? data.gates.join(" ") : "No gates assigned");
    setClass(field(row, "gates"), data.gates.length ? "" : "text-muted");
  }

  function reconcileRows(body, rows, key, createRow, updateRow) {
    const existing = new Map(Array.from(body.children, (row) => [row.dataset.key, row]));
    for (const data of rows) {
      const id = data[key];
      let row = existing.get(id);
      if (row) {
        existing.delete(id);
        updateRow(row, data);
      } else {
        row = createRow();
        row.dataset.key = id;
        updateRow(row, data);
        body.appendChild(row);
      }
    }
    for (const row of existing.values()) row.remove();
  }

  function validateSnapshot(data, mode) {
    if (!data || typeof data.updated_at !== "string" || !Number.isFinite(Date.parse(data.updated_at)) ||
        (data.warning !== null && typeof data.warning !== "string")) {
      throw new Error("Invalid IDS snapshot");
    }
    const overview = mode === "overview";
    const detail = data.detail;
    const nullableString = (value) => value === null || typeof value === "string";
    if (!overview && (!detail || typeof detail.icao !== "string" ||
        ![detail.dep_flow, detail.suggested_flow, detail.error].every(nullableString))) {
      throw new Error("Invalid IDS detail");
    }
    const rows = overview ? data.rows : detail.dep_rows;
    const key = overview ? "icao" : "corridor";
    const keys = new Set();
    if (!Array.isArray(rows)) throw new Error("Invalid IDS rows");
    for (const row of rows) {
      if (!row || typeof row[key] !== "string" || !row[key] || keys.has(row[key])) {
        throw new Error("Invalid IDS row key");
      }
      keys.add(row[key]);
      if (overview) {
        if (typeof row.is_split !== "boolean" ||
            ![row.dep_rwys, row.arr_rwys, row.atis_info].every((value) => typeof value === "string") ||
            ![row.flow_name, row.dep_name, row.arr_name, row.conditions, row.wind,
              row.altimeter, row.raw_metar, row.error].every(nullableString)) {
          throw new Error("Invalid IDS airport row");
        }
      } else if (typeof row.direction !== "string" || typeof row.rwy !== "string" ||
          !Array.isArray(row.gates) || !row.gates.every((gate) => typeof gate === "string")) {
        throw new Error("Invalid IDS departure row");
      }
    }
  }

  function applySnapshot(page, data) {
    const overview = page.dataset.mode === "overview";
    validateSnapshot(data, page.dataset.mode);
    const rows = overview ? data.rows : data.detail.dep_rows;
    const body = page.querySelector("#ids-rows");
    const template = page.querySelector("#ids-row-template");
    reconcileRows(body, rows, overview ? "icao" : "corridor",
      () => template.content.firstElementChild.cloneNode(true),
      overview ? updateOverviewRow : updateDepartureRow);
    setHidden(page.querySelector("#ids-table"), rows.length === 0);
    setHidden(page.querySelector("#ids-empty"), rows.length !== 0);
    if (!overview) {
      const detail = data.detail;
      setText(page.querySelector("#ids-error"), detail.error);
      setHidden(page.querySelector("#ids-error"), !detail.error);
      setHidden(page.querySelector("#ids-detail"), !!detail.error);
      setHidden(page.querySelector("#ids-suggestion"), !detail.suggested_flow);
      setText(page.querySelector("#ids-suggested-flow"), detail.suggested_flow ? `Suggested Flow: ${detail.suggested_flow}` : "");
      setText(page.querySelector("#ids-dep-flow"), detail.dep_flow || "—");
    }
    const timestamp = page.querySelector("#ids-updated");
    setText(timestamp, data.updated_at);
    setAttribute(timestamp, "datetime", data.updated_at);
  }

  function startPolling(page, environment = globalThis) {
    const document = page.ownerDocument;
    let timer = null;
    let controller = null;
    let running = false;
    let stopped = false;
    let serverWarning = page.dataset.warning || "";

    function showWarning(failure = "") {
      const message = [serverWarning, failure].filter(Boolean).join(" ");
      setText(page.querySelector("#ids-warning"), message ? `${message} Displaying last available data; it may be stale.` : "");
      setHidden(page.querySelector("#ids-warning"), !message);
      setClass(page.querySelector("#ids-status"), message ? "text-warning" : "text-muted");
    }

    function clearTimer() {
      if (timer !== null) environment.clearTimeout(timer);
      timer = null;
    }

    function schedule() {
      clearTimer();
      if (!stopped && !document.hidden) timer = environment.setTimeout(refresh, POLL_INTERVAL);
    }

    async function refresh() {
      if (stopped || running || document.hidden) return;
      clearTimer();
      running = true;
      controller = new environment.AbortController();
      try {
        const deadline = new Promise((resolve, reject) => {
          timer = environment.setTimeout(() => {
            reject(new Error("IDS request timed out"));
            controller.abort();
          }, REQUEST_TIMEOUT);
        });
        const request = async () => {
          const response = await environment.fetch(page.dataset.url, {
            headers: { Accept: "application/json" },
            credentials: "same-origin",
            cache: "no-store",
            signal: controller.signal,
          });
          if (response.status === 401 || response.status === 403) return { accessDenied: true };
          if (!response.ok) throw new Error(`IDS request failed: ${response.status}`);
          return { data: await response.json() };
        };
        const result = await Promise.race([request(), deadline]);
        if (stopped) return;
        if (result.accessDenied) {
          stopped = true;
          document.removeEventListener("visibilitychange", visibilityChanged);
          showWarning("IDS access or session expired. Sign in or confirm roster access, then reload. Automatic updates stopped.");
        } else {
          applySnapshot(page, result.data);
          serverWarning = result.data.warning || "";
          showWarning();
        }
      } catch (error) {
        if (!stopped) showWarning("IDS refresh failed. Retrying automatically.");
      } finally {
        clearTimer();
        controller = null;
        running = false;
        schedule();
      }
    }

    function visibilityChanged() {
      if (document.hidden) {
        if (!running) clearTimer();
      } else {
        refresh();
      }
    }

    function stop() {
      stopped = true;
      clearTimer();
      if (controller) controller.abort();
      document.removeEventListener("visibilitychange", visibilityChanged);
    }

    document.addEventListener("visibilitychange", visibilityChanged);
    schedule();
    return { refresh, stop };
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = { setText, updateOverviewRow, updateDepartureRow, reconcileRows, applySnapshot, startPolling };
  } else {
    const page = document.querySelector("#ids-page");
    if (page) startPolling(page);
  }
})();
