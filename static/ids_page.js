(() => {
  const POLL_INTERVAL = 30000;
  const REQUEST_TIMEOUT = 15000;

  const SOURCE_BADGES = {
    atis: { text: "ATIS", className: "badge text-bg-success ms-1", title: "From the online ATIS" },
    matched: { text: "MATCH", className: "badge text-bg-info ms-1", title: "" },
    weather: { text: "EST", className: "badge text-bg-secondary ms-1", title: "Estimated from the current wind and SOP rules" },
  };

  const CONDITION_BADGES = {
    VFR: "badge rounded-pill text-bg-success",
    MVFR: "badge rounded-pill text-bg-info",
    IFR: "badge rounded-pill text-bg-danger",
    LIFR: "badge rounded-pill",
  };

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

  // Show text in an element, hiding it when there's nothing to show.
  function setOptionalText(element, text) {
    setText(element, text || "");
    setHidden(element, !text);
  }

  function zulu(timestamp) {
    const date = new Date(timestamp);
    const pad = (value) => String(value).padStart(2, "0");
    return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}Z`;
  }

  function updateSourceBadge(element, source, matchIcao) {
    const known = Object.hasOwn(SOURCE_BADGES, source);
    const badge = known ? SOURCE_BADGES[source] : null;
    setText(element, known ? badge.text : "");
    setClass(element, known ? badge.className : "badge ms-1");
    setAttribute(element, "title", !known ? "" : source === "matched" ? `Matched to ${matchIcao}'s flow` : badge.title);
    setHidden(element, !known);
  }

  function updateConditions(element, conditions) {
    const known = Object.hasOwn(CONDITION_BADGES, conditions);
    setText(element, known ? conditions : "—");
    setClass(element, known ? CONDITION_BADGES[conditions] : "text-muted");
    const background = conditions === "LIFR" ? "purple" : "";
    if (element.style.backgroundColor !== background) element.style.backgroundColor = background;
  }

  function updateOverviewRow(row, data) {
    const link = field(row, "airport-link");
    setText(link, data.icao);
    setAttribute(link, "href", `/ids/${encodeURIComponent(data.icao)}`);

    for (const key of ["dep_rwys", "arr_rwys", "wind", "altimeter", "raw_metar"]) {
      const element = field(row, key);
      setText(element, data[key] || "—");
      setClass(element, data[key] ? (key === "raw_metar" ? "font-monospace small" : "") : "text-muted");
    }
    updateSourceBadge(field(row, "dep_source"), data.dep_source, data.match_icao);
    updateSourceBadge(field(row, "arr_source"), data.arr_source, data.match_icao);
    setText(field(row, "atis_info"), data.atis_info);

    setHidden(field(row, "split"), !data.is_split);
    setText(field(row, "dep_name"), data.is_split ? `DEP: ${data.dep_name || "—"}` : "");
    setText(field(row, "arr_name"), data.is_split ? `ARR: ${data.arr_name || "—"}` : "");
    setHidden(field(row, "flow_name"), data.is_split);
    setText(field(row, "flow_name"), data.is_split ? "" : data.dep_name || "—");
    setOptionalText(field(row, "suggestion"), data.suggestion && `Suggested: ${data.suggestion}`);
    setOptionalText(field(row, "issue"), data.issue);

    updateConditions(field(row, "conditions"), data.conditions);
  }

  function updateGates(element, gates) {
    setText(element, gates.length ? gates.join(" ") : "No gates assigned");
    setClass(element, gates.length ? "" : "text-muted");
  }

  function updateDepartureRow(row, data) {
    setText(field(row, "direction"), data.direction);
    setText(field(row, "rwy"), data.rwy);
    updateGates(field(row, "gates"), data.gates);
  }

  function updateArrivalRow(row, data) {
    setText(field(row, "rwy"), data.rwy);
    updateGates(field(row, "gates"), data.gates);
  }

  function updateAtisRow(row, data) {
    setText(field(row, "label"), data.label);
    setText(field(row, "letter"), data.letter);
    setText(field(row, "preset"), data.preset);
    setText(field(row, "received"), `Received ${data.received} (${data.age})`);
    setOptionalText(field(row, "airport_conditions"), data.airport_conditions);
    setOptionalText(field(row, "notams"), data.notams);
    setOptionalText(field(row, "text_atis"), data.text_atis);
  }

  function updateDetailSummary(page, detail) {
    const element = (id) => page.querySelector(`#ids-${id}`);
    for (const side of ["dep", "arr"]) {
      setText(element(`${side}-name`), detail[`${side}_name`] || "—");
      setText(element(`${side}-rwys`), detail[`${side}_rwys`] || "—");
      updateSourceBadge(element(`${side}-source`), detail[`${side}_source`], detail.match_icao);
    }
    setHidden(element("suggestion"), !detail.suggestion);
    setText(element("suggested-flow"), detail.suggestion ? `Suggested: ${detail.suggestion}` : "");
    setClass(element("suggested-flow"), detail.suggestion_differs ? "badge text-bg-warning" : "badge text-bg-info");
    setOptionalText(element("issue"), detail.issue);
    updateConditions(element("conditions"), detail.conditions);
    setText(element("weather"), [detail.wind, detail.altimeter].filter(Boolean).join(" · ") || "No METAR");
    setOptionalText(element("metar"), detail.raw_metar);
  }

  const isString = (value) => typeof value === "string";
  const nullableString = (value) => value === null || isString(value);
  const stringArray = (value) => Array.isArray(value) && value.every(isString);

  const FLOW_FIELDS = ["dep_name", "arr_name", "dep_source", "arr_source", "match_icao", "suggestion",
    "conditions", "wind", "altimeter", "raw_metar", "issue"];
  const SHAPES = {
    airport: { strings: ["icao", "dep_rwys", "arr_rwys", "atis_info"], nullable: FLOW_FIELDS, booleans: ["is_split"] },
    detail: { strings: ["icao", "dep_rwys", "arr_rwys"], nullable: FLOW_FIELDS, booleans: ["is_split", "suggestion_differs"] },
    dep: { strings: ["corridor", "direction", "rwy"], arrays: ["gates"] },
    arr: { strings: ["rwy"], arrays: ["gates"] },
    atis: { strings: ["atis_type", "label", "letter", "preset", "received", "age", "airport_conditions", "notams", "text_atis"] },
  };

  function matchesShape(value, shape) {
    return !!value && typeof value === "object" &&
      (shape.strings || []).every((key) => isString(value[key])) &&
      (shape.nullable || []).every((key) => nullableString(value[key])) &&
      (shape.booleans || []).every((key) => typeof value[key] === "boolean") &&
      (shape.arrays || []).every((key) => stringArray(value[key]));
  }

  // Keyed, reconciled lists on each page. Elements are found by id:
  // `#ids-{name}-rows`, `-template`, and optionally `-table` and `-empty`.
  const SECTIONS = {
    overview: [
      { name: "airports", rows: (data) => data.rows, key: "icao", shape: SHAPES.airport, update: updateOverviewRow },
    ],
    airport: [
      { name: "dep", rows: (data) => data.detail.dep_rows, key: "corridor", shape: SHAPES.dep, update: updateDepartureRow },
      { name: "arr", rows: (data) => data.detail.arr_rows, key: "rwy", shape: SHAPES.arr, update: updateArrivalRow },
      { name: "atis", rows: (data) => data.detail.atis, key: "atis_type", shape: SHAPES.atis, update: updateAtisRow },
    ],
  };

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
    if (!data || !isString(data.updated_at) || !Number.isFinite(Date.parse(data.updated_at)) ||
        !nullableString(data.warning) || !Object.hasOwn(SECTIONS, mode)) {
      throw new Error("Invalid IDS snapshot");
    }
    if (mode === "airport" && !matchesShape(data.detail, SHAPES.detail)) {
      throw new Error("Invalid IDS detail");
    }
    for (const section of SECTIONS[mode]) {
      const rows = section.rows(data);
      if (!Array.isArray(rows)) throw new Error("Invalid IDS rows");
      const keys = new Set();
      for (const row of rows) {
        if (!matchesShape(row, section.shape) || !row[section.key] || keys.has(row[section.key])) {
          throw new Error("Invalid IDS row");
        }
        keys.add(row[section.key]);
      }
    }
  }

  function applySnapshot(page, data) {
    const mode = page.dataset.mode;
    validateSnapshot(data, mode);
    for (const section of SECTIONS[mode]) {
      const body = page.querySelector(`#ids-${section.name}-rows`);
      if (!body) continue;
      const rows = section.rows(data);
      const template = page.querySelector(`#ids-${section.name}-template`);
      reconcileRows(body, rows, section.key,
        () => template.content.firstElementChild.cloneNode(true), section.update);
      const table = page.querySelector(`#ids-${section.name}-table`);
      if (table) setHidden(table, rows.length === 0);
      const empty = page.querySelector(`#ids-${section.name}-empty`);
      if (empty) setHidden(empty, rows.length !== 0);
    }
    if (mode === "airport") updateDetailSummary(page, data.detail);
    const timestamp = page.querySelector("#ids-updated");
    setText(timestamp, zulu(data.updated_at));
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
    module.exports = { setText, updateOverviewRow, updateDepartureRow, reconcileRows, applySnapshot, startPolling, POLL_INTERVAL };
  } else {
    const page = document.querySelector("#ids-page");
    if (page) startPolling(page);
  }
})();
