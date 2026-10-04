const test = require("node:test");
const assert = require("node:assert/strict");
const { zuluTime, zuluDate, startClock, filterCharts } = require("../../static/ids_tools.js");

class Element {
  constructor(dataset = {}, children = []) {
    this.dataset = dataset;
    this.children = children;
    this.hidden = false;
    this.open = false;
    this.textContent = "";
  }

  querySelectorAll(selector) {
    const key = { "[data-chart-group]": "chartGroup", "[data-chart-name]": "chartName" }[selector];
    return this.children.flatMap((child) => [
      ...(Object.hasOwn(child.dataset, key) ? [child] : []),
      ...child.querySelectorAll(selector),
    ]);
  }

  querySelector(selector) {
    if (selector !== "[data-chart-empty]") return null;
    return this.children.find((child) => Object.hasOwn(child.dataset, "chartEmpty")) ?? null;
  }
}

function chartList() {
  const chart = (name) => new Element({ chartName: name });
  const groups = [
    new Element({ chartGroup: "" }, [chart("BAYLR SIX (RNAV)"), chart("PLAINS TWO")]),
    new Element({ chartGroup: "" }, [chart("ILS OR LOC RWY 16R"), chart("ILS OR LOC RWY 16L"), chart("RNAV (GPS) Y RWY 16R")]),
  ];
  const empty = new Element({ chartEmpty: "" });
  empty.hidden = true;
  return { container: new Element({}, [...groups, empty]), groups, empty };
}

const visibleNames = (group) => group.children.filter((item) => !item.hidden).map((item) => item.dataset.chartName);

test("formats Zulu time and date", () => {
  const date = new Date("2026-01-02T03:04:05.678Z");
  assert.equal(zuluTime(date), "03:04:05Z");
  assert.equal(zuluDate(date), "2026-01-02");
});

test("clock ticks just after each second", () => {
  let now = Date.parse("2026-10-04T23:59:59.250Z");
  const delays = [];
  let pending = null;
  const environment = {
    Date: class extends Date {
      constructor() {
        super(now);
      }
    },
    setTimeout(callback, delay) {
      delays.push(delay);
      pending = callback;
    },
  };
  const clock = new Element();
  const date = new Element();
  startClock(clock, date, environment);
  assert.equal(clock.textContent, "23:59:59Z");
  assert.equal(date.textContent, "2026-10-04");
  assert.equal(delays[0], 750);

  now += 750;
  pending();
  assert.equal(clock.textContent, "00:00:00Z");
  assert.equal(date.textContent, "2026-10-05");
  assert.equal(delays[1], 1000);
});

test("chart filter matches every word and opens groups with matches", () => {
  const { container, groups, empty } = chartList();

  filterCharts(container, "ils 16r");
  assert.deepEqual(visibleNames(groups[1]), ["ILS OR LOC RWY 16R"]);
  assert.equal(groups[0].hidden, true);
  assert.equal(groups[1].hidden, false);
  assert.equal(groups[1].open, true);
  assert.equal(empty.hidden, true);

  filterCharts(container, "  16r   rnav ");
  assert.deepEqual(visibleNames(groups[1]), ["RNAV (GPS) Y RWY 16R"]);

  filterCharts(container, "nothing like this");
  assert.equal(groups[0].hidden, true);
  assert.equal(groups[1].hidden, true);
  assert.equal(empty.hidden, false);

  // clearing shows everything again, collapsed
  filterCharts(container, "");
  assert.deepEqual(visibleNames(groups[0]), ["BAYLR SIX (RNAV)", "PLAINS TWO"]);
  assert.equal(visibleNames(groups[1]).length, 3);
  assert.equal(groups.every((group) => !group.hidden && !group.open), true);
  assert.equal(empty.hidden, true);
});
