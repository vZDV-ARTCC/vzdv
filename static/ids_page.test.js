const test = require("node:test");
const assert = require("node:assert/strict");
const { applySnapshot, startPolling } = require("./ids_page.js");

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

function fixture(mode = "overview", warning = "") {
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
  const ids = ["ids-status", "ids-warning", "ids-updated", "ids-table", "ids-empty", "ids-rows", "ids-row-template"];
  if (mode === "airport") ids.push("ids-error", "ids-detail", "ids-suggestion", "ids-suggested-flow", "ids-dep-flow");
  for (const id of ids) page.appendChild(new Element({ id }));
  const prototype = new Element();
  const fields = mode === "overview"
    ? ["airport-link", "airport-name", "dep_rwys", "arr_rwys", "error", "split", "dep_name", "arr_name", "flow_name", "atis_info", "conditions", "wind", "altimeter", "raw_metar"]
    : ["direction", "rwy", "gates"];
  for (const field of fields) prototype.appendChild(new Element({ "data-field": field }));
  page.querySelector("#ids-row-template").content = { firstElementChild: prototype };
  if (warning) page.querySelector("#ids-warning").textContent = `${warning} Displaying last available data; it may be stale.`;
  return page;
}

function airport(icao = "KDEN", changes = {}) {
  return {
    icao, dep_rwys: "25", arr_rwys: "26", flow_name: "WEST", dep_name: "WEST",
    arr_name: "WEST", is_split: true, atis_info: "DEPARTURE A", conditions: "VFR",
    wind: "270@10", altimeter: "30.01", raw_metar: "KDEN 27010KT", error: null,
    ...changes,
  };
}

function snapshot(rows = [airport()], changes = {}) {
  return { updated_at: "2026-09-05T12:00:00Z", warning: null, rows, ...changes };
}

function detailSnapshot(dep_rows, changes = {}) {
  return {
    updated_at: "2026-09-05T12:00:00Z", warning: null,
    detail: {
      icao: "KDEN", dep_flow: "WEST", arr_flow: "WEST", suggested_flow: "WEST",
      dep_rows, arr_rows: [], error: null, ...changes,
    },
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
  const body = page.querySelector("#ids-rows");
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

test("overview recovers from errors and switches split flow, conditions, and membership in place", () => {
  const page = fixture();
  applySnapshot(page, snapshot([airport("KDEN", { error: "No METAR", conditions: null }), airport("KAPA")]));
  const body = page.querySelector("#ids-rows");
  const den = body.children[0];
  const badge = field(den, "conditions");
  const split = field(den, "split");
  assert.equal(field(den, "error").hidden, false);
  assert.equal(split.hidden, true);
  applySnapshot(page, snapshot([airport("KDEN", { conditions: "LIFR", dep_name: "NORTH" }), airport("KCOS")]));
  assert.equal(body.children[0], den);
  assert.deepEqual(body.children.map((row) => row.dataset.key), ["KDEN", "KCOS"]);
  assert.equal(field(den, "error").hidden, true);
  assert.equal(field(den, "conditions"), badge);
  assert.equal(badge.style.backgroundColor, "purple");
  assert.equal(split.hidden, false);
  assert.equal(field(den, "dep_name").textContent, "D-ATIS: NORTH");
  applySnapshot(page, snapshot([airport("KDEN", { is_split: false, conditions: "IFR" })]));
  assert.equal(split.hidden, true);
  assert.equal(field(den, "dep_name").textContent, "");
  assert.equal(field(den, "flow_name").textContent, "WEST");
  assert.equal(field(den, "airport-link").hidden, true);
  assert.equal(field(den, "airport-name").textContent, "KDEN");
  assert.equal(badge.style.backgroundColor, "");
  assert.equal(badge.className, "badge rounded-pill text-bg-danger");
  applySnapshot(page, snapshot([]));
  assert.equal(body.children.length, 0);
  assert.equal(page.querySelector("#ids-table").hidden, true);
  assert.equal(page.querySelector("#ids-empty").hidden, false);
});

test("departure corridor identity survives runway changes, empty/error recovery, and key-set changes", () => {
  const page = fixture("airport");
  applySnapshot(page, detailSnapshot([], { error: "No data", suggested_flow: null }));
  const detail = page.querySelector("#ids-detail");
  assert.equal(detail.hidden, true);
  const north = { corridor: "north", direction: "N", rwy: "34L", gates: ["BAYLR"] };
  const south = { corridor: "south", direction: "S", rwy: "16R", gates: [] };
  applySnapshot(page, detailSnapshot([north, south]));
  const body = page.querySelector("#ids-rows");
  const row = body.children[0];
  const runway = field(row, "rwy");
  assert.equal(detail.hidden, false);
  assert.equal(field(body.children[1], "gates").textContent, "No gates assigned");
  applySnapshot(page, detailSnapshot([{ ...north, rwy: "25", gates: ["BAYLR", "COORZ"] }, { ...south, corridor: "east" }], { dep_flow: "EAST", suggested_flow: null }));
  assert.equal(body.children[0], row);
  assert.equal(field(row, "rwy"), runway);
  assert.equal(runway.textContent, "25");
  assert.equal(field(row, "gates").textContent, "BAYLR COORZ");
  assert.deepEqual(body.children.map((child) => child.dataset.key), ["north", "east"]);
  assert.equal(page.querySelector("#ids-dep-flow").textContent, "EAST");
  assert.equal(page.querySelector("#ids-suggestion").hidden, true);
  const writes = textWrites(page);
  applySnapshot(page, detailSnapshot([{ ...north, rwy: "25", gates: ["BAYLR", "COORZ"] }, { ...south, corridor: "east" }], { dep_flow: "EAST", suggested_flow: null }));
  assert.equal(textWrites(page), writes);
  applySnapshot(page, detailSnapshot([]));
  assert.equal(body.children.length, 0);
  assert.equal(page.querySelector("#ids-empty").hidden, false);
});

test("server strings are literal text, not markup or executable URLs", () => {
  const page = fixture();
  const attack = '<img src=x onerror="alert(1)">';
  applySnapshot(page, snapshot([airport(attack, { error: attack, raw_metar: attack, conditions: "__proto__" })]));
  const row = page.querySelector("#ids-rows").children[0];
  for (const name of ["airport-link", "error", "raw_metar"]) {
    assert.equal(field(row, name).textContent, attack);
    assert.equal(field(row, name).children.length, 0);
  }
  assert.equal(field(row, "airport-link").getAttribute("href"), `/ids/${encodeURIComponent(attack)}`);
  assert.equal(field(row, "conditions").textContent, "—");
  const detail = fixture("airport");
  applySnapshot(detail, detailSnapshot([{ corridor: attack, direction: attack, rwy: attack, gates: [attack] }]));
  assert.equal(field(detail.querySelector("#ids-rows").children[0], "gates").textContent, attack);
});

test("polling waits 60 seconds, retains stale data on failure, and clears warnings after recovery", async () => {
  const page = fixture("overview", "Weather source unavailable.");
  applySnapshot(page, snapshot());
  const body = page.querySelector("#ids-rows");
  const row = body.children[0];
  const timestamp = page.querySelector("#ids-updated").textContent;
  const env = environment(async () => response(null, 503));
  const polling = startPolling(page, env);
  assert.equal(env.timers.size, 1);
  await env.tick(60000);
  assert.equal(body.children[0], row);
  assert.equal(page.querySelector("#ids-updated").textContent, timestamp);
  assert.match(page.querySelector("#ids-warning").textContent, /Weather source unavailable.*refresh failed.*stale/);
  const writes = textWrites(page);
  await env.tick(60000);
  assert.equal(textWrites(page), writes);
  env.fetch = async (url, options) => {
    assert.equal(url, "/ids/data");
    assert.equal(options.credentials, "same-origin");
    assert.equal(options.cache, "no-store");
    assert.equal(options.headers.Accept, "application/json");
    return response(snapshot([airport("KDEN", { wind: "180@5" })], { updated_at: "2026-09-05T12:02:00Z" }));
  };
  await env.tick(60000);
  assert.equal(body.children[0], row);
  assert.equal(field(row, "wind").textContent, "180@5");
  assert.equal(page.querySelector("#ids-warning").hidden, true);
  assert.equal(page.querySelector("#ids-status").className, "text-muted");
  assert.equal(page.querySelector("#ids-updated").textContent, "2026-09-05T12:02:00Z");
  assert.equal(env.maxTimers, 1);
  polling.stop();
  assert.equal(env.timers.size, 0);
});

for (const status of [401, 403]) {
  test(`HTTP ${status} stops polling and preserves data with an access/session warning`, async () => {
    const page = fixture();
    applySnapshot(page, snapshot());
    const row = page.querySelector("#ids-rows").children[0];
    let requests = 0;
    const env = environment(async () => { requests++; return response(null, status); });
    const polling = startPolling(page, env);
    await polling.refresh();
    assert.equal(page.querySelector("#ids-rows").children[0], row);
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
    await env.tick(60000);
    assert.equal(page.querySelector("#ids-warning").hidden, true);
    polling.stop();
  }
});

test("network errors and malformed JSON snapshots leave the last DOM and timestamp intact", async () => {
  for (const fetch of [
    async () => { throw new Error("offline"); },
    async () => ({ ok: true, status: 200, json: async () => { throw new SyntaxError("invalid JSON"); } }),
    async () => response(snapshot([airport(), airport()])),
    async () => response(snapshot([airport()], { updated_at: "invalid" })),
    async () => response({ updated_at: "2026-09-05T12:00:00Z", warning: null, rows: null }),
  ]) {
    const page = fixture();
    applySnapshot(page, snapshot());
    const row = page.querySelector("#ids-rows").children[0];
    const timestamp = page.querySelector("#ids-updated").textContent;
    const env = environment(fetch);
    const polling = startPolling(page, env);
    await polling.refresh();
    assert.equal(page.querySelector("#ids-rows").children[0], row);
    assert.equal(page.querySelector("#ids-updated").textContent, timestamp);
    assert.match(page.querySelector("#ids-warning").textContent, /refresh failed/);
    assert.equal(env.timers.size, 1);
    polling.stop();
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
