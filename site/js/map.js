// The zoomable world map. Pan and zoom only change the viewBox. Each grid square is
// a dot at its real position, drawn as a zero-length round-capped stroke: the stroke
// does not scale, so a dot keeps its size at every zoom with no redraw, and one path
// per size and color keeps the node count small however many squares there are.
import { calm } from "./motion.js";
import { ago, el, esc, fmtTime, hours, num } from "./util.js";
import { bearingCell, compass, sortableTable } from "./figures.js";

const NS = "http://www.w3.org/2000/svg";
const RAD = Math.PI / 180;
const MAX_ZOOM = 40;
// From here a square is wide enough on screen for its true extent to be worth showing.
const OUTLINE_ZOOM = 6;
const HEADS_ZOOM = 2.5;
// Closer than this, the finer coastline is worth loading.
const FINE_ZOOM = 3;
// Playback shows this many hours at a time, a step every so many milliseconds.
const PLAY_HOURS = 3;
const PLAY_MS = 700;
const FLY_MS = 620;
// A dot pulses when its square was heard within this many minutes of the newest report.
const PULSE_MINUTES = 60;
const MAX_PULSES = 70;
// Sun below the horizon by these degrees: sunset, then civil, nautical and astronomical dusk.
const TWILIGHT = [0, -6, -12, -18];
// What the flat map shows at first; the poles are still there to pan to.
const WORLD_NORTH = 84;
const WORLD_SOUTH = -62;
// Single-series scales in the radio family's amber, low to high.
const STEPS = ["#72460f", "#9a6018", "#c47c21", "#ed9a37", "#fcc58f"];
// Different stations heard in a square, and the dot's diameter in screen pixels.
const SPOT_SIZES = [
  { from: 1, label: "1", px: 3 },
  { from: 2, label: "2–4", px: 5 },
  { from: 5, label: "5–19", px: 7.5 },
  { from: 20, label: "20+", px: 11 },
];
// Marks on a map can touch, so the brand's chart rules allow three colors here. HF
// splits naturally into low, middle and high bands; the band chips separate the rest.
const BAND_GROUPS = ["160–40 m", "30–17 m", "15–10 m"];
const PRESETS = {
  World: null,
  "N. America": [-130, 20, -60, 56],
  "N. Atlantic": [-80, 25, -5, 62],
  Caribbean: [-92, 8, -55, 30],
  "S. America": [-85, -56, -33, 14],
  Europe: [-12, 35, 42, 66],
};

function bandGroup(label) {
  const meters = parseInt(label, 10);
  if (!Number.isFinite(meters)) return BAND_GROUPS.length;
  return meters >= 40 ? 0 : meters >= 17 ? 1 : 2;
}

// Stations per band, busiest first, two to a row so the side panels stay a fixed height.
function bandList(bands) {
  const rows = Object.entries(bands).sort((a, b) => b[1].size - a[1].size);
  return rows.length ? `<dl class="bandlist">${rows.map(([b, ids]) => `<div><dt>${esc(b)}</dt><dd>${num(ids.size)}</dd></div>`).join("")}</dl>` : "";
}

// Bands as small chips in their group's color, shortest wavelength last.
export const bandPills = (labels) => labels.map((b) => `<span class="bd g${bandGroup(b)}">${esc(String(b).replace(" m", ""))}</span>`).join("");

function sizeOf(n) {
  let size = 0;
  SPOT_SIZES.forEach((s, i) => { if (n >= s.from) size = i; });
  return size;
}

const millerY = (lat) => (1.25 * Math.log(Math.tan(Math.PI / 4 + 0.4 * lat * RAD))) / RAD;
const millerLat = (y) => (2.5 * Math.atan(Math.exp(0.8 * y * RAD)) - (5 * Math.PI) / 8) / RAD;

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
      this.height = 2 * millerY(90);
    }
  }

  // The vertical center of the first view.
  get homeY() {
    return this.kind === "az" ? 0 : -(millerY(WORLD_NORTH) + millerY(WORLD_SOUTH)) / 2;
  }

  forward(lon, lat) {
    if (this.kind !== "az") return [lon, -millerY(clamp(lat, -89.9, 89.9))];
    const phi = lat * RAD;
    const dl = (lon - this.lon0) * RAD;
    const cosc = clamp(this.sin0 * Math.sin(phi) + this.cos0 * Math.cos(phi) * Math.cos(dl), -1, 1);
    const c = Math.acos(cosc);
    if (c < 1e-9) return [0, 0];
    const k = (c / Math.sin(c || 1)) / RAD;
    return [k * Math.cos(phi) * Math.sin(dl), -k * (this.cos0 * Math.sin(phi) - this.sin0 * Math.cos(phi) * Math.cos(dl))];
  }

  inverse(x, y) {
    if (this.kind !== "az") return [wrapLon(x), millerLat(-y)];
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

// A fill has to stay one closed outline, so unlike pathOf it never breaks.
function ringOf(proj, ring) {
  return `M${ring.map(([lon, lat]) => proj.forward(lon, lat).map((v) => v.toFixed(2)).join(" ")).join("L")}Z`;
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

// Degrees clockwise from north, from a to b, both [lon, lat].
export function bearing(a, b) {
  const [p1, p2] = [a[1] * RAD, b[1] * RAD];
  const dl = (b[0] - a[0]) * RAD;
  const deg = Math.atan2(Math.sin(dl) * Math.cos(p2), Math.cos(p1) * Math.sin(p2) - Math.sin(p1) * Math.cos(p2) * Math.cos(dl)) / RAD;
  return (deg + 360) % 360;
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

// Where the sun stands `altitude` degrees above the horizon, one latitude per longitude.
function terminator(ms, altitude = 0) {
  const [sunLon, dec] = subsolar(ms);
  const sinAlt = Math.sin(altitude * RAD);
  const line = [];
  for (let lon = -180; lon <= 180; lon += 2) {
    const a = Math.cos(dec * RAD) * Math.cos((lon - sunLon) * RAD);
    const b = Math.sin(dec * RAD);
    const offset = Math.acos(clamp(sinAlt / Math.hypot(a, b), -1, 1));
    const centre = Math.atan2(b, a);
    const options = [centre + offset, centre - offset].map((r) => r / RAD);
    const lat = options.find((v) => v >= -90 && v <= 90) ?? clamp(options[0], -90, 90);
    line.push([lon, lat]);
  }
  return { line, sunnyNorth: dec > 0 };
}

// Every kind of traffic on one map, each a layer of its own: amateur squares, aircraft
// tracks, HFDL ground stations, ship positions and the night side. A layer that is off
// leaves the map, the totals, the time strip and the list under the map alike.
export const LAYERS = ["squares", "tracks", "ground", "dsc", "night"];
const LAYER_MODES = { squares: new Set(["FT8", "FT4", "WSPR", "JS8"]), tracks: new Set(["HFDL"]), ground: new Set(["HFDL"]), dsc: new Set(["DSC"]) };
// How many rows the list under the map shows before asking.
const LIST_PAGE = 60;

export function mountMap(root, data, opts) {
  const { coast, land, ground, params, onParams, loadFine } = opts;
  const flightInfo = opts.flightInfo || (() => ({}));
  const shipInfo = opts.shipInfo || (() => ({}));
  const presets = opts.presets || PRESETS;
  const now = () => Date.now() / 1000;
  // Still reporting: the aircraft's last fix is within the hour.
  const isRecent = (t) => now() - t < 3600;
  let base = { coast, land };
  let fine = null;
  let fineAsked = false;
  const home = data.station_grid ? gridCenter(data.station_grid) : null;
  const askedLayers = params.l === undefined ? null : new Set(params.l.split(",").filter(Boolean));
  const state = {
    proj: params.p === "az" && home ? "az" : "eq",
    cx: 0, cy: 0, z: 1,
    from: 0, to: data.hours,
    band: new Set(), mode: new Set(),
    layers: Object.fromEntries(LAYERS.map((l) => [l, askedLayers ? askedLayers.has(l) : true])),
    airRecent: params.a === "1",
    selected: null, box: false, pinned: null, hoverFlight: null, ship: null,
    list: params.list || "",
    listPicked: Boolean(params.list),
  };
  let proj = new Projection(state.proj, home);
  state.cy = proj.homeY;

  const stage = root.querySelector(".stage");
  const map = svg("svg", { role: "img", "aria-label": "World map of what the receiver logged. The list under the map gives the same things as text." });
  stage.prepend(map);
  // Selection and pulses move on their own layer, so that they never repaint the map.
  const over = svg("svg", { class: "over", "aria-hidden": "true" });
  map.after(over);
  const g = {};
  for (const name of ["base", "night", "squares", "spots", "tracks", "points", "ui"]) map.append((g[name] = svg("g")));
  for (const name of ["pulses", "sel"]) over.append((g[name] = svg("g")));

  const totals = root.querySelector("#totals");
  const card = root.querySelector("#mapcard");
  const listBox = root.querySelector("#inview");
  const listTabs = root.querySelector("#listtabs");

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
      g.base.append(svg("circle", { class: "disc", cx: 0, cy: 0, r: 180 }));
    }
    g.base.append(svg("path", { class: "land", d: base.land.map((r) => ringOf(proj, r)).join("") }));
    if (state.proj === "az") {
      for (const r of [45, 90, 135, 180]) g.base.append(svg("circle", { class: "rings", cx: 0, cy: 0, r }));
    } else {
      // Every twentieth meridian and tenth parallel: the edges of the grid fields.
      let d = "";
      const top = -millerY(85);
      for (let lon = -160; lon <= 160; lon += 20) d += `M${lon} ${top.toFixed(2)}V${(-top).toFixed(2)}`;
      for (let lat = -80; lat <= 80; lat += 10) d += `M-180 ${(-millerY(lat)).toFixed(2)}H180`;
      g.base.append(svg("path", { class: "graticule", d }));
    }
    g.base.append(svg("path", { class: "coast", d: base.coast.map((l) => pathOf(proj, l)).join("") }));
  }

  // The coarse coastline at world scale, the finer one once zoomed in.
  function chooseBase() {
    if (state.z >= FINE_ZOOM && !fine && !fineAsked) {
      fineAsked = true;
      loadFine().then((f) => { fine = f; chooseBase(); }).catch(() => {});
    }
    const next = state.z >= FINE_ZOOM && fine ? fine : { coast, land };
    if (next.coast !== base.coast) {
      base = next;
      drawBase();
    }
  }

  function drawNight() {
    g.night.replaceChildren();
    if (!state.layers.night) return;
    // The night side belongs to the moment the chosen hours end.
    const at = state.to >= data.hours ? Date.now() : (data.start_utc + state.to * 3600) * 1000;
    const { line } = terminator(at);
    if (state.proj === "eq") {
      // Four bands deepening from sunset to full night, as the sky actually does.
      for (const altitude of TWILIGHT) {
        const band = terminator(at, altitude);
        const edge = (band.sunnyNorth ? 1 : -1) * (proj.height / 2 + 40);
        const pts = band.line.map(([lon, lat]) => proj.forward(lon, lat).map((v) => v.toFixed(2)).join(" ")).join("L");
        g.night.append(svg("path", { class: "night", d: `M-180 ${edge}L${pts}L180 ${edge}Z` }));
      }
    }
    g.night.append(svg("path", { class: "dusk", d: pathOf(proj, line) }));
  }

  function chosen(m, b, h) {
    return h >= state.from && h < state.to
      && (!state.band.size || state.band.has(data.bands[b]))
      && (!state.mode.size || state.mode.has(data.modes[m]));
  }
  const inHours = (t) => {
    const h = (t - data.start_utc) / 3600;
    return h >= state.from && h < state.to;
  };

  // The stations inside the chosen hours, bands and modes, added up by square.
  function aggregate() {
    const cells = new Map();
    if (!state.layers.squares) return cells;
    for (const [sq, m, b, h, id, n, best] of data.spots) {
      if (!chosen(m, b, h)) continue;
      const grid = data.squares[sq];
      const c = cells.get(grid) || { n: 0, best: null, stations: new Set(), bands: {}, groups: new Array(BAND_GROUPS.length + 1).fill(0) };
      c.n += n;
      c.stations.add(id);
      if (best !== null && (c.best === null || best > c.best)) c.best = best;
      (c.bands[data.bands[b]] ||= new Set()).add(id);
      c.groups[bandGroup(data.bands[b])] += n;
      cells.set(grid, c);
    }
    return cells;
  }

  // Who was heard in one square, under the same filters as the dots.
  function stationsIn(key) {
    const sq = data.squares.indexOf(key);
    const who = new Map();
    for (const [s, m, b, h, id, n, best, t] of data.spots) {
      if (s !== sq || !chosen(m, b, h)) continue;
      const w = who.get(id) || { id, call: data.idents[id], n: 0, best: null, t: 0, bands: new Set(), modes: new Set() };
      w.n += n;
      w.t = Math.max(w.t, data.start_utc + t * 60);
      w.bands.add(data.bands[b]);
      w.modes.add(data.modes[m]);
      if (best !== null && (w.best === null || best > w.best)) w.best = best;
      who.set(id, w);
    }
    return [...who.values()];
  }

  function cellPath(key) {
    const [w, s, e, n] = gridBounds(key);
    if (state.proj === "eq") {
      const [x0, y0] = proj.forward(w, n);
      const [x1, y1] = proj.forward(e, s);
      return `M${x0.toFixed(2)} ${y0.toFixed(2)}H${x1.toFixed(2)}V${y1.toFixed(2)}H${x0.toFixed(2)}Z`;
    }
    const ring = [];
    const step = key.length === 2 ? 5 : 1;
    for (let x = w; x <= e; x += step) ring.push([x, n]);
    for (let y = n; y >= s; y -= key.length === 2 ? 2.5 : 0.5) ring.push([e, y]);
    for (let x = e; x >= w; x -= step) ring.push([x, s]);
    for (let y = s; y <= n; y += key.length === 2 ? 2.5 : 0.5) ring.push([w, y]);
    return pathOf(proj, ring) + "Z";
  }

  let cellsNow = new Map();
  let dots = [];
  let hoverKey = null;
  function inView(lon, lat, pad = 0) {
    const [vx, vy, vw, vh] = viewBox();
    const [x, y] = proj.forward(lon, lat);
    return x >= vx - pad && x <= vx + vw + pad && y >= vy - pad && y <= vy + vh + pad;
  }

  // Largest first, so a small square beside a busy one is never buried, and every
  // dot sits on a thin ring of the ocean color to keep neighbors apart.
  function drawSpots() {
    cellsNow = aggregate();
    dots = [];
    const bySize = SPOT_SIZES.map(() => new Array(BAND_GROUPS.length + 1).fill(""));
    for (const [key, c] of cellsNow) {
      const [lon, lat] = gridCenter(key);
      const [x, y] = proj.forward(lon, lat);
      const size = sizeOf(c.stations.size);
      bySize[size][c.groups.indexOf(Math.max(...c.groups))] += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
      dots.push({ key, x, y, px: SPOT_SIZES[size].px });
    }
    const paths = [];
    for (let s = SPOT_SIZES.length - 1; s >= 0; s--) {
      const px = SPOT_SIZES[s].px;
      const all = bySize[s].join("");
      if (all && s >= 2) paths.push(svg("path", { class: "bloom", d: all, "stroke-width": px + 9 }));
      if (all) paths.push(svg("path", { class: "halo", d: all, "stroke-width": px + 2 }));
      bySize[s].forEach((d, group) => {
        if (d) paths.push(svg("path", { class: `spot b${group}`, d, "stroke-width": px }));
      });
    }
    g.spots.replaceChildren(...paths);
    drawSquares();
    drawPulses();
  }

  // What was heard in the last hour of the log: squares by band, aircraft still reporting.
  // Only when the hours end at the present.
  function drawPulses() {
    const out = [];
    if (!calm && state.to >= data.hours) {
      const groups = new Array(BAND_GROUPS.length + 1).fill("");
      if (state.layers.squares) {
        const newest = data.spots.reduce((a, r) => Math.max(a, r[7]), 0);
        const latest = new Map();
        for (const [sq, m, b, h, , , , t] of data.spots) {
          if (t >= newest - PULSE_MINUTES && chosen(m, b, h) && t >= (latest.get(sq)?.t ?? -1)) latest.set(sq, { t, group: bandGroup(data.bands[b]) });
        }
        for (const [sq, l] of [...latest].sort((a, b) => b[1].t - a[1].t).slice(0, MAX_PULSES)) {
          const [lon, lat] = gridCenter(data.squares[sq]);
          const [x, y] = proj.forward(lon, lat);
          groups[l.group] += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
        }
      }
      groups.forEach((d, i) => { if (d) out.push(svg("path", { class: `pulse b${i}`, d, style: `animation-delay:${i * 0.5}s` })); });
      if (state.layers.tracks) {
        let air = "";
        for (const t of data.tracks) {
          const f = t.s[t.s.length - 1].at(-1);
          if (!isRecent(f[0])) continue;
          const [x, y] = proj.forward(f[2], f[1]);
          air += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
        }
        if (air) out.push(svg("path", { class: "pulse air", d: air, style: "animation-delay:1.2s" }));
      }
    }
    g.pulses.replaceChildren(...out);
  }

  // A dot is only the center of a square about 200 km wide. Close in, show the square.
  function drawSquares() {
    const show = state.layers.squares && state.z >= OUTLINE_ZOOM;
    g.squares.replaceChildren(...(show ? [svg("path", { class: "sq", d: dots.map((d) => cellPath(d.key)).join("") })] : []));
  }

  function nearestDot(x, y, extraPx = 5) {
    const u = unitsPerPx();
    let best = null;
    let bestGap = Infinity;
    for (const d of dots) {
      const dist = Math.hypot(d.x - x, d.y - y) / u;
      const gap = dist - d.px / 2;
      if (gap <= extraPx && gap < bestGap) { bestGap = gap; best = { ...d, dist, gap }; }
    }
    return best;
  }

  const lastFix = (t) => t.s[t.s.length - 1].at(-1);
  // A flight is drawn when the layer is on, it has a fix in the chosen hours, and, when
  // only those still reporting are asked for, its latest fix is within the hour.
  const flightShown = (t) => !state.airRecent || isRecent(lastFix(t)[0]);

  // Each flight's drawn path in map units, for finding the one under the pointer.
  let trackHits = [];

  function drawTracks() {
    g.tracks.replaceChildren();
    trackHits = [];
    if (!state.layers.tracks) return;
    let lines = "";
    let old = "";
    let fixes = "";
    for (const t of data.tracks) {
      if (!flightShown(t)) continue;
      const fresh = isRecent(lastFix(t)[0]);
      for (const seg of t.s) {
        const shown = seg.filter((f) => inHours(f[0]));
        if (shown.length) trackHits.push({ t, fresh, pts: shown.map((f) => [...proj.forward(f[2], f[1]), f]) });
        if (shown.length >= 2) {
          const d = pathOf(proj, shown.map((f) => [f[2], f[1]]));
          if (fresh) lines += d; else old += d;
        }
        for (const f of shown) {
          const [x, y] = proj.forward(f[2], f[1]);
          fixes += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
        }
      }
    }
    g.tracks.append(svg("path", { class: "track old", d: old }), svg("path", { class: "track", d: lines }), svg("path", { class: "fix", d: fixes }));
  }

  // An arrowhead on each flight's latest leg, pointing the way it was going. Those still
  // reporting always get one; the rest only once zoomed in, or they would bury the dots.
  function headsPath(u, wantFresh) {
    let d = "";
    for (const t of data.tracks) {
      if (!flightShown(t) || isRecent(lastFix(t)[0]) !== wantFresh) continue;
      let last = null;
      for (const seg of t.s) {
        const shown = seg.filter((f) => inHours(f[0]));
        if (shown.length >= 2) last = shown;
      }
      if (!last) continue;
      const [a, b] = last.slice(-2);
      if (!inView(b[2], b[1], 2)) continue;
      const [ax, ay] = proj.forward(a[2], a[1]);
      const [bx, by] = proj.forward(b[2], b[1]);
      if (Math.abs(bx - ax) + Math.abs(by - ay) > proj.jump() || (ax === bx && ay === by)) continue;
      const angle = Math.atan2(by - ay, bx - ax);
      const at = (forward, side) => [
        bx + (forward * Math.cos(angle) - side * Math.sin(angle)) * u,
        by + (forward * Math.sin(angle) + side * Math.cos(angle)) * u,
      ].map((v) => v.toFixed(3)).join(" ");
      d += `M${at(7.5, 0)}L${at(-5, 4.5)}L${at(-2.2, 0)}L${at(-5, -4.5)}Z`;
    }
    return d;
  }

  // The ship calls with a position, latest first, each MMSI once: its most recent fix.
  function shipsShown() {
    if (!state.layers.dsc) return [];
    const seen = new Map();
    for (const d of data.dsc) {
      if (!inHours(d.t)) continue;
      const s = seen.get(d.i) || { i: d.i, t: d.t, lat: d.lat, lon: d.lon, k: d.k, fixes: 0 };
      s.fixes += 1;
      if (d.t > s.t) Object.assign(s, { t: d.t, lat: d.lat, lon: d.lon, k: d.k });
      seen.set(d.i, s);
    }
    return [...seen.values()];
  }
  const groundShown = () => (state.layers.ground ? ground.filter((s) => String(s.id) in data.ground) : []);

  function drawPoints() {
    g.points.replaceChildren();
    const u = unitsPerPx();
    if (state.layers.tracks) {
      g.points.append(svg("path", { class: "plane", d: headsPath(u, true) }));
      if (state.z >= HEADS_ZOOM || !state.layers.squares) g.points.append(svg("path", { class: "plane old", d: headsPath(u, false) }));
    }
    for (const s of groundShown()) {
      if (!inView(s.lon, s.lat, 2)) continue;
      const [x, y] = proj.forward(s.lon, s.lat);
      const r = 5.5 * u;
      g.points.append(svg("rect", { class: "gs", x: x - r, y: y - r, width: 2 * r, height: 2 * r }));
    }
    for (const d of shipsShown()) {
      if (!inView(d.lon, d.lat, 2)) continue;
      const [x, y] = proj.forward(d.lon, d.lat);
      const r = 4.6 * u;
      g.points.append(svg("rect", { class: "ship", x: x - r, y: y - r, width: 2 * r, height: 2 * r, transform: `rotate(45 ${x} ${y})` }));
    }
    if (home) {
      const [x, y] = proj.forward(home[0], home[1]);
      g.points.append(
        svg("circle", { class: "home-ring", cx: x, cy: y, r: 8 * u }),
        svg("circle", { class: "home-dot", cx: x, cy: y, r: 2.5 * u }),
      );
    }
  }

  function ring(key, cls) {
    const d = dots.find((o) => o.key === key);
    if (d) g.sel.append(svg("circle", { class: cls, cx: d.x, cy: d.y, r: (d.px / 2 + 3.5) * unitsPerPx() }));
  }

  // The steel ring is what is chosen; the ember ring is what is being pointed at.
  function drawSelection() {
    g.sel.replaceChildren();
    if (state.selected && !cellsNow.has(state.selected)) state.selected = null;
    if (state.selected) {
      g.sel.append(svg("path", { class: "cell-sel", d: cellPath(state.selected) }));
      if (home) g.sel.append(svg("path", { class: "path-sel", d: pathOf(proj, greatCircle(home, gridCenter(state.selected))) }));
      ring(state.selected, "ring-sel");
    }
    if (hoverKey && hoverKey !== state.selected) ring(hoverKey, "ring-hover");
    const showFlight = (ident, cls) => {
      const flight = ident && state.layers.tracks && data.tracks.find((t) => t.i === ident);
      if (!flight) return;
      const shown = flight.s.map((seg) => seg.filter((f) => inHours(f[0]))).filter((seg) => seg.length >= 2);
      g.sel.append(svg("path", { class: cls, d: shown.map((seg) => pathOf(proj, seg.map((f) => [f[2], f[1]]))).join("") }));
    };
    if (state.hoverFlight !== state.pinned) showFlight(state.hoverFlight, "track-hover");
    showFlight(state.pinned, "track-sel");
    const ship = state.ship && shipsShown().find((s) => s.i === state.ship);
    if (ship) {
      const [x, y] = proj.forward(ship.lon, ship.lat);
      g.sel.append(svg("circle", { class: "ring-sel", cx: x, cy: y, r: 10 * unitsPerPx() }));
      if (home) g.sel.append(svg("path", { class: "path-sel", d: pathOf(proj, greatCircle(home, [ship.lon, ship.lat])) }));
    }
  }

  function setHover(key) {
    if (key === hoverKey) return;
    hoverKey = key;
    drawSelection();
  }

  // What each layer holds inside the view and the chosen hours: the map's own legend counts,
  // the totals card and the list under the map all read from here.
  function inViewNow() {
    const squares = [];
    for (const [key, c] of cellsNow) if (inView(...gridCenter(key))) squares.push({ key, ...c });
    const flights = [];
    if (state.layers.tracks) {
      for (const t of data.tracks) {
        if (!flightShown(t)) continue;
        let here = 0;
        let last = null;
        for (const seg of t.s) for (const f of seg) {
          if (!inHours(f[0])) continue;
          if (inView(f[2], f[1])) here += 1;
          if (!last || f[0] > last[0]) last = f;
        }
        if (here) flights.push({ t, here, last, fresh: isRecent(lastFix(t)[0]) });
      }
    }
    const ships = shipsShown().filter((s) => inView(s.lon, s.lat));
    const stations = groundShown().filter((s) => inView(s.lon, s.lat));
    return { squares, flights, ships, stations };
  }

  const layerCount = (name, n, word) => {
    const out = root.querySelector(`[data-count="${name}"]`);
    if (out) out.textContent = state.layers[name] ? num(n) : "";
    if (out) out.title = state.layers[name] ? `${num(n)} ${word} in this view and these hours` : "";
  };

  function summarize(v) {
    const listened = data.monitored.filter(([h, m]) => h >= state.from && h < state.to && enabledModes().has(data.modes[m]));
    const hoursOn = new Set(listened.map(([h]) => h)).size;
    const secs = listened.reduce((a, [, , s]) => a + s, 0);
    const span = state.to - state.from;
    const stationIds = new Set();
    let decodes = 0;
    const bands = {};
    for (const c of v.squares) {
      decodes += c.n;
      for (const id of c.stations) stationIds.add(id);
      for (const b in c.bands) {
        const ids = (bands[b] ||= new Set());
        for (const id of c.bands[b]) ids.add(id);
      }
    }
    layerCount("squares", stationIds.size, "stations");
    layerCount("tracks", v.flights.length, "aircraft");
    layerCount("ground", v.stations.length, "ground stations");
    layerCount("dsc", v.ships.length, "ships");
    const rows = [];
    if (state.layers.squares) rows.push(["Amateur stations", num(stationIds.size)], ["Decodes from them", num(decodes)], ["Grid squares", num(v.squares.length)]);
    if (state.layers.tracks) rows.push(["Aircraft with a track", num(v.flights.length)], ["Positions", num(v.flights.reduce((a, f) => a + f.here, 0))]);
    if (state.layers.ground) rows.push(["Ground stations", num(v.stations.length)]);
    if (state.layers.dsc) rows.push(["Ships placed", num(v.ships.length)]);
    if (rows.length) rows.push(["Hours listened", `${hoursOn} of ${span}`], ["Channel-hours", (secs / 3600).toFixed(1)]);
    const byBand = Object.entries(bands).sort((a, b) => b[1].size - a[1].size);
    totals.innerHTML = rows.length
      ? `<dl class="view-totals">${rows.map(([k, val]) => `<div><dt>${k}</dt><dd>${val}</dd></div>`).join("")}</dl>
        ${state.layers.squares && byBand.length ? `<p class="view-bands"><span class="lbl">Stations by band</span>${byBand.map(([b, ids]) => `<span>${bandPills([b])}${num(ids.size)}</span>`).join("")}</p>` : ""}
        ${span > hoursOn ? `<p class="note">The receiver covered these modes for only ${hoursOn} of these ${span} hours. A blank region can mean it was listening elsewhere.</p>` : ""}`
      : "";
  }

  // The card on the map: what the pointer is over, or else what is chosen.
  let cardFor = null;
  function showCard(html, key = null) {
    cardFor = key;
    card.innerHTML = html;
    card.hidden = false;
  }
  function restCard() {
    if (state.pinned && state.layers.tracks) {
      const held = data.tracks.find((t) => t.i === state.pinned);
      if (held) return showCard(flightText(held, lastFix(held)), `f:${held.i}`);
    }
    if (state.ship) {
      const s = shipsShown().find((x) => x.i === state.ship);
      if (s) return showCard(shipText(s), `s:${s.i}`);
    }
    if (state.selected && cellsNow.has(state.selected)) return showCard(cellText(state.selected), `q:${state.selected}`);
    cardFor = null;
    card.hidden = true;
  }
  card.addEventListener("click", (ev) => {
    if (ev.target.closest("[data-unpin]")) {
      clearChoice();
      redrawList();
      pushParams();
    }
  });

  const head = (title, sub, held) => `<div class="fc-head"><div><b>${title}</b>${sub ? `<span class="fc-sub">${sub}</span>` : ""}</div>${held ? `<button type="button" data-unpin aria-label="Clear this selection">Clear</button>` : ""}</div>`;
  const row = (label, value) => (value || value === 0 ? `<dt>${label}</dt><dd class="wrap">${value}</dd>` : "");
  const away = (lon, lat) => (home ? `${num(Math.round(kmBetween(home, [lon, lat])))} km ${compass(bearing(home, [lon, lat]))}` : "");

  function cellText(key) {
    const c = cellsNow.get(key);
    const held = state.selected === key;
    const [lon, lat] = gridCenter(key);
    return `${head(esc(key), "Grid square", held)}<dl class="kv">${row("Stations", num(c.stations.size))}${row("Decodes", num(c.n))}
      ${c.best === null ? "" : row("Best report", `${c.best} dB`)}${row("From the station", away(lon, lat))}</dl>${bandList(c.bands)}
      ${held ? `<p class="fc-hint">The stations are listed under the map.</p>` : `<p class="fc-hint">Click to list its stations</p>`}`;
  }

  function flightText(t, f) {
    const info = flightInfo(t.i);
    const count = t.s.reduce((a, seg) => a + seg.length, 0);
    const last = lastFix(t);
    const held = state.pinned === t.i;
    return `${head(esc(t.i), isRecent(last[0]) ? `<span class="ip-live">Reporting</span>` : "Aircraft, HFDL", held)}
      <dl class="kv">${row("Operator", esc(info.operator || ""))}${row("Registration", esc(info.reg || ""))}${row("Ground station", esc(info.gs || ""))}
      ${row("Messages, 24 h", info.n ? num(info.n) : "")}${row("Positions on track", num(count))}
      ${row(held ? "Latest position" : "This position", `${f[1].toFixed(2)}, ${f[2].toFixed(2)}`)}${row("Reported", fmtTime(f[0]))}${row("Last heard", ago(last[0]))}</dl>
      ${held ? `<a class="fc-link" href="#/who?c=${encodeURIComponent(t.i)}">Full history</a>` : `<p class="fc-hint">Click to keep this selected</p>`}`;
  }

  function shipText(s) {
    const info = shipInfo(s.i);
    const held = state.ship === s.i;
    return `${head(esc(s.i), info.from === "coast" ? "Coast station, DSC" : "Ship, DSC", held)}
      <dl class="kv">${row("Flag", esc(info.flag || ""))}${row("Latest call", esc([info.cat, info.what].filter(Boolean).join(", ")))}
      ${row("Calls, 7 days", info.n ? num(info.n) : "")}${row("Position", `${s.lat.toFixed(2)}, ${s.lon.toFixed(2)}`)}${row("From the station", away(s.lon, s.lat))}${row("Position sent", fmtTime(s.t))}</dl>
      ${held ? `<a class="fc-link" href="#/who?c=${encodeURIComponent(s.i)}">Full history</a>` : `<p class="fc-hint">Click to keep this selected</p>`}`;
  }

  // Whatever is closest to the pointer, within a few pixels, across every layer that is on.
  function under(x, y, touch) {
    const u = unitsPerPx();
    const reach = touch ? 16 : 8;
    let best = null;
    const offer = (gap, hit) => { if (gap <= reach && (!best || gap < best.gap)) best = { gap, ...hit }; };
    for (const s of groundShown()) {
      const [mx, my] = proj.forward(s.lon, s.lat);
      offer(Math.hypot(mx - x, my - y) / u - 3, { kind: "gs", s });
    }
    for (const s of shipsShown()) {
      const [mx, my] = proj.forward(s.lon, s.lat);
      offer(Math.hypot(mx - x, my - y) / u - 3, { kind: "ship", s });
    }
    const dot = nearestDot(x, y, reach);
    if (dot) offer(Math.max(dot.gap, 0), { kind: "cell", key: dot.key });
    // A track is a thin line, so it yields to a mark the pointer is actually on.
    const hit = flightNear(x, y, touch ? 26 : 14);
    if (hit) offer(hit.d + 2, { kind: "flight", t: hit.t, f: hit.f });
    return best;
  }

  function hover(ev) {
    const [x, y] = toWorld(ev);
    const hit = under(x, y, false);
    map.classList.toggle("on-target", Boolean(hit));
    setHover(hit?.kind === "cell" ? hit.key : null);
    setHoverFlight(hit?.kind === "flight" ? hit.t.i : null);
    if (!hit) return restCard();
    if (hit.kind === "cell") showCard(cellText(hit.key));
    else if (hit.kind === "flight") showCard(flightText(hit.t, hit.f));
    else if (hit.kind === "ship") showCard(shipText(hit.s));
    else showCard(`${head(esc(hit.s.name), "HFDL ground station", false)}<dl class="kv">${row("Messages heard", num(data.ground[String(hit.s.id)]))}${row("From the station", away(hit.s.lon, hit.s.lat))}</dl>`);
  }

  function clearChoice() {
    state.selected = null;
    state.pinned = null;
    state.ship = null;
    drawSelection();
    restCard();
  }

  function choose(hit) {
    state.selected = hit?.kind === "cell" ? hit.key : null;
    state.pinned = hit?.kind === "flight" ? hit.t.i : null;
    state.ship = hit?.kind === "ship" ? hit.s.i : null;
    if (state.selected) state.list = "square";
    else if (state.list === "square") state.list = "";
    drawSelection();
    restCard();
    redrawList();
    pushParams();
  }

  // The flight whose drawn path is nearest the pointer, within reach, or none. Older
  // tracks yield to ones still reporting, when they are about equally close.
  function flightNear(x, y, reachPx) {
    const u = unitsPerPx();
    let best = null;
    let bestScore = Infinity;
    const offer = (hit, dist, f) => {
      const d = dist / u;
      const score = d + (hit.fresh ? 0 : 4);
      if (d <= reachPx && score < bestScore) {
        bestScore = score;
        best = { t: hit.t, f, d };
      }
    };
    for (const hit of trackHits) {
      hit.pts.forEach(([ax, ay, fa], i) => {
        offer(hit, Math.hypot(ax - x, ay - y), fa);
        const next = hit.pts[i + 1];
        if (!next) return;
        const [bx, by, fb] = next;
        const [dx, dy] = [bx - ax, by - ay];
        const span = dx * dx + dy * dy;
        if (!span || Math.abs(dx) + Math.abs(dy) > proj.jump()) return;
        const k = clamp(((x - ax) * dx + (y - ay) * dy) / span, 0, 1);
        offer(hit, Math.hypot(ax + k * dx - x, ay + k * dy - y), k < 0.5 ? fa : fb);
      });
    }
    return best;
  }

  function setHoverFlight(ident) {
    if (ident === state.hoverFlight) return;
    state.hoverFlight = ident;
    drawSelection();
  }

  // Bring something into view and hold it selected.
  function fitPoints(points, minPad = 8) {
    const xs = points.map((p) => p[0]);
    const ys = points.map((p) => p[1]);
    const pad = Math.max(minPad, (Math.max(...xs) - Math.min(...xs)) * 0.25);
    fitWorld(Math.min(...xs) - pad, Math.min(...ys) - pad, Math.max(...xs) + pad, Math.max(...ys) + pad);
  }
  function focusFlight(ident) {
    const flight = data.tracks.find((t) => t.i === ident);
    if (!flight) return;
    choose({ kind: "flight", t: flight, f: lastFix(flight) });
    fitPoints(flight.s.flat().filter((f) => inHours(f[0])).map((f) => proj.forward(f[2], f[1])));
  }
  function focusShip(ident) {
    const s = shipsShown().find((x) => x.i === ident);
    if (!s) return;
    choose({ kind: "ship", s });
    fitPoints([proj.forward(s.lon, s.lat)], 14);
  }
  function focusSquare(key) {
    if (!cellsNow.has(key)) return;
    choose({ kind: "cell", key });
    const [x, y] = proj.forward(...gridCenter(key));
    flyTo(x, y, Math.max(state.z, 6));
  }

  function applyView() {
    clampView();
    const box = viewBox().join(" ");
    map.setAttribute("viewBox", box);
    over.setAttribute("viewBox", box);
  }

  let settle = 0;
  function settled() {
    clearTimeout(settle);
    settle = setTimeout(() => {
      chooseBase();
      drawSquares();
      drawPoints();
      drawSelection();
      redrawList();
      pushParams();
    }, 90);
  }

  function pushParams() {
    const p = { w: data.hours === 24 ? "24h" : "7d" };
    if (LAYERS.some((l) => !state.layers[l])) p.l = LAYERS.filter((l) => state.layers[l]).join(",");
    if (state.airRecent) p.a = "1";
    if (state.proj === "az") p.p = "az";
    if (state.z > 1.01) p.v = `${state.cx.toFixed(2)},${state.cy.toFixed(2)},${state.z.toFixed(2)}`;
    if (state.band.size) p.b = [...state.band].join(",");
    if (state.mode.size) p.m = [...state.mode].join(",");
    if (state.selected) p.s = state.selected;
    if (state.pinned) p.f = state.pinned;
    if (state.ship) p.d = state.ship;
    if (state.listPicked && state.list !== "square") p.list = state.list;
    if (state.from !== 0 || state.to !== data.hours) p.t = `${state.from},${state.to}`;
    onParams(p);
  }

  function redraw() {
    drawSpots();
    drawTracks();
    drawPoints();
    drawSelection();
    restCard();
    redrawList();
    pushParams();
  }

  // The list under the map: the same things as the map, as text, inside the view.
  const fromHome = (lon, lat) => (home ? kmBetween(home, [lon, lat]) : null);
  const bearingTo = (lon, lat) => (home ? bearing(home, [lon, lat]) : null);
  const kmCell = (v) => (v == null ? "" : num(Math.round(v)));
  const callCell = (id) => `<a href="#/who?c=${encodeURIComponent(id)}">${esc(id)}</a>`;
  function listTabsFor(v) {
    const tabs = [];
    if (state.selected) tabs.push(["square", `Stations in ${state.selected}`, cellsNow.get(state.selected)?.stations.size || 0]);
    if (state.layers.squares) tabs.push(["squares", "Grid squares", v.squares.length]);
    if (state.layers.tracks) tabs.push(["aircraft", "Aircraft", v.flights.length]);
    if (state.layers.dsc) tabs.push(["ships", "Ships", v.ships.length]);
    if (state.layers.ground) tabs.push(["ground", "Ground stations", v.stations.length]);
    return tabs;
  }
  let listSig = "";
  function redrawList() {
    const v = inViewNow();
    summarize(v);
    if (!listBox) return;
    const tabs = listTabsFor(v);
    if (!tabs.some(([k]) => k === state.list)) state.list = tabs[0]?.[0] || "";
    listTabs.innerHTML = tabs.map(([k, label, n]) => `<button type="button" role="tab" data-list="${k}" aria-selected="${k === state.list}" aria-pressed="${k === state.list}">${esc(label)} <small>${num(n)}</small></button>`).join("");
    // Rebuilding the table resets its sort and paging, so only do it when its rows changed.
    const sig = `${state.list}|${state.selected}|${state.pinned}|${state.ship}|${tabs.map((t) => t[2]).join(",")}|${viewBox().map((n) => n.toFixed(1)).join(",")}|${state.from},${state.to}|${[...state.band]}|${[...state.mode]}|${state.airRecent}`;
    if (sig === listSig) return;
    listSig = sig;
    const empty = (what) => { listBox.innerHTML = `<p class="empty">${what}</p>`; };
    if (!state.list) return empty("Every layer is off, so there is nothing to list. Turn a layer on above.");
    if (state.list === "square") {
      const rows = stationsIn(state.selected);
      sortableTable(listBox, [
        { label: "Station", value: (r) => r.call, html: (r) => callCell(r.call), cls: "id", asc: true },
        { label: "Decodes", value: (r) => r.n, num: true },
        { label: "Best", value: (r) => r.best, html: (r) => (r.best === null ? "" : `${r.best} dB`), num: true },
        { label: "Modes", value: (r) => [...r.modes].join(" "), cls: "hide-s", asc: true },
        { label: "Bands", value: (r) => r.bands.size, html: (r) => bandPills([...r.bands].sort((a, b) => parseInt(b, 10) - parseInt(a, 10))) },
        { label: "Last heard", value: (r) => r.t, html: (r) => fmtTime(r.t) },
      ], rows, { sort: 1, page: LIST_PAGE, caption: `Stations heard in ${state.selected}` });
      return;
    }
    if (state.list === "squares") {
      if (!v.squares.length) return empty("No amateur station with a locator in this view and these hours.");
      sortableTable(listBox, [
        { label: "Square", value: (r) => r.key, cls: "id", asc: true },
        { label: "Stations", value: (r) => r.stations.size, num: true },
        { label: "Decodes", value: (r) => r.n, num: true },
        { label: "Best", value: (r) => r.best, html: (r) => (r.best === null ? "" : `${r.best} dB`), num: true },
        { label: "km", value: (r) => fromHome(...gridCenter(r.key)), html: (r) => kmCell(fromHome(...gridCenter(r.key))), num: true },
        { label: "Bearing", value: (r) => bearingTo(...gridCenter(r.key)), html: (r) => bearingCell(bearingTo(...gridCenter(r.key))), cls: "hide-s", asc: true },
        { label: "Bands", value: (r) => Object.keys(r.bands).length, html: (r) => bandPills(Object.keys(r.bands).sort((a, b) => parseInt(b, 10) - parseInt(a, 10))), cls: "hide-s" },
      ], v.squares, { sort: 1, page: LIST_PAGE, onPick: (r) => focusSquare(r.key), caption: "Grid squares in this view" });
      return;
    }
    if (state.list === "aircraft") {
      if (!v.flights.length) return empty("No aircraft reported a position in this view and these hours.");
      sortableTable(listBox, [
        { label: "Flight", value: (r) => r.t.i, html: (r) => `${callCell(r.t.i)}${r.fresh ? ' <span class="live-dot" title="Still reporting"></span>' : ""}`, cls: "id", asc: true },
        { label: "Operator", value: (r) => flightInfo(r.t.i).operator || null, html: (r) => esc(flightInfo(r.t.i).operator || ""), asc: true, cls: "wrap" },
        { label: "Registration", value: (r) => flightInfo(r.t.i).reg || null, html: (r) => esc(flightInfo(r.t.i).reg || ""), cls: "hide-s", asc: true },
        { label: "Ground station", value: (r) => flightInfo(r.t.i).gs || null, html: (r) => esc(flightInfo(r.t.i).gs || ""), cls: "hide-s", asc: true },
        { label: "Positions", value: (r) => r.here, num: true },
        { label: "Last position", value: (r) => r.last[0], html: (r) => fmtTime(r.last[0]) },
      ], v.flights, { sort: 5, page: LIST_PAGE, onPick: (r) => focusFlight(r.t.i), picked: (r) => r.t.i === state.pinned, caption: "Aircraft in this view" });
      return;
    }
    if (state.list === "ships") {
      if (!v.ships.length) return empty("No ship sent a position in this view and these hours. Most calls carry none; the Log lists every ship heard.");
      sortableTable(listBox, [
        { label: "MMSI", value: (r) => r.i, html: (r) => callCell(r.i), cls: "id", asc: true },
        { label: "Flag", value: (r) => shipInfo(r.i).flag || null, html: (r) => esc(shipInfo(r.i).flag || ""), asc: true },
        { label: "Latest call", value: (r) => [shipInfo(r.i).cat, shipInfo(r.i).what].filter(Boolean).join(", ") || null, html: (r) => esc([shipInfo(r.i).cat, shipInfo(r.i).what].filter(Boolean).join(", ")), cls: "hide-s wrap", asc: true },
        { label: "km", value: (r) => fromHome(r.lon, r.lat), html: (r) => kmCell(fromHome(r.lon, r.lat)), num: true },
        { label: "Position", value: (r) => r.lat, html: (r) => `${r.lat.toFixed(2)}, ${r.lon.toFixed(2)}`, cls: "hide-s" },
        { label: "Sent", value: (r) => r.t, html: (r) => fmtTime(r.t) },
      ], v.ships, { sort: 5, page: LIST_PAGE, onPick: (r) => focusShip(r.i), picked: (r) => r.i === state.ship, caption: "Ships in this view" });
      return;
    }
    if (state.list === "ground") {
      if (!v.stations.length) return empty("No HFDL ground station heard from in this view.");
      sortableTable(listBox, [
        { label: "Ground station", value: (r) => r.name, cls: "id", asc: true },
        { label: "Messages", value: (r) => data.ground[String(r.id)], num: true },
        { label: "km", value: (r) => fromHome(r.lon, r.lat), html: (r) => kmCell(fromHome(r.lon, r.lat)), num: true },
        { label: "Bearing", value: (r) => bearingTo(r.lon, r.lat), html: (r) => bearingCell(bearingTo(r.lon, r.lat)), asc: true },
      ], v.stations, { sort: 1, page: LIST_PAGE, onPick: (r) => fitPoints([proj.forward(r.lon, r.lat)], 20), caption: "HFDL ground stations in this view" });
    }
  }
  listTabs?.addEventListener("click", (ev) => {
    const b = ev.target.closest("[data-list]");
    if (!b) return;
    state.list = b.dataset.list;
    state.listPicked = true;
    redrawList();
    pushParams();
  });

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
    cancelAnimationFrame(flight);
    const [x, y] = toWorld(ev);
    zoomAt(Math.exp(-ev.deltaY * 0.0016), x, y);
  }, { passive: false });

  map.addEventListener("dblclick", (ev) => {
    const [x, y] = toWorld(ev);
    const z = clamp(state.z * 2, 1, MAX_ZOOM);
    const k = state.z / z;
    flyTo(x - (x - state.cx) * k, y - (y - state.cy) * k, z);
  });

  const pointers = new Map();
  let drag = null;
  let pinch = null;
  let boxRect = null;
  map.addEventListener("pointerdown", (ev) => {
    cancelAnimationFrame(flight);
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
      // A hand holding still is rarely perfectly still; a click shouldn't become a drag.
      if (Math.hypot(dx, dy) > 5) drag.moved = true;
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
      if (b.x1 !== undefined && Math.abs(b.x1 - b.x0) > unitsPerPx() * 10) {
        fitWorld(Math.min(b.x0, b.x1), Math.min(b.y0, b.y1), Math.max(b.x0, b.x1), Math.max(b.y0, b.y1));
      }
      setBox(false);
      return;
    }
    if (drag) {
      if (!drag.moved) {
        const [x, y] = toWorld(ev);
        choose(under(x, y, ev.pointerType === "touch"));
      }
      drag = null;
      settled();
    }
  }
  map.addEventListener("pointerup", pointerEnd);
  map.addEventListener("pointercancel", pointerEnd);
  map.addEventListener("pointerleave", () => {
    setHover(null);
    setHoverFlight(null);
    restCard();
  });

  // Glide to a view: fast away, long settle, in equal steps of zoom.
  let flight = 0;
  function flyTo(cx, cy, z) {
    cancelAnimationFrame(flight);
    if (calm) {
      Object.assign(state, { cx, cy, z });
      applyView();
      settled();
      return;
    }
    const from = { cx: state.cx, cy: state.cy, z: state.z };
    const start = performance.now();
    const step = (t0) => {
      const t = Math.min(1, (t0 - start) / FLY_MS);
      const e = 1 - (1 - t) ** 4;
      state.z = Math.exp(Math.log(from.z) + (Math.log(z) - Math.log(from.z)) * e);
      state.cx = from.cx + (cx - from.cx) * e;
      state.cy = from.cy + (cy - from.cy) * e;
      applyView();
      if (t < 1) flight = requestAnimationFrame(step);
      else settled();
    };
    flight = requestAnimationFrame(step);
  }

  function fitWorld(x0, y0, x1, y1) {
    const [w, h] = size();
    flyTo((x0 + x1) / 2, (y0 + y1) / 2, clamp(Math.min(proj.width / (x1 - x0), (proj.width * h / w) / (y1 - y0)), 1, MAX_ZOOM));
  }

  function fitPreset(box) {
    if (!box) {
      flyTo(0, proj.homeY, 1);
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
  for (const name in presets) {
    const b = el("button", { type: "button" }, name);
    b.addEventListener("click", () => fitPreset(presets[name]));
    presetBox.append(b);
  }

  const projButtons = root.querySelectorAll("[data-proj]");
  function setProj(kind) {
    state.proj = kind;
    proj = new Projection(kind, home);
    state.cx = 0; state.cy = proj.homeY; state.z = 1;
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

  // The modes whose listening counts toward the layers that are on.
  function enabledModes() {
    const out = new Set();
    for (const l in LAYER_MODES) if (state.layers[l]) for (const m of LAYER_MODES[l]) out.add(m);
    return out;
  }

  function showLayerState() {
    root.querySelectorAll("[data-layer]").forEach((input) => {
      input.checked = state.layers[input.dataset.layer];
      input.closest(".layer")?.classList.toggle("off", !input.checked);
    });
    stage.classList.toggle("with-squares", state.layers.squares);
  }
  root.querySelectorAll("[data-layer]").forEach((input) => {
    input.addEventListener("change", () => {
      state.layers[input.dataset.layer] = input.checked;
      if (!state.layers.tracks) state.pinned = null;
      if (!state.layers.dsc) state.ship = null;
      showLayerState();
      drawNight();
      drawStrip();
      redraw();
    });
  });
  // Show one layer alone, or every layer again.
  root.querySelectorAll("[data-only]").forEach((b) => b.addEventListener("click", () => {
    const only = b.dataset.only;
    const alone = LAYERS.every((l) => state.layers[l] === (l === only || l === "night"));
    for (const l of LAYERS) state.layers[l] = alone ? true : l === only || l === "night";
    showLayerState();
    drawNight();
    drawStrip();
    redraw();
  }));
  root.querySelectorAll("[data-air-recent]").forEach((b) => {
    b.setAttribute("aria-pressed", String((b.dataset.airRecent === "1") === state.airRecent));
    b.addEventListener("click", () => {
      state.airRecent = b.dataset.airRecent === "1";
      root.querySelectorAll("[data-air-recent]").forEach((o) => o.setAttribute("aria-pressed", String(o === b)));
      redraw();
    });
  });

  function chipSet(container, values, set, picked, withAir) {
    for (const v of values) {
      const b = el("button", { type: "button", "aria-pressed": String(picked.includes(v)) }, v);
      if (withAir) b.append(" ", el("small", { "data-air": v }));
      if (picked.includes(v)) set.add(v);
      b.addEventListener("click", () => {
        set.has(v) ? set.delete(v) : set.add(v);
        b.setAttribute("aria-pressed", String(set.has(v)));
        drawStrip();
        redraw();
      });
      container.append(b);
    }
  }
  const spotModes = [...new Set(data.spots.map((s) => data.modes[s[1]]))];
  // Every band the receiver spent time on, whether or not it heard anyone there.
  const air = data.air || [];
  const bandNames = [...new Set([...data.spots.map((s) => data.bands[s[2]]), ...air.map((a) => data.bands[a[1]])])]
    .sort((a, b) => parseInt(b, 10) - parseInt(a, 10));
  chipSet(root.querySelector("#bands"), bandNames, state.band, (params.b || "").split(","), true);
  function drawAir() {
    const per = {};
    for (const [h, b, s] of air) if (h >= state.from && h < state.to) per[data.bands[b]] = (per[data.bands[b]] || 0) + s;
    root.querySelectorAll("[data-air]").forEach((n) => {
      const s = per[n.dataset.air] || 0;
      n.textContent = s ? hours(s) : "";
      n.parentElement.title = s ? `Listened to amateur modes on ${n.dataset.air} for ${hours(s)} in these hours` : `No listening on ${n.dataset.air} in these hours`;
    });
  }
  chipSet(root.querySelector("#modes"), spotModes, state.mode, (params.m || "").split(","));

  // The time strip: what each layer heard per hour, stacked, with hours nobody was
  // listening to any of them hatched.
  const strip = root.querySelector("#strip");
  const fromInput = root.querySelector("#from");
  const toInput = root.querySelector("#to");
  fromInput.max = data.hours - 1;
  toInput.max = data.hours;
  toInput.min = 1;

  function listenedHours() {
    const modes = enabledModes();
    return new Set(data.monitored.filter(([, m]) => modes.has(data.modes[m])).map(([h]) => h));
  }

  // Distinct stations, aircraft and ships per hour: the three are counted alike, so one
  // scale serves them all.
  function perHour() {
    const make = () => Array.from({ length: data.hours }, () => new Set());
    const amateur = make();
    const aircraft = make();
    const ships = make();
    if (state.layers.squares) {
      for (const [, m, b, h, id] of data.spots) {
        if (h >= 0 && h < data.hours && (!state.band.size || state.band.has(data.bands[b])) && (!state.mode.size || state.mode.has(data.modes[m]))) amateur[h].add(id);
      }
    }
    const hourOf = (t) => Math.floor((t - data.start_utc) / 3600);
    if (state.layers.tracks) {
      for (const t of data.tracks) {
        if (!flightShown(t)) continue;
        for (const seg of t.s) for (const f of seg) { const h = hourOf(f[0]); if (h >= 0 && h < data.hours) aircraft[h].add(t.i); }
      }
    }
    if (state.layers.dsc) for (const d of data.dsc) { const h = hourOf(d.t); if (h >= 0 && h < data.hours) ships[h].add(d.i); }
    return [amateur, aircraft, ships].map((a) => a.map((s) => s.size));
  }

  function drawStrip() {
    const series = perHour();
    const listened = listenedHours();
    const max = Math.max(1, ...series[0].map((_, h) => series[0][h] + series[1][h] + series[2][h]));
    strip.setAttribute("viewBox", `0 0 ${data.hours} 40`);
    strip.setAttribute("preserveAspectRatio", "none");
    strip.innerHTML = `<defs><pattern id="hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45) scale(${data.hours / 900} 1)"><rect class="hatch-bar" width="3" height="6"/></pattern></defs>`;
    for (let h = 0; h < data.hours; h++) {
      if (!listened.has(h)) { strip.append(svg("rect", { class: "out", x: h, y: 0, width: 1, height: 40 })); continue; }
      let y = 40;
      ["sp-ham", "sp-air", "sp-ship"].forEach((cls, i) => {
        const v = series[i][h];
        if (!v) return;
        const hgt = Math.max(0.8, (v / max) * 38);
        y -= hgt;
        strip.append(svg("rect", { class: `sp ${cls}`, x: h + 0.08, y, width: 0.84, height: hgt }));
      });
    }
    strip.append(svg("rect", { class: "dim", x: 0, y: 0, width: state.from, height: 40 }));
    strip.append(svg("rect", { class: "dim", x: state.to, y: 0, width: data.hours - state.to, height: 40 }));
    if (state.from > 0 || state.to < data.hours) {
      for (const h of [state.from, state.to]) strip.append(svg("rect", { class: "edge", x: h - 0.04, y: 0, width: 0.08, height: 40 }));
    }
    const label = (h) => fmtTime(data.start_utc + h * 3600);
    root.querySelector("#fromv").textContent = label(state.from);
    root.querySelector("#tov").textContent = label(state.to);
  }
  function rangeChanged() {
    let [a, b] = [+fromInput.value, +toInput.value];
    if (b <= a) { if (document.activeElement === fromInput) b = a + 1; else a = b - 1; }
    setRange(a, b);
  }
  function setRange(a, b) {
    state.from = a; state.to = b;
    fromInput.value = a; toInput.value = b;
    drawStrip();
    drawAir();
    drawNight();
    redraw();
  }

  // Playback: a short window walking through the hours that were listened to. With
  // reduced motion asked for, the button steps once per press instead of running.
  const playButton = root.querySelector("#play");
  let playTimer = 0;
  if (calm) playButton.textContent = "Next hours";
  function stepPlay() {
    const listened = [...listenedHours()].sort((x, y) => x - y);
    if (!listened.length) return stopPlay();
    let a = state.to - state.from === PLAY_HOURS ? state.from + 1 : listened[0];
    if (a > listened[listened.length - 1] || a + PLAY_HOURS > data.hours) a = listened[0];
    setRange(a, Math.min(a + PLAY_HOURS, data.hours));
  }
  function stopPlay() {
    clearInterval(playTimer);
    playTimer = 0;
    playButton.setAttribute("aria-pressed", "false");
    if (!calm) playButton.textContent = "Play";
  }
  playButton.addEventListener("click", () => {
    if (calm) return stepPlay();
    if (playTimer) return stopPlay();
    playButton.setAttribute("aria-pressed", "true");
    playButton.textContent = "Pause";
    stepPlay();
    playTimer = setInterval(stepPlay, PLAY_MS);
  });
  fromInput.addEventListener("pointerdown", stopPlay);
  toInput.addEventListener("pointerdown", stopPlay);
  // Drag across the strip to choose the hours; a tap on it clears the choice.
  const hourAt = (ev) => {
    const r = strip.getBoundingClientRect();
    return clamp(Math.floor(((ev.clientX - r.left) / r.width) * data.hours), 0, data.hours - 1);
  };
  let brush = null;
  strip.addEventListener("pointerdown", (ev) => {
    stopPlay();
    strip.setPointerCapture(ev.pointerId);
    brush = { a: hourAt(ev), moved: false };
  });
  strip.addEventListener("pointermove", (ev) => {
    if (!brush) return;
    const h = hourAt(ev);
    if (h !== brush.a) brush.moved = true;
    if (brush.moved) setRange(Math.min(brush.a, h), Math.max(brush.a, h) + 1);
  });
  const brushEnd = () => {
    if (brush && !brush.moved) setRange(0, data.hours);
    brush = null;
  };
  strip.addEventListener("pointerup", brushEnd);
  strip.addEventListener("pointercancel", brushEnd);
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
  // A link to a square opens on it.
  if (/^[A-Ra-r]{2}\d{2}$/.test(params.s || "")) {
    state.selected = params.s.toUpperCase();
    state.list = "square";
    if (!params.v) {
      const [x, y] = proj.forward(...gridCenter(state.selected));
      Object.assign(state, { cx: x, cy: y, z: 8 });
    }
  }

  showLayerState();
  chooseBase();
  drawBase();
  drawNight();
  drawStrip();
  drawAir();
  applyView();
  drawSpots();
  drawTracks();
  drawPoints();
  if (params.f && data.tracks.some((t) => t.i === params.f)) {
    state.pinned = params.f;
    if (!params.v) focusFlight(params.f);
  }
  if (params.d && shipsShown().some((s) => s.i === params.d)) {
    state.ship = params.d;
    if (!params.v) focusShip(params.d);
  }
  drawSelection();
  restCard();
  redrawList();

  const ro = new ResizeObserver(() => { applyView(); settled(); });
  ro.observe(stage);
  const nightTimer = setInterval(drawNight, 60000);
  return () => { ro.disconnect(); clearInterval(nightTimer); clearTimeout(settle); clearInterval(playTimer); cancelAnimationFrame(flight); };
}

export { bandGroup, BAND_GROUPS, SPOT_SIZES, STEPS };
