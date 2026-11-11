import { bandGroup, BAND_GROUPS, bandPills, bearing, gridCenter, kmBetween, SPOT_SIZES, mountMap, STEPS } from "./map.js";
import { mountGlobe } from "./globe.js";
import { atmosphere, calm, dress, moveTick } from "./motion.js";
import { ago, esc, fmtClock, fmtTime, hours, khz, num } from "./util.js";
import { bearingCell, callLink, dialFigure, modeBoard, modeChip, modeColor, MODE_SLOT, rowsTable, sortableTable, wireReadout } from "./figures.js";

const view = document.getElementById("view");
const STALE_S = 20 * 60;
let unmount = null;
let coast = null;
let land = null;
let ground = null;
let lookups = null;
// When the data on screen was built, to tell when something newer exists.
let shownBuilt = null;

async function load(name) {
  const r = await fetch(`data/${name}?t=${Math.floor(Date.now() / 60000)}`);
  if (!r.ok) throw new Error(`${name}: ${r.status}`);
  return r.json();
}

function route() {
  const [path, query] = (location.hash.slice(2) || "now").split("?");
  return { name: path, params: Object.fromEntries(new URLSearchParams(query || "")) };
}

function emptyLog() {
  return `<p class="empty">The log is empty. Nothing has arrived from the receiver yet, so there is nothing to show.</p>`;
}

async function showNow() {
  const [s, m, health] = await Promise.all([load("status.json"), load("map-24h.json"), load("health.json").catch(() => null)]);
  land ||= await load("land-110m.json");
  shownBuilt = s.generated_utc;
  const total = s.by_mode.reduce((a, m) => a + m.n, 0);
  const flights = s.by_mode.find((m) => m.mode === "HFDL");
  const amateurs = s.by_mode.filter((m) => ["FT8", "FT4", "WSPR", "JS8"].includes(m.mode)).reduce((a, m) => a + m.idents, 0);
  const lastLine = s.last ? `Last on ${esc(s.last.mode)} at ${khz(s.last.dial_hz)} kHz, ${ago(s.last.end_utc)}.` : "It has not reported a stretch of listening yet.";
  const dial = dialFigure(health, s.last);
  view.innerHTML = `
    <header class="hero ip-atmos">
      <div class="ip-atmos-bg" aria-hidden="true"><i class="ip-fog f1"></i><i class="ip-fog f2"></i><i class="ip-fog f3"></i></div>
      <div class="hero-text">
        <p class="eyebrow">Last 24 hours</p>
        <h1>One receiver, listening below 30 MHz.</h1>
        <p class="lede">It moves between amateur digital modes, aircraft datalink, ship calls and weather charts on its own schedule. ${lastLine}</p>
        <div id="nowline"></div>
        <p class="hero-links"><a class="go" href="#/map">Open the map</a><a class="go" href="#/log">Read the log</a></p>
      </div>
      ${m.spots.length ? `<figure class="hero-globe">
        <div class="globe-wrap"><div class="ip-atmos-dial" aria-hidden="true"></div>
        <canvas id="globe" role="img" aria-label="A turning globe marking the ${num(m.squares.length)} grid squares heard in the last 24 hours."></canvas></div>
        <figcaption>${num(m.idents.length)} stations in 24 h · paths replay the last hour</figcaption>
      </figure>` : ""}
    </header>
    <dl class="ip-readout">
      <div><dt>Decodes logged</dt><dd>${num(total)}</dd></div>
      <div><dt>Spent listening</dt><dd>${hours(s.monitored_s)}</dd></div>
      <div><dt>Amateur stations</dt><dd>${num(amateurs)}</dd></div>
      <div><dt>Aircraft on HFDL</dt><dd>${flights ? num(flights.idents) : "0"}</dd></div>
      <div><dt>Grid squares</dt><dd>${num(m.squares.length)}</dd></div>
    </dl>
    ${s.unhealthy_s || s.lost_decodes ? `<p class="ip-notice ip-notice-warn" role="status"><svg class="ip-i sm" aria-hidden="true"><use href="brand/sprite.svg#ip-s-warn"/></svg><span>${s.unhealthy_s ? `${hours(s.unhealthy_s)} of listening failed its radio check and is left out of the figures. ` : ""}${s.lost_decodes ? `${num(s.lost_decodes)} decodes were lost to a full queue.` : ""}</span></p>` : ""}
    ${dial ? `<section aria-labelledby="hdial" class="dial-sec"><h2 id="hdial">Across the dial</h2>
      <p class="note">Every frequency it sat on in the last 24 hours, placed where it falls between 0 and 30 MHz. A stem's height is what was decoded there, on a log scale; the shaded strips are the amateur bands.</p>
      ${dial}
    </section>` : ""}
    <div class="now-cols">
      <section aria-labelledby="yield"><h2 id="yield">Decodes per hour on air</h2>
        ${s.by_mode.length ? modeBoard(s.by_mode) : emptyLog()}
      </section>
      <section aria-labelledby="latest"><h2 id="latest">Latest</h2>
        ${s.latest.length ? `${rowsTable(s.latest.slice(0, 14), { compact: true })}<p class="more"><a class="go" href="#/log">Everyone heard today</a></p>` : emptyLog()}
      </section>
    </div>`;
  drawNowLine();
  wireReadout(view.querySelector(".dial"), view.querySelector("#dial-read"), "Point at a stem for a reading.");
  const canvas = view.querySelector("#globe");
  if (canvas) unmount = mountGlobe(canvas, m, land);
}

const clock = (t) => { const d = new Date(t * 1000); return `${String(d.getUTCHours()).padStart(2, "0")}:${String(d.getUTCMinutes()).padStart(2, "0")}Z`; };

// What the receiver says it is doing, from the file it refreshes once a minute.
async function drawNowLine() {
  const line = document.getElementById("nowline");
  if (!line) return;
  let n = null;
  try { n = await load("now.json"); } catch { /* not sent yet */ }
  if (!document.body.contains(line)) return;
  if (!n || Date.now() / 1000 - n.at > 300) { line.innerHTML = ""; return; }
  const said = {
    manual: `<span class="ip-status ip-status-idle">In use</span> Someone is operating the receiver by hand. The schedule picks up again afterward.`,
    off: `<span class="ip-status ip-status-idle">Off</span> The schedule is switched off.`,
    failed: `<span class="ip-status ip-status-warn">Check failing</span> The receiver's radio check is failing.`,
    idle: `<span class="ip-status ip-status-idle">Between slots</span> The schedule is choosing what to listen to next.`,
  }[n.state] || `<span class="ip-live">On air</span> ${esc(n.label)}${n.ends_utc ? `, until ${clock(n.ends_utc)}` : ""}.`;
  line.innerHTML = `<p class="nowline">${said}${n.next_chart ? ` <span class="next">Next chart: ${esc(n.next_chart)}.</span>` : ""}</p>`;
}

// One glyph per layer, shared by the layer list and anything else that names a layer.
const GLYPH = {
  squares: `<svg viewBox="0 0 18 12" aria-hidden="true"><circle cx="5" cy="6" r="3.2" fill="var(--innerpulse-chart-1)"/><circle cx="12" cy="4.5" r="2.4" fill="var(--innerpulse-chart-2)"/><circle cx="13" cy="9" r="1.8" fill="var(--innerpulse-chart-3)"/></svg>`,
  tracks: `<svg viewBox="0 0 18 12" aria-hidden="true"><path d="M1 10.5 L9 6.5" fill="none" stroke="var(--air)" stroke-width="1.2"/><path d="M17 2 L8.5 3.2 L10.6 6.5 L10.2 10.4Z" fill="var(--air)"/></svg>`,
  ground: `<svg viewBox="0 0 18 12" aria-hidden="true"><rect x="4.5" y="1.5" width="9" height="9" fill="none" stroke="var(--innerpulse-text)" stroke-width="1.5"/></svg>`,
  dsc: `<svg viewBox="0 0 18 12" aria-hidden="true"><rect x="5.5" y="2.5" width="7" height="7" transform="rotate(45 9 6)" fill="var(--sea)"/></svg>`,
  night: `<svg viewBox="0 0 18 12" aria-hidden="true"><rect x=".5" y=".5" width="17" height="11" rx="2" fill="#020305" stroke="var(--innerpulse-line-2)"/><path d="M9 .5V11.5" stroke="rgba(227,230,233,.4)" stroke-dasharray="1.5 1.5"/></svg>`,
};

function layerRow(key, name, sub = "") {
  const only = key === "night" ? "" : `<button type="button" class="only" data-only="${key}" title="Show only this layer, or every layer again">Only</button>`;
  return `<div class="layer" data-for="${key}">
    <div class="layer-row"><label class="ip-check"><input type="checkbox" data-layer="${key}"><span class="glyph">${GLYPH[key]}</span><span>${name}</span></label><b class="lc num" data-count="${key}"></b>${only}</div>
    ${sub ? `<div class="layer-sub">${sub}</div>` : ""}
  </div>`;
}

function mapShell(w) {
  return `
    <div class="map-head">
      <div><p class="eyebrow">The map</p><h1>Who it heard, and where.</h1></div>
      <div class="tools">
        <div class="chips"><span class="lbl">Window</span><button type="button" data-w="24h" aria-pressed="${w === "24h"}">24 hours</button><button type="button" data-w="7d" aria-pressed="${w === "7d"}">7 days</button></div>
        <div class="chips"><span class="lbl">Projection</span><button type="button" data-proj="eq">Flat</button><button type="button" data-proj="az">From the station</button></div>
        <div class="chips" id="presets"><span class="lbl">Go to</span></div>
      </div>
    </div>
    <div class="map-wrap">
      <div class="map-main">
        <div class="stage tall">
          <div class="zoomer"><button type="button" id="zin" aria-label="Zoom in">+</button><button type="button" id="zout" aria-label="Zoom out">−</button><button type="button" id="box" aria-pressed="false" aria-label="Drag a box to zoom to an area" title="Drag a box to zoom to an area">▭</button></div>
          <div class="flight-card" id="mapcard" aria-live="polite" hidden></div>
        </div>
        <div class="strip-wrap">
          <svg id="strip" class="strip" role="img" aria-label="Stations, aircraft and ships heard per hour, for the layers that are on. Hatched hours had no listening to any of them."></svg>
          <div class="range"><label for="from">From</label><input id="from" type="range" min="0" step="1"><label for="to">To</label><input id="to" type="range" step="1"></div>
          <div class="strip-foot"><button type="button" id="play" aria-pressed="false">Play</button><p class="note"><span id="fromv" class="num"></span> to <span id="tov" class="num"></span>. Bars count <i class="kd ham"></i>stations, <i class="kd air"></i>aircraft and <i class="kd ship"></i>ships each hour. Hatched hours had no listening at all, which is not the same as a quiet band.</p></div>
        </div>
      </div>
      <aside class="side" aria-label="Layers and totals">
        <div class="card layers-card"><h3>On the map</h3>
          <div class="layers">
            ${layerRow("squares", "Amateur stations", `
              <div class="chips" id="modes"><span class="lbl">Mode</span></div>
              <div class="chips" id="bands"><span class="lbl">Band</span></div>
              <div class="key">
                <div class="row" aria-label="Dot size is the number of different stations heard in that square">${SPOT_SIZES.map((s) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="${s.px / 2}" fill="var(--innerpulse-text-2)"/></svg>${s.label}</span>`).join("")}</div>
                <div class="row" aria-label="Dot color is the band group heard most in that square">${BAND_GROUPS.map((b, i) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="5" fill="var(--innerpulse-chart-${i + 1})"/></svg>${b}</span>`).join("")}</div>
              </div>
              <p class="note">One dot per grid square: size is stations heard, color the bands with the most decodes. Band chips show time spent listening there.</p>`)}
            ${layerRow("tracks", "Aircraft, HFDL", `<div class="chips"><span class="lbl">Show</span><button type="button" data-air-recent="0">Every track</button><button type="button" data-air-recent="1">Still reporting</button></div>
              <p class="note">Bright tracks reported within the hour. Positions are what the aircraft sent, in the order of their own timestamps.</p>`)}
            ${layerRow("ground", "HFDL ground stations")}
            ${layerRow("dsc", "Ships, DSC", `<p class="note">Only calls that carried a position. Most don't; the Log lists every ship heard.</p>`)}
            ${layerRow("night", "Night side, at the end of these hours")}
          </div>
        </div>
      </aside>
    </div>
    <section class="inview-sec" aria-labelledby="hinview">
      <div class="inview-head"><h2 id="hinview">In this view</h2><div class="chips" id="listtabs" role="tablist" aria-label="What to list"></div></div>
      <div id="totals" class="view-sum" aria-live="polite"></div>
      <p class="note">What the map shows, as a list. It follows the view, the hours and the filters; pan or zoom to narrow it. Click a row to find it on the map, or a heading to sort.</p>
      <div id="inview"></div>
    </section>`;
}

// Ship and aircraft details the map shows on its card and in its list.
function trafficLookups(t, names, stations) {
  const gsName = Object.fromEntries(stations.map((s) => [String(s.id), s.name]));
  const flights = new Map(t.flights.map((f) => [f.i, f]));
  const ships = new Map(t.ships.map((s) => [s.i, s]));
  return {
    flightInfo: (flight) => {
      const row = flights.get(flight);
      return { operator: aircraftAirline(flight, names.airlines), reg: row?.reg, gs: gsName[row?.gs], n: row?.n };
    },
    shipInfo: (mmsi) => {
      const row = ships.get(mmsi) || { i: mmsi };
      return { flag: shipFlag(row, names.flags), from: row.from, cat: row.cat, what: row.what, n: row.n };
    },
  };
}

async function showMap(params) {
  const w = params.w === "7d" ? "7d" : "24h";
  const [data, t, names] = await Promise.all([
    load(`map-${w}.json`), load("traffic.json").catch(() => ({ flights: [], ships: [] })), lookups || load("lookups-site.json"),
    coast || load("coast-110m.json").then((v) => { coast = v; }), land || load("land-110m.json").then((v) => { land = v; }),
    ground || load("ground-stations.json").then((v) => { ground = v; }),
  ]);
  lookups = names;
  shownBuilt = data.generated_utc;
  view.innerHTML = mapShell(w);
  view.querySelectorAll("[data-w]").forEach((b) => b.addEventListener("click", () => {
    const p = new URLSearchParams(location.hash.split("?")[1] || "");
    p.set("w", b.dataset.w);
    for (const k of ["t", "v", "s", "f", "d"]) p.delete(k);
    location.hash = `#/map?${p}`;
  }));
  unmount = mountMap(view, data, {
    coast, land, ground, params, ...trafficLookups(t, names, ground),
    loadFine: async () => {
      const [fineCoast, fineLand] = await Promise.all([load("coast-50m.json"), load("land-50m.json")]);
      return { coast: fineCoast, land: fineLand };
    },
    onParams: (p) => history.replaceState(null, "", `#/map?${new URLSearchParams(p)}`),
  });
}

function heatTable(rows, unit) {
  let max = 0;
  for (const r of rows) for (const [n, s] of r.cells) if (s >= 60) max = Math.max(max, n / (s / 3600));
  const cell = ([n, s], label, h) => {
    const at = `${label}, ${String(h).padStart(2, "0")}Z`;
    if (!s) return `<i class="out" data-tip="${esc(at)}: not monitored"></i>`;
    const rate = n / (s / 3600);
    if (!n) return `<i class="q" data-tip="${esc(at)}: monitored ${hours(s)}, nothing heard"></i>`;
    const step = Math.min(STEPS.length - 1, Math.floor((Math.log(rate + 1) / Math.log(max + 1)) * STEPS.length));
    return `<i style="background:${STEPS[step]}" data-tip="${esc(at)}: ${num(Math.round(rate))} ${unit} per hour, ${hours(s)} monitored"></i>`;
  };
  return `<div class="scroll"><div class="heat">
    ${rows.map((r) => `<div class="rowlabel">${esc(r.label)}</div><div class="cells24">${r.cells.map((c, h) => cell(c, r.label, h)).join("")}</div>`).join("")}
    <div class="hours">${Array.from({ length: 24 }, (_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`).join("")}</div>
  </div></div><p class="fig-read heat-read" aria-live="off">Point at a cell for a reading. Hours are UTC.</p>`;
}

// Which way the stations were, per band group, in 15-degree wedges. The radius is the
// square root of the count so that area, not length, reads as stations.
function antennaSection(m) {
  if (!m.station_grid || !m.spots.length) return `<p class="empty">This needs a week of spots and the station's grid square.</p>`;
  const home = gridCenter(m.station_grid);
  const wedges = BAND_GROUPS.map(() => Array.from({ length: 24 }, () => new Set()));
  const totals = BAND_GROUPS.map(() => new Set());
  for (const [sq, , b, , id] of m.spots) {
    const group = bandGroup(m.bands[b]);
    if (group >= BAND_GROUPS.length || m.squares[sq] === m.station_grid) continue;
    wedges[group][Math.round(bearing(home, gridCenter(m.squares[sq])) / 15) % 24].add(id);
    totals[group].add(id);
  }
  const max = Math.max(1, ...wedges.flat().map((w) => w.size));
  const R = 100;
  const point = (deg, r) => [Math.sin(deg * Math.PI / 180) * r, -Math.cos(deg * Math.PI / 180) * r].map((v) => v.toFixed(1)).join(" ");
  const rose = (group) => {
    const shapes = wedges[group].map((w, i) => {
      if (!w.size) return "";
      const r = R * Math.sqrt(w.size / max);
      return `<path d="M0 0L${point(i * 15 - 6.5, r)}A${r.toFixed(1)} ${r.toFixed(1)} 0 0 1 ${point(i * 15 + 6.5, r)}Z" fill="var(--innerpulse-chart-${group + 1})"><title>${i * 15}°: ${w.size} stations</title></path>`;
    }).join("");
    return `<figure class="rose"><svg viewBox="-128 -128 256 256" role="img" aria-label="Stations by compass bearing on ${BAND_GROUPS[group]}">
      <circle class="ring" r="${R}"/><circle class="ring" r="${R / 2}"/>${shapes}
      <text x="0" y="-110" text-anchor="middle">N</text><text x="110" y="4" text-anchor="middle">E</text><text x="0" y="118" text-anchor="middle">S</text><text x="-110" y="4" text-anchor="middle">W</text>
    </svg><figcaption>${BAND_GROUPS[group]} · ${num(totals[group].size)} stations</figcaption></figure>`;
  };
  return `<div class="roses">${BAND_GROUPS.map((_, g) => rose(g)).join("")}</div>
    <p class="note" style="margin-top:6px">The outer ring is ${num(max)} stations in one 15-degree wedge, the same scale on all three. Spots pile up where radio amateurs live, so a big wedge is partly geography; compare the same direction across bands to see the antenna and the propagation.</p>`;
}

async function showActivity() {
  const [a, week, spectrum] = await Promise.all([load("activity.json"), load("map-7d.json"), load("spectrum.json").catch(() => null)]);
  shownBuilt = a.generated_utc;
  const historyDays = a.first_listening_utc ? (Date.now() / 1000 - a.first_listening_utc) / 86400 : 0;
  const key = `<div class="heat-key"><span><i style="background:${STEPS[4]}"></i>busiest</span><span><i style="background:${STEPS[0]}"></i>few</span><span><i class="q"></i>listened, heard nothing</span><span><i class="out"></i>not monitored</span></div>`;
  const days = historyDays >= 1 ? `${Math.floor(historyDays)} days` : "less than a day";
  const hourly = Array.from({ length: 24 }, (_, h) => a.modes.reduce((sum, row) => sum + row.cells[h][0], 0));
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(hourly)}
    <p class="eyebrow">Activity</p>
    <h1>When each band came alive.</h1>
    <p class="lede">The log so far covers ${days}, grouped by UTC hour over up to ${a.days} days. Every cell is divided by the time the receiver actually spent there.</p>
    </header>
    ${spectrumSection(spectrum)}
    <section aria-labelledby="h1"><h2 id="h1" data-rail="Bands">Amateur decodes by band and hour</h2>${a.bands.length ? heatTable(a.bands, "decodes") + key : emptyLog()}</section>
    <section aria-labelledby="h2"><h2 id="h2" data-rail="Modes">All modes by hour</h2>${a.modes.length ? heatTable(a.modes, "decodes") + key : emptyLog()}</section>
    <section aria-labelledby="hrose"><h2 id="hrose" data-rail="Bearings">Which way it hears</h2><p class="note">Stations with a locator over the last 7 days, by compass bearing from the station.</p>${antennaSection(week)}</section>
    <div class="twocol">
      <section aria-labelledby="h3"><h2 id="h3" data-rail="Farthest">Farthest squares</h2><p class="note">Measured between grid-square centers.</p>
        ${a.farthest.length ? `<div class="scroll"><table><thead><tr><th>km</th><th>Square</th><th>Who</th><th class="hide-s">Band</th><th>Heard</th></tr></thead><tbody>${a.farthest.map((f) => `<tr><td>${num(f.km)}</td><td>${esc(f.grid)}</td><td class="id">${callLink(f.mode, f.ident)}</td><td class="hide-s">${esc(f.band)}</td><td>${fmtTime(f.t)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">Distances need the station's grid square, which has not arrived yet.</p>`}
      </section>
      <section aria-labelledby="h4"><h2 id="h4" data-rail="New squares">New squares today</h2>
        ${historyDays < 7 ? `<p class="empty">With ${days} of history almost every square is new, so this list waits for a week of data.</p>` : a.new_squares.length ? `<div class="scroll"><table><thead><tr><th>Square</th><th>First heard</th></tr></thead><tbody>${a.new_squares.map((n) => `<tr><td class="id">${esc(n.grid)}</td><td>${fmtTime(n.t)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">No new squares in the last 24 hours.</p>`}
      </section>
    </div>
    <section aria-labelledby="h5"><h2 id="h5" data-rail="SELCAL">SELCAL calls</h2><p class="note">A SELCAL call is a two-pair code sent to wake one aircraft's radio. It carries no position.</p>
      ${a.selcal.length ? `<div class="scroll frame short" tabindex="0" role="region" aria-label="SELCAL calls"><table><thead><tr><th>Time</th><th>Code</th><th>kHz</th></tr></thead><tbody>${a.selcal.map((c) => `<tr><td>${fmtTime(c.t)}</td><td class="id">${esc(c.code)}</td><td>${khz(c.dial_hz)}</td></tr>`).join("")}</tbody></table></div><p class="more"><a class="go" href="#/log?tab=selcal">Every SELCAL code in the log</a></p>` : `<p class="empty">No SELCAL calls logged.</p>`}
    </section>
    ${navtexSection(a.navtex)}
    ${beaconSection(a.beacons)}`;
  wireSpectrum(spectrum);
}

// The NCDXF/IARU beacons: which of the 18 each frequency heard in the last hour,
// at what strength and down to which power step, then how many of them each
// hour of the last day. Shown once a beacon channel has listened.
const BEACON_POWER = ["the callsign only", "the 100 W dash", "down to 10 W", "down to 1 W", "down to 100 mW"];
function beaconSection(b) {
  if (!b) return "";
  const mhz = (f) => (f / 1e6).toFixed(3);
  const signed = (x) => `${x > 0 ? "+" : x < 0 ? "−" : ""}${Math.abs(Math.round(x))}`;
  // −20 dB is a beacon just heard, +30 dB a loud one.
  const shadeStep = (snr) => Math.max(0, Math.min(STEPS.length - 1, Math.floor(((snr + 20) / 50) * STEPS.length)));
  const pips = (k) => `<span class="pips" aria-hidden="true">${[1, 2, 3, 4].map((i) => `<i${i <= k ? ' class="lit"' : ""}></i>`).join("")}</span>`;
  const cell = (row, f) => {
    const at = `${row.call} on ${mhz(f)} MHz`;
    const listened = b.listened[f] || 0;
    const h = row.heard[f];
    if (h) {
      const tip = `${at}: heard ${num(h.n)} ${h.n === 1 ? "time" : "times"} in the last hour, best ${signed(h.snr)} dB, ${BEACON_POWER[h.steps]}, last ${fmtTime(h.t)}`;
      const step = shadeStep(h.snr ?? -20);
      return `<td class="bc hit${step < 2 ? " dim" : ""}" data-tip="${esc(tip)}" style="--c:${STEPS[step]}"><span class="v">${signed(h.snr ?? 0)}</span>${pips(h.steps)}<span class="sr">${esc(tip)}</span></td>`;
    }
    if (listened < 60) return `<td class="bc out" data-tip="${esc(at)}: not listened to in the last hour"><span class="sr">not listened to</span></td>`;
    return `<td class="bc q" data-tip="${esc(`${at}: listened ${hours(listened)}, not heard`)}"><span class="sr">not heard</span></td>`;
  };
  const heardOn = (f) => b.beacons.filter((r) => r.heard[f]).length;
  const hourCell = ([n, s], f, k) => {
    const at = `${mhz(f)} MHz, ${utcClock((b.first_hour + k * 3600) / 60)}Z`;
    if (!s) return `<i class="out" style="--h:${k}" data-tip="${esc(at)}: not listened to"></i>`;
    if (!n) return `<i class="q" style="--h:${k}" data-tip="${esc(at)}: listened ${hours(s)}, no beacon heard"></i>`;
    return `<i style="--h:${k};background:${STEPS[Math.min(STEPS.length - 1, Math.floor((n / 18) * STEPS.length))]}" data-tip="${esc(at)}: ${n} of 18 beacons heard, ${hours(s)} listened"></i>`;
  };
  return `<section aria-labelledby="h7"><h2 id="h7" data-rail="Beacons">Beacons around the world</h2>
      <p class="note">Eighteen beacons on six continents take turns on five frequencies, ten seconds each, every three minutes: a callsign, then dashes at 100 W, 10 W, 1 W and 100 mW. A beacon heard down to its last dash is an open path; one heard at 100 W only, a path just open. Strength is over the noise in 2500 Hz, as FT8 reports it.</p>
      <div class="scroll"><table class="beacons"><thead><tr><th>Beacon</th>${b.freqs.map((f) => `<th><span class="mhz">${mhz(f)}</span><span class="cnt">${heardOn(f)} heard</span></th>`).join("")}</tr></thead>
      <tbody>${b.beacons.map((r) => `<tr><th scope="row"><span class="id">${esc(r.call)}</span><span class="place">${esc(r.place)}</span></th>${b.freqs.map((f) => cell(r, f)).join("")}</tr>`).join("")}</tbody></table></div>
      <p class="fig-read heat-read" aria-live="off">Point at a cell for a reading. The last hour; dots are the dashes heard.</p>
      <h3 class="sub">Beacons heard each hour, last 24 hours</h3>
      <div class="scroll"><div class="heat">
        ${b.freqs.map((f, i) => `<div class="rowlabel">${mhz(f)}</div><div class="cells24">${b.hours[i].map((c, k) => hourCell(c, f, k)).join("")}</div>`).join("")}
        <div class="hours">${Array.from({ length: 24 }, (_, k) => { const h = new Date((b.first_hour + k * 3600) * 1000).getUTCHours(); return `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`; }).join("")}</div>
      </div></div><p class="fig-read heat-read" aria-live="off">Point at a cell for a reading. Hours are UTC.</p>
    </section>`;
}

// Each message once, its text behind its heading: a station repeats a message
// at every broadcast until it is canceled.
function navtexSection(list = []) {
  // Shown only once a message is logged: the station does not visit 518 kHz on its own.
  if (!list.length) return "";
  const from = (m) => (m.name ? `${m.name} (${m.station})` : m.station ? `Station ${m.station}` : "");
  const head = (m) => [from(m), m.subject, fmtTime(m.t), m.n > 1 ? `heard ${num(m.n)} times` : "", m.complete ? "" : "cut off"].filter(Boolean).map(esc).join(" · ");
  return `<section aria-labelledby="h6"><h2 id="h6" data-rail="NAVTEX">NAVTEX messages</h2><p class="note">Maritime safety broadcasts on 518 kHz: navigational warnings, weather and search and rescue. Each message is listed once, from its best copy.</p>
      <div class="navtex">${list.map((m) => `<details><summary><span class="id">${esc(m.id)}</span><span>${head(m)}</span></summary><pre>${esc(m.text)}</pre></details>`).join("")}</div>
    </section>`;
}

// Where each amateur band starts, in MHz, for the axis and the readout.
const SPECTRUM_BANDS = [[3.5, 4, "80 m"], [5.33, 5.41, "60 m"], [7, 7.3, "40 m"], [10.1, 10.15, "30 m"], [14, 14.35, "20 m"], [18.068, 18.168, "17 m"], [21, 21.45, "15 m"], [24.89, 24.99, "12 m"], [28, 29.7, "10 m"]];
const utcClock = (minute) => new Date(minute * 60000).toISOString().slice(11, 16);

function spectrumSection(s) {
  if (!s?.rows) return `<section aria-labelledby="hspec"><h2 id="hspec">What the radio saw</h2><p class="empty">The spectrum record has not started yet.</p></section>`;
  const yOf = (mhz) => ((s.hi_hz / 1e6 - mhz) / ((s.hi_hz - s.lo_hz) / 1e6)) * 100;
  const hours = [];
  for (let m = s.first_minute + ((180 - (s.first_minute % 180)) % 180); m < s.first_minute + s.minutes; m += 180) hours.push(m);
  const late = s.earliest_minute > s.first_minute + 5;
  const modes = [...new Set(s.marks.map((m) => m[2]))].sort();
  return `<section aria-labelledby="hspec" class="spec-sec">
    <h2 id="hspec" data-rail="Spectrum">What the radio saw, last 24 hours</h2>
    <p class="note">Each column is one minute, from 2 MHz at the bottom to 30 MHz at the top. Brightness is how far a signal rose above the noise around it, not an average, so quiet spectrum stays dark. Colored marks are what was decoded there.${late ? ` The record began at ${utcClock(s.earliest_minute)} UTC.` : ""}</p>
    <div class="spec-chart">
      <div class="spec-y" aria-hidden="true">${SPECTRUM_BANDS.map(([lo, , name]) => `<span style="top:${yOf(lo).toFixed(2)}%">${name}</span>`).join("")}</div>
      <div class="spec-plot" id="spec-plot" role="img" aria-label="Signal strength above the noise from 2 to 30 MHz over the last 24 hours, with the frequencies decoded marked on it. Hatched areas are frequencies the receiver was not covering.">
        <img id="spec-img" alt="" src="data/spectrum.png?t=${s.generated_utc}">
        <canvas id="spec-marks"></canvas>
        <i class="spec-h" hidden></i><i class="spec-v" hidden></i>
      </div>
      <div class="spec-x" aria-hidden="true">${hours.map((m) => `<span style="left:${(((m - s.first_minute) / s.minutes) * 100).toFixed(2)}%">${utcClock(m).slice(0, 2)}</span>`).join("")}</div>
    </div>
    <p class="spec-read" id="spec-read" aria-live="off">Point at the chart for a reading.</p>
    <div class="key-row spec-key">
      <span><i class="spec-ramp"></i>${s.quiet_db} to ${s.top_db} dB above the noise</span>
      ${modes.map((m) => `<span><i style="background:${modeColor(m)}"></i>${esc(m)}</span>`).join("")}
      <span><i class="k-out"></i>Tuned elsewhere</span>
      <button type="button" class="spec-toggle" id="spec-toggle" aria-pressed="true">Hide decodes</button>
    </div>
  </section>`;
}

function wireSpectrum(s) {
  const plot = document.getElementById("spec-plot");
  if (!plot || !s?.rows) return;
  const [img, canvas, read, toggle] = ["spec-img", "spec-marks", "spec-read", "spec-toggle"].map((id) => document.getElementById(id));
  const [lineH, lineV] = [plot.querySelector(".spec-h"), plot.querySelector(".spec-v")];
  const swatch = document.createElement("span");
  plot.append(swatch);
  const colors = {};
  for (const [, , mode] of s.marks) {
    if (colors[mode]) continue;
    swatch.style.color = modeColor(mode);
    colors[mode] = getComputedStyle(swatch).color;
  }
  swatch.remove();
  const span = s.hi_hz - s.lo_hz;
  let showing = true;
  const draw = () => {
    const { width, height } = plot.getBoundingClientRect();
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    canvas.width = Math.round(width * dpr);
    canvas.height = Math.round(height * dpr);
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    if (!showing) return;
    for (const [x, khz, mode, n] of s.marks) {
      const px = (x / s.minutes) * width;
      const py = (1 - (khz * 1000 - s.lo_hz) / span) * height;
      ctx.fillStyle = "rgba(0,0,0,.6)";
      ctx.fillRect(px - 2, py - 2, 4, 4);
      ctx.globalAlpha = 0.6 + 0.4 * Math.min(1, Math.log10(n + 1) / 2);
      ctx.fillStyle = colors[mode];
      ctx.fillRect(px - 1.25, py - 1.25, 2.5, 2.5);
      ctx.globalAlpha = 1;
    }
  };
  let pixels = null;
  const paletteIndex = new Map(s.palette.map((c, i) => [(c[0] << 16) | (c[1] << 8) | c[2], i]));
  const grab = () => {
    const off = document.createElement("canvas");
    off.width = img.naturalWidth;
    off.height = img.naturalHeight;
    const ctx = off.getContext("2d", { willReadFrequently: true });
    ctx.drawImage(img, 0, 0);
    pixels = ctx.getImageData(0, 0, off.width, off.height).data;
  };
  if (img.complete && img.naturalWidth) grab();
  else img.addEventListener("load", grab, { once: true });
  const level = (column, row) => {
    if (!pixels) return null;
    const at = (row * s.minutes + column) * 4;
    if (!pixels[at + 3]) return "tuned elsewhere";
    const index = paletteIndex.get((pixels[at] << 16) | (pixels[at + 1] << 8) | pixels[at + 2]);
    if (index === undefined || index <= 1) return "at the noise";
    const db = s.quiet_db + ((index - 2) / 253) * (s.top_db - s.quiet_db);
    return index === 255 ? `${s.top_db} dB or more above the noise` : `${Math.round(db)} dB above the noise`;
  };
  const where = (ev) => {
    const box = plot.getBoundingClientRect();
    const fx = Math.min(0.9999, Math.max(0, (ev.clientX - box.left) / box.width));
    const fy = Math.min(0.9999, Math.max(0, (ev.clientY - box.top) / box.height));
    return { fx, fy, x: fx * box.width, y: fy * box.height };
  };
  plot.addEventListener("pointermove", (ev) => {
    const { fx, fy, x, y } = where(ev);
    lineV.hidden = lineH.hidden = false;
    lineV.style.transform = `translateX(${x}px)`;
    lineH.style.transform = `translateY(${y}px)`;
    const column = Math.floor(fx * s.minutes);
    const hz = s.hi_hz - fy * span;
    const mhz = hz / 1e6;
    const band = SPECTRUM_BANDS.find(([lo, hi]) => mhz >= lo && mhz <= hi);
    const heard = {};
    for (const [mx, khz, mode, n] of s.marks) {
      if (Math.abs(mx + 2.5 - column) <= 5 && Math.abs(khz * 1000 - hz) <= 30000) heard[mode] = (heard[mode] || 0) + n;
    }
    const decoded = Object.entries(heard).map(([m, n]) => `${m} ${num(n)}`).join(", ");
    const parts = [`${mhz.toFixed(2)} MHz${band ? ` (${band[2]})` : ""}`, `${utcClock(s.first_minute + column)} UTC`];
    const reading = level(column, Math.floor(fy * s.height));
    if (reading) parts.push(reading);
    if (decoded) parts.push(`decoded: ${decoded}`);
    read.textContent = parts.join(" · ");
  });
  plot.addEventListener("pointerleave", () => {
    lineV.hidden = lineH.hidden = true;
    read.textContent = "Point at the chart for a reading.";
  });
  toggle.addEventListener("click", () => {
    showing = !showing;
    toggle.setAttribute("aria-pressed", String(showing));
    toggle.textContent = showing ? "Hide decodes" : "Show decodes";
    draw();
  });
  new ResizeObserver(draw).observe(plot);
  draw();
}

// Pictures: grouped by UTC day, newest first, with only the latest few days open, so the
// page stays one screen of recent pictures however long the station runs. Older months
// are files of their own, loaded only when asked for.
const PIC_KINDS = { WEFAX: "Weather charts", SSTV: "SSTV" };
const OPEN_DAYS = 3;
// An open day shows this many pictures until asked for the rest.
const DAY_SHOWN = 12;
const MONTH_NAMES = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

// Radiofax is tuned 1.9 kHz below the frequency stations publish; SSTV is listed by its dial.
const picKhz = (p) => khz(p.mode === "WEFAX" ? p.dial_hz + 1900 : p.dial_hz);
const picTitle = (p) => (p.mode === "WEFAX" ? p.scheduled_title || "Untitled chart" : "SSTV picture");
const picAlt = (p) => (p.mode === "WEFAX"
  ? `Radiofax chart received ${fmtTime(p.start_utc)}${p.scheduled_title ? `, scheduled as ${p.scheduled_title}` : ""}`
  : `SSTV picture received ${fmtTime(p.start_utc)} on ${picKhz(p)} kHz`);
const picMeta = (p) => [fmtTime(p.start_utc), p.station, `${picKhz(p)} kHz`].filter(Boolean).join(" · ");
const dayOf = (t) => Math.floor(t / 86400);

function dayLabel(day, today) {
  const d = new Date(day * 86400000);
  const date = `${MONTH_NAMES[d.getUTCMonth()].slice(0, 3)} ${d.getUTCDate()}`;
  return day === today ? `Today, ${date}` : day === today - 1 ? `Yesterday, ${date}` : `${["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"][d.getUTCDay()]}, ${date}`;
}
const monthLabel = (m) => `${MONTH_NAMES[Number(m.slice(5)) - 1]} ${m.slice(0, 4)}`;

function picCard(p, i) {
  return `<article class="pic${p.mode === "SSTV" ? " sstv" : ""}">
    <a href="pictures/${encodeURIComponent(p.file)}" data-pic="${i}"><img loading="lazy" src="pictures/${encodeURIComponent(p.thumb || p.file)}" alt="${esc(picAlt(p))}"></a>
    <div><span class="kind">${esc(p.mode === "WEFAX" ? "Chart" : "SSTV")}</span><b>${esc(picTitle(p))}</b><span>${esc(picMeta(p))}</span></div>
  </article>`;
}

async function showPictures(params) {
  const index = await load("pictures.json");
  shownBuilt = index.generated_utc;
  const state = { kind: PIC_KINDS[params.k] ? params.k : "", station: params.st || "", month: params.mo || "" };
  let pictures = index.pictures;
  if (state.month && index.months?.some((m) => m.month === state.month)) {
    pictures = (await load(`pictures-${state.month}.json`)).pictures;
  } else {
    state.month = "";
  }
  const all = index.months ? index.months.reduce((a, m) => a + m.n, 0) : pictures.length;

  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(index.pictures.map((p) => p.lines))}
    <p class="eyebrow">Pictures</p>
    <h1>Pictures off the air.</h1>
    <p class="lede">Weather charts sent by radiofax, and SSTV pictures sent by radio amateurs, kept as they arrived, noise included. A chart's title comes from the station's timetable, because a chart does not state its own. SSTV pictures are checked by hand before they appear.</p>
    </header>
    <div id="latest-charts"></div>
    <div class="tools pic-tools" id="pictools"></div>
    <p class="note" id="picn" aria-live="polite"></p>
    <div id="days"></div>`;

  const shown = () => pictures.filter((p) => (!state.kind || p.mode === state.kind) && (!state.station || p.station === state.station));
  const save = () => {
    const p = new URLSearchParams();
    if (state.kind) p.set("k", state.kind);
    if (state.station) p.set("st", state.station);
    if (state.month) p.set("mo", state.month);
    history.replaceState(null, "", `#/pictures${p.size ? `?${p}` : ""}`);
  };

  // The newest copy of each chart the stations send, so today's analysis is one look away.
  function drawLatest() {
    const box = view.querySelector("#latest-charts");
    const charts = new Map();
    for (const p of index.pictures) {
      if (p.mode !== "WEFAX" || !p.scheduled_title) continue;
      const key = `${p.station}|${p.scheduled_title}`;
      if (!charts.has(key)) charts.set(key, p);
    }
    if (state.month || state.kind === "SSTV" || charts.size < 2) { box.innerHTML = ""; return; }
    const list = [...charts.values()].filter((p) => !state.station || p.station === state.station).slice(0, 10);
    box.innerHTML = list.length ? `<section class="latest" aria-labelledby="hlatest"><h2 id="hlatest">The latest of each chart</h2>
      <p class="note">The newest copy of every chart received in the last 30 days.</p>
      <div class="latest-row">${list.map((p) => `<a class="latest-card" href="pictures/${encodeURIComponent(p.file)}" data-latest="${esc(p.file)}">
        <img loading="lazy" src="pictures/${encodeURIComponent(p.thumb || p.file)}" alt="${esc(picAlt(p))}"><b>${esc(p.scheduled_title)}</b><span>${esc(p.station || "")} · ${ago(p.start_utc)}</span></a>`).join("")}</div></section>` : "";
    box.querySelectorAll("[data-latest]").forEach((a) => a.addEventListener("click", (ev) => {
      ev.preventDefault();
      openViewer(list.map(viewerItem), list.findIndex((p) => p.file === a.dataset.latest));
    }));
  }

  function drawTools() {
    const kinds = Object.keys(PIC_KINDS).filter((k) => pictures.some((p) => p.mode === k));
    const stations = [...new Set(pictures.filter((p) => p.mode === "WEFAX" && p.station).map((p) => p.station))].sort();
    const count = (pred) => pictures.filter(pred).length;
    view.querySelector("#pictools").innerHTML = `
      <div class="chips"><span class="lbl">Show</span><button type="button" data-k="" aria-pressed="${!state.kind}">Everything <small>${num(pictures.length)}</small></button>${kinds.map((k) => `<button type="button" data-k="${k}" aria-pressed="${state.kind === k}">${PIC_KINDS[k]} <small>${num(count((p) => p.mode === k))}</small></button>`).join("")}</div>
      ${stations.length > 1 && state.kind !== "SSTV" ? `<div class="chips"><span class="lbl">Station</span><button type="button" data-st="" aria-pressed="${!state.station}">All</button>${stations.map((s) => `<button type="button" data-st="${esc(s)}" aria-pressed="${state.station === s}">${esc(s)} <small>${num(count((p) => p.station === s))}</small></button>`).join("")}</div>` : ""}
      ${index.months && index.months.length > 1 ? `<div class="chips"><label class="lbl" for="picmonth">Month</label><select id="picmonth"><option value="">Last 30 days</option>${index.months.map((m) => `<option value="${m.month}"${m.month === state.month ? " selected" : ""}>${monthLabel(m.month)} (${num(m.n)})</option>`).join("")}</select></div>` : ""}`;
  }

  const viewerItem = (p) => ({ src: `pictures/${encodeURIComponent(p.file)}`, alt: picAlt(p), title: `${picTitle(p)} · ${picMeta(p)}`, sstv: p.mode === "SSTV" });

  function drawDays() {
    const list = shown();
    const box = view.querySelector("#days");
    const where = state.month ? `in ${monthLabel(state.month)}` : "in the last 30 days";
    view.querySelector("#picn").textContent = list.length ? `${num(list.length)} picture${list.length === 1 ? "" : "s"} ${where}${all > index.pictures.length && !state.month ? `, ${num(all)} in all; older months are under Month` : ""}.` : "";
    if (!pictures.length) {
      box.innerHTML = `<p class="empty">No pictures ${where} yet. Charts appear once they pass a quality check, and SSTV pictures once they are approved.</p>`;
      return;
    }
    if (!list.length) {
      box.innerHTML = `<p class="empty">No pictures match these filters.</p>`;
      return;
    }
    const today = dayOf(Date.now() / 1000);
    const days = [];
    for (const p of list) {
      const d = dayOf(p.start_utc);
      if (!days.length || days[days.length - 1].day !== d) days.push({ day: d, items: [] });
      days[days.length - 1].items.push(p);
    }
    let at = 0;
    box.innerHTML = days.map((d, n) => {
      const first = at;
      at += d.items.length;
      const tally = Object.keys(PIC_KINDS).map((k) => [k, d.items.filter((p) => p.mode === k).length]).filter(([, c]) => c)
        .map(([k, c]) => `${num(c)} ${k === "WEFAX" ? (c === 1 ? "chart" : "charts") : "SSTV"}`).join(", ");
      const open = n < OPEN_DAYS;
      return `<details class="day"${open ? " open" : ""} data-first="${first}" data-count="${d.items.length}">
        <summary><h2>${dayLabel(d.day, today)}</h2><span class="day-n">${tally}</span>
          <span class="day-peek" aria-hidden="true">${d.items.slice(0, 8).map((p) => `<img loading="lazy" src="pictures/${encodeURIComponent(p.thumb || p.file)}" alt="">`).join("")}</span></summary>
        <div class="gallery">${open ? d.items.slice(0, DAY_SHOWN).map((p, i) => picCard(p, first + i)).join("") : ""}</div>
        ${open && d.items.length > DAY_SHOWN ? `<p class="more-rows"><button type="button" data-rest="${first}">Show the other ${num(d.items.length - DAY_SHOWN)}</button><span>${num(DAY_SHOWN)} of ${num(d.items.length)}</span></p>` : ""}
      </details>`;
    }).join("");
    // An open day's other pictures, when asked for.
    box.querySelectorAll("[data-rest]").forEach((b) => b.addEventListener("click", () => {
      const det = b.closest("details.day");
      const first = Number(det.dataset.first);
      det.querySelector(".gallery").innerHTML = list.slice(first, first + Number(det.dataset.count)).map((p, i) => picCard(p, first + i)).join("");
      b.parentElement.remove();
    }));
    // A closed day fills its gallery the first time it is opened.
    box.querySelectorAll("details.day").forEach((det) => det.addEventListener("toggle", () => {
      const gal = det.querySelector(".gallery");
      if (!det.open || gal.children.length) return;
      const first = Number(det.dataset.first);
      gal.innerHTML = list.slice(first, first + Number(det.dataset.count)).map((p, i) => picCard(p, first + i)).join("");
    }));
    box.onclick = (ev) => {
      const a = ev.target.closest("[data-pic]");
      if (!a) return;
      ev.preventDefault();
      openViewer(list.map(viewerItem), Number(a.dataset.pic));
    };
  }

  view.querySelector("#pictools").addEventListener("click", (ev) => {
    const b = ev.target.closest("button");
    if (!b) return;
    if (b.dataset.k !== undefined) { state.kind = b.dataset.k; if (state.kind === "SSTV") state.station = ""; }
    else if (b.dataset.st !== undefined) state.station = b.dataset.st;
    else return;
    save();
    drawTools();
    drawLatest();
    drawDays();
  });
  view.querySelector("#pictools").addEventListener("change", async (ev) => {
    if (ev.target.id !== "picmonth") return;
    state.month = ev.target.value;
    pictures = state.month ? (await load(`pictures-${state.month}.json`)).pictures : index.pictures;
    state.station = pictures.some((p) => p.station === state.station) ? state.station : "";
    save();
    drawTools();
    drawLatest();
    drawDays();
  });
  drawTools();
  drawLatest();
  drawDays();
}

// A picture at full size in a dialog, stepping through the others with the arrow keys.
function openViewer(items, start) {
  let box = document.getElementById("viewer");
  if (!box) {
    box = document.createElement("dialog");
    box.id = "viewer";
    box.className = "viewer";
    box.innerHTML = `<div class="viewer-bar"><p id="viewer-title"></p><div class="viewer-nav"><a class="viewer-full" target="_blank" rel="noopener">Full size</a><button type="button" data-step="-1" aria-label="Previous picture">Previous</button><button type="button" data-step="1" aria-label="Next picture">Next</button><button type="button" data-close>Close</button></div></div><div class="viewer-pic"><img alt=""></div>`;
    document.body.append(box);
    box.addEventListener("click", (ev) => { if (ev.target === box) box.close(); });
    box.querySelector("[data-close]").addEventListener("click", () => box.close());
  }
  let at = start;
  const show = () => {
    const p = items[at];
    const img = box.querySelector("img");
    img.src = p.src;
    img.alt = p.alt;
    box.querySelector("#viewer-title").textContent = p.title;
    box.querySelector(".viewer-full").href = p.src;
    box.classList.toggle("small-pic", Boolean(p.sstv));
    box.querySelector('[data-step="-1"]').disabled = at === 0;
    box.querySelector('[data-step="1"]').disabled = at === items.length - 1;
  };
  const step = (n) => { if (items[at + n]) { at += n; show(); } };
  box.querySelectorAll("[data-step]").forEach((b) => { b.onclick = () => step(Number(b.dataset.step)); });
  box.onkeydown = (ev) => {
    if (ev.key === "ArrowLeft") step(-1);
    if (ev.key === "ArrowRight") step(1);
  };
  show();
  box.showModal();
}

// The log: one line per station, aircraft, ship and SELCAL code, never one per decode,
// so a busy minute of FT8 cannot push everything else off the page.
const LOG_TABS = [
  { key: "amateur", label: "Amateur stations" },
  { key: "aircraft", label: "Aircraft" },
  { key: "ships", label: "Ships" },
  { key: "selcal", label: "SELCAL" },
];
// "First time in the log" means little until the log has a few days behind it.
const NEW_AFTER_S = 3 * 86400;

const newBadge = (on) => (on ? ` <span class="new-badge">New</span>` : "");
const agoCell = (t) => `<span title="${fmtTime(t)}">${ago(t)}</span>`;

async function showLog(params) {
  const [log, t, status, names] = await Promise.all([
    load("log.json"), load("traffic.json").catch(() => ({ flights: [], ships: [] })), load("status.json").catch(() => ({})), lookups || load("lookups-site.json"),
  ]);
  ground ||= await load("ground-stations.json").catch(() => []);
  lookups = names;
  shownBuilt = log.generated_utc;
  if (!log.amateur) throw new Error("log.json is from before this page; reload in a few minutes");
  const home = status.station_grid ? gridCenter(status.station_grid) : null;
  const gsName = Object.fromEntries(ground.map((s) => [String(s.id), s.name]));
  const showNew = log.log_began && log.log_began < log.since - NEW_AFTER_S;

  const amateur = log.amateur.map(([call, modes, bands, n, first, last, best, grid, isNew]) => {
    const at = grid ? gridCenter(grid) : null;
    return {
      call, n, first, last, best, grid, isNew: Boolean(isNew && showNew),
      modes: log.modes.filter((_, i) => modes & (1 << i)),
      bands: log.bands.filter((_, i) => bands & (1 << i)).reverse(),
      km: at && home ? kmBetween(home, at) : null,
      brg: at && home ? bearing(home, at) : null,
    };
  });
  const flights = t.flights.map((f) => ({ ...f, op: aircraftAirline(f.i, names.airlines), gsn: gsName[f.gs] || "", isNew: Boolean(f.new && showNew) }));
  const ships = t.ships.map((s) => ({ ...s, flag: shipFlag(s, names.flags), call: [s.cat, s.what].filter(Boolean).join(", "), isNew: Boolean(s.new && showNew), km: s.la != null && home ? kmBetween(home, [s.lo, s.la]) : null }));
  const selcal = log.selcal;

  // When stations were last heard, by hour: the header's backdrop.
  const perHour = new Array(24).fill(0);
  for (const r of amateur) {
    const h = Math.floor((log.generated_utc - r.last) / 3600);
    if (h >= 0 && h < 24) perHour[23 - h] += 1;
  }
  const located = amateur.filter((r) => r.km != null);
  const farthest = located.reduce((a, r) => (!a || r.km > a.km ? r : a), null);
  const newCount = amateur.filter((r) => r.isNew).length;
  const counts = { amateur: amateur.length, aircraft: flights.length, ships: ships.length, selcal: selcal.length };
  let tab = LOG_TABS.some((x) => x.key === params.tab) ? params.tab : "amateur";

  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(perHour)}
    <p class="eyebrow">Log</p>
    <h1>Everyone it heard.</h1>
    <p class="lede">One line for each amateur station and aircraft heard in the last 24 hours, and each ship and SELCAL code in the last 7 days. Sort by distance for the far ones, or look up a call to search everything the log has kept.</p>
    ${lookupForm("")}
    </header>
    <dl class="ip-readout">
      <div><dt>Amateur stations</dt><dd>${num(amateur.length)}</dd></div>
      <div><dt>With a locator</dt><dd>${num(located.length)}</dd></div>
      <div><dt>Farthest</dt><dd>${farthest ? `${num(Math.round(farthest.km))} km` : "None"}</dd>${farthest ? `<span class="q"><a href="#/who?c=${encodeURIComponent(farthest.call)}">${esc(farthest.call)}</a> in ${esc(farthest.grid)}</span>` : ""}</div>
      <div><dt>Aircraft</dt><dd>${num(flights.length)}</dd></div>
      <div><dt>Ships, 7 days</dt><dd>${num(ships.length)}</dd></div>
    </dl>
    ${showNew ? (newCount ? `<p class="note log-new">${num(newCount)} amateur station${newCount === 1 ? " is" : "s are"} in the log for the first time today. Filter for them below.</p>` : "") : log.log_began ? `<p class="note log-new">The log began ${fmtTime(log.log_began)}, so nearly everyone is new to it. Stations heard for the first time will be marked once it is ${NEW_AFTER_S / 86400} days old.</p>` : ""}
    <div class="log-tabs" role="tablist" aria-label="What to list">${LOG_TABS.map((x) => `<button type="button" role="tab" data-tab="${x.key}" aria-selected="${x.key === tab}">${x.label}<small>${num(counts[x.key])}</small></button>`).join("")}</div>
    <div class="tools log-tools" id="logtools"></div>
    <p class="note" id="logn" aria-live="polite"></p>
    <div id="logrows"></div>`;
  wireLookup();

  const tools = view.querySelector("#logtools");
  const rowsBox = view.querySelector("#logrows");
  const countLine = view.querySelector("#logn");
  const filter = { q: "", modes: new Set(), bands: new Set(), onlyNew: false, from: "", placed: false };
  const chip = (attr, value, label, on) => `<button type="button" data-${attr}="${esc(value)}" aria-pressed="${on}">${label}</button>`;
  const search = (placeholder) => `<div class="chips"><label class="lbl" for="logq">Filter</label><input id="logq" type="search" autocomplete="off" autocapitalize="characters" spellcheck="false" placeholder="${placeholder}" value="${esc(filter.q)}"></div>`;

  function drawTools() {
    if (tab === "amateur") {
      const modes = log.modes.filter((m) => amateur.some((r) => r.modes.includes(m)));
      const bands = log.bands.filter((b) => amateur.some((r) => r.bands.includes(b))).reverse();
      tools.innerHTML = `${search("Call or square")}
        <div class="chips"><span class="lbl">Mode</span>${modes.map((m) => chip("mode", m, `<i style="background:${modeColor(m)}"></i>${esc(m)}`, filter.modes.has(m))).join("")}</div>
        <div class="chips"><span class="lbl">Band</span>${bands.map((b) => chip("band", b, esc(b), filter.bands.has(b))).join("")}</div>
        ${showNew ? `<div class="chips">${chip("new", "1", `First time today <small>${num(newCount)}</small>`, filter.onlyNew)}</div>` : ""}`;
    } else if (tab === "aircraft") {
      tools.innerHTML = `${search("Flight, operator or registration")}<div class="chips">${chip("placed", "1", "With a position", filter.placed)}</div>`;
    } else if (tab === "ships") {
      tools.innerHTML = `${search("MMSI or flag")}<div class="chips"><span class="lbl">From</span>${chip("from", "", "Both", filter.from === "")}${chip("from", "ship", "Ships", filter.from === "ship")}${chip("from", "coast", "Coast stations", filter.from === "coast")}</div><div class="chips">${chip("placed", "1", "With a position", filter.placed)}</div>`;
    } else {
      tools.innerHTML = search("Code");
    }
    const q = tools.querySelector("#logq");
    q.addEventListener("input", () => { filter.q = q.value.trim().toUpperCase(); drawRows(); });
  }
  tools.addEventListener("click", (ev) => {
    const b = ev.target.closest("button");
    if (!b) return;
    const toggle = (set, v) => (set.has(v) ? set.delete(v) : set.add(v));
    if (b.dataset.mode) toggle(filter.modes, b.dataset.mode);
    else if (b.dataset.band) toggle(filter.bands, b.dataset.band);
    else if (b.dataset.new) filter.onlyNew = !filter.onlyNew;
    else if (b.dataset.placed) filter.placed = !filter.placed;
    else if (b.dataset.from !== undefined) filter.from = b.dataset.from;
    else return;
    drawTools();
    drawRows();
  });

  const has = (...fields) => !filter.q || fields.some((f) => String(f || "").toUpperCase().includes(filter.q));
  const said = (shown, total, what) => { countLine.textContent = shown === total ? `${num(total)} ${what}.` : `${num(shown)} of ${num(total)} ${what} match.`; };
  const empty = (text) => { rowsBox.innerHTML = `<p class="empty">${text}</p>`; };

  function drawRows() {
    if (tab === "amateur") {
      const shown = amateur.filter((r) => has(r.call, r.grid)
        && (!filter.modes.size || r.modes.some((m) => filter.modes.has(m)))
        && (!filter.bands.size || r.bands.some((b) => filter.bands.has(b)))
        && (!filter.onlyNew || r.isNew));
      said(shown.length, amateur.length, "amateur stations in the last 24 hours");
      if (!amateur.length) return empty("No amateur station has been heard in the last 24 hours.");
      if (!shown.length) return empty("No station matches these filters.");
      sortableTable(rowsBox, [
        { label: "Station", value: (r) => r.call, html: (r) => `${callLink(r.modes[0], r.call)}${newBadge(r.isNew)}`, cls: "id", asc: true },
        { label: "Square", value: (r) => r.grid, html: (r) => (r.grid ? `<a href="#/map?w=24h&s=${esc(r.grid)}" title="Show ${esc(r.grid)} on the map">${esc(r.grid)}</a>` : ""), asc: true },
        { label: "km", value: (r) => r.km, html: (r) => (r.km == null ? "" : num(Math.round(r.km))), num: true },
        { label: "Bearing", value: (r) => r.brg, html: (r) => bearingCell(r.brg), cls: "hide-s", asc: true },
        { label: "Bands", value: (r) => r.bands.length, html: (r) => bandPills(r.bands) },
        { label: "Modes", value: (r) => r.modes.join(" "), html: (r) => r.modes.map(esc).join(" "), cls: "hide-s", asc: true },
        { label: "Decodes", value: (r) => r.n, num: true },
        { label: "Best", value: (r) => r.best, html: (r) => (r.best == null ? "" : `${r.best} dB`), num: true, cls: "hide-s" },
        { label: "Last heard", value: (r) => r.last, html: (r) => agoCell(r.last) },
        { label: "First today", value: (r) => r.first, html: (r) => fmtTime(r.first), cls: "hide-s", asc: true },
      ], shown, { sort: 8, page: 100, caption: "Amateur stations heard in the last 24 hours" });
    } else if (tab === "aircraft") {
      const shown = flights.filter((f) => has(f.i, f.op, f.reg) && (!filter.placed || f.la != null));
      said(shown.length, flights.length, "aircraft in the last 24 hours");
      if (!flights.length) return empty("No aircraft has been heard on HFDL in the last 24 hours.");
      if (!shown.length) return empty("No aircraft matches these filters.");
      sortableTable(rowsBox, [
        { label: "Flight", value: (f) => f.i, html: (f) => `${callLink("HFDL", f.i)}${newBadge(f.isNew)}`, cls: "id", asc: true },
        { label: "Operator", value: (f) => f.op || null, html: (f) => esc(f.op), cls: "wrap", asc: true },
        { label: "Registration", value: (f) => f.reg || null, html: (f) => esc(f.reg || ""), cls: "hide-s", asc: true },
        { label: "Ground station", value: (f) => f.gsn || null, html: (f) => esc(f.gsn), cls: "hide-s", asc: true },
        { label: "Messages", value: (f) => f.n, num: true },
        { label: "Last position", value: (f) => (f.la == null ? null : f.la), html: (f) => (f.la == null ? "" : `<a href="#/map?w=24h&l=tracks,ground,night&f=${encodeURIComponent(f.i)}" title="Show this flight on the map">${f.la}, ${f.lo}</a>`), cls: "hide-s" },
        { label: "Last heard", value: (f) => f.t, html: (f) => agoCell(f.t) },
      ], shown, { sort: 6, page: 100, caption: "Aircraft heard on HFDL in the last 24 hours" });
    } else if (tab === "ships") {
      const shown = ships.filter((s) => has(s.i, s.flag) && (!filter.from || s.from === filter.from) && (!filter.placed || s.la != null));
      said(shown.length, ships.length, "ships and coast stations in the last 7 days");
      if (!ships.length) return empty("No DSC call has been heard in the last 7 days.");
      if (!shown.length) return empty("No ship matches these filters.");
      sortableTable(rowsBox, [
        { label: "MMSI", value: (s) => s.i, html: (s) => `${callLink("DSC", s.i)}${newBadge(s.isNew)}`, cls: "id", asc: true },
        { label: "Flag", value: (s) => s.flag || null, html: (s) => esc(s.flag), asc: true },
        { label: "From", value: (s) => s.from || null, html: (s) => (s.from === "coast" ? "Coast station" : s.from === "ship" ? "Ship" : ""), cls: "hide-s", asc: true },
        { label: "Calls", value: (s) => s.n, num: true },
        { label: "Latest call", value: (s) => s.call || null, html: (s) => esc(s.call), cls: "wrap", asc: true },
        { label: "MHz", value: (s) => s.mhz.length, html: (s) => s.mhz.join(", "), cls: "hide-s" },
        { label: "Position", value: (s) => s.km, html: (s) => (s.la == null ? "" : `<a href="#/map?w=7d&l=dsc,night&d=${encodeURIComponent(s.i)}" title="Show this position on the map">${s.la.toFixed(2)}, ${s.lo.toFixed(2)}</a>`), cls: "hide-s" },
        { label: "Last heard", value: (s) => s.t, html: (s) => agoCell(s.t) },
      ], shown, { sort: 7, page: 100, caption: "DSC ship and coast station calls in the last 7 days" });
    } else {
      const shown = selcal.filter((c) => has(c.code));
      said(shown.length, selcal.length, "SELCAL codes in the last 7 days");
      if (!selcal.length) return empty("No SELCAL call has been heard in the last 7 days.");
      if (!shown.length) return empty("No code matches.");
      sortableTable(rowsBox, [
        { label: "Code", value: (c) => c.code, cls: "id", asc: true },
        { label: "Calls", value: (c) => c.n, num: true },
        { label: "kHz", value: (c) => c.khz.length, html: (c) => c.khz.map((k) => k.toFixed(1)).join(", ") },
        { label: "First heard", value: (c) => c.a, html: (c) => fmtTime(c.a), cls: "hide-s", asc: true },
        { label: "Last heard", value: (c) => c.t, html: (c) => agoCell(c.t) },
      ], shown, { sort: 4, page: 100, caption: "SELCAL codes heard in the last 7 days" });
      countLine.textContent += " A code is the aircraft's selective-calling address. The register that would name the aircraft isn't public, so codes stay codes.";
    }
  }

  view.querySelector(".log-tabs").addEventListener("click", (ev) => {
    const b = ev.target.closest("[data-tab]");
    if (!b || b.dataset.tab === tab) return;
    tab = b.dataset.tab;
    filter.q = "";
    filter.placed = false;
    view.querySelectorAll("[data-tab]").forEach((o) => o.setAttribute("aria-selected", String(o === b)));
    history.replaceState(null, "", tab === "amateur" ? "#/log" : `#/log?tab=${tab}`);
    drawTools();
    drawRows();
  });
  drawTools();
  drawRows();
}

function lookupForm(value) {
  return `<form id="lookup" class="lookup" role="search"><label class="lbl" for="lookupq">Look up</label><input id="lookupq" type="search" autocomplete="off" autocapitalize="characters" spellcheck="false" placeholder="Callsign, flight, MMSI or square" value="${esc(value)}"><button type="submit">Search</button></form>`;
}

function wireLookup() {
  view.querySelector("#lookup").addEventListener("submit", (ev) => {
    ev.preventDefault();
    const q = view.querySelector("#lookupq").value.trim().toUpperCase();
    if (q) location.hash = `#/who?c=${encodeURIComponent(q)}`;
  });
}

// The same rule the backend uses to name the lookup files.
const shardOf = (id) => id.slice(0, 2).toUpperCase().replace(/[^A-Z0-9]/g, "_").padEnd(2, "_");

async function loadShard(shard) {
  const r = await fetch(`data/who/${shard}.json?t=${Math.floor(Date.now() / 60000)}`);
  if (r.status === 404) return { stations: {} };
  if (!r.ok) throw new Error(`lookup: ${r.status}`);
  return r.json();
}

function stationPage(id, rows) {
  const sum = (i) => rows.reduce((a, r) => a + r[i], 0);
  const first = Math.min(...rows.map((r) => r[2]));
  const last = Math.max(...rows.map((r) => r[3]));
  const bests = rows.map((r) => r[5]).filter((b) => b !== null);
  const latest = [...rows].sort((a, b) => b[3] - a[3]);
  const squares = [...new Set(latest.map((r) => r[6]).filter(Boolean))];
  return `<h2 class="who-id">${esc(id)}</h2>
    <dl class="ip-readout">
      <div><dt>Decodes</dt><dd>${num(sum(4))}</dd></div>
      <div><dt>First heard</dt><dd class="when">${fmtTime(first)}</dd></div>
      <div><dt>Last heard</dt><dd class="when">${fmtTime(last)}</dd><span class="q">${ago(last)}</span></div>
      <div><dt>Best report</dt><dd>${bests.length ? `${Math.max(...bests)} dB` : "None"}</dd></div>
    </dl>
    <div class="scroll"><table><thead><tr><th>Mode</th><th>Band</th><th>Decodes</th><th>Best</th><th>First heard</th><th>Last heard</th><th>Square</th></tr></thead><tbody>
    ${latest.map((r) => `<tr><td>${modeChip(r[0])}</td><td>${esc(r[1])}</td><td>${num(r[4])}</td><td>${r[5] === null ? "" : `${r[5]} dB`}</td><td>${fmtTime(r[2])}</td><td>${fmtTime(r[3])}</td><td class="id">${r[6] ? `<a href="#/map?w=7d&s=${esc(r[6])}">${esc(r[6])}</a>` : ""}</td></tr>`).join("")}
    </tbody></table></div>
    ${squares.length ? `<p class="note" style="margin-top:12px">The square is the one most recently reported on that band and mode. The map link shows the last 7 days.</p>` : ""}`;
}

async function showWho(params) {
  const q = (params.c || "").trim().toUpperCase();
  if (/^[A-R]{2}\d{2}$/.test(q)) {
    location.replace(`#/map?w=7d&s=${q}`);
    return;
  }
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere([])}
    <p class="eyebrow">Station lookup</p>
    <h1>Was it heard here?</h1>
    <p class="lede">Every amateur station, HFDL flight and DSC ship call in the log, by callsign, flight or MMSI.</p>
    ${lookupForm(q)}
    </header>
    <div id="result"></div>`;
  wireLookup();
  const out = view.querySelector("#result");
  if (q.length < 2) {
    out.innerHTML = q ? `<p class="empty">Enter at least two characters.</p>` : "";
    return;
  }
  const { stations } = await loadShard(shardOf(q));
  if (stations[q]) {
    out.innerHTML = stationPage(q, stations[q]);
    return;
  }
  const near = Object.entries(stations).filter(([id]) => id.startsWith(q)).sort((a, b) => a[0].localeCompare(b[0]));
  out.innerHTML = near.length
    ? `<h2 class="who-id">${num(near.length)} beginning with ${esc(q)}</h2><div class="scroll frame" tabindex="0" role="region" aria-label="Stations beginning with ${esc(q)}"><table><thead><tr><th>Station</th><th>Decodes</th><th>Last heard</th></tr></thead><tbody>
       ${near.slice(0, 200).map(([id, rows]) => `<tr><td class="id"><a href="#/who?c=${encodeURIComponent(id)}">${esc(id)}</a></td><td>${num(rows.reduce((a, r) => a + r[4], 0))}</td><td>${fmtTime(Math.max(...rows.map((r) => r[3])))}</td></tr>`).join("")}
       </tbody></table></div>`
    : `<p class="empty">${esc(q)} is not in this log. The receiver listens to one frequency at a time, so a station missing here may simply have been on the air while it was listening somewhere else.</p>`;
}

// The chain from antenna to log, in the page's own terms. Drawn twice, once for
// each shape of screen, so its text is always a readable size.
function chainSvg(alongside, tall) {
  const label = (x, y, text, cls = "") => `<text x="${x}" y="${y}" text-anchor="middle" class="${cls}">${esc(text)}</text>`;
  const box = (x, y, w, h, lines, sub) => {
    const cx = x + w / 2;
    const first = y + h / 2 + (lines.length === 2 || sub ? -3 : 5);
    return `<rect class="cn" x="${x}" y="${y}" width="${w}" height="${h}" rx="4"/>${lines.map((t, i) => label(cx, first + i * 18, t)).join("")}${sub ? label(cx, first + 18, sub, "cs") : ""}`;
  };
  const arrow = (d, tipX, tipY, dir) => {
    const heads = { r: `M${tipX - 7} ${tipY - 4}L${tipX} ${tipY}L${tipX - 7} ${tipY + 4}`, d: `M${tipX - 4} ${tipY - 7}L${tipX} ${tipY}L${tipX + 4} ${tipY - 7}` };
    return `<path class="cl" d="${d}"/><path class="cl" d="${heads[dir]}"/>`;
  };
  const flow = (d, delay) => (calm ? "" : `<circle r="2.6" class="cf"><animateMotion dur="4.2s" begin="${delay}s" repeatCount="indefinite" path="${d}"/></circle>`);
  if (!tall) {
    const upper = "M152 92H590V46H900V92H1020";
    const lower = "M152 92H590V138H900V92H1020";
    return `<svg class="chain wide" viewBox="0 0 1040 200" role="img" aria-label="The signal path: a wire loop antenna feeds one receiver, which takes in 8 MHz at a time. A scheduled decoder and channels alongside it read that capture, and both write to a validated log.">
      <circle class="cl" cx="90" cy="92" r="62"/><path class="cl" d="M90 154V176M50 176H130"/>${label(90, 88, "Wire loop")}${label(90, 106, "antenna")}
      ${arrow("M152 92H270", 270, 92, "r")}${label(211, 80, "below 30 MHz", "cs")}
      ${box(270, 62, 110, 60, ["uSDR", "receiver"])}${arrow("M380 92H430", 430, 92, "r")}
      ${box(430, 62, 110, 60, ["8 MHz", "capture"])}
      <path class="cl" d="M540 92H590M590 46V138"/>${arrow("M590 46H620", 620, 46, "r")}${arrow("M590 138H620", 620, 138, "r")}
      ${box(620, 22, 230, 48, ["Scheduled decoder"], "one mode, one frequency")}
      ${box(620, 114, 230, 48, ["Channels alongside"], alongside)}
      <path class="cl" d="M850 46H870V138H850M870 92H900"/>${arrow("M870 92H900", 900, 92, "r")}
      ${box(900, 62, 120, 60, ["Validated", "log"])}
      ${flow(upper, 0)}${flow(lower, 2.1)}
    </svg>`;
  }
  const left = "M170 122V320H90V450H170V520";
  const right = "M170 122V320H250V450H170V520";
  return `<svg class="chain tall" viewBox="0 0 340 540" role="img" aria-label="The signal path: a wire loop antenna feeds one receiver, which takes in 8 MHz at a time. A scheduled decoder and channels alongside it read that capture, and both write to a validated log.">
    <circle class="cl" cx="170" cy="66" r="56"/>${label(170, 62, "Wire loop")}${label(170, 80, "antenna")}
    ${arrow("M170 122V162", 170, 162, "d")}${label(246, 146, "below 30 MHz", "cs")}
    ${box(90, 162, 160, 48, ["uSDR receiver"])}${arrow("M170 210V242", 170, 242, "d")}
    ${box(90, 242, 160, 48, ["8 MHz capture"])}
    <path class="cl" d="M170 290V320M90 320H250"/>${arrow("M90 320V342", 90, 342, "d")}${arrow("M250 320V342", 250, 342, "d")}
    ${box(8, 342, 160, 64, ["Scheduled decoder"], "one at a time")}${box(172, 342, 160, 64, ["Channels alongside"], alongside)}
    <path class="cl" d="M90 406V450M250 406V450M90 450H250"/>${arrow("M170 450V472", 170, 472, "d")}
    ${box(90, 472, 160, 48, ["Validated log"])}
    ${flow(left, 0)}${flow(right, 2.1)}
  </svg>`;
}

function timelineHtml(h) {
  const span = h.generated_utc - h.since;
  const pos = (t) => Math.max(0, Math.min(100, ((t - h.since) / span) * 100));
  const hoursBack = [];
  for (let t = Math.ceil(h.since / 10800) * 10800; t < h.generated_utc; t += 10800) hoursBack.push(`<span style="left:${pos(t).toFixed(2)}%">${String(new Date(t * 1000).getUTCHours()).padStart(2, "0")}</span>`);
  const main = h.timeline.filter((r) => r[6] !== "wideband");
  const wide = h.timeline.filter((r) => r[6] === "wideband");
  const bar = ([a, b, mode, dial, healthy, decodes], extra) => {
    const left = pos(a);
    const note = healthy === 0 ? "failed its radio check" : decodes ? `${num(decodes)} decodes` : "nothing decoded";
    return `<i class="${extra}" style="left:${left.toFixed(2)}%;width:${Math.max(0.1, pos(b) - left).toFixed(2)}%;--c:${modeColor(mode)}" data-tip="${esc(mode)} ${khz(dial)} kHz · ${clock(a)} to ${clock(b)} · ${note}"></i>`;
  };
  const modes = [...new Set(h.timeline.map((r) => r[2]))].sort((x, y) => (MODE_SLOT[x] || 9) - (MODE_SLOT[y] || 9));
  return `<div class="key-row">${modes.map((m) => `<span><i style="background:${modeColor(m)}"></i>${esc(m)}</span>`).join("")}<span><i class="k-fail"></i>Failed check</span><span><i class="k-out"></i>Not monitored</span></div>
    <div class="tl lanes">
      <div class="tl-label">Tuned to</div><div class="tl-track big">${main.map((r) => bar(r, r[4] === 0 ? "bad" : "")).join("")}</div>
      <div class="tl-label">Heard</div><div class="tl-track thin">${main.map((r) => bar(r, r[5] ? "heard" : "quiet")).join("")}</div>
      <div class="tl-label">Alongside</div><div class="tl-track thin">${wide.map((r) => bar(r, "")).join("")}</div>
      <div></div><div class="tl-ticks">${hoursBack.join("")}</div>
    </div><p class="fig-read heat-read" aria-live="off">Point at a stretch for a reading.</p>`;
}

const hexRgb = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
function rampColor(f) {
  const x = Math.max(0, Math.min(0.9999, f)) * (STEPS.length - 1);
  const i = Math.floor(x);
  const [a, b] = [hexRgb(STEPS[i]), hexRgb(STEPS[i + 1])];
  return `rgb(${a.map((v, k) => Math.round(v + (b[k] - v) * (x - i))).join(",")})`;
}

// The amateur bands, top to bottom by frequency; the other dials say less at a glance.
function noiseHtml(bands) {
  const ham = bands.filter((b) => / m$/.test(b.label));
  const values = ham.flatMap((b) => b.cells.filter(Boolean).map((c) => c[0]));
  if (!values.length) return `<p class="empty">No noise measurements yet.</p>`;
  const [low, high] = [Math.min(...values), Math.max(...values)];
  const clipped = bands.flatMap((b) => b.cells.filter((c) => c && c[1] >= 0.99)).length;
  return `<div class="scroll"><div class="heat">
    ${ham.map((b) => `<div class="rowlabel">${esc(b.label)}</div><div class="cells24">${b.cells.map((c, h) => {
      const at = `${b.label}, ${String(h).padStart(2, "0")}Z`;
      return c ? `<i style="background:${rampColor(high === low ? 0 : (c[0] - low) / (high - low))}" data-tip="${esc(at)}: ${c[0]} dBFS on average, peak ${c[1]} of full scale, ${hours(c[2])} measured"></i>` : `<i class="out" data-tip="${esc(at)}: not monitored"></i>`;
    }).join("")}</div>`).join("")}
    <div class="hours">${Array.from({ length: 24 }, (_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`).join("")}</div>
  </div></div><p class="fig-read heat-read" aria-live="off">Point at a cell for a reading. Hours are UTC.</p>
  <div class="ramp-key"><span>Lower, ${low} dBFS</span><i style="background:linear-gradient(90deg,${STEPS.join(",")})"></i><span>Higher, ${high} dBFS</span><span class="ramp-note">${clipped ? `${num(clipped)} band-hours reached full scale` : "No band-hour reached full scale"}</span></div>`;
}

function aircraftAirline(flight, table) {
  const m = /^([A-Z]{3})\d/.exec(flight);
  return m && table[m[1]] ? table[m[1]] : "";
}

function shipFlag(ship, table) {
  const id = ship.i || "";
  const mid = ship.mid || (id.startsWith("00") ? id.slice(2, 5) : id.startsWith("0") ? id.slice(1, 4) : id.slice(0, 3));
  return table[mid] || "";
}

const USDR_SHOTS = [
  { file: "usdr-ft8.webp", title: "uSDR desktop, FT8 on 20 meters", caption: "FT8 on 20 meters: every signal in the passband, decoded together.", alt: "The uSDR desktop app tuned to FT8 on 14.074 MHz, with spectrum, waterfall and a table of decoded stations." },
  { file: "usdr-wwv.webp", title: "uSDR desktop, WWV at 10 MHz", caption: "WWV at 10 MHz, the time code read off the air. The station uses this signal for its radio check.", alt: "The uSDR desktop app tuned to WWV on 10 MHz, showing the spectrum and the decoded time code." },
];

const NOW_WORDS = { monitoring: "Monitoring", chart: "Receiving a chart", checking: "Radio check", idle: "Between slots", manual: "In use by hand", off: "Schedule off", failed: "Radio check failing" };

async function showStation() {
  const [health, status, nowFile] = await Promise.all([
    load("health.json").catch(() => null), load("status.json").catch(() => null), load("now.json").catch(() => null),
  ]);
  if (health) shownBuilt = health.generated_utc;
  const listening = new Array(96).fill(0);
  if (health) {
    for (const [a, b] of health.timeline) {
      const i = Math.floor(((a - health.since) / (health.generated_utc - health.since)) * 96);
      if (i >= 0 && i < 96) listening[i] += b - a;
    }
  }
  const fresh = nowFile && Date.now() / 1000 - nowFile.at < 300;
  const glyph = (name) => `<svg class="ip-i sm" aria-hidden="true"><use href="brand/sprite.svg#ip-s-${name}"/></svg>`;
  const c = health?.checks;
  // Modes only: with a channel on every band, the full list would not fit in the diagram.
  const alongside = (health?.alongside || "").split(", ").map((s) => s.split(" ")[0]).filter(Boolean).join(" ");
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(listening)}
    <p class="eyebrow">Station</p>
    <h1>Inside the listening station.</h1>
    </header>
    <div class="station-top">
      <div class="card chain-card">
        ${chainSvg(alongside, false)}${chainSvg(alongside, true)}
        <ol class="chain-steps">
          <li><b>Receive.</b> A large wire loop feeds one software-defined receiver covering HF, run by uSDR.</li>
          <li><b>Capture.</b> It takes in 8 MHz of spectrum at a time, and only receives.</li>
          <li><b>Decode.</b> A schedule picks what to tune and decode, while channels alongside keep listening inside the same capture.</li>
          <li><b>Log.</b> Only messages that pass their own checks are written to the log.</li>
        </ol>
      </div>
      <div class="station-about">
        <h2>One receiver, a scheduled scan.</h2>
        <p class="note-lead">It runs unattended on uSDR. A schedule decides where it tunes and what it decodes there, and channels alongside it decode the other digital frequencies that fall inside the capture.</p>
        <p class="ip-notice ip-notice-warn" role="note">${glyph("warn")}<span><b>This is not a distress monitor.</b> DSC calls appear as logged, the receiver is only there part of the time, and nobody is watching it. If someone is in trouble at sea, contact a rescue coordination center.</span></p>
      </div>
    </div>
    <section aria-labelledby="husdr" class="usdr-sec">
      <h2 id="husdr" data-rail="uSDR">Driven by uSDR</h2>
      <p class="lede">Everything here is received and decoded by <a href="https://www.innerpulse.net/portfolio/#p-usdr">uSDR</a>, a software-defined radio suite from innerpulse. A daemon sits beside the receiver and does the listening, decoding and logging on its own, and the desktop app connects to it for looking and tuning by hand.</p>
      <div class="shots">
        ${USDR_SHOTS.map((s, i) => `<figure><a href="shots/${s.file}" data-shot="${i}"><img loading="lazy" src="shots/${s.file}" width="2000" height="1210" alt="${esc(s.alt)}"></a><figcaption>${esc(s.caption)}</figcaption></figure>`).join("")}
      </div>
    </section>
    ${health ? `<section aria-labelledby="hhealth"><h2 id="hhealth" data-rail="Health">Station health and activity</h2>
      <div class="health-grid">
        <div class="health-main">
          <div class="card"><h3>Sessions, last 24 hours, UTC</h3>${health.timeline.length ? timelineHtml(health) : emptyLog()}</div>
          <div class="card" style="margin-top:12px"><h3>Noise by band and UTC hour, up to 14 days</h3><p class="note" style="margin-top:6px">As the receiver measures it, not calibrated to a field strength.</p>${noiseHtml(health.bands)}</div>
        </div>
        <div class="health-side">
          <div class="card"><h3>Station status</h3>
            <dl class="kv">
              <dt>Now</dt><dd class="wrap">${fresh ? esc(NOW_WORDS[nowFile.state] || nowFile.state) : "No recent report"}</dd>
              ${fresh && nowFile.label ? `<dt>On</dt><dd class="wrap">${esc(nowFile.label)}</dd>` : ""}
              <dt>Receiver</dt><dd>1, receive-only</dd>
              <dt>Alongside</dt><dd class="wrap">${esc(health.alongside || "None yet")}</dd>
              ${status ? `<dt>Listening, 24 h</dt><dd>${hours(status.monitored_s)}</dd>` : ""}
              ${status?.station_grid ? `<dt>Location, coarse</dt><dd>${esc(status.station_grid)}</dd>` : ""}
            </dl>
          </div>
          ${c ? `<div class="card checks"><h3>Radio checks, 24 hours</h3>
            ${c.passed + c.failed ? `<p>${glyph(c.failed ? "warn" : "ok")}<span>Stretches that passed</span><b>${num(c.passed)} of ${num(c.passed + c.failed)}</b></p>
            <p>${glyph(c.failed ? "warn" : "ok")}<span>Left out of the figures</span><b>${num(c.failed)}</b></p>` : `<p>${glyph("idle")}<span>No stretch has been judged yet</span></p>`}
          </div>` : ""}
        </div>
      </div>
    </section>` : ""}
    <section aria-labelledby="hnotes" class="notes-sec">
      <h2 id="hnotes" data-rail="Notes">Reading the log</h2>
      <div class="notes">
        <article><h3>What it listens to</h3><p>Amateur digital modes (FT8, FT4, JS8 and WSPR), HFDL aircraft datalink, DSC ship calls, SELCAL, radiofax weather charts, SSTV and the NCDXF/IARU beacons, all below 30 MHz.</p></article>
        <article><h3>How reception is checked</h3><p>Once an hour the receiver checks itself against a time-signal station. A stretch that failed the check or overloaded the receiver is left out of the figures.</p></article>
        <article><h3>What stays private</h3><p>The content of aircraft messages is not published. The station itself stays anonymous, and its position is given only as a four-character grid square. Radio amateurs heard on the air appear by their callsigns and locators, as they do on any public spotting network.</p></article>
        <article class="wide"><h3>What an empty map means</h3><div class="two"><p>Most frequencies are unattended at any given moment. Every stretch of listening is logged, pages divide counts by that time, and hours with none are hatched: an empty cell and a hatched cell mean different things.</p><p>A wire loop this long has a lobed pattern on the higher bands, strong in some directions and weak in others, so a region with no spots does not show the band was closed to it.</p></div></article>
        <article><h3>How aircraft tracks are drawn</h3><p>Aircraft queue position reports and sometimes send several together. Tracks follow each fix's own timestamp, and split where two fixes could not belong to one flight.</p></article>
      </div>
    </section>`;
  const shots = USDR_SHOTS.map((s) => ({ src: `shots/${s.file}`, alt: s.alt, title: s.title }));
  view.querySelectorAll("[data-shot]").forEach((a) => a.addEventListener("click", (ev) => {
    ev.preventDefault();
    openViewer(shots, Number(a.dataset.shot));
  }));
}

const VIEWS = { now: showNow, map: showMap, activity: showActivity, pictures: showPictures, log: showLog, station: showStation, who: showWho };

const LOADING = `<div class="ip-skeleton-block" style="height:64px;max-width:520px"></div><div class="ip-skeleton-block" style="height:320px;margin-top:20px"></div>`;

async function render(entering = true) {
  if (unmount) { unmount(); unmount = null; }
  const { name, params } = route();
  const show = VIEWS[name] || showNow;
  document.querySelectorAll("nav a").forEach((a) => {
    if (a.dataset.view === (name === "who" ? "log" : VIEWS[name] ? name : "now")) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  moveTick();
  const root = document.documentElement;
  root.classList.add("ip-busy");
  const loading = entering ? setTimeout(() => { view.innerHTML = LOADING; }, 180) : 0;
  try {
    await show(params);
    clearTimeout(loading);
    dress(view, entering);
  } catch (e) {
    clearTimeout(loading);
    view.innerHTML = `<p class="empty">The data for this page could not be loaded (${esc(e.message)}).</p>`;
  }
  root.classList.remove("ip-busy");
}

addEventListener("resize", moveTick);
document.fonts?.ready.then(moveTick);
document.addEventListener("visibilitychange", () => document.documentElement.classList.toggle("ip-paused", document.hidden));

async function freshness() {
  const out = document.getElementById("fresh");
  drawNowLine();
  const glyph = (name) => `<svg aria-hidden="true"><use href="brand/sprite.svg#ip-s-${name}"/></svg>`;
  try {
    const s = await load("status.json");
    const seen = s.last_ingest_utc;
    if (!seen) {
      out.innerHTML = `<span class="ip-status ip-status-idle">${glyph("idle")}No data yet</span>`;
      return;
    }
    const age = Date.now() / 1000 - seen;
    const state = age < STALE_S ? `<span class="ip-live">Receiving</span>` : `<span class="ip-status ip-status-warn">${glyph("warn")}Stale</span>`;
    out.innerHTML = `${state}<span class="age">last batch ${fmtClock(seen)}Z, ${ago(seen)}</span>`;
    // Newer figures exist. Now has nothing to lose by redrawing; elsewhere that is the visitor's call.
    if (shownBuilt && s.generated_utc > shownBuilt && !document.hidden) {
      if (route().name === "now") {
        const y = scrollY;
        await render(false);
        scrollTo(0, y);
      } else {
        const refresh = document.createElement("button");
        refresh.type = "button";
        refresh.textContent = "New data";
        refresh.title = `This page was built ${ago(shownBuilt)}. Load the newer figures.`;
        refresh.addEventListener("click", () => render(false));
        out.prepend(refresh);
      }
    }
  } catch {
    out.innerHTML = `<span class="ip-status ip-status-warn">${glyph("warn")}Status unavailable</span>`;
  }
}

// The phone menu closes once a link has been followed.
const bar = document.querySelector(".ip-masthead-bar");
const menu = bar.querySelector(".ip-masthead-menu");
menu.addEventListener("click", () => menu.setAttribute("aria-expanded", bar.classList.toggle("is-open")));
bar.querySelector("nav").addEventListener("click", () => {
  bar.classList.remove("is-open");
  menu.setAttribute("aria-expanded", "false");
});

// Every time on the site is UTC, so the masthead keeps the clock.
const utc = document.getElementById("utc");
if (utc) {
  const tick = () => { if (!document.hidden) utc.textContent = fmtClock(Date.now() / 1000).slice(0, 5); };
  tick();
  setInterval(tick, 5000);
}

// The lookup box in the masthead; "/" reaches it from anywhere.
const find = document.getElementById("find");
if (find) {
  const box = find.querySelector("input");
  find.addEventListener("submit", (ev) => {
    ev.preventDefault();
    const q = box.value.trim().toUpperCase();
    if (!q) return;
    box.value = "";
    box.blur();
    location.hash = `#/who?c=${encodeURIComponent(q)}`;
  });
  addEventListener("keydown", (ev) => {
    if (ev.key !== "/" || ev.ctrlKey || ev.metaKey || ev.altKey || /^(INPUT|TEXTAREA|SELECT)$/.test(ev.target.tagName)) return;
    if (!find.offsetParent) return;
    ev.preventDefault();
    box.focus();
  });
}

let lastRoute = "";
function hashChanged() {
  const { name, params } = route();
  // Traffic was a page of its own; it is now the map with aircraft and ships on.
  if (name === "traffic") {
    const p = new URLSearchParams({ w: params.w === "7d" ? "7d" : "24h", l: "tracks,ground,dsc,night" });
    if (params.f) p.set("f", params.f);
    lastRoute = "";
    location.replace(`#/map?${p}`);
    return;
  }
  const key = `${name}|${name === "map" ? params.w || "24h" : name === "who" ? params.c || "" : ""}`;
  if (key === lastRoute) return;
  // A new page starts at its top; the first one keeps wherever the browser restored.
  if (lastRoute) scrollTo(0, 0);
  lastRoute = key;
  render();
}
addEventListener("hashchange", hashChanged);
hashChanged();
freshness();
setInterval(freshness, 60000);
