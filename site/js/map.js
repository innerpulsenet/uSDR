// The zoomable world map. Pan and zoom only change the viewBox. Squares are drawn
// as one path per brightness step and found under the pointer by arithmetic, so
// the node count stays small however many squares there are.
import { el, esc, fmtTime, num } from "./util.js";

const NS = "http://www.w3.org/2000/svg";
const RAD = Math.PI / 180;
const FIELD_ZOOM = 2.6;
const MAX_ZOOM = 40;
const STEPS = ["#17405f", "#1c5e8f", "#2a81c2", "#6db4ea", "#cfe8fb"];
const PRESETS = {
  World: null,
  "N. Atlantic": [-75, 25, -5, 65],
  Caribbean: [-92, 8, -55, 30],
  Europe: [-14, 34, 42, 66],
  Pacific: [140, -30, 245, 62],
};

const svg = (name, attrs = {}) => {
  const n = document.createElementNS(NS, name);
  for (const k in attrs) n.setAttribute(k, attrs[k]);
  return n;
};
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
const wrapLon = (l) => ((l + 540) % 360) - 180;

export function gridCenter(g) {
  const u = g.toUpperCase();
  if (u.length === 2) return [(u.charCodeAt(0) - 65) * 20 - 170, (u.charCodeAt(1) - 65) * 10 - 85];
  return [(u.charCodeAt(0) - 65) * 20 - 180 + (+u[2]) * 2 + 1, (u.charCodeAt(1) - 65) * 10 - 90 + (+u[3]) + 0.5];
}

function gridBounds(g) {
  const [lon, lat] = gridCenter(g);
  const [w, h] = g.length === 2 ? [10, 5] : [1, 0.5];
  return [lon - w, lat - h, lon + w, lat + h];
}

class Projection {
  constructor(kind, home) {
    this.kind = kind;
    if (kind === "az") {
      this.lon0 = home[0];
      this.phi0 = home[1] * RAD;
      this.sin0 = Math.sin(this.phi0);
      this.cos0 = Math.cos(this.phi0);
      this.width = 360;
      this.height = 360;
    } else {
      this.width = 360;
      this.height = 180;
    }
  }

  forward(lon, lat) {
    if (this.kind !== "az") return [lon, -lat];
    const phi = lat * RAD;
    const dl = (lon - this.lon0) * RAD;
    const cosc = clamp(this.sin0 * Math.sin(phi) + this.cos0 * Math.cos(phi) * Math.cos(dl), -1, 1);
    const c = Math.acos(cosc);
    if (c < 1e-9) return [0, 0];
    const k = (c / Math.sin(c || 1)) / RAD;
    return [k * Math.cos(phi) * Math.sin(dl), -k * (this.cos0 * Math.sin(phi) - this.sin0 * Math.cos(phi) * Math.cos(dl))];
  }

  inverse(x, y) {
    if (this.kind !== "az") return [wrapLon(x), -y];
    const rho = Math.hypot(x, y);
    if (rho > 180) return null;
    if (rho < 1e-9) return [this.lon0, this.phi0 / RAD];
    const c = rho * RAD;
    const lat = Math.asin(clamp(Math.cos(c) * this.sin0 + (-y * Math.sin(c) * this.cos0) / rho, -1, 1));
    const lon = this.lon0 + Math.atan2(x * Math.sin(c), rho * this.cos0 * Math.cos(c) + y * this.sin0 * Math.sin(c)) / RAD;
    return [wrapLon(lon), lat / RAD];
  }

  // Where a run of points should break: at the antimeridian, or across the far
  // side of an azimuthal map, a straight segment would be a false line.
  jump() {
    return this.kind === "az" ? 40 : 180;
  }
}

function pathOf(proj, line) {
  let d = "";
  let prev = null;
  const limit = proj.jump();
  for (const [lon, lat] of line) {
    const [x, y] = proj.forward(lon, lat);
    const broke = !prev || Math.abs(x - prev[0]) + Math.abs(y - prev[1]) > limit;
    d += `${broke ? "M" : "L"}${x.toFixed(2)} ${y.toFixed(2)}`;
    prev = [x, y];
  }
  return d;
}

function greatCircle(a, b, n = 64) {
  const toV = ([lon, lat]) => [Math.cos(lat * RAD) * Math.cos(lon * RAD), Math.cos(lat * RAD) * Math.sin(lon * RAD), Math.sin(lat * RAD)];
  const [p, q] = [toV(a), toV(b)];
  const w = Math.acos(clamp(p[0] * q[0] + p[1] * q[1] + p[2] * q[2], -1, 1));
  if (w < 1e-6) return [a, b];
  const out = [];
  for (let i = 0; i <= n; i++) {
    const t = i / n;
    const [s, r] = [Math.sin((1 - t) * w) / Math.sin(w), Math.sin(t * w) / Math.sin(w)];
    const v = [0, 1, 2].map((k) => s * p[k] + r * q[k]);
    out.push([Math.atan2(v[1], v[0]) / RAD, Math.asin(clamp(v[2], -1, 1)) / RAD]);
  }
  return out;
}

export function kmBetween(a, b) {
  const [p1, p2] = [a[1] * RAD, b[1] * RAD];
  const [dp, dl] = [p2 - p1, (b[0] - a[0]) * RAD];
  const h = Math.sin(dp / 2) ** 2 + Math.cos(p1) * Math.cos(p2) * Math.sin(dl / 2) ** 2;
  return 6371 * 2 * Math.asin(Math.min(1, Math.sqrt(h)));
}

// Where the sun is overhead now, to a fraction of a degree: enough to draw a line.
function subsolar(ms) {
  const d = ms / 86400000 - 10957.5;
  const g = (357.529 + 0.98560028 * d) * RAD;
  const q = 280.459 + 0.98564736 * d;
  const ecl = (q + 1.915 * Math.sin(g) + 0.02 * Math.sin(2 * g)) * RAD;
  const e = 23.439 * RAD;
  const dec = Math.asin(Math.sin(e) * Math.sin(ecl));
  const ra = Math.atan2(Math.cos(e) * Math.sin(ecl), Math.cos(ecl)) / RAD;
  const gmst = (18.697374558 + 24.06570982441908 * d) * 15;
  return [wrapLon(ra - gmst), dec / RAD];
}

function terminator(ms) {
  const [sunLon, dec] = subsolar(ms);
  const line = [];
  for (let lon = -180; lon <= 180; lon += 2) {
    line.push([lon, Math.atan(-Math.cos((lon - sunLon) * RAD) / Math.tan(dec * RAD)) / RAD]);
  }
  return { line, sunnyNorth: dec > 0 };
}

export function mountMap(root, data, opts) {
  const { coast, ground, params, onParams } = opts;
  const home = data.station_grid ? gridCenter(data.station_grid) : null;
  const state = {
    proj: params.p === "az" && home ? "az" : "eq",
    cx: 0, cy: 0, z: 1,
    from: 0, to: data.hours,
    band: new Set(), mode: new Set(),
    layers: { squares: true, tracks: true, ground: true, dsc: true, night: true },
    selected: null, box: false,
  };
  let proj = new Projection(state.proj, home);

  const stage = root.querySelector(".stage");
  const map = svg("svg", { role: "img", "aria-label": "World map of what the receiver logged. Use the panel beside it for the same figures as text." });
  stage.prepend(map);
  const g = {};
  for (const name of ["base", "night", "cells", "tracks", "points", "sel", "ui"]) map.append((g[name] = svg("g")));

  const readout = root.querySelector("#readout");
  const totals = root.querySelector("#totals");

  function size() {
    return [Math.max(stage.clientWidth, 1), Math.max(stage.clientHeight, 1)];
  }
  function viewBox() {
    const [w, h] = size();
    const vw = proj.width / state.z;
    return [state.cx - vw / 2, state.cy - (vw * h / w) / 2, vw, vw * h / w];
  }
  function unitsPerPx() {
    return proj.width / state.z / size()[0];
  }
  function clampView() {
    state.z = clamp(state.z, 1, MAX_ZOOM);
    const [, , vw, vh] = viewBox();
    const rangeX = Math.max(0, (proj.width - vw) / 2);
    const rangeY = Math.max(0, (proj.height - vh) / 2);
    state.cx = clamp(state.cx, -rangeX, rangeX);
    state.cy = clamp(state.cy, -rangeY, rangeY);
  }

  function drawBase() {
    g.base.replaceChildren();
    if (state.proj === "az") {
      for (const r of [45, 90, 135, 180]) g.base.append(svg("circle", { class: "rings", cx: 0, cy: 0, r }));
    } else {
      let d = "";
      for (let lon = -160; lon <= 160; lon += 20) d += `M${lon} -90L${lon} 90`;
      for (let lat = -80; lat <= 80; lat += 10) d += `M-180 ${lat}L180 ${lat}`;
      g.base.append(svg("path", { class: "graticule", d }));
    }
    g.base.append(svg("path", { class: "coast", d: coast.map((l) => pathOf(proj, l)).join("") }));
    if (home) {
      const [x, y] = proj.forward(home[0], home[1]);
      g.base.dataset.home = `${x},${y}`;
    }
  }

  function drawNight() {
    g.night.replaceChildren();
    if (!state.layers.night) return;
    const { line, sunnyNorth } = terminator(Date.now());
    if (state.proj === "eq") {
      const edge = sunnyNorth ? 90 : -90;
      const pts = line.map(([lon, lat]) => `${lon} ${-lat}`).join("L");
      g.night.append(svg("path", { class: "night", d: `M-180 ${edge}L${pts}L180 ${edge}Z` }));
    }
    g.night.append(svg("path", { class: "dusk", d: pathOf(proj, line) }));
  }

  // The spots inside the chosen hours, bands and modes, added up by square.
  function aggregate(level) {
    const cells = new Map();
    for (const [grid, m, b, h, n, best] of data.spots) {
      if (h < state.from || h >= state.to) continue;
      if (state.band.size && !state.band.has(data.bands[b])) continue;
      if (state.mode.size && !state.mode.has(data.modes[m])) continue;
      const key = level === 2 ? grid.slice(0, 2) : grid;
      const c = cells.get(key) || { n: 0, best: null, bands: {}, modes: {} };
      c.n += n;
      if (best !== null && (c.best === null || best > c.best)) c.best = best;
      c.bands[data.bands[b]] = (c.bands[data.bands[b]] || 0) + n;
      c.modes[data.modes[m]] = (c.modes[data.modes[m]] || 0) + n;
      cells.set(key, c);
    }
    return cells;
  }

  function cellPath(key) {
    const [w, s, e, n] = gridBounds(key);
    if (state.proj === "eq") return `M${w} ${-n}H${e}V${-s}H${w}Z`;
    const ring = [];
    const step = key.length === 2 ? 5 : 1;
    for (let x = w; x <= e; x += step) ring.push([x, n]);
    for (let y = n; y >= s; y -= key.length === 2 ? 2.5 : 0.5) ring.push([e, y]);
    for (let x = e; x >= w; x -= step) ring.push([x, s]);
    for (let y = s; y <= n; y += key.length === 2 ? 2.5 : 0.5) ring.push([w, y]);
    return pathOf(proj, ring) + "Z";
  }

  let cellsNow = new Map();
  let level = 2;
  function inView(lon, lat, pad = 0) {
    const [vx, vy, vw, vh] = viewBox();
    const [x, y] = proj.forward(lon, lat);
    return x >= vx - pad && x <= vx + vw + pad && y >= vy - pad && y <= vy + vh + pad;
  }

  function drawCells() {
    level = state.z < FIELD_ZOOM ? 2 : 4;
    cellsNow = aggregate(level);
    const groups = STEPS.map(() => "");
    const max = Math.max(1, ...[...cellsNow.values()].map((c) => c.n));
    if (state.layers.squares) {
      for (const [key, c] of cellsNow) {
        const step = clamp(Math.floor((Math.log(c.n + 1) / Math.log(max + 1)) * STEPS.length), 0, STEPS.length - 1);
        groups[step] += cellPath(key);
      }
    }
    g.cells.replaceChildren(...groups.map((d, i) => svg("path", { class: "cells", d, fill: STEPS[i], "fill-opacity": 0.86 })));
  }

  function trackVisible(fixTime) {
    const t = (fixTime - data.start_utc) / 3600;
    return t >= state.from && t < state.to;
  }

  function drawTracks() {
    g.tracks.replaceChildren();
    if (!state.layers.tracks) return;
    for (const t of data.tracks) {
      for (const seg of t.s) {
        const fixes = seg.filter((f) => trackVisible(f[0]));
        if (fixes.length < 2) continue;
        const dup = svg("path", { class: "track", d: pathOf(proj, fixes.map((f) => [f[2], f[1]])) });
        g.tracks.append(dup);
      }
    }
  }

  function drawPoints() {
    g.points.replaceChildren();
    const u = unitsPerPx();
    if (state.layers.tracks) {
      for (const t of data.tracks) for (const seg of t.s) for (const f of seg) {
        if (!trackVisible(f[0]) || !inView(f[2], f[1], 2)) continue;
        const [x, y] = proj.forward(f[2], f[1]);
        g.points.append(svg("circle", { class: "fix", cx: x, cy: y, r: 2 * u }));
      }
    }
    if (state.layers.ground) {
      for (const s of ground) {
        if (!(String(s.id) in data.ground) || !inView(s.lon, s.lat, 2)) continue;
        const [x, y] = proj.forward(s.lon, s.lat);
        const r = 5 * u;
        g.points.append(svg("path", { class: "gs", d: `M${x} ${y - r}L${x + r} ${y}L${x} ${y + r}L${x - r} ${y}Z` }));
      }
    }
    if (state.layers.dsc) {
      for (const d of data.dsc) {
        if ((d.t - data.start_utc) / 3600 < state.from || (d.t - data.start_utc) / 3600 >= state.to || !inView(d.lon, d.lat, 2)) continue;
        const [x, y] = proj.forward(d.lon, d.lat);
        const r = 3.2 * u;
        g.points.append(svg("rect", { class: "ship", x: x - r, y: y - r, width: 2 * r, height: 2 * r, transform: `rotate(45 ${x} ${y})` }));
      }
    }
    if (home) {
      const [x, y] = proj.forward(home[0], home[1]);
      const [w, s, e, n] = gridBounds(data.station_grid);
      if (state.proj === "eq" && state.z >= FIELD_ZOOM) g.points.append(svg("path", { class: "home", d: `M${w} ${-n}H${e}V${-s}H${w}Z` }));
      else {
        const r = 6 * u;
        g.points.append(svg("rect", { class: "home", x: x - r, y: y - r, width: 2 * r, height: 2 * r }));
      }
    }
  }

  function drawSelection() {
    g.sel.replaceChildren();
    if (!state.selected) return;
    g.sel.append(svg("path", { class: "cell-sel", d: cellPath(state.selected) }));
    if (home) g.sel.append(svg("path", { class: "path-sel", d: pathOf(proj, greatCircle(home, gridCenter(state.selected))) }));
  }

  function summarize() {
    const seen = { n: 0, cells: 0, bands: {} };
    for (const [key, c] of cellsNow) {
      const [lon, lat] = gridCenter(key);
      if (!inView(lon, lat)) continue;
      seen.n += c.n;
      seen.cells += 1;
      for (const b in c.bands) seen.bands[b] = (seen.bands[b] || 0) + c.bands[b];
    }
    let flights = 0;
    for (const t of data.tracks) {
      if (t.s.some((s) => s.some((f) => trackVisible(f[0]) && inView(f[2], f[1])))) flights += 1;
    }
    const monitored = data.monitored.filter(([h]) => h >= state.from && h < state.to);
    const hours = new Set(monitored.map(([h]) => h)).size;
    const secs = monitored.reduce((a, [, , s]) => a + s, 0);
    const span = state.to - state.from;
    totals.innerHTML = `
      <h3>In this view</h3>
      <dl class="kv">
        <dt>Spots with a locator</dt><dd>${num(seen.n)}</dd>
        <dt>${level === 2 ? "Grid fields" : "Grid squares"}</dt><dd>${num(seen.cells)}</dd>
        <dt>Aircraft with a track</dt><dd>${num(flights)}</dd>
        <dt>Hours with listening</dt><dd>${hours} of ${span}</dd>
        <dt>Time on air, all modes</dt><dd>${(secs / 3600).toFixed(1)} h</dd>
      </dl>
      ${Object.keys(seen.bands).length ? `<dl class="kv">${Object.entries(seen.bands).sort((a, b) => b[1] - a[1]).map(([b, n]) => `<dt>${esc(b)}</dt><dd>${num(n)}</dd>`).join("")}</dl>` : ""}
      ${span > hours ? `<p class="note" style="margin-top:8px">The receiver was tuned here for only part of this range, one mode at a time. A blank region can mean it was listening elsewhere.</p>` : ""}`;
  }

  function showReadout(html) {
    readout.innerHTML = `<h3>Under the pointer</h3>${html}`;
  }

  function describeCell(key) {
    const c = cellsNow.get(key);
    if (!c) return false;
    const distance = home ? `<dt>From the station's square</dt><dd>${num(Math.round(kmBetween(home, gridCenter(key))))} km</dd>` : "";
    showReadout(`<dl class="kv"><dt>${level === 2 ? "Grid field" : "Grid square"}</dt><dd>${esc(key)}</dd><dt>Spots</dt><dd>${num(c.n)}</dd>
      ${c.best === null ? "" : `<dt>Best report</dt><dd>${c.best} dB</dd>`}${distance}
      ${Object.entries(c.bands).sort((a, b) => b[1] - a[1]).map(([b, n]) => `<dt>${esc(b)}</dt><dd>${num(n)}</dd>`).join("")}</dl>`);
    return true;
  }

  function hover(ev) {
    const [x, y] = toWorld(ev);
    const px = unitsPerPx();
    let best = null;
    let bestD = 8 * px;
    if (state.layers.tracks) {
      for (const t of data.tracks) for (const seg of t.s) for (const f of seg) {
        if (!trackVisible(f[0])) continue;
        const [fx, fy] = proj.forward(f[2], f[1]);
        const d = Math.hypot(fx - x, fy - y);
        if (d < bestD) { bestD = d; best = { t, f }; }
      }
    }
    if (best) {
      showReadout(`<dl class="kv"><dt>Flight</dt><dd>${esc(best.t.i)}</dd><dt>Position at</dt><dd>${fmtTime(best.f[0])}</dd><dt>Reported</dt><dd>${best.f[1].toFixed(2)}, ${best.f[2].toFixed(2)}</dd></dl>`);
      return;
    }
    const at = proj.inverse(x, y);
    if (!at) return;
    const key = cellKey(at[0], at[1]);
    if (state.layers.squares && describeCell(key)) return;
    showReadout(`<p class="note" style="margin-top:8px">Nothing logged in this square for the chosen hours.</p>`);
  }

  function cellKey(lon, lat) {
    const a = Math.floor((lon + 180) / 20);
    const b = Math.floor((lat + 90) / 10);
    const f = String.fromCharCode(65 + clamp(a, 0, 17)) + String.fromCharCode(65 + clamp(b, 0, 17));
    if (level === 2) return f;
    return f + Math.floor((lon + 180 - a * 20) / 2) + Math.floor(lat + 90 - b * 10);
  }

  function applyView() {
    clampView();
    map.setAttribute("viewBox", viewBox().join(" "));
  }

  let settle = 0;
  function settled() {
    clearTimeout(settle);
    settle = setTimeout(() => {
      const before = level;
      if ((state.z < FIELD_ZOOM ? 2 : 4) !== before) { drawCells(); drawSelection(); }
      drawPoints();
      summarize();
      pushParams();
    }, 90);
  }

  function pushParams() {
    const p = { w: data.hours === 24 ? "24h" : "7d" };
    if (state.proj === "az") p.p = "az";
    if (state.z > 1.01) p.v = `${state.cx.toFixed(2)},${state.cy.toFixed(2)},${state.z.toFixed(2)}`;
    if (state.band.size) p.b = [...state.band].join(",");
    if (state.mode.size) p.m = [...state.mode].join(",");
    if (state.from !== 0 || state.to !== data.hours) p.t = `${state.from},${state.to}`;
    onParams(p);
  }

  function redraw() {
    drawCells();
    drawTracks();
    drawPoints();
    drawSelection();
    summarize();
    pushParams();
  }

  function toWorld(ev) {
    const r = map.getBoundingClientRect();
    const [vx, vy, vw, vh] = viewBox();
    return [vx + ((ev.clientX - r.left) / r.width) * vw, vy + ((ev.clientY - r.top) / r.height) * vh];
  }

  function zoomAt(factor, wx, wy) {
    const z = clamp(state.z * factor, 1, MAX_ZOOM);
    const k = state.z / z;
    state.cx = wx - (wx - state.cx) * k;
    state.cy = wy - (wy - state.cy) * k;
    state.z = z;
    applyView();
    settled();
  }

  map.addEventListener("wheel", (ev) => {
    ev.preventDefault();
    const [x, y] = toWorld(ev);
    zoomAt(Math.exp(-ev.deltaY * 0.0016), x, y);
  }, { passive: false });

  map.addEventListener("dblclick", (ev) => {
    const [x, y] = toWorld(ev);
    zoomAt(2, x, y);
  });

  const pointers = new Map();
  let drag = null;
  let pinch = null;
  let boxRect = null;
  map.addEventListener("pointerdown", (ev) => {
    map.setPointerCapture(ev.pointerId);
    pointers.set(ev.pointerId, ev);
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      pinch = { d: Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY) };
      drag = null;
      return;
    }
    const [x, y] = toWorld(ev);
    if (state.box || ev.shiftKey) {
      boxRect = { x0: x, y0: y, el: svg("rect", { class: "boxrect", x, y, width: 0, height: 0 }) };
      g.ui.append(boxRect.el);
    } else {
      drag = { x: ev.clientX, y: ev.clientY, cx: state.cx, cy: state.cy, moved: false };
      map.classList.add("dragging");
    }
  });
  map.addEventListener("pointermove", (ev) => {
    if (!pointers.has(ev.pointerId)) { if (ev.pointerType === "mouse") hover(ev); return; }
    pointers.set(ev.pointerId, ev);
    if (pinch && pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const d = Math.hypot(a.clientX - b.clientX, a.clientY - b.clientY);
      const [x, y] = toWorld({ clientX: (a.clientX + b.clientX) / 2, clientY: (a.clientY + b.clientY) / 2 });
      zoomAt(d / pinch.d, x, y);
      pinch.d = d;
      return;
    }
    if (boxRect) {
      const [x, y] = toWorld(ev);
      boxRect.x1 = x; boxRect.y1 = y;
      boxRect.el.setAttribute("x", Math.min(boxRect.x0, x));
      boxRect.el.setAttribute("y", Math.min(boxRect.y0, y));
      boxRect.el.setAttribute("width", Math.abs(x - boxRect.x0));
      boxRect.el.setAttribute("height", Math.abs(y - boxRect.y0));
      return;
    }
    if (drag) {
      const u = unitsPerPx();
      const [dx, dy] = [ev.clientX - drag.x, ev.clientY - drag.y];
      if (Math.hypot(dx, dy) > 3) drag.moved = true;
      state.cx = drag.cx - dx * u;
      state.cy = drag.cy - dy * u;
      applyView();
    }
  });
  function pointerEnd(ev) {
    pointers.delete(ev.pointerId);
    if (pointers.size < 2) pinch = null;
    map.classList.remove("dragging");
    if (boxRect) {
      const b = boxRect;
      boxRect = null;
      b.el.remove();
      if (b.x1 !== undefined && Math.abs(b.x1 - b.x0) * 1 > unitsPerPx() * 10) {
        fitWorld(Math.min(b.x0, b.x1), Math.min(b.y0, b.y1), Math.max(b.x0, b.x1), Math.max(b.y0, b.y1));
      }
      setBox(false);
      return;
    }
    if (drag) {
      if (!drag.moved) {
        const [x, y] = toWorld(ev);
        const at = proj.inverse(x, y);
        const key = at && cellKey(at[0], at[1]);
        state.selected = key && cellsNow.has(key) ? key : null;
        drawSelection();
        if (state.selected) describeCell(state.selected);
      }
      drag = null;
      settled();
    }
  }
  map.addEventListener("pointerup", pointerEnd);
  map.addEventListener("pointercancel", pointerEnd);

  function fitWorld(x0, y0, x1, y1) {
    const [w, h] = size();
    state.cx = (x0 + x1) / 2;
    state.cy = (y0 + y1) / 2;
    state.z = clamp(Math.min(proj.width / (x1 - x0), (proj.width * h / w) / (y1 - y0)), 1, MAX_ZOOM);
    applyView();
    settled();
  }

  function fitPreset(box) {
    if (!box) {
      state.cx = 0; state.cy = 0; state.z = 1;
      applyView();
      settled();
      return;
    }
    const [w, s, e, n] = box;
    const xs = [], ys = [];
    for (let i = 0; i <= 8; i++) for (const lat of [s, (s + n) / 2, n]) {
      const [x, y] = proj.forward(wrapLon(w + ((e - w) * i) / 8), lat);
      xs.push(x); ys.push(y);
    }
    fitWorld(Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys));
  }

  function setBox(on) {
    state.box = on;
    map.classList.toggle("box", on);
    root.querySelector("#box").setAttribute("aria-pressed", String(on));
  }

  // Controls
  root.querySelector("#box").addEventListener("click", () => setBox(!state.box));
  root.querySelector("#zin").addEventListener("click", () => zoomAt(1.7, state.cx, state.cy));
  root.querySelector("#zout").addEventListener("click", () => zoomAt(1 / 1.7, state.cx, state.cy));
  const presetBox = root.querySelector("#presets");
  for (const name in PRESETS) {
    const b = el("button", { type: "button" }, name);
    b.addEventListener("click", () => fitPreset(PRESETS[name]));
    presetBox.append(b);
  }

  const projButtons = root.querySelectorAll("[data-proj]");
  function setProj(kind) {
    state.proj = kind;
    proj = new Projection(kind, home);
    state.cx = 0; state.cy = 0; state.z = 1;
    projButtons.forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.proj === kind)));
    drawBase();
    drawNight();
    applyView();
    redraw();
  }
  projButtons.forEach((b) => {
    if (b.dataset.proj === "az" && !home) { b.disabled = true; b.title = "Needs the station's grid square"; }
    b.addEventListener("click", () => setProj(b.dataset.proj));
    b.setAttribute("aria-pressed", String(b.dataset.proj === state.proj));
  });

  root.querySelectorAll("[data-layer]").forEach((input) => {
    input.addEventListener("change", () => {
      state.layers[input.dataset.layer] = input.checked;
      drawNight();
      redraw();
    });
  });

  function chipSet(container, values, set, chosen) {
    for (const v of values) {
      const b = el("button", { type: "button", "aria-pressed": String(chosen.includes(v)) }, v);
      if (chosen.includes(v)) set.add(v);
      b.addEventListener("click", () => {
        set.has(v) ? set.delete(v) : set.add(v);
        b.setAttribute("aria-pressed", String(set.has(v)));
        redraw();
      });
      container.append(b);
    }
  }
  const spotModes = [...new Set(data.spots.map((s) => data.modes[s[1]]))];
  chipSet(root.querySelector("#bands"), [...new Set(data.spots.map((s) => data.bands[s[2]]))], state.band, (params.b || "").split(","));
  chipSet(root.querySelector("#modes"), spotModes, state.mode, (params.m || "").split(","));

  // The time strip: spots per hour, with hours nobody was listening hatched.
  const strip = root.querySelector("#strip");
  const fromInput = root.querySelector("#from");
  const toInput = root.querySelector("#to");
  fromInput.max = data.hours - 1;
  toInput.max = data.hours;
  toInput.min = 1;

  function drawStrip() {
    const per = new Array(data.hours).fill(0);
    for (const [, , , h, n] of data.spots) if (h >= 0 && h < data.hours) per[h] += n;
    const listened = new Set(data.monitored.map(([h]) => h));
    const max = Math.max(1, ...per);
    strip.setAttribute("viewBox", `0 0 ${data.hours} 40`);
    strip.setAttribute("preserveAspectRatio", "none");
    strip.innerHTML = `<defs><pattern id="hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45) scale(${data.hours / 900} 1)"><rect width="3" height="6" fill="#20242a"/></pattern></defs>`;
    per.forEach((n, h) => {
      if (!listened.has(h)) strip.append(svg("rect", { class: "out", x: h, y: 0, width: 1, height: 40 }));
      else if (n) strip.append(svg("rect", { class: "sp", x: h + 0.08, y: 40 - (n / max) * 38, width: 0.84, height: (n / max) * 38 }));
    });
    strip.append(svg("rect", { class: "dim", x: 0, y: 0, width: state.from, height: 40 }));
    strip.append(svg("rect", { class: "dim", x: state.to, y: 0, width: data.hours - state.to, height: 40 }));
    const label = (h) => fmtTime(data.start_utc + h * 3600);
    root.querySelector("#fromv").textContent = label(state.from);
    root.querySelector("#tov").textContent = label(state.to);
  }
  function rangeChanged() {
    let [a, b] = [+fromInput.value, +toInput.value];
    if (b <= a) { if (document.activeElement === fromInput) b = a + 1; else a = b - 1; }
    state.from = a; state.to = b;
    fromInput.value = a; toInput.value = b;
    drawStrip();
    redraw();
  }
  fromInput.addEventListener("input", rangeChanged);
  toInput.addEventListener("input", rangeChanged);

  // Restore what the address asked for.
  if (params.t) {
    const [a, b] = params.t.split(",").map(Number);
    if (a >= 0 && b > a && b <= data.hours) { state.from = a; state.to = b; }
  }
  fromInput.value = state.from;
  toInput.value = state.to;
  if (params.v) {
    const [cx, cy, z] = params.v.split(",").map(Number);
    if ([cx, cy, z].every(Number.isFinite)) Object.assign(state, { cx, cy, z });
  }

  drawBase();
  drawNight();
  drawStrip();
  applyView();
  drawCells();
  drawTracks();
  drawPoints();
  summarize();

  const ro = new ResizeObserver(() => { applyView(); settled(); });
  ro.observe(stage);
  const nightTimer = setInterval(drawNight, 60000);
  return () => { ro.disconnect(); clearInterval(nightTimer); clearTimeout(settle); };
}

export { STEPS };
