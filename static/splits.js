const config = JSON.parse(document.getElementById("splits-config").textContent);
const geojsonUrl = document.getElementById("geojson-url").textContent.trim();
const currentSplitName = document
  .getElementById("current-split")
  .textContent.trim();

const PALETTE = [
  "#1f77b4",
  "#ff7f0e",
  "#2ca02c",
  "#d62728",
  "#9467bd",
  "#8c564b",
  "#e377c2",
  "#bcbd22",
  "#17becf",
];

const map = L.map("split-map").setView([39.0, -105.0], 6);
L.tileLayer("https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png", {
  attribution:
    '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors &copy; <a href="https://carto.com/attributions">CARTO</a>',
  subdomains: "abcd",
  maxZoom: 20,
}).addTo(map);

let geojsonData = null;
let layers = {};
let labelLayers = {};
let bandColorMaps = {};
let activeColorMap = {};
let activeSplit = currentSplitName;
let customSplit = null;
let showLow = false;
const bandsVisible = { high: true, low: false };
const customModalEl = document.getElementById("customSplitModal");
let customModal = null;
function getCustomModal() {
  if (!customModalEl) return null;
  if (!customModal) customModal = new bootstrap.Modal(customModalEl);
  return customModal;
}

function getBand(feature) {
  const p = feature.properties;
  if (p.alt_hi <= 260) return "low";
  if (p.alt_lo >= 270) return "high";
  return null;
}

function getBandRaw(split, band) {
  return Object.keys(split[band] || {}).length
    ? split[band]
    : band === "low" && Object.keys(split.high || {}).length
      ? split.high
      : {};
}

function getActiveSplit() {
  if (activeSplit === "custom" && customSplit) {
    return customSplit;
  }
  return config.splits[activeSplit];
}

function resolveAssignments(split, band) {
  const raw = getBandRaw(split, band);
  const assignments = {};
  for (const [controllerId, entries] of Object.entries(raw)) {
    for (const entry of entries) {
      if (typeof entry === "string" && config.areas[entry]) {
        for (const sectorId of config.areas[entry]) {
          assignments[sectorId] = controllerId;
        }
      } else if (typeof entry === "number") {
        assignments[entry] = controllerId;
      }
    }
  }
  return assignments;
}

function getControllers(split) {
  const ids = new Set();
  for (const band of ["high", "low"]) {
    const raw = Object.keys(split[band] || {}).length
      ? split[band]
      : band === "low" && Object.keys(split.high || {}).length
        ? split.high
        : {};
    for (const id of Object.keys(raw)) ids.add(id);
  }
  return Array.from(ids).sort((a, b) => parseInt(a, 10) - parseInt(b, 10));
}

function buildLegend(controllers) {
  const legend = document.getElementById("legend");
  legend.innerHTML = "";
  const heading = document.createElement("h6");
  heading.className = "text-secondary";
  heading.textContent = "Controller assignments";
  legend.appendChild(heading);
  const list = document.createElement("div");
  list.className = "d-flex flex-wrap gap-3";

  for (const id of controllers) {
    const freq = config.frequencies[id];
    const label = freq
      ? `${id} — ${freq.name} ${freq.freq.toFixed(3)}`
      : `Controller ${id}`;
    const item = document.createElement("div");
    item.innerHTML = `<span class="legend-swatch" style="background-color: ${activeColorMap[id] || "#999"};"></span>${label}`;
    list.appendChild(item);
  }
  legend.appendChild(list);
}

function render() {
  const split = getActiveSplit();
  if (!split) return;

  for (const band of ["high", "low"]) {
    if (layers[band]) map.removeLayer(layers[band]);
    if (labelLayers[band]) map.removeLayer(labelLayers[band]);
  }

  for (const band of ["high", "low"]) {
    const assignments = resolveAssignments(split, band);
    const raw = getBandRaw(split, band);
    const sortedControllerIds = Object.keys(raw).sort(
      (a, b) => parseInt(a, 10) - parseInt(b, 10),
    );
    const colorMap = {};
    sortedControllerIds.forEach((id, i) => {
      colorMap[id] = PALETTE[i % PALETTE.length];
    });
    bandColorMaps[band] = colorMap;

    const layer = L.geoJSON(geojsonData, {
      filter: (feature) => getBand(feature) === band,
      style: (feature) => {
        const controller = assignments[feature.properties.id];
        if (controller && colorMap[controller]) {
          return {
            color: "#ffffff",
            weight: 1,
            fillColor: colorMap[controller],
            fillOpacity: 0.45,
          };
        }
        return {
          color: "#666666",
          weight: 0.5,
          fillColor: "#333333",
          fillOpacity: 0.05,
        };
      },
      onEachFeature: (feature, layer) => {
        const controller = assignments[feature.properties.id];
        const freq = controller && config.frequencies[controller];
        let html = `<strong>Sector ${feature.properties.id}</strong>`;
        if (controller) {
          html += `<br>Controller: ${controller}`;
          if (freq) {
            html += `<br>${freq.name} ${freq.freq.toFixed(3)}`;
          }
        } else {
          html += "<br><em>Not assigned</em>";
        }
        layer.bindTooltip(html);
      },
    });

    const labels = L.layerGroup();
    layer.eachLayer((polygonLayer) => {
      const sectorId = polygonLayer.feature.properties.id;
      const controller = assignments[sectorId];
      if (!controller) return;
      const freq = config.frequencies[controller];
      const html = `<div style="text-align:center;line-height:1.1">${controller}${freq ? `<br><small>${freq.freq.toFixed(3)}</small>` : ""}</div>`;
      const center = polygonLayer.getBounds().getCenter();
      const marker = L.marker(center, {
        icon: L.divIcon({
          className: "sector-label",
          html,
          iconSize: [40, 28],
        }),
        interactive: false,
      });
      labels.addLayer(marker);
    });

    layers[band] = layer;
    labelLayers[band] = labels;
  }

  const hasLowSplit = Object.keys(split.low || {}).length > 0;
  const toggleCol = document.getElementById("band-toggle-col");
  if (toggleCol) toggleCol.classList.toggle("d-none", !hasLowSplit);
  if (!hasLowSplit) showLow = false;

  activeColorMap = bandColorMaps[showLow ? "low" : "high"] || {};
  syncBandToggle(split);
}

function getVisibleControllers(split) {
  const ids = new Set();
  for (const band of ["high", "low"]) {
    if (!bandsVisible[band]) continue;
    const raw = Object.keys(split[band] || {}).length
      ? split[band]
      : band === "low" && Object.keys(split.high || {}).length
        ? split.high
        : {};
    for (const id of Object.keys(raw)) ids.add(id);
  }
  return Array.from(ids).sort((a, b) => parseInt(a, 10) - parseInt(b, 10));
}

function syncBandToggle(split) {
  bandsVisible.high = !showLow;
  bandsVisible.low = showLow;
  activeColorMap = bandColorMaps[showLow ? "low" : "high"] || {};
  document.getElementById("btn-high").classList.toggle("active", !showLow);
  document.getElementById("btn-low").classList.toggle("active", showLow);

  const visibleLayers = [];
  for (const band of ["high", "low"]) {
    const layer = layers[band];
    const labels = labelLayers[band];
    if (!layer) continue;
    if (bandsVisible[band]) {
      layer.addTo(map);
      labels.addTo(map);
      visibleLayers.push(layer);
    } else {
      map.removeLayer(layer);
      map.removeLayer(labels);
    }
  }
  if (visibleLayers.length > 0) {
    map.fitBounds(L.featureGroup(visibleLayers).getBounds().pad(0.05));
  }
}

function updateURL() {
  const url = new URL(window.location.href);
  url.searchParams.set("split", activeSplit);
  if (activeSplit === "custom" && customSplit) {
    url.searchParams.set("config", encodeCustomConfig(customSplit));
  } else {
    url.searchParams.delete("config");
  }
  window.history.replaceState({}, "", url);
}

function ensureCustomOption() {
  const select = document.getElementById("split-select");
  if (!Array.from(select.options).some((o) => o.value === "custom")) {
    const opt = document.createElement("option");
    opt.value = "custom";
    opt.textContent = "Custom";
    select.appendChild(opt);
  }
}

function encodeCustomConfig(split) {
  return btoa(JSON.stringify(split));
}

function decodeCustomConfig(str) {
  try {
    return JSON.parse(atob(str));
  } catch (e) {
    console.error("Invalid custom split config:", e);
    return null;
  }
}

function addCustomRow(band, data) {
  const container =
    band === "high"
      ? document.getElementById("custom-high-rows")
      : document.getElementById("custom-low-rows");
  const row = document.createElement("div");
  row.className = "custom-row mb-2 p-2 border rounded";
  row.innerHTML = `
      <input type="number" class="form-control form-control-sm mb-2 controller-input" placeholder="Controller (e.g. 17)">
      <div class="row g-2 mb-2">
        <div class="col-6">
          <div class="form-label small text-muted mb-1">Areas</div>
          <div class="custom-checkbox-list areas-list"></div>
        </div>
        <div class="col-6">
          <div class="form-label small text-muted mb-1">Sectors</div>
          <div class="custom-checkbox-list sectors-list"></div>
        </div>
      </div>
      <button type="button" class="btn btn-sm btn-link text-danger remove-row">Remove</button>
    `;
  container.appendChild(row);

  function addCheckbox(container, value, text) {
    const label = document.createElement("label");
    label.innerHTML = `<input type="checkbox" value="${value.replace(/"/g, "&quot;")}"> <span>${text.replace(/</g, "&lt;")}</span>`;
    container.appendChild(label);
    return label.querySelector("input");
  }

  const areasList = row.querySelector(".areas-list");
  const sectorsList = row.querySelector(".sectors-list");

  Object.keys(config.areas)
    .sort()
    .forEach((name) => addCheckbox(areasList, name, name));

  Object.keys(config.frequencies)
    .sort((a, b) => parseInt(a, 10) - parseInt(b, 10))
    .forEach((id) => {
      const freq = config.frequencies[id];
      const label = freq ? `${id} - ${freq.name}` : id;
      addCheckbox(sectorsList, id, label);
    });

  if (data) {
    row.querySelector(".controller-input").value = data.controller;
    data.entries.forEach((entry) => {
      const value = String(entry).replace(/"/g, '\\"');
      const checkbox = row.querySelector(
        `input[type="checkbox"][value="${value}"]`,
      );
      if (checkbox) checkbox.checked = true;
    });
  }

  row
    .querySelector(".remove-row")
    .addEventListener("click", () => row.remove());
}

function populateCustomModal(split) {
  const highRows = document.getElementById("custom-high-rows");
  const lowRows = document.getElementById("custom-low-rows");
  highRows.innerHTML = "";
  lowRows.innerHTML = "";

  if (split) {
    for (const [controller, entries] of Object.entries(split.high || {})) {
      addCustomRow("high", { controller, entries });
    }
    for (const [controller, entries] of Object.entries(split.low || {})) {
      addCustomRow("low", { controller, entries });
    }
  }

  if (!highRows.children.length) addCustomRow("high");
  if (!lowRows.children.length) addCustomRow("low");
}

function collectCustomSplit() {
  const buildBand = (band) => {
    const container =
      band === "high"
        ? document.getElementById("custom-high-rows")
        : document.getElementById("custom-low-rows");
    const consolidation = {};
    container.querySelectorAll(".custom-row").forEach((row) => {
      const controller = row.querySelector(".controller-input").value.trim();
      if (!controller) return;
      const entries = [];
      const areas = Array.from(
        row.querySelectorAll(".areas-list input:checked"),
      ).map((cb) => cb.value);
      const sectors = Array.from(
        row.querySelectorAll(".sectors-list input:checked"),
      )
        .map((cb) => parseInt(cb.value, 10))
        .filter((v) => !Number.isNaN(v));
      entries.push(...areas);
      entries.push(...sectors);
      if (entries.length) consolidation[controller] = entries;
    });
    return consolidation;
  };
  return { high: buildBand("high"), low: buildBand("low") };
}

document.getElementById("split-select").addEventListener("change", (e) => {
  activeSplit = e.target.value;
  if (activeSplit !== "custom") {
    customSplit = null;
  }
  updateURL();
  render();
});

const createSplitBtn = document.getElementById("create-split");
if (createSplitBtn) {
  createSplitBtn.addEventListener("click", () => {
    populateCustomModal(getActiveSplit());
    getCustomModal().show();
  });
}

const saveSplitBtn = document.getElementById("save-split");
const saveSplitModalEl = document.getElementById("saveSplitModal");
let saveSplitModal = null;
function getSaveSplitModal() {
  if (!saveSplitModalEl) return null;
  if (!saveSplitModal) saveSplitModal = new bootstrap.Modal(saveSplitModalEl);
  return saveSplitModal;
}
if (saveSplitBtn) {
  saveSplitBtn.addEventListener("click", () => {
    const split =
      activeSplit === "custom" && customSplit
        ? customSplit
        : config.splits[activeSplit];
    if (split) {
      document.getElementById("save-split-config").value =
        JSON.stringify(split);
      getSaveSplitModal().show();
    }
  });
}

const deleteSplitBtn = document.getElementById("delete-split");
if (deleteSplitBtn) {
  deleteSplitBtn.addEventListener("click", () => {
    const name = document.getElementById("split-select").value;
    if (name && confirm(`Delete split "${name}"?`)) {
      document.getElementById("delete-split-name").value = name;
      document.getElementById("delete-split-form").submit();
    }
  });
}

const addHighRowBtn = document.getElementById("add-high-row");
if (addHighRowBtn)
  addHighRowBtn.addEventListener("click", () => addCustomRow("high"));

const addLowRowBtn = document.getElementById("add-low-row");
if (addLowRowBtn)
  addLowRowBtn.addEventListener("click", () => addCustomRow("low"));

const previewBtn = document.getElementById("preview-custom-split");
if (previewBtn) {
  previewBtn.addEventListener("click", () => {
    customSplit = collectCustomSplit();
    activeSplit = "custom";
    ensureCustomOption();
    document.getElementById("split-select").value = "custom";
    updateURL();
    render();
    getCustomModal().hide();
  });
}

document.getElementById("btn-high").addEventListener("click", () => {
  showLow = false;
  syncBandToggle(getActiveSplit());
});
document.getElementById("btn-low").addEventListener("click", () => {
  showLow = true;
  syncBandToggle(getActiveSplit());
});

document.getElementById("copy-link").addEventListener("click", () => {
  const url = new URL(window.location.href);
  url.searchParams.set("split", activeSplit);
  if (activeSplit === "custom" && customSplit) {
    url.searchParams.set("config", encodeCustomConfig(customSplit));
  } else {
    url.searchParams.delete("config");
  }
  navigator.clipboard.writeText(url.toString()).then(() => {
    const btn = document.getElementById("copy-link");
    const original = btn.innerHTML;
    btn.innerHTML = '<i class="bi bi-check2"></i> Copied';
    setTimeout(() => (btn.innerHTML = original), 1500);
  });
});

fetch(geojsonUrl)
  .then((res) => res.json())
  .then((data) => {
    geojsonData = data;
    const urlParams = new URLSearchParams(window.location.search);
    const urlSplit = urlParams.get("split");
    const urlConfig = urlParams.get("config");
    if (urlSplit === "custom" && urlConfig) {
      activeSplit = "custom";
      customSplit = decodeCustomConfig(urlConfig);
      ensureCustomOption();
    } else if (urlSplit && config.splits[urlSplit]) {
      activeSplit = urlSplit;
    }
    document.getElementById("split-select").value = activeSplit;
    render();
  })
  .catch((err) => {
    console.error("Failed to load GeoJSON:", err);
    document.getElementById("split-map").innerHTML =
      '<div class="p-3 text-danger">Could not load sector geometry.</div>';
  });
