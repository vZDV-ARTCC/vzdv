const test = require("node:test");
const assert = require("node:assert/strict");
const { applySnapshot, startPolling, POLL_INTERVAL } = require("../../static/ids_page.js");

class Element {
  constructor(attributes = {}) {
    this.attributes = { ...attributes };
    this.dataset = {};
    this.children = [];
    this.style = { backgroundColor: "" };
    this.hidden = false;
    this.className = "";
    this.textWrites = 0;
    this.appends = 0;
    this._text = "";
  }

  get textContent() {
    return this._text + this.children.map((child) => child.textContent).join("");
  }

  set textContent(value) {
    this.textWrites++;
    this._text = value;
    this.children = [];
  }

  getAttribute(name) {
    return this.attributes[name] ?? null;
  }

  setAttribute(name, value) {
    this.attributes[name] = value;
  }

  appendChild(child) {
    child.remove();
    this.children.push(child);
    child.parentNode = this;
    this.appends++;
    return child;
  }

  remove() {
    if (this.parentNode) {
      const siblings = this.parentNode.children;
      siblings.splice(siblings.indexOf(this), 1);
      this.parentNode = null;
    }
  }

  querySelector(selector) {
    for (const child of this.children) {
      if (selector.startsWith("#") && child.attributes.id === selector.slice(1)) return child;
      if (selector === `[data-field="${child.attributes["data-field"]}"]`) return child;
      const descendant = child.querySelector(selector);
      if (descendant) return descendant;
    }
    return null;
  }

  cloneNode() {
    const clone = new Element(this.attributes);
    clone.dataset = { ...this.dataset };
    clone.style = { ...this.style };
    clone.hidden = this.hidden;
    clone.className = this.className;
    clone._text = this._text;
    for (const child of this.children) clone.appendChild(child.cloneNode(true));
    return clone;
  }
}

const OVERVIEW_FIELDS = ["airport-link", "dep_rwys", "dep_source", "arr_rwys", "arr_source", "split", "dep_name",
  "arr_name", "flow_name", "suggestion", "issue", "atis_info", "conditions", "wind", "altimeter", "raw_metar"];
const SECTION_FIELDS = {
  dep: ["direction", "rwy", "gates"],
  arr: ["rwy", "gates"],
  atis: ["label", "letter", "preset", "received", "airport_conditions", "notams", "text_atis"],
};
const SUMMARY_IDS = ["ids-issue", "ids-dep-rwys", "ids-dep-source", "ids-dep-name", "ids-arr-rwys", "ids-arr-source",
  "ids-arr-name", "ids-suggestion", "ids-suggested-flow", "ids-conditions", "ids-weather", "ids-metar"];

function addSection(page, name, fields, { table = true } = {}) {
  const ids = [`ids-${name}-rows`, `ids-${name}-empty`, `ids-${name}-template`];
  if (table) ids.push(`ids-${name}-table`);
  for (const id of ids) page.appendChild(new Element({ id }));
  const prototype = new Element();
  for (const name of fields) prototype.appendChild(new Element({ "data-field": name }));
  page.querySelector(`#ids-${name}-template`).content = { firstElementChild: prototype };
}

// A fake IDS page. Airport pages for combined airports have no corridor or gate tables.
function fixture(mode = "overview", { warning = "", split = true } = {}) {
  const page = new Element();
  page.dataset = { mode, url: mode === "overview" ? "/ids/data" : "/ids/KDEN/data", warning };
  const listeners = new Map();
  page.ownerDocument = {
    hidden: false,
    addEventListener: (name, callback) => listeners.set(name, callback),
    removeEventListener: (name) => listeners.delete(name),
    setHidden(hidden) {
      this.hidden = hidden;
      listeners.get("visibilitychange")?.();
    },
  };
  for (const id of ["ids-status", "ids-warning", "ids-updated"]) page.appendChild(new Element({ id }));
  if (mode === "overview") {
    addSection(page, "airports", OVERVIEW_FIELDS);
  } else {
    for (const id of SUMMARY_IDS) page.appendChild(new Element({ id }));
    if (split) {
      addSection(page, "dep", SECTION_FIELDS.dep);
      addSection(page, "arr", SECTION_FIELDS.arr);
    }
    addSection(page, "atis", SECTION_FIELDS.atis, { table: false });
  }
  if (warning) page.querySelector("#ids-warning").textContent = `${warning} Displaying last available data; it may be stale.`;
  return page;
}

function airport(icao = "KDEN", changes = {}) {
  return {
    icao, dep_rwys: "25", arr_rwys: "26", dep_name: "WEST", arr_name: "WEST", dep_source: "atis",
    arr_source: "weather", match_icao: null, suggestion: null, is_split: true, atis_info: "DEP A 1453Z",
    conditions: "VFR", wind: "270@10", altimeter: "30.01", raw_metar: "KDEN 27010KT", issue: null,
    ...changes,
  };
}

function snapshot(rows = [airport()], changes = {}) {
  return { updated_at: "2026-09-05T12:00:00Z", warning: null, rows, ...changes };
}

function atis(atis_type = "departure", changes = {}) {
  return {
    atis_type, label: "Departure ATIS", letter: "A", preset: "WEST", received: "1453Z", age: "just now",
    airport_conditions: "", notams: "", text_atis: "", ...changes,
  };
}

function detailSnapshot(detailChanges = {}, changes = {}) {
  return {
    updated_at: "2026-09-05T12:00:00Z", warning: null,
    detail: {
      ...airport("KDEN"), suggestion: "D WEST / A WEST", suggestion_differs: false,
      dep_rows: [], arr_rows: [], atis: [], ...detailChanges,
    },
    ...changes,
  };
}

function field(row, name) {
  return row.querySelector(`[data-field="${name}"]`);
}

function textWrites(element) {
  return element.textWrites + element.children.reduce((sum, child) => sum + textWrites(child), 0);
}

function environment(fetch) {
  const timers = new Map();
  let id = 0;
  let maxTimers = 0;
  return {
    fetch,
    AbortController,
    timers,
    get maxTimers() { return maxTimers; },
    setTimeout(callback, delay) {
      timers.set(++id, { callback, delay });
      maxTimers = Math.max(maxTimers, timers.size);
      return id;
    },
    clearTimeout(timer) { timers.delete(timer); },
    async tick(delay) {
      const entry = Array.from(timers).find(([, timer]) => timer.delay === delay);
      assert.ok(entry, `Expected a ${delay}ms timer`);
      timers.delete(entry[0]);
      await entry[1].callback();
    },
  };
}

function response(data, status = 200) {
  return { status, ok: status >= 200 && status < 300, json: async () => data };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

test("unchanged snapshots retain rows, cells, selected text, and user-sorted order without text writes", () => {
  const page = fixture();
  const data = snapshot([airport("KDEN"), airport("KAPA", { is_split: false })]);
  applySnapshot(page, data);
  const body = page.querySelector("#ids-airports-rows");
  const [den, apa] = body.children;
  const selected = field(den, "raw_metar");
  body.appendChild(den);
  const appends = body.appends;
  const writes = textWrites(page);
  applySnapshot(page, data);
  assert.deepEqual(body.children, [apa, den]);
  assert.equal(field(den, "raw_metar"), selected);
  assert.equal(textWrites(page), writes);
  assert.equal(body.appends, appends);
});

test("overview switches split flow, sources, issues, conditions, and membership in place", () => {
  const page = fixture();
  applySnapshot(page, snapshot([
    airport("KDEN", { issue: "No METAR available", conditions: null, arr_rwys: "", arr_name: null, arr_source: null }),
    airport("KAPA"),
  ]));
  const body = page.querySelector("#ids-airports-rows");
  const den = body.children[0];
  const badge = field(den, "conditions");
  const split = field(den, "split");
  assert.equal(field(den, "issue").hidden, false);
  assert.equal(field(den, "issue").textContent, "No METAR available");
  assert.equal(field(den, "arr_rwys").textContent, "—");
  assert.equal(field(den, "arr_source").hidden, true);
  assert.equal(field(den, "dep_source").textContent, "ATIS");
  assert.equal(split.hidden, false);
  assert.equal(field(den, "arr_name").textContent, "ARR: —");

  applySnapshot(page, snapshot([
    airport("KDEN", { conditions: "LIFR", dep_name: "NORTH", suggestion: "D SOUTH / A SOUTH" }),
    airport("KCOS"),
  ]));
  assert.equal(body.children[0], den);
  assert.deepEqual(body.children.map((row) => row.dataset.key), ["KDEN", "KCOS"]);
  assert.equal(field(den, "issue").hidden, true);
  assert.equal(field(den, "suggestion").textContent, "Suggested: D SOUTH / A SOUTH");
  assert.equal(field(den, "suggestion").hidden, false);
  assert.equal(field(den, "conditions"), badge);
  assert.equal(badge.style.backgroundColor, "purple");
  assert.equal(field(den, "dep_name").textContent, "DEP: NORTH");
  assert.equal(field(den, "arr_source").textContent, "EST");

  applySnapshot(page, snapshot([airport("KDEN", {
    is_split: false, conditions: "IFR", dep_source: "matched", arr_source: "matched", match_icao: "KDEN",
  })]));
  assert.equal(split.hidden, true);
  assert.equal(field(den, "dep_name").textContent, "");
  assert.equal(field(den, "flow_name").textContent, "WEST");
  assert.equal(field(den, "suggestion").hidden, true);
  assert.equal(field(den, "airport-link").textContent, "KDEN");
  assert.equal(field(den, "dep_source").textContent, "MATCH");
  assert.equal(field(den, "dep_source").getAttribute("title"), "Matched to KDEN's flow");
  assert.equal(badge.style.backgroundColor, "");
  assert.equal(badge.className, "badge rounded-pill text-bg-danger");

  applySnapshot(page, snapshot([]));
  assert.equal(body.children.length, 0);
  assert.equal(page.querySelector("#ids-airports-table").hidden, true);
  assert.equal(page.querySelector("#ids-airports-empty").hidden, false);
});

test("detail sections keep row identity across corridor, gate, and ATIS changes", () => {
  const page = fixture("airport");
  applySnapshot(page, detailSnapshot({ issue: "No METAR available", suggestion: null, wind: null, altimeter: null, raw_metar: null }));
  assert.equal(page.querySelector("#ids-issue").hidden, false);
  assert.equal(page.querySelector("#ids-suggestion").hidden, true);
  assert.equal(page.querySelector("#ids-weather").textContent, "No METAR");
  assert.equal(page.querySelector("#ids-metar").hidden, true);
  assert.equal(page.querySelector("#ids-dep-empty").hidden, false);
  assert.equal(page.querySelector("#ids-atis-empty").hidden, false);

  const north = { corridor: "north", direction: "N", rwy: "34L", gates: ["BAYLR"] };
  const south = { corridor: "south", direction: "S", rwy: "16R", gates: [] };
  const arrivals = [{ rwy: "35L", gates: ["LANDR"] }, { rwy: "35R", gates: [] }];
  applySnapshot(page, detailSnapshot({
    dep_rows: [north, south], arr_rows: arrivals,
    atis: [atis("departure", { airport_conditions: "DEPG RWY 34L." }), atis("arrival", { label: "Arrival ATIS", letter: "N" })],
  }));
  const depBody = page.querySelector("#ids-dep-rows");
  const arrBody = page.querySelector("#ids-arr-rows");
  const atisBody = page.querySelector("#ids-atis-rows");
  const depRow = depBody.children[0];
  const runway = field(depRow, "rwy");
  const departureAtis = atisBody.children[0];
  assert.equal(page.querySelector("#ids-issue").hidden, true);
  assert.equal(page.querySelector("#ids-weather").textContent, "270@10 · 30.01");
  assert.equal(page.querySelector("#ids-dep-source").textContent, "ATIS");
  assert.equal(page.querySelector("#ids-arr-source").textContent, "EST");
  assert.equal(field(depBody.children[1], "gates").textContent, "No gates assigned");
  assert.deepEqual(arrBody.children.map((row) => row.dataset.key), ["35L", "35R"]);
  assert.equal(field(arrBody.children[0], "gates").textContent, "LANDR");
  assert.equal(field(departureAtis, "received").textContent, "Received 1453Z (just now)");
  assert.equal(field(departureAtis, "airport_conditions").hidden, false);
  assert.equal(field(departureAtis, "notams").hidden, true);

  applySnapshot(page, detailSnapshot({
    dep_name: "EAST", suggestion: "D WEST / A WEST", suggestion_differs: true,
    dep_rows: [{ ...north, rwy: "25", gates: ["BAYLR", "COORZ"] }, { ...south, corridor: "east" }],
    arr_rows: [arrivals[1]],
    atis: [atis("departure", { letter: "B", age: "2 min ago" })],
  }));
  assert.equal(depBody.children[0], depRow);
  assert.equal(field(depRow, "rwy"), runway);
  assert.equal(runway.textContent, "25");
  assert.equal(field(depRow, "gates").textContent, "BAYLR COORZ");
  assert.deepEqual(depBody.children.map((child) => child.dataset.key), ["north", "east"]);
  assert.deepEqual(arrBody.children.map((child) => child.dataset.key), ["35R"]);
  assert.equal(atisBody.children[0], departureAtis);
  assert.equal(atisBody.children.length, 1);
  assert.equal(field(departureAtis, "letter").textContent, "B");
  assert.equal(page.querySelector("#ids-dep-name").textContent, "EAST");
  assert.equal(page.querySelector("#ids-suggested-flow").className, "badge text-bg-warning");

  const writes = textWrites(page);
  applySnapshot(page, detailSnapshot({
    dep_name: "EAST", suggestion: "D WEST / A WEST", suggestion_differs: true,
    dep_rows: [{ ...north, rwy: "25", gates: ["BAYLR", "COORZ"] }, { ...south, corridor: "east" }],
    arr_rows: [arrivals[1]],
    atis: [atis("departure", { letter: "B", age: "2 min ago" })],
  }));
  assert.equal(textWrites(page), writes);
});

test("combined airport pages render without corridor or gate tables", () => {
  const page = fixture("airport", { split: false });
  applySnapshot(page, detailSnapshot({
    icao: "KAPA", is_split: false, dep_rwys: "17L, 17R", dep_source: "matched", match_icao: "KDEN",
    atis: [atis("combined", { label: "ATIS" })],
  }));
  assert.equal(page.querySelector("#ids-dep-rwys").textContent, "17L, 17R");
  assert.equal(page.querySelector("#ids-dep-source").getAttribute("title"), "Matched to KDEN's flow");
  assert.equal(page.querySelector("#ids-atis-rows").children.length, 1);
});

test("server strings are literal text, not markup or executable URLs", () => {
  const page = fixture();
  const attack = '<img src=x onerror="alert(1)">';
  applySnapshot(page, snapshot([airport(attack, {
    issue: attack, raw_metar: attack, conditions: "__proto__", dep_source: "__proto__",
  })]));
  const row = page.querySelector("#ids-airports-rows").children[0];
  for (const name of ["airport-link", "issue", "raw_metar"]) {
    assert.equal(field(row, name).textContent, attack);
    assert.equal(field(row, name).children.length, 0);
  }
  assert.equal(field(row, "airport-link").getAttribute("href"), `/ids/${encodeURIComponent(attack)}`);
  assert.equal(field(row, "conditions").textContent, "—");
  assert.equal(field(row, "dep_source").hidden, true);
  const detail = fixture("airport");
  applySnapshot(detail, detailSnapshot({
    dep_rows: [{ corridor: attack, direction: attack, rwy: attack, gates: [attack] }],
    atis: [atis(attack, { text_atis: attack })],
  }));
  assert.equal(field(detail.querySelector("#ids-dep-rows").children[0], "gates").textContent, attack);
  assert.equal(field(detail.querySelector("#ids-atis-rows").children[0], "text_atis").textContent, attack);
});

test("polling waits 30 seconds, retains stale data on failure, and clears warnings after recovery", async () => {
  assert.equal(POLL_INTERVAL, 30000);
  const page = fixture("overview", { warning: "Weather source unavailable." });
  applySnapshot(page, snapshot());
  const body = page.querySelector("#ids-airports-rows");
  const row = body.children[0];
  const timestamp = page.querySelector("#ids-updated").textContent;
  assert.equal(timestamp, "12:00:00Z");
  const env = environment(async () => response(null, 503));
  const polling = startPolling(page, env);
  assert.equal(env.timers.size, 1);
  await env.tick(POLL_INTERVAL);
  assert.equal(body.children[0], row);
  assert.equal(page.querySelector("#ids-updated").textContent, timestamp);
  assert.match(page.querySelector("#ids-warning").textContent, /Weather source unavailable.*refresh failed.*stale/);
  const writes = textWrites(page);
  await env.tick(POLL_INTERVAL);
  assert.equal(textWrites(page), writes);
  env.fetch = async (url, options) => {
    assert.equal(url, "/ids/data");
    assert.equal(options.credentials, "same-origin");
    assert.equal(options.cache, "no-store");
    assert.equal(options.headers.Accept, "application/json");
    return response(snapshot([airport("KDEN", { wind: "180@5" })], { updated_at: "2026-09-05T12:02:00Z" }));
  };
  await env.tick(POLL_INTERVAL);
  assert.equal(body.children[0], row);
  assert.equal(field(row, "wind").textContent, "180@5");
  assert.equal(page.querySelector("#ids-warning").hidden, true);
  assert.equal(page.querySelector("#ids-status").className, "text-muted");
  assert.equal(page.querySelector("#ids-updated").textContent, "12:02:00Z");
  assert.equal(page.querySelector("#ids-updated").getAttribute("datetime"), "2026-09-05T12:02:00Z");
  assert.equal(env.maxTimers, 1);
  polling.stop();
  assert.equal(env.timers.size, 0);
});

for (const status of [401, 403]) {
  test(`HTTP ${status} stops polling and preserves data with an access/session warning`, async () => {
    const page = fixture();
    applySnapshot(page, snapshot());
    const row = page.querySelector("#ids-airports-rows").children[0];
    let requests = 0;
    const env = environment(async () => { requests++; return response(null, status); });
    const polling = startPolling(page, env);
    await polling.refresh();
    assert.equal(page.querySelector("#ids-airports-rows").children[0], row);
    assert.match(page.querySelector("#ids-warning").textContent, /access or session expired.*Automatic updates stopped/);
    assert.equal(env.timers.size, 0);
    page.ownerDocument.setHidden(true);
    page.ownerDocument.setHidden(false);
    await polling.refresh();
    assert.equal(requests, 1);
  });
}

test("visibility pauses polling and returning visible refreshes without overlapping requests", async () => {
  const page = fixture();
  applySnapshot(page, snapshot());
  let complete;
  let requests = 0;
  const env = environment(() => {
    requests++;
    return new Promise((resolve) => { complete = resolve; });
  });
  const polling = startPolling(page, env);
  page.ownerDocument.setHidden(true);
  assert.equal(env.timers.size, 0);
  await polling.refresh();
  assert.equal(requests, 0);
  page.ownerDocument.setHidden(false);
  assert.equal(requests, 1);
  await polling.refresh();
  page.ownerDocument.setHidden(true);
  page.ownerDocument.setHidden(false);
  assert.equal(requests, 1);
  assert.equal(env.timers.size, 1);
  complete(response(snapshot()));
  await settle();
  assert.equal(env.timers.size, 1);
  assert.equal(env.maxTimers, 1);
  polling.stop();
});

test("request and JSON-body timeout aborts and retries with one timer", async () => {
  for (const bodyStalls of [false, true]) {
    const page = fixture();
    applySnapshot(page, snapshot());
    let signal;
    const env = environment((url, options) => {
      signal = options.signal;
      const stalled = new Promise(() => {});
      return bodyStalls ? Promise.resolve({ ok: true, status: 200, json: () => stalled }) : stalled;
    });
    const polling = startPolling(page, env);
    const pending = polling.refresh();
    await env.tick(15000);
    await pending;
    assert.equal(signal.aborted, true);
    assert.match(page.querySelector("#ids-warning").textContent, /refresh failed/);
    assert.equal(env.timers.size, 1);
    assert.equal(env.maxTimers, 1);
    env.fetch = async () => response(snapshot());
    await env.tick(POLL_INTERVAL);
    assert.equal(page.querySelector("#ids-warning").hidden, true);
    polling.stop();
  }
});

test("network errors and malformed snapshots leave the last DOM and timestamp intact", async () => {
  for (const fetch of [
    async () => { throw new Error("offline"); },
    async () => ({ ok: true, status: 200, json: async () => { throw new SyntaxError("invalid JSON"); } }),
    async () => response(snapshot([airport(), airport()])),
    async () => response(snapshot([airport()], { updated_at: "invalid" })),
    async () => response(snapshot([airport("KDEN", { dep_source: 5 })])),
    async () => response({ updated_at: "2026-09-05T12:00:00Z", warning: null, rows: null }),
  ]) {
    const page = fixture();
    applySnapshot(page, snapshot());
    const row = page.querySelector("#ids-airports-rows").children[0];
    const timestamp = page.querySelector("#ids-updated").textContent;
    const env = environment(fetch);
    const polling = startPolling(page, env);
    await polling.refresh();
    assert.equal(page.querySelector("#ids-airports-rows").children[0], row);
    assert.equal(page.querySelector("#ids-updated").textContent, timestamp);
    assert.match(page.querySelector("#ids-warning").textContent, /refresh failed/);
    assert.equal(env.timers.size, 1);
    polling.stop();
  }
  for (const bad of [
    detailSnapshot({ atis: null }),
    detailSnapshot({ atis: [atis(), atis()] }),
    detailSnapshot({ arr_rows: [{ rwy: "35L", gates: "LANDR" }] }),
    detailSnapshot({ suggestion_differs: "yes" }),
  ]) {
    assert.throws(() => applySnapshot(fixture("airport"), bad), /Invalid IDS/);
  }
});

test("successful server stale warnings remain visible and unchanged polls do not announce again", async () => {
  const page = fixture();
  applySnapshot(page, snapshot());
  const env = environment(async () => response(snapshot([airport()], { warning: "Cached weather." })));
  const polling = startPolling(page, env);
  await polling.refresh();
  const warning = page.querySelector("#ids-warning");
  assert.match(warning.textContent, /Cached weather.*stale/);
  assert.equal(warning.hidden, false);
  const writes = textWrites(page);
  await polling.refresh();
  assert.equal(textWrites(page), writes);
  polling.stop();
});
