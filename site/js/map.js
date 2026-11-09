// The zoomable world map. Pan and zoom only change the viewBox. Each grid square is
// a dot at its real position, drawn as a zero-length round-capped stroke: the stroke
// does not scale, so a dot keeps its size at every zoom with no redraw, and one path
// per size and color keeps the node count small however many squares there are.
import { el, esc, fmtTime, hours, num } from "./util.js";

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

function terminator(ms) {
  const [sunLon, dec] = subsolar(ms);
  const line = [];
  for (let lon = -180; lon <= 180; lon += 2) {
    line.push([lon, Math.atan(-Math.cos((lon - sunLon) * RAD) / Math.tan(dec * RAD)) / RAD]);
  }
  return { line, sunnyNorth: dec > 0 };
}

export function mountMap(root, data, opts) {
  const { coast, land, ground, params, onParams, loadFine } = opts;
  let base = { coast, land };
  let fine = null;
  let fineAsked = false;
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
  state.cy = proj.homeY;

  const stage = root.querySelector(".stage");
  const map = svg("svg", { role: "img", "aria-label": "World map of what the receiver logged. Use the panel beside it for the same figures as text." });
  stage.prepend(map);
  const g = {};
  for (const name of ["base", "night", "squares", "tracks", "spots", "points", "sel", "ui"]) map.append((g[name] = svg("g")));

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
    const { line, sunnyNorth } = terminator(at);
    if (state.proj === "eq") {
      const edge = (sunnyNorth ? 1 : -1) * (proj.height / 2 + 40);
      const pts = line.map(([lon, lat]) => proj.forward(lon, lat).map((v) => v.toFixed(2)).join(" ")).join("L");
      g.night.append(svg("path", { class: "night", d: `M-180 ${edge}L${pts}L180 ${edge}Z` }));
    }
    g.night.append(svg("path", { class: "dusk", d: pathOf(proj, line) }));
  }

  function chosen(m, b, h) {
    return h >= state.from && h < state.to
      && (!state.band.size || state.band.has(data.bands[b]))
      && (!state.mode.size || state.mode.has(data.modes[m]));
  }

  // The stations inside the chosen hours, bands and modes, added up by square.
  function aggregate() {
    const cells = new Map();
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

  // Who was heard in the chosen square, under the same filters as the dots.
  function drawSquareList() {
    const key = state.selected;
    const panel = root.querySelector("#square");
    if (!key) {
      panel.innerHTML = `<h3>In this square</h3><p class="note" style="margin-top:8px">Click a dot to list the stations heard there.</p>`;
      return;
    }
    const sq = data.squares.indexOf(key);
    const who = new Map();
    for (const [s, m, b, h, id, n, best, t] of data.spots) {
      if (s !== sq || !chosen(m, b, h)) continue;
      const w = who.get(id) || { n: 0, best: null, t: 0, bands: new Set() };
      w.n += n;
      w.t = Math.max(w.t, t);
      w.bands.add(data.bands[b]);
      if (best !== null && (w.best === null || best > w.best)) w.best = best;
      who.set(id, w);
    }
    const rows = [...who].sort((a, b) => b[1].n - a[1].n);
    panel.innerHTML = `<h3>In ${esc(key)}: ${num(rows.length)} station${rows.length === 1 ? "" : "s"}</h3>
      <table><thead><tr><th>Station</th><th>Decodes</th><th>Best</th><th>Bands</th><th>Last heard</th></tr></thead><tbody>
      ${rows.map(([id, w]) => {
        const call = data.idents[id];
        return `<tr><td class="id"><a href="#/who?c=${encodeURIComponent(call)}">${esc(call)}</a></td><td>${num(w.n)}</td><td>${w.best === null ? "" : `${w.best} dB`}</td><td>${[...w.bands].map(esc).join(", ")}</td><td>${fmtTime(data.start_utc + w.t * 60)}</td></tr>`;
      }).join("")}</tbody></table>`;
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
    if (state.layers.squares) {
      for (const [key, c] of cellsNow) {
        const [lon, lat] = gridCenter(key);
        const [x, y] = proj.forward(lon, lat);
        const size = sizeOf(c.stations.size);
        bySize[size][c.groups.indexOf(Math.max(...c.groups))] += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
        dots.push({ key, x, y, px: SPOT_SIZES[size].px });
      }
    }
    const paths = [];
    for (let s = SPOT_SIZES.length - 1; s >= 0; s--) {
      const px = SPOT_SIZES[s].px;
      const all = bySize[s].join("");
      if (all) paths.push(svg("path", { class: "halo", d: all, "stroke-width": px + 2 }));
      bySize[s].forEach((d, group) => {
        if (d) paths.push(svg("path", { class: `spot b${group}`, d, "stroke-width": px }));
      });
    }
    g.spots.replaceChildren(...paths);
    drawSquares();
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
      if (gap <= extraPx && gap < bestGap) { bestGap = gap; best = { ...d, dist }; }
    }
    return best;
  }

  function trackVisible(fixTime) {
    const t = (fixTime - data.start_utc) / 3600;
    return t >= state.from && t < state.to;
  }

  function drawTracks() {
    g.tracks.replaceChildren();
    if (!state.layers.tracks) return;
    let lines = "";
    let fixes = "";
    for (const t of data.tracks) {
      for (const seg of t.s) {
        const shown = seg.filter((f) => trackVisible(f[0]));
        if (shown.length >= 2) lines += pathOf(proj, shown.map((f) => [f[2], f[1]]));
        for (const f of shown) {
          const [x, y] = proj.forward(f[2], f[1]);
          fixes += `M${x.toFixed(2)} ${y.toFixed(2)}h0`;
        }
      }
    }
    g.tracks.append(svg("path", { class: "track", d: lines }), svg("path", { class: "fix", d: fixes }));
  }

  // An arrowhead on each flight's latest leg, pointing the way it was going.
  function headsPath(u) {
    let d = "";
    for (const t of data.tracks) {
      let last = null;
      for (const seg of t.s) {
        const shown = seg.filter((f) => trackVisible(f[0]));
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
      d += `M${at(5, 0)}L${at(-3.5, 3)}L${at(-1.5, 0)}L${at(-3.5, -3)}Z`;
    }
    return d;
  }

  function drawPoints() {
    g.points.replaceChildren();
    const u = unitsPerPx();
    // At world scale two hundred arrowheads would bury the spots; the tracks alone will do.
    if (state.layers.tracks && state.z >= HEADS_ZOOM) g.points.append(svg("path", { class: "plane", d: headsPath(u) }));
    if (state.layers.ground) {
      for (const s of ground) {
        if (!(String(s.id) in data.ground) || !inView(s.lon, s.lat, 2)) continue;
        const [x, y] = proj.forward(s.lon, s.lat);
        const r = 4.5 * u;
        g.points.append(svg("rect", { class: "gs", x: x - r, y: y - r, width: 2 * r, height: 2 * r }));
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

  // The steel ring is the square chosen; the one ember ring is the one being pointed at.
  function drawSelection() {
    g.sel.replaceChildren();
    if (state.selected && !cellsNow.has(state.selected)) state.selected = null;
    if (state.selected) {
      g.sel.append(svg("path", { class: "cell-sel", d: cellPath(state.selected) }));
      if (home) g.sel.append(svg("path", { class: "path-sel", d: pathOf(proj, greatCircle(home, gridCenter(state.selected))) }));
      ring(state.selected, "ring-sel");
    }
    if (hoverKey && hoverKey !== state.selected) ring(hoverKey, "ring-hover");
  }

  function setHover(key) {
    if (key === hoverKey) return;
    hoverKey = key;
    drawSelection();
  }

  function summarize() {
    const seen = { n: 0, cells: 0, stations: new Set(), bands: {} };
    for (const [key, c] of cellsNow) {
      const [lon, lat] = gridCenter(key);
      if (!inView(lon, lat)) continue;
      seen.n += c.n;
      seen.cells += 1;
      for (const id of c.stations) seen.stations.add(id);
      for (const b in c.bands) {
        const ids = (seen.bands[b] ||= new Set());
        for (const id of c.bands[b]) ids.add(id);
      }
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
        <dt>Stations with a locator</dt><dd>${num(seen.stations.size)}</dd>
        <dt>Decodes from them</dt><dd>${num(seen.n)}</dd>
        <dt>Grid squares</dt><dd>${num(seen.cells)}</dd>
        <dt>Aircraft with a track</dt><dd>${num(flights)}</dd>
        <dt>Hours with listening</dt><dd>${hours} of ${span}</dd>
        <dt>Time on air, all modes</dt><dd>${(secs / 3600).toFixed(1)} h</dd>
      </dl>
      ${bandList(seen.bands)}
      ${span > hours ? `<p class="note" style="margin-top:8px">The receiver was tuned here for only part of this range, one mode at a time. A blank region can mean it was listening elsewhere.</p>` : ""}`;
  }

  function showReadout(html) {
    readout.innerHTML = `<h3>Under the pointer</h3>${html}`;
  }

  function describeCell(key) {
    const c = cellsNow.get(key);
    if (!c) return false;
    const distance = home ? `<dt>From the station's square</dt><dd>${num(Math.round(kmBetween(home, gridCenter(key))))} km</dd>` : "";
    showReadout(`<dl class="kv"><dt>Grid square</dt><dd>${esc(key)}</dd><dt>Stations</dt><dd>${num(c.stations.size)}</dd><dt>Decodes</dt><dd>${num(c.n)}</dd>
      ${c.best === null ? "" : `<dt>Best report</dt><dd>${c.best} dB</dd>`}${distance}</dl>${bandList(c.bands)}`);
    return true;
  }

  function hover(ev) {
    const [x, y] = toWorld(ev);
    const u = unitsPerPx();
    // Whatever is closest to the pointer, within a few pixels, of the things on the map.
    let near = null;
    let nearD = 8;
    const consider = (lon, lat, describe) => {
      const [mx, my] = proj.forward(lon, lat);
      const d = Math.hypot(mx - x, my - y) / u;
      if (d < nearD) { nearD = d; near = describe; }
    };
    if (state.layers.tracks) {
      for (const t of data.tracks) for (const seg of t.s) for (const f of seg) {
        if (trackVisible(f[0])) consider(f[2], f[1], () => `<dl class="kv"><dt>Flight</dt><dd>${esc(t.i)}</dd><dt>Position at</dt><dd>${fmtTime(f[0])}</dd><dt>Reported</dt><dd>${f[1].toFixed(2)}, ${f[2].toFixed(2)}</dd></dl>`);
      }
    }
    if (state.layers.ground) {
      for (const s of ground) {
        if (String(s.id) in data.ground) consider(s.lon, s.lat, () => `<dl class="kv"><dt>HFDL ground station</dt><dd>${esc(s.name)}</dd><dt>Messages heard</dt><dd>${num(data.ground[String(s.id)])}</dd></dl>`);
      }
    }
    if (state.layers.dsc) {
      for (const c of data.dsc) {
        const h = (c.t - data.start_utc) / 3600;
        if (h >= state.from && h < state.to) consider(c.lon, c.lat, () => `<dl class="kv"><dt>Ship call</dt><dd>${esc(c.i || "")}</dd><dt>Logged</dt><dd>${fmtTime(c.t)}</dd><dt>Position</dt><dd>${c.lat.toFixed(2)}, ${c.lon.toFixed(2)}</dd></dl>`);
      }
    }
    const dot = nearestDot(x, y);
    if (near && (!dot || nearD < dot.dist)) {
      setHover(null);
      showReadout(near());
      return;
    }
    if (dot) {
      setHover(dot.key);
      describeCell(dot.key);
      return;
    }
    setHover(null);
    showReadout(`<p class="note" style="margin-top:8px">Nothing logged here for the chosen hours.</p>`);
  }

  function applyView() {
    clampView();
    map.setAttribute("viewBox", viewBox().join(" "));
  }

  let settle = 0;
  function settled() {
    clearTimeout(settle);
    settle = setTimeout(() => {
      chooseBase();
      drawSquares();
      drawPoints();
      drawSelection();
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
    if (state.selected) p.s = state.selected;
    if (state.from !== 0 || state.to !== data.hours) p.t = `${state.from},${state.to}`;
    onParams(p);
  }

  function redraw() {
    drawSpots();
    drawTracks();
    drawPoints();
    drawSelection();
    drawSquareList();
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
        const dot = nearestDot(x, y, ev.pointerType === "touch" ? 14 : 5);
        state.selected = dot ? dot.key : null;
        drawSelection();
        drawSquareList();
        if (state.selected) describeCell(state.selected);
      }
      drag = null;
      settled();
    }
  }
  map.addEventListener("pointerup", pointerEnd);
  map.addEventListener("pointercancel", pointerEnd);
  map.addEventListener("pointerleave", () => setHover(null));

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
      state.cx = 0; state.cy = proj.homeY; state.z = 1;
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

  root.querySelectorAll("[data-layer]").forEach((input) => {
    input.addEventListener("change", () => {
      state.layers[input.dataset.layer] = input.checked;
      drawNight();
      redraw();
    });
  });

  function chipSet(container, values, set, chosen, withAir) {
    for (const v of values) {
      const b = el("button", { type: "button", "aria-pressed": String(chosen.includes(v)) }, v);
      if (withAir) b.append(" ", el("small", { "data-air": v }));
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

  // The time strip: spots per hour, with hours nobody was listening hatched.
  const strip = root.querySelector("#strip");
  const fromInput = root.querySelector("#from");
  const toInput = root.querySelector("#to");
  fromInput.max = data.hours - 1;
  toInput.max = data.hours;
  toInput.min = 1;

  function drawStrip() {
    const per = new Array(data.hours).fill(0);
    for (const [, , , h, , n] of data.spots) if (h >= 0 && h < data.hours) per[h] += n;
    const listened = new Set(data.monitored.map(([h]) => h));
    const max = Math.max(1, ...per);
    strip.setAttribute("viewBox", `0 0 ${data.hours} 40`);
    strip.setAttribute("preserveAspectRatio", "none");
    strip.innerHTML = `<defs><pattern id="hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45) scale(${data.hours / 900} 1)"><rect class="hatch-bar" width="3" height="6"/></pattern></defs>`;
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
  const calm = matchMedia("(prefers-reduced-motion: reduce)").matches;
  let playTimer = 0;
  if (calm) playButton.textContent = "Next hours";
  function stepPlay() {
    const listened = [...new Set(data.monitored.map(([h]) => h))].sort((x, y) => x - y);
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
    if (!params.v) {
      const [x, y] = proj.forward(...gridCenter(state.selected));
      Object.assign(state, { cx: x, cy: y, z: 8 });
    }
  }

  chooseBase();
  drawBase();
  drawNight();
  drawStrip();
  drawAir();
  applyView();
  drawSpots();
  drawTracks();
  drawPoints();
  drawSelection();
  drawSquareList();
  summarize();

  const ro = new ResizeObserver(() => { applyView(); settled(); });
  ro.observe(stage);
  const nightTimer = setInterval(drawNight, 60000);
  return () => { ro.disconnect(); clearInterval(nightTimer); clearTimeout(settle); clearInterval(playTimer); };
}

export { bandGroup, BAND_GROUPS, SPOT_SIZES, STEPS };
