/**
 * Leaflet map bridge used by the WASM module (window.TransitMap).
 *
 * Geometry: WASM prefers leg.geometry [{lat,lon}] from GraphQL; otherwise
 * rebuilds fromStop → intermediateStops → toStop (transit) or walk endpoints.
 * Colors: routeColor hex or MODE_COLORS by mode.
 *
 * Live vehicles:
 * - Journey layer: setLiveVehicles() for selected itinerary trips
 * - Global layer: setGlobalLiveVehicles() for “Réseau en direct” (all VP in viewport)
 * Both interpolate lat/lon over ~1s via requestAnimationFrame.
 */
(function () {
  "use strict";

  const MODE_COLORS = {
    RAIL: "#1d4ed8",
    TRAIN: "#1d4ed8",
    METRO: "#dc2626",
    SUBWAY: "#dc2626",
    TRAM: "#7c3aed",
    BUS: "#ea580c",
    COACH: "#0d9488",
    FERRY: "#0284c8",
    WALK: "#64748b",
    BIKE: "#16a34a",
    BICYCLE: "#16a34a",
    OTHER: "#a855f7",
  };

  /** IDFM line palette when GTFS routeColor is missing (hex without #). */
  const IDFM_LINE_COLORS = {
    A: "EB2132",
    B: "5091CB",
    C: "FFCC30",
    D: "008B5B",
    E: "B94E9A",
    1: "FFBE00",
    2: "0055C8",
    3: "6E6E00",
    4: "A0006E",
    5: "FF7E2E",
    6: "6ECA97",
    7: "FF82B4",
    8: "D282BE",
    9: "B6BD00",
    10: "C9910D",
    11: "704B1C",
    12: "007852",
    13: "6EC4E8",
    14: "640082",
    T1: "0055C8",
    T2: "CF009E",
    T3A: "FF7E2E",
    T3B: "00AE41",
    T4: "F68F4B",
    T5: "6E6E00",
    T6: "E2231A",
    T7: "704B1C",
    T8: "C5A3CD",
    T9: "4CC4F2",
    T10: "6E6E00",
    T11: "F58400",
    T12: "A50034",
    T13: "8D5E2A",
    H: "7B5840",
    J: "CDCD00",
    K: "C5A3CD",
    L: "7575C9",
    N: "00A092",
    P: "F0B600",
    R: "E4B4D0",
    U: "D3403B",
    V: "9F9825",
  };

  const BRAND_COLORS = {
    ouigo: { bg: "#00A0E3", fg: "#ffffff" },
    "tgv-inoui": { bg: "#8D104B", fg: "#ffffff" },
    tgv: { bg: "#ea580c", fg: "#ffffff" },
    transilien: { bg: "#166534", fg: "#ffffff" },
  };

  /** Inline SVG fallbacks when assets/icons/*.svg fail to load. */
  const INLINE_ICONS = {
    train:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="4" y="3" width="16" height="14" rx="2"/><circle cx="8" cy="14" r="1.5"/><circle cx="16" cy="14" r="1.5"/><path d="M6 21h12M8 17l-2 4M16 17l2 4"/></svg>',
    metro:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M8 16V8l4 5 4-5v8"/></svg>',
    tram:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M7 4h10v11H7z"/><path d="M9 19h6M8 15l-2 4M16 15l2 4M12 4V2"/></svg>',
    bus: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="4" y="3" width="16" height="14" rx="2"/><path d="M4 10h16"/><circle cx="8" cy="17" r="1.5"/><circle cx="16" cy="17" r="1.5"/></svg>',
    walk: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="5" r="2"/><path d="M10 22l2-7 2 2 3 5M8 10l4 2 3-4"/></svg>',
    bike: '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="6.5" cy="16" r="3.5"/><circle cx="17.5" cy="16" r="3.5"/><path d="M6.5 16l4-8h3l2 4h3"/><path d="M12 8V5"/></svg>',
    ferry:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 17c2 1 4 1 6 0s4-1 6 0 4 1 6 0"/><path d="M5 17V9l7-4 7 4v8"/></svg>',
    other:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 8v4l3 2"/></svg>',
    origin:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><circle cx="12" cy="12" r="8" fill="#34d399" stroke="#059669" stroke-width="2"/><circle cx="12" cy="12" r="3" fill="#065f46"/></svg>',
    destination:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M12 2C8 2 5 5.2 5 9.2 5 14.5 12 22 12 22s7-7.5 7-12.8C19 5.2 16 2 12 2z" fill="#f87171" stroke="#b91c1c" stroke-width="1.5"/><circle cx="12" cy="9" r="2.5" fill="#fff"/></svg>',
    transfer:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#475569" stroke-width="2"><path d="M7 7h10l-2-2M17 17H7l2 2"/><path d="M17 7v4H7v6"/></svg>',
    vehicle:
      '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"><circle cx="12" cy="12" r="9" fill="currentColor" opacity="0.15"/><circle cx="12" cy="12" r="7" fill="currentColor" opacity="0.25" stroke="currentColor" stroke-width="1.5"/><path fill="currentColor" d="M12 6.5l4.5 8H7.5L12 6.5z"/></svg>',
  };

  const LIVE_ANIM_MS = 1000;

  let map = null;
  let layerGroup = null;
  let liveLayerGroup = null;
  let globalLiveLayerGroup = null;
  /** IDFM network lines (référentiel + traces). */
  let idfmLinesLayer = null;
  let idfmLinesData = null;
  let idfmLinesLoading = null;
  /** @type {Set<string>} */
  let idfmModeFilter = new Set(["metro", "rail", "tram", "bus", "funicular", "cableway"]);
  let idfmShowSoon = true;
  let idfmVisible = true;
  /** When true: hide network lines + all vehicles; only journey geometry stays. */
  let itineraryFocus = false;
  /** User preference for IDFM lines (restored when leaving focus). */
  let idfmUserPref = true;
  let markers = [];
  let lines = [];
  /** @type {Map<string, { marker: L.Marker, lat: number, lon: number, bearing: number|null, anim: number|null }>} */
  let liveById = new Map();
  /** @type {Map<string, { marker: L.Marker, lat: number, lon: number, bearing: number|null, anim: number|null }>} */
  let globalLiveById = new Map();
  let moveEndHandlers = [];

  function modeKey(mode) {
    const m = (mode || "other").toLowerCase();
    if (m === "rail" || m === "train") return "train";
    if (m === "subway") return "metro";
    if (m === "coach") return "bus";
    if (m === "bicycle" || m === "cycling" || m === "velo") return "bike";
    return m;
  }

  function iconHtml(mode, color) {
    const c = color || MODE_COLORS[(mode || "OTHER").toUpperCase()] || "#334155";
    const key = modeKey(mode);
    const src = `assets/icons/${key}.svg`;
    const fallback = INLINE_ICONS[key] || INLINE_ICONS.other;
    return (
      `<div class="map-mode-icon" style="background:${c}">` +
      `<img src="${src}" alt="${mode || ""}" onerror="this.outerHTML=\`${fallback.replace(/`/g, "")}\`"/>` +
      `</div>`
    );
  }

  function pinIcon(kind) {
    const file =
      kind === "origin"
        ? "pin-origin"
        : kind === "destination"
          ? "pin-destination"
          : kind === "transfer"
            ? "transfer"
            : "station";
    const src = `assets/icons/${file}.svg`;
    const fallback = INLINE_ICONS[kind] || INLINE_ICONS.other;
    const html =
      `<div class="map-pin map-pin-${kind}">` +
      `<img src="${src}" alt="${kind}" width="28" height="28" ` +
      `onerror="this.outerHTML=\`${fallback.replace(/`/g, "")}\`"/>` +
      `</div>`;
    return L.divIcon({
      className: "pin-icon",
      html,
      iconSize: [28, 36],
      iconAnchor: [14, 34],
      popupAnchor: [0, -28],
    });
  }

  /** Short badge text for a vehicle (line / train number). Max ~14 chars. */
  function vehicleBadgeText(v) {
    const label = vehicleMapLabel(v);
    if (!label) return "";
    return label.length > 14 ? label.slice(0, 13) + "…" : label;
  }

  /** Human-readable map label (Métro 1, RER B, Ouigo …). */
  function vehicleMapLabel(v) {
    if (!v) return "";
    const display =
      (v.displayLabel || v.display_label || "").trim();
    if (display && !looksCrypticId(display)) return display;

    const mode = (v.mode || "").toUpperCase();
    const route = (v.routeShortName || v.route || "").trim();
    const longName = (v.routeLongName || v.route_long_name || "").trim();
    const blob = `${longName} ${route} ${v.label || ""} ${v.tripShortName || ""}`.toLowerCase();

    if (blob.includes("ouigo")) {
      if (v.tripShortName) return `Ouigo ${v.tripShortName}`;
      if (route) return `Ouigo ${route}`;
      return "Ouigo";
    }
    if (blob.includes("inoui") || blob.includes("in oui")) {
      if (v.tripShortName) return `TGV InOui ${v.tripShortName}`;
      return "TGV InOui";
    }
    if (blob.includes("tgv") && v.tripShortName) return `TGV ${v.tripShortName}`;

    const longUp = longName.toUpperCase();
    if (longUp.includes("RER")) {
      const m = longName.match(/RER\s*([A-E])/i);
      if (m) return `RER ${m[1].toUpperCase()}`;
    }
    if (route.length === 1 && /^[A-E]$/i.test(route) && (mode === "RAIL" || mode === "TRAIN")) {
      return `RER ${route.toUpperCase()}`;
    }
    if (longUp.includes("TRANSILIEN")) {
      const m = longName.match(/TRANSILIEN\s*([HJKLNPRUV])/i);
      if (m) return `Transilien ${m[1].toUpperCase()}`;
      if (route.length === 1) return `Transilien ${route.toUpperCase()}`;
    }
    if (mode === "METRO" || mode === "SUBWAY") {
      if (/^\d{1,2}$/.test(route)) return `Métro ${route}`;
      const m = longName.match(/(?:métro|ligne)\s*(\d{1,2})/i);
      if (m) return `Métro ${m[1]}`;
    }
    if (mode === "TRAM" || mode === "TRAMWAY") {
      const tram = /^T\d/i.test(route) ? route : route ? `T${route}` : "";
      if (tram) return `Tram ${tram}`;
    }
    if ((mode === "BUS" || mode === "COACH") && route && !/^C\d{5}$/i.test(route)) {
      return `Bus ${route}`;
    }

    if (display) return display;
    if (route && v.headsign) {
      const h = String(v.headsign).slice(0, 18);
      return `${route} → ${h}`;
    }
    if (route) return route;
    if (v.tripShortName) return String(v.tripShortName);
    if (v.label) return String(v.label);
    if (v.vehicleId) return String(v.vehicleId);
    return "";
  }

  function looksCrypticId(s) {
    const t = String(s).trim();
    if (!t) return true;
    if (/^C\d{5}$/i.test(t)) return true;
    if (/^(STIF|RATP|IDFM):/i.test(t)) return true;
    if (t.length > 20 && /[:@]/.test(t)) return true;
    return false;
  }

  function detectVehicleProduct(v) {
    const mode = modeKey(v && v.mode);
    const route = (v && (v.routeShortName || v.route) || "").trim();
    const longName = (v && (v.routeLongName || v.route_long_name) || "").trim();
    const blob = `${longName} ${route} ${(v && v.label) || ""} ${(v && v.displayLabel) || ""} ${(v && v.tripShortName) || ""}`.toLowerCase();

    if (blob.includes("ouigo")) return "ouigo";
    if (blob.includes("inoui") || blob.includes("in oui")) return "tgv-inoui";
    if (blob.includes("tgv")) return "tgv";
    if (blob.includes("rer") || (route.length === 1 && /^[a-e]$/i.test(route) && mode === "train")) {
      return "rer";
    }
    if (blob.includes("transilien")) return "transilien";
    if (mode === "metro") return "metro";
    if (mode === "tram") return "tram";
    if (mode === "bus") return "bus";
    if (mode === "ferry") return "ferry";
    if (mode === "train") return "train";
    return mode || "other";
  }

  function vehicleGlyphPath(v) {
    const product = detectVehicleProduct(v);
    const branded = {
      ouigo: "vehicles/ouigo",
      "tgv-inoui": "vehicles/tgv-inoui",
      tgv: "vehicles/tgv",
      rer: "vehicles/rer",
      transilien: "vehicles/transilien",
    };
    if (branded[product]) return `assets/icons/${branded[product]}.svg`;
    const mode = modeKey(v && v.mode);
    return `assets/icons/${mode === "other" ? "train" : mode}.svg`;
  }

  function contrastText(hex) {
    const h = String(hex || "").replace(/^#/, "");
    if (h.length !== 6) return "#ffffff";
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    const lum = 0.299 * r + 0.587 * g + 0.114 * b;
    return lum > 150 ? "#0f172a" : "#ffffff";
  }

  function resolveVehicleColors(v, fallbackColor) {
    const product = detectVehicleProduct(v);
    if (BRAND_COLORS[product]) {
      const b = BRAND_COLORS[product];
      return { bg: b.bg, fg: b.fg };
    }
    const route = (v.routeShortName || v.route || "").trim().toUpperCase();
    const fromGtfs = modeColor(v.mode, v.routeColor || v.route_color);
    let bg = fallbackColor || fromGtfs;
    if (!v.routeColor && !v.route_color && IDFM_LINE_COLORS[route]) {
      bg = `#${IDFM_LINE_COLORS[route]}`;
    }
    return { bg, fg: contrastText(bg) };
  }

  function isEstimatedStatus(status) {
    if (!status) return false;
    const s = String(status).toUpperCase();
    return s.startsWith("ESTIMATED") || s.includes("ESTIMATED_");
  }

  /** Prefer explicit GraphQL `isEstimated` / `positionSource`, then status prefix. */
  function isEstimatedVehicle(v) {
    if (!v) return false;
    if (v.isEstimated === true || v.is_estimated === true) return true;
    if (v.isEstimated === false || v.is_estimated === false) return false;
    const src = v.positionSource || v.position_source;
    if (src != null && String(src).length) {
      return String(src).toUpperCase() === "ESTIMATED";
    }
    return isEstimatedStatus(v.currentStatus || v.current_status);
  }

  /**
   * Live vehicle marker: colored label pill + mode/brand glyph, optional bearing.
   * @param {object} opts - { bearing, color, mode, badge, label, estimated, v }
   */
  function liveVehicleIcon(opts) {
    let bearing = null;
    let color = "#38bdf8";
    let mode = "OTHER";
    let badge = "";
    let label = "";
    let estimated = false;
    let v = null;
    if (opts != null && typeof opts !== "object") {
      bearing = opts;
      color = arguments[1] || color;
    } else if (opts && typeof opts === "object") {
      bearing = opts.bearing;
      color = opts.color || color;
      mode = opts.mode || mode;
      badge = opts.badge != null ? String(opts.badge) : "";
      label = opts.label != null ? String(opts.label) : badge;
      estimated = !!opts.estimated;
      v = opts.v || null;
    }
    const rot =
      bearing != null && Number.isFinite(Number(bearing))
        ? Number(bearing)
        : null;
    const colors = v ? resolveVehicleColors(v, color) : { bg: color, fg: contrastText(color) };
    const c = colors.bg;
    const fg = colors.fg;
    const key = modeKey(mode);
    const src = v ? vehicleGlyphPath(v) : `assets/icons/${key}.svg`;
    const fallback = INLINE_ICONS[key] || INLINE_ICONS.vehicle || INLINE_ICONS.other;
    const estClass = estimated ? " live-vehicle--estimated" : " live-vehicle--gps";
    const rotStyle =
      rot != null ? `transform:rotate(${rot}deg)` : "transform:rotate(0deg)";
    const labelText = label || badge;
    const labelHtml = labelText
      ? `<span class="live-vehicle-label" style="background:${c};color:${fg}" title="${escapeHtml(labelText)}">${escapeHtml(labelText)}</span>`
      : "";
    const approx = estimated
      ? `<span class="live-vehicle-approx" title="Position estimée">≈</span>`
      : "";
    const html =
      `<div class="live-vehicle${estClass}" style="--lv-color:${c};--lv-fg:${fg}" data-mode="${escapeHtml(key)}" data-product="${escapeHtml(detectVehicleProduct(v || { mode }))}">` +
      labelHtml +
      `<div class="live-vehicle-rot" style="${rotStyle}">` +
      `<div class="live-vehicle-body" style="border-color:${c}">` +
      `<img class="live-vehicle-glyph" src="${src}" alt="${escapeHtml(mode || "vehicle")}" width="20" height="20" ` +
      `onerror="this.src='assets/icons/train.svg'"/>` +
      `</div>` +
      `</div>` +
      approx +
      `</div>`;
    const labelLen = labelText ? Math.min(labelText.length, 14) : 0;
    const w = Math.max(52, 8 + labelLen * 6.5);
    return L.divIcon({
      className: "live-vehicle-icon",
      html,
      iconSize: [w, 46],
      iconAnchor: [w / 2, 30],
      popupAnchor: [0, -24],
    });
  }

  function vehicleIconFromData(v, bearing, color) {
    const label = vehicleMapLabel(v);
    return liveVehicleIcon({
      bearing,
      color,
      mode: v.mode || "OTHER",
      badge: vehicleBadgeText(v),
      label,
      estimated: isEstimatedVehicle(v),
      v,
    });
  }

  function formatModeCounts(list) {
    const counts = {};
    list.forEach((v) => {
      const k = modeKey(v.mode || "other");
      counts[k] = (counts[k] || 0) + 1;
    });
    const order = ["metro", "train", "tram", "bus", "ferry", "other"];
    const labels = {
      metro: "Métro",
      train: "Train/RER",
      tram: "Tram",
      bus: "Bus",
      ferry: "Ferry",
      other: "Autre",
    };
    const parts = [];
    order.forEach((k) => {
      if (counts[k]) parts.push(`${labels[k] || k}: ${counts[k]}`);
    });
    Object.keys(counts).forEach((k) => {
      if (!order.includes(k) && counts[k]) {
        parts.push(`${k}: ${counts[k]}`);
      }
    });
    return parts.join(" · ");
  }

  /** Update map badge / panel status with mode breakdown (global layer). */
  function updateGlobalLiveStatusUi(list) {
    const n = list.length;
    const byMode = formatModeCounts(list);
    const span = document.getElementById("map-global-live-count");
    if (span) {
      const head =
        n === 0
          ? "0 véhicules en direct"
          : n === 1
            ? "1 véhicule en direct"
            : `${n} véhicules en direct`;
      span.textContent = byMode ? `${head} — ${byMode}` : head;
    }
    const status = document.getElementById("global-live-status");
    if (status && n > 0) {
      const gps = list.filter((v) => !isEstimatedVehicle(v)).length;
      const est = n - gps;
      const detail = [
        byMode,
        gps ? `${gps} GPS` : null,
        est ? `${est} estimé${est > 1 ? "s" : ""}` : null,
      ]
        .filter(Boolean)
        .join(" · ");
      status.innerHTML = escapeHtml(detail);
      status.removeAttribute("hidden");
    }
  }

  function ensureMap() {
    if (map) return map;
    // IDF default view so network lines are visible immediately
    map = L.map("map", { zoomControl: true, preferCanvas: true }).setView(
      [48.8566, 2.3522],
      11
    );
    L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
      maxZoom: 19,
      attribution:
        '&copy; <a href="https://www.openstreetmap.org/copyright">OSM</a> · IDFM lignes',
    }).addTo(map);
    // Order: basemap → IDFM lines → journey → global live → journey live
    idfmLinesLayer = L.layerGroup().addTo(map);
    layerGroup = L.layerGroup().addTo(map);
    globalLiveLayerGroup = L.layerGroup().addTo(map);
    liveLayerGroup = L.layerGroup().addTo(map);
    map.on("moveend", () => {
      moveEndHandlers.forEach((fn) => {
        try {
          fn();
        } catch (_) {
          /* ignore handler errors */
        }
      });
    });
    if (idfmVisible) {
      setIdfmLinesVisible(true);
    }
    return map;
  }

  function idfmStyle(feature) {
    const p = feature.properties || {};
    const mode = (p.mode || "bus").toLowerCase();
    const soon = !!p.soon || p.status === "prochainement active";
    let weight = 2;
    if (mode === "metro") weight = 4;
    else if (mode === "rail") weight = 3.5;
    else if (mode === "tram") weight = 3;
    else if (mode === "bus") weight = 1.5;
    const color = p.color || MODE_COLORS[mode.toUpperCase()] || "#64748b";
    return {
      color,
      weight,
      opacity: soon ? 0.55 : 0.82,
      dashArray: soon ? "6 8" : null,
      lineCap: "round",
      lineJoin: "round",
    };
  }

  function escapeHtml(s) {
    return String(s || "")
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function idfmPopup(feature) {
    const p = feature.properties || {};
    const soon = p.soon || p.status === "prochainement active";
    const badge = soon
      ? '<span style="color:#fbbf24">Prochainement active</span>'
      : '<span style="color:#34d399">Active</span>';
    return (
      `<div style="min-width:160px">` +
      `<strong style="font-size:1.1em">${escapeHtml(p.short_name || p.name || "")}</strong> ` +
      `<span style="opacity:.8">${escapeHtml(p.mode || "")}</span><br/>` +
      `${escapeHtml(p.name || "")}<br/>` +
      `${badge}<br/>` +
      `<span style="opacity:.7;font-size:.85em">${escapeHtml(p.network || "")}</span>` +
      (p.operator
        ? `<br/><span style="opacity:.65;font-size:.8em">${escapeHtml(p.operator)}</span>`
        : "") +
      `</div>`
    );
  }

  function rebuildIdfmLayer() {
    if (!idfmLinesLayer || !idfmLinesData) return;
    idfmLinesLayer.clearLayers();
    if (!idfmVisible || itineraryFocus) return;
    const layer = L.geoJSON(idfmLinesData, {
      style: idfmStyle,
      filter: (feature) => {
        const p = feature.properties || {};
        const mode = (p.mode || "bus").toLowerCase();
        if (!idfmModeFilter.has(mode)) return false;
        if (!idfmShowSoon && (p.soon || p.status === "prochainement active")) {
          return false;
        }
        return true;
      },
      onEachFeature: (feature, lyr) => {
        lyr.bindPopup(() => idfmPopup(feature));
      },
      renderer: L.canvas({ padding: 0.5 }),
    });
    layer.addTo(idfmLinesLayer);
    const n = layer.getLayers().length;
    const badge = document.getElementById("idfm-lines-count");
    if (badge) badge.textContent = `${n} lignes IDFM`;
  }

  /** Load / toggle IDFM network lines (référentiel + traces). */
  function setIdfmLinesVisible(visible) {
    if (!itineraryFocus) {
      idfmUserPref = !!visible;
    }
    idfmVisible = !!visible;
    ensureMap();
    if (!idfmVisible) {
      if (idfmLinesLayer) idfmLinesLayer.clearLayers();
      const badge = document.getElementById("idfm-lines-count");
      if (badge) badge.textContent = "Lignes IDFM masquées";
      return Promise.resolve({ n: 0 });
    }
    if (idfmLinesData) {
      rebuildIdfmLayer();
      return Promise.resolve({ n: (idfmLinesData.features || []).length });
    }
    if (idfmLinesLoading) return idfmLinesLoading;
    idfmLinesLoading = fetch("assets/idfm/lignes.geojson")
      .then((r) => {
        if (!r.ok) throw new Error("IDFM lignes HTTP " + r.status);
        return r.json();
      })
      .then((data) => {
        idfmLinesData = data;
        idfmLinesLoading = null;
        rebuildIdfmLayer();
        try {
          const b = L.geoJSON(data).getBounds();
          if (b.isValid() && map.getZoom() <= 12) {
            map.fitBounds(b, { padding: [20, 20], maxZoom: 11 });
          }
        } catch (_) {}
        return { n: (data.features || []).length };
      })
      .catch((e) => {
        idfmLinesLoading = null;
        console.error("IDFM lines load failed", e);
        const badge = document.getElementById("idfm-lines-count");
        if (badge) badge.textContent = "Échec chargement lignes IDFM";
        throw e;
      });
    return idfmLinesLoading;
  }

  function setIdfmLineFilters(modes, showSoon) {
    if (Array.isArray(modes)) {
      idfmModeFilter = new Set(modes.map((m) => String(m).toLowerCase()));
    }
    if (typeof showSoon === "boolean") idfmShowSoon = showSoon;
    rebuildIdfmLayer();
  }

  function clearJourney() {
    ensureMap();
    layerGroup.clearLayers();
    markers = [];
    lines = [];
  }

  function cancelAnim(entry) {
    if (entry && entry.anim != null) {
      cancelAnimationFrame(entry.anim);
      entry.anim = null;
    }
  }

  function clearLiveVehicles() {
    ensureMap();
    liveById.forEach((entry) => {
      cancelAnim(entry);
    });
    liveById.clear();
    if (liveLayerGroup) liveLayerGroup.clearLayers();
  }

  function clearGlobalLiveVehicles() {
    ensureMap();
    globalLiveById.forEach((entry) => {
      cancelAnim(entry);
    });
    globalLiveById.clear();
    if (globalLiveLayerGroup) globalLiveLayerGroup.clearLayers();
  }

  function easeInOut(t) {
    return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
  }

  function animateMarker(entry, toLat, toLon, toBearing, durationMs) {
    cancelAnim(entry);
    const fromLat = entry.lat;
    const fromLon = entry.lon;
    const fromBearing =
      entry.bearing != null && Number.isFinite(entry.bearing)
        ? entry.bearing
        : toBearing;
    const start = performance.now();

    function frame(now) {
      const t = Math.min(1, (now - start) / durationMs);
      const e = easeInOut(t);
      const lat = fromLat + (toLat - fromLat) * e;
      const lon = fromLon + (toLon - fromLon) * e;
      entry.marker.setLatLng([lat, lon]);
      if (toBearing != null && Number.isFinite(toBearing) && fromBearing != null) {
        // Shortest-path bearing interpolation
        let d = ((toBearing - fromBearing + 540) % 360) - 180;
        const b = fromBearing + d * e;
        const el = entry.marker.getElement();
        if (el) {
          const rot = el.querySelector(".live-vehicle-rot");
          if (rot) rot.style.transform = `rotate(${b}deg)`;
        }
      }
      if (t < 1) {
        entry.anim = requestAnimationFrame(frame);
      } else {
        entry.anim = null;
        entry.lat = toLat;
        entry.lon = toLon;
        if (toBearing != null && Number.isFinite(toBearing)) {
          entry.bearing = toBearing;
        }
      }
    }

    entry.anim = requestAnimationFrame(frame);
  }

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function vehiclePopup(v) {
    const parts = [];
    const mapLabel = vehicleMapLabel(v);
    const line =
      v.route || v.routeShortName || v.tripShortName || v.label || "";
    const dest = v.headsign || "";
    const title = mapLabel || line || dest || v.label || v.id || "Véhicule";
    parts.push(`<strong class="lv-popup-title">${escapeHtml(title)}</strong>`);
    if (dest && dest !== title) {
      parts.push(
        `<div class="lv-popup-dest">→ ${escapeHtml(dest)}</div>`
      );
    }
    if (line && line !== title) {
      parts.push(`<div>Ligne: ${escapeHtml(line)}</div>`);
    }
    if (v.mode) {
      parts.push(`<div>Mode: ${escapeHtml(String(v.mode))}</div>`);
    }
    const status = v.currentStatus || v.current_status;
    const est = isEstimatedVehicle(v);
    const src = v.positionSource || v.position_source;
    if (est || src || status) {
      const head = est ? "Estimé ≈" : "GPS";
      const detail = status
        ? ` (${String(status)})`
        : src
          ? ` (${String(src)})`
          : "";
      parts.push(
        `<div>Position: ${escapeHtml(head + detail)}</div>`
      );
    }
    if (v.delaySeconds != null && Number(v.delaySeconds) !== 0) {
      const d = Number(v.delaySeconds);
      const mins = Math.round(Math.abs(d) / 60);
      const sign = d > 0 ? "+" : "−";
      const human =
        mins >= 1 ? `${sign}${mins} min` : `${d > 0 ? "+" : ""}${d}s`;
      parts.push(`<div>Retard: ${escapeHtml(human)}</div>`);
    } else if (v.status) {
      parts.push(`<div>RT: ${escapeHtml(v.status)}</div>`);
    }
    if (v.occupancy) {
      parts.push(`<div>Occupation: ${escapeHtml(v.occupancy)}</div>`);
    }
    if (v.feed || v.feedId) {
      parts.push(
        `<div>Feed: ${escapeHtml(v.feed || v.feedId)}</div>`
      );
    }
    const tripId = v.tripId || (v.id && String(v.id).includes(":") ? v.id : null);
    if (tripId) {
      parts.push(
        `<div class="muted">Trip: ${escapeHtml(String(tripId))}</div>`
      );
    } else if (v.id) {
      parts.push(
        `<div class="muted">Id: ${escapeHtml(String(v.id))}</div>`
      );
    }
    if (v.updatedAt) {
      parts.push(
        `<div class="muted">MAJ ${escapeHtml(String(v.updatedAt))}</div>`
      );
    }
    return `<div class="lv-popup">${parts.join("")}</div>`;
  }

  function modeColor(mode, routeColor) {
    if (routeColor) {
      const raw = String(routeColor).trim();
      // Accept "#RRGGBB", "RRGGBB", or short forms; always return #hex for Leaflet.
      const hex = raw.replace(/^#/, "");
      if (/^[0-9a-fA-F]{6}$/.test(hex)) return `#${hex}`;
      if (/^[0-9a-fA-F]{3}$/.test(hex)) {
        return `#${hex[0]}${hex[0]}${hex[1]}${hex[1]}${hex[2]}${hex[2]}`;
      }
      // Invalid color (e.g. bare word) — fall through to mode default (avoid black).
    }
    const m = (mode || "OTHER").toUpperCase();
    return MODE_COLORS[m] || MODE_COLORS.OTHER;
  }

  /** Normalize leg/line color for Leaflet (never bare hex without #). */
  function lineColor(mode, routeColor) {
    return modeColor(mode, routeColor);
  }

  /**
   * Upsert animated vehicle markers into a layer map.
   * vehicles: [{ id, lat, lon, bearing?, label?, occupancy?, currentStatus?,
   *              delaySeconds?, status?, mode?, route?, updatedAt?, color?,
   *              headsign?, feed?, routeColor?, routeShortName?, tripShortName? }]
   */
  function setVehiclesOnLayer(layerGroup, byId, payload, zIndexOffset) {
    ensureMap();
    const data = typeof payload === "string" ? JSON.parse(payload) : payload;
    const list = Array.isArray(data) ? data : data.vehicles || [];
    const seen = new Set();

    list.forEach((v) => {
      if (v == null || v.lat == null || v.lon == null) return;
      const id = String(v.id || v.tripId || `${v.lat},${v.lon}`);
      seen.add(id);
      const lat = Number(v.lat);
      const lon = Number(v.lon);
      if (!Number.isFinite(lat) || !Number.isFinite(lon)) return;
      const bearing =
        v.bearing != null && Number.isFinite(Number(v.bearing))
          ? Number(v.bearing)
          : null;
      const color =
        v.color ||
        modeColor(v.mode, v.routeColor || v.route_color) ||
        "#38bdf8";
      const popup = vehiclePopup(v);
      const icon = vehicleIconFromData(v, bearing, color);
      const badge = vehicleBadgeText(v);
      const mapLabel = vehicleMapLabel(v);
      const estimated = isEstimatedVehicle(v);
      const sig = `${modeKey(v.mode)}|${mapLabel}|${color}|${detectVehicleProduct(v)}|${estimated ? 1 : 0}`;

      let entry = byId.get(id);
      if (!entry) {
        const marker = L.marker([lat, lon], {
          icon,
          zIndexOffset: zIndexOffset,
          keyboard: false,
        });
        marker.bindPopup(popup);
        marker.addTo(layerGroup);
        entry = {
          marker,
          lat,
          lon,
          bearing,
          anim: null,
          sig,
        };
        byId.set(id, entry);
      } else {
        entry.marker.setPopupContent(popup);
        if (entry.sig !== sig) {
          entry.marker.setIcon(icon);
          entry.sig = sig;
        }
        const el = entry.marker.getElement();
        const dist =
          Math.abs(entry.lat - lat) + Math.abs(entry.lon - lon);
        if (dist < 1e-9) {
          entry.lat = lat;
          entry.lon = lon;
          if (bearing != null) {
            entry.bearing = bearing;
            const rot = el && el.querySelector(".live-vehicle-rot");
            if (rot) rot.style.transform = `rotate(${bearing}deg)`;
          }
        } else {
          animateMarker(entry, lat, lon, bearing, LIVE_ANIM_MS);
        }
      }
    });

    byId.forEach((entry, id) => {
      if (!seen.has(id)) {
        cancelAnim(entry);
        layerGroup.removeLayer(entry.marker);
        byId.delete(id);
      }
    });

    return list;
  }

  /**
   * Journey-scoped live vehicles (selected itinerary).
   * Allowed during itinerary focus (GPS + estimé for the plan's trips).
   */
  function setLiveVehicles(payload) {
    const list = setVehiclesOnLayer(liveLayerGroup, liveById, payload, 800);
    const jBadge = document.getElementById("map-live-badge");
    if (jBadge) {
      if (list && list.length) {
        jBadge.removeAttribute("hidden");
      } else {
        jBadge.setAttribute("hidden", "true");
      }
    }
    return list;
  }

  /**
   * Global live network vehicles (all directions, independent of plan).
   * Suppressed while itinerary focus is on so the plan stays readable.
   */
  function setGlobalLiveVehicles(payload) {
    if (itineraryFocus) {
      clearGlobalLiveVehicles();
      updateGlobalLiveStatusUi([]);
      return [];
    }
    const list = setVehiclesOnLayer(
      globalLiveLayerGroup,
      globalLiveById,
      payload,
      500
    );
    updateGlobalLiveStatusUi(list || []);
    return list;
  }

  /**
   * Focus map on a selected itinerary: hide network lines + global live layer.
   * Journey-scoped vehicles (Temps réel) can still be drawn via setLiveVehicles.
   */
  function setItineraryFocus(on) {
    ensureMap();
    // WASM bridge may pass "true"/"false" strings via call_map.
    itineraryFocus =
      on === true ||
      on === 1 ||
      on === "true" ||
      on === "1";
    if (itineraryFocus) {
      clearGlobalLiveVehicles();
      // Hide background network so the plan stands out
      if (idfmLinesLayer) idfmLinesLayer.clearLayers();
      idfmVisible = false;
      const badge = document.getElementById("idfm-lines-count");
      if (badge) badge.textContent = "Focus itinéraire — lignes masquées";
      const gBadge = document.getElementById("map-global-live-badge");
      if (gBadge) gBadge.setAttribute("hidden", "true");
      const wrap = document.getElementById("map-wrap");
      if (wrap) wrap.classList.add("itinerary-focus");
      const focusBanner = document.getElementById("map-focus-banner");
      if (focusBanner) focusBanner.removeAttribute("hidden");
    } else {
      const wrap = document.getElementById("map-wrap");
      if (wrap) wrap.classList.remove("itinerary-focus");
      const focusBanner = document.getElementById("map-focus-banner");
      if (focusBanner) focusBanner.setAttribute("hidden", "true");
      // Restore user IDFM preference
      setIdfmLinesVisible(idfmUserPref);
    }
    return { focus: itineraryFocus };
  }

  /**
   * legs: [{ mode, routeColor, dashed, points: [[lat,lon],...], label }]
   * markers: [{ lat, lon, kind: 'origin'|'destination'|'transfer'|'vehicle', title, mode }]
   * opts.focus (default true): enter itinerary focus mode (clear vehicles, hide network).
   */
  function drawJourney(payload) {
    ensureMap();
    const data = typeof payload === "string" ? JSON.parse(payload) : payload;
    const wantFocus = data.focus !== false;
    if (wantFocus) {
      setItineraryFocus(true);
    }
    clearJourney();
    const bounds = [];

    (data.legs || []).forEach((leg) => {
      const pts = (leg.points || []).filter((p) => p && p.length >= 2);
      if (pts.length < 2) return;
      const modeU = (leg.mode || "").toUpperCase();
      const isWalk = modeU === "WALK";
      const isBike = modeU === "BIKE" || modeU === "BICYCLE";
      // Always normalize via modeColor so GTFS colors without "#" are not black.
      const color = lineColor(leg.mode, leg.routeColor);
      const line = L.polyline(pts, {
        color,
        weight: isWalk || isBike ? 5 : 6,
        opacity: isWalk || isBike ? 0.9 : 0.95,
        // Dashed street path for walk/bike (vs solid transit)
        dashArray: leg.dashed || isWalk || isBike ? "8 10" : null,
        lineJoin: "round",
        lineCap: "round",
      });
      if (leg.label) line.bindPopup(leg.label);
      line.addTo(layerGroup);
      lines.push(line);
      pts.forEach((p) => bounds.push(p));
    });

    (data.markers || []).forEach((m) => {
      if (m.lat == null || m.lon == null) return;
      const kind = m.kind || "transfer";
      // In itinerary focus, skip vehicle markers — only route + OD pins.
      if (kind === "vehicle" && (wantFocus || itineraryFocus)) {
        return;
      }
      let marker;
      if (kind === "origin" || kind === "destination" || kind === "transfer") {
        marker = L.marker([m.lat, m.lon], { icon: pinIcon(kind) });
      } else if (kind === "vehicle") {
        const bearing =
          m.bearing != null && Number.isFinite(Number(m.bearing))
            ? Number(m.bearing)
            : null;
        const ic = vehicleIconFromData(
          m,
          bearing,
          modeColor(m.mode, m.routeColor)
        );
        marker = L.marker([m.lat, m.lon], { icon: ic, zIndexOffset: 600 });
      } else {
        marker = L.circleMarker([m.lat, m.lon], {
          radius: 6,
          color: "#475569",
          fillColor: "#f8fafc",
          fillOpacity: 1,
          weight: 2,
        });
      }
      if (m.title) marker.bindPopup(m.title);
      marker.addTo(layerGroup);
      markers.push(marker);
      bounds.push([m.lat, m.lon]);
    });

    if (bounds.length) {
      map.fitBounds(bounds, { padding: [56, 56], maxZoom: 14 });
    }
  }

  function clearJourneyAndFocus() {
    clearJourney();
    setItineraryFocus(false);
  }

  function focusStop(lat, lon, title) {
    ensureMap();
    if (lat == null || lon == null) return;
    map.setView([lat, lon], Math.max(map.getZoom(), 13));
    L.popup().setLatLng([lat, lon]).setContent(title || "Stop").openOn(map);
  }

  function setView(lat, lon, zoom) {
    ensureMap();
    map.setView([lat, lon], zoom || 12);
  }

  /** Current viewport as { minLat, minLon, maxLat, maxLon }. */
  function getBounds() {
    ensureMap();
    const b = map.getBounds();
    return {
      minLat: b.getSouth(),
      minLon: b.getWest(),
      maxLat: b.getNorth(),
      maxLon: b.getEast(),
    };
  }

  /** Map center + zoom: { lat, lon, zoom }. */
  function getCenterZoom() {
    ensureMap();
    const c = map.getCenter();
    return {
      lat: c.lat,
      lon: c.lng,
      zoom: map.getZoom(),
    };
  }

  /**
   * Register a moveend callback.
   * - If `payload` is a string: call `window[payload]()` (WASM bridge).
   * - If function: call it directly.
   */
  function onMoveEnd(payload) {
    ensureMap();
    const handler = () => {
      if (typeof payload === "function") {
        payload();
      } else if (typeof payload === "string" && typeof window[payload] === "function") {
        window[payload]();
      }
    };
    moveEndHandlers.push(handler);
  }

  window.TransitMap = {
    init: ensureMap,
    clearJourney,
    clearJourneyAndFocus,
    drawJourney,
    setItineraryFocus,
    focusStop,
    setView,
    setLiveVehicles,
    clearLiveVehicles,
    setGlobalLiveVehicles,
    clearGlobalLiveVehicles,
    setIdfmLinesVisible,
    setIdfmLineFilters,
    getBounds,
    getCenterZoom,
    onMoveEnd,
    MODE_COLORS,
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", ensureMap);
  } else {
    ensureMap();
  }
})();
