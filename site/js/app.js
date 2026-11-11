import { bandGroup, BAND_GROUPS, bearing, gridCenter, SPOT_SIZES, mountMap, STEPS } from "./map.js";
import { mountGlobe } from "./globe.js";
import { atmosphere, calm, dress, moveTick } from "./motion.js";
import { ago, esc, fmtClock, fmtTime, hours, khz, num } from "./util.js";
import { callLink, dialFigure, modeBoard, modeChip, modeColor, MODE_SLOT, rowsTable, wireReadout } from "./figures.js";

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
        ${s.latest.length ? `${rowsTable(s.latest.slice(0, 14), { compact: true })}<p class="more"><a class="go" href="#/log">The latest 500 entries</a></p>` : emptyLog()}
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

function mapShell(w) {
  return `
    <div class="map-head"><p class="eyebrow">The map</p><h1>Where it heard from.</h1></div>
    <details class="filters" id="filters" open><summary>Filters</summary>
    <div class="tools">
      <div class="chips"><span class="lbl">Window</span><button type="button" data-w="24h" aria-pressed="${w === "24h"}">24 hours</button><button type="button" data-w="7d" aria-pressed="${w === "7d"}">7 days</button></div>
      <div class="chips"><span class="lbl">Projection</span><button type="button" data-proj="eq">Flat</button><button type="button" data-proj="az">From the station</button></div>
      <div class="chips" id="presets"><span class="lbl">Go to</span></div>
    </div>
    <div class="tools"><div class="chips" id="bands"><span class="lbl">Band</span></div><div class="chips" id="modes"><span class="lbl">Mode</span></div></div>
    </details>
    <div class="map-wrap" style="margin-top:6px">
      <div class="map-main">
        <div class="stage">
          <div class="zoomer"><button type="button" id="zin" aria-label="Zoom in">+</button><button type="button" id="zout" aria-label="Zoom out">−</button><button type="button" id="box" aria-pressed="false" aria-label="Drag a box to zoom to an area" title="Drag a box to zoom to an area">▭</button></div>
        </div>
        <div class="strip-wrap">
          <svg id="strip" class="strip" role="img" aria-label="Spots per hour. Hatched hours had no listening."></svg>
          <div class="range"><label for="from">From</label><input id="from" type="range" min="0" step="1"><label for="to">To</label><input id="to" type="range" step="1"></div>
          <div class="strip-foot"><button type="button" id="play" aria-pressed="false">Play</button><p class="note"><span id="fromv" class="num"></span> to <span id="tov" class="num"></span>. Hatched hours had no listening at all, which is not the same as a quiet band.</p></div>
        </div>
        <div class="card" id="square" aria-live="polite"></div>
      </div>
      <div class="side">
        <div class="card" id="readout"><h3>Under the pointer</h3><p class="note" style="margin-top:8px">Point at a dot or a flight. Scroll to zoom, drag to pan, or use ▭ to drag out an area (shift-drag does the same).</p></div>
        <div class="card" id="totals"></div>
        <div class="card"><h3>Reading the dots</h3>
          <div class="key">
            <div class="row" aria-label="Dot size is the number of different stations heard in that square">${SPOT_SIZES.map((s) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="${s.px / 2}" fill="var(--innerpulse-text-2)"/></svg>${s.label}</span>`).join("")}</div>
            <div class="row" aria-label="Dot color is the band group heard most in that square">${BAND_GROUPS.map((b, i) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="5" fill="var(--innerpulse-chart-${i + 1})"/></svg>${b}</span>`).join("")}</div>
          </div>
          <p class="note" style="margin-top:8px">One dot per grid square, at the square's center. Size is different stations heard; color is the bands with the most decodes there. A pulsing dot was heard in the last hour of the log.</p>
          <div class="layers">
            <label class="ip-check"><input type="checkbox" data-layer="squares" checked><span>Amateur stations</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="tracks" checked><svg viewBox="0 0 18 12" aria-hidden="true"><path d="M1 10 L9 6" fill="none" stroke="var(--innerpulse-text-2)" stroke-width="1"/><path d="M17 2 L8.5 3.2 L10.6 6.5 L10.2 10.4Z" fill="var(--innerpulse-text)"/></svg><span>Aircraft positions</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="ground" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="4.5" y="1.5" width="9" height="9" fill="none" stroke="var(--innerpulse-text)" stroke-width="1.5"/></svg><span>HFDL ground stations</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="dsc" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="6" y="3" width="6" height="6" transform="rotate(45 9 6)" fill="var(--innerpulse-text)"/></svg><span>Ship calls</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="night" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x=".5" y=".5" width="17" height="11" fill="#020305" stroke="var(--innerpulse-line-2)"/></svg><span>Night side, at the end of these hours</span></label>
          </div>
        </div>
      </div>
    </div>`;
}

async function showMap(params) {
  const w = params.w === "7d" ? "7d" : "24h";
  [coast, land, ground] = await Promise.all([coast || load("coast-110m.json"), land || load("land-110m.json"), ground || load("ground-stations.json")]);
  const data = await load(`map-${w}.json`);
  shownBuilt = data.generated_utc;
  view.innerHTML = mapShell(w);
  view.querySelector("#filters").open = !matchMedia("(max-width: 760px)").matches;
  view.querySelectorAll("[data-w]").forEach((b) => b.addEventListener("click", () => {
    location.hash = `#/map?w=${b.dataset.w}`;
  }));
  unmount = mountMap(view, data, {
    coast, land, ground, params,
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
      ${a.selcal.length ? `<div class="scroll"><table><thead><tr><th>Time</th><th>Code</th><th>kHz</th></tr></thead><tbody>${a.selcal.map((c) => `<tr><td>${fmtTime(c.t)}</td><td class="id">${esc(c.code)}</td><td>${khz(c.dial_hz)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">No SELCAL calls logged.</p>`}
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

async function showPictures() {
  const { pictures, generated_utc: built } = await load("pictures.json");
  shownBuilt = built;
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(pictures.map((p) => p.lines))}
    <p class="eyebrow">Pictures</p>
    <h1>Weather charts off the air.</h1>
    <p class="lede">Received as radiofax and kept as they arrived, noise included. Titles come from the transmitting station's timetable, because a chart does not state its own.</p>
    </header>
    ${pictures.length ? `<div class="gallery">${pictures.map((p) => `<article class="pic">
      <a href="pictures/${encodeURIComponent(p.file)}"><img loading="lazy" src="pictures/${encodeURIComponent(p.thumb || p.file)}" alt="Radiofax chart received ${fmtTime(p.start_utc)}${p.scheduled_title ? `, scheduled as ${esc(p.scheduled_title)}` : ""}"></a>
      <div><b>${esc(p.scheduled_title || "Untitled picture")}</b><span>${fmtTime(p.start_utc)} · ${esc(p.station || "")} · ${khz(p.dial_hz + 1900)} kHz</span></div>
    </article>`).join("")}</div>` : `<p class="empty">No charts have been published yet. Charts appear here once they pass a quality check.</p>`}`;
  view.querySelectorAll(".pic a").forEach((a, i) => a.addEventListener("click", (ev) => {
    ev.preventDefault();
    openViewer(pictures.map((p) => ({
      src: `pictures/${encodeURIComponent(p.file)}`,
      alt: `Radiofax chart received ${fmtTime(p.start_utc)}${p.scheduled_title ? `, scheduled as ${p.scheduled_title}` : ""}`,
      title: `${p.scheduled_title || "Untitled picture"} · ${fmtTime(p.start_utc)}`,
    })), i);
  }));
}

// A picture at full size in a dialog, stepping through the others with the arrow keys.
function openViewer(items, start) {
  let box = document.getElementById("viewer");
  if (!box) {
    box = document.createElement("dialog");
    box.id = "viewer";
    box.className = "viewer";
    box.innerHTML = `<div class="viewer-bar"><p id="viewer-title"></p><div class="viewer-nav"><button type="button" data-step="-1" aria-label="Previous picture">Previous</button><button type="button" data-step="1" aria-label="Next picture">Next</button><button type="button" data-close>Close</button></div></div><div class="viewer-pic"><img alt=""></div>`;
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

function minuteCounts(rows) {
  if (!rows.length) return [];
  const newest = rows[0].t;
  const buckets = new Array(Math.min(96, Math.floor((newest - rows[rows.length - 1].t) / 60) + 1)).fill(0);
  for (const r of rows) {
    const i = buckets.length - 1 - Math.floor((newest - r.t) / 60);
    if (i >= 0) buckets[i] += 1;
  }
  return buckets;
}

async function showLog() {
  const { rows, generated_utc: built } = await load("log.json");
  shownBuilt = built;
  const counts = {};
  for (const r of rows) counts[r.m] = (counts[r.m] || 0) + 1;
  const modes = Object.keys(counts).sort((a, b) => counts[b] - counts[a]);
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(minuteCounts(rows))}
    <p class="eyebrow">Log</p>
    <h1>The latest 500 entries.</h1>
    <p class="lede">${rows.length ? `These reach back to ${fmtTime(rows[rows.length - 1].t)}. Look up a station to search everything the log has kept.` : "Nothing has arrived from the receiver yet."}</p>
    ${lookupForm("")}
    </header>
    <div class="tools log-tools">
      <div class="chips" id="logmodes" role="group" aria-label="Mode"><span class="lbl">Mode</span><button type="button" data-m="" aria-pressed="true">All <small>${num(rows.length)}</small></button>${modes.map((m) => `<button type="button" data-m="${esc(m)}" aria-pressed="false"><i style="background:${modeColor(m)}"></i>${esc(m)} <small>${num(counts[m])}</small></button>`).join("")}</div>
      <div class="chips"><label class="lbl" for="logq">Filter</label><input id="logq" type="search" placeholder="Call or square, within these rows"></div>
      <p class="note" id="logn" aria-live="polite"></p>
    </div>
    <div id="logrows"></div>`;
  let mode = "";
  const draw = () => {
    const q = view.querySelector("#logq").value.trim().toUpperCase();
    const shown = rows.filter((r) => (!mode || r.m === mode) && (!q || `${r.i || ""} ${r.g || ""}`.toUpperCase().includes(q)));
    view.querySelector("#logn").textContent = shown.length === rows.length ? "" : `${num(shown.length)} of ${num(rows.length)} shown`;
    view.querySelector("#logrows").innerHTML = shown.length ? rowsTable(shown.slice(0, 500), { sticky: true }) : rows.length ? `<p class="empty">No entries match.</p>` : emptyLog();
  };
  wireLookup();
  view.querySelectorAll("#logmodes button").forEach((b) => b.addEventListener("click", () => {
    mode = b.dataset.m;
    view.querySelectorAll("#logmodes button").forEach((o) => o.setAttribute("aria-pressed", String(o === b)));
    draw();
  }));
  view.querySelector("#logq").addEventListener("input", draw);
  draw();
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
    ? `<h2 class="who-id">${num(near.length)} beginning with ${esc(q)}</h2><div class="scroll"><table><thead><tr><th>Station</th><th>Decodes</th><th>Last heard</th></tr></thead><tbody>
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

const TRAFFIC_VIEWS = {
  World: null,
  "N. Atlantic": [-80, 25, -5, 62],
  "N. America": [-130, 20, -60, 56],
  Caribbean: [-92, 8, -55, 30],
  Europe: [-12, 35, 42, 66],
};

function trafficMap(w) {
  return `<section class="traffic-map" aria-labelledby="hmap"><h2 id="hmap" data-rail="Map">Aircraft and ships on the map</h2>
    <p class="note">Point anywhere along a track for that flight's details. Click to keep it selected.</p>
    <div class="tools" style="margin-top:12px">
      <div class="chips"><span class="lbl">Window</span><button type="button" data-w="24h" aria-pressed="${w === "24h"}">24 hours</button><button type="button" data-w="7d" aria-pressed="${w === "7d"}">7 days</button></div>
      <div class="chips"><span class="lbl">Projection</span><button type="button" data-proj="eq">Flat</button><button type="button" data-proj="az">From the station</button></div>
      <div class="chips" id="presets"><span class="lbl">Go to</span></div>
    </div>
    <div class="stage tall">
      <div class="zoomer"><button type="button" id="zin" aria-label="Zoom in">+</button><button type="button" id="zout" aria-label="Zoom out">−</button><button type="button" id="box" aria-pressed="false" aria-label="Drag a box to zoom to an area" title="Drag a box to zoom to an area">▭</button></div>
      <div class="flight-card" id="flightcard" aria-live="polite" hidden></div>
    </div>
    <div class="strip-wrap">
      <svg id="strip" class="strip" role="img" aria-label="Aircraft positions and ship calls per hour. Hatched hours had no listening to either."></svg>
      <div class="range"><label for="from">From</label><input id="from" type="range" min="0" step="1"><label for="to">To</label><input id="to" type="range" step="1"></div>
      <div class="strip-foot"><button type="button" id="play" aria-pressed="false">Play</button><p class="note"><span id="fromv" class="num"></span> to <span id="tov" class="num"></span>. Hatched hours had no listening on HFDL or DSC.</p></div>
    </div>
    <div class="traffic-panels">
      <div class="card" id="totals"></div>
      <div class="card"><h3>Layers</h3>
        <div class="layers" style="border:0;padding:0;margin-top:10px">
          <label class="ip-check"><input type="checkbox" data-layer="tracks" checked><svg viewBox="0 0 18 12" aria-hidden="true"><path d="M1 10 L9 6" fill="none" stroke="#ffb347" stroke-width="1"/><path d="M17 2 L8.5 3.2 L10.6 6.5 L10.2 10.4Z" fill="#ffb347"/></svg><span>Aircraft, brighter if heard this hour</span></label>
          <label class="ip-check"><input type="checkbox" data-layer="ground" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="4.5" y="1.5" width="9" height="9" fill="none" stroke="#e3e6e9" stroke-width="1.5"/></svg><span>HFDL ground stations</span></label>
          <label class="ip-check"><input type="checkbox" data-layer="dsc" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="6" y="3" width="6" height="6" transform="rotate(45 9 6)" fill="#e3e6e9"/></svg><span>Ship calls with a position</span></label>
          <label class="ip-check"><input type="checkbox" data-layer="night" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x=".5" y=".5" width="17" height="11" fill="#020305" stroke="rgba(227,230,233,.16)"/></svg><span>Night side, at the end of these hours</span></label>
        </div>
      </div>
    </div>
  </section>`;
}

async function showTraffic(params) {
  const w = params.w === "7d" ? "7d" : "24h";
  const [t, names, stations, m, coastLines, landRings] = await Promise.all([
    load("traffic.json"), lookups || load("lookups-site.json"), ground || load("ground-stations.json"),
    load(`map-${w}.json`), coast || load("coast-110m.json"), land || load("land-110m.json"),
  ]);
  lookups = names;
  ground = stations;
  coast = coastLines;
  land = landRings;
  shownBuilt = t.generated_utc;
  const tracked = new Set(m.tracks.map((x) => x.i));
  const gsName = Object.fromEntries(stations.map((s) => [String(s.id), s.name]));
  view.innerHTML = `
    <header class="page-head ip-atmos">${atmosphere(t.flights.slice(0, 96).map((f) => f.n))}
    <p class="eyebrow">Traffic</p>
    <h1>Who was on the air.</h1>
    <p class="lede">Aircraft heard on HFDL in the last 24 hours and DSC ship calls in the last 30 days. Airlines and flag states come from published designator tables, and are blank where a code doesn't match one.</p>
    </header>
    ${trafficMap(w)}
    <section aria-labelledby="hair"><h2 id="hair" data-rail="Aircraft">Aircraft</h2>
      ${t.flights.length ? `<div class="scroll"><table><thead><tr><th>Flight</th><th>Operator</th><th class="hide-s">Registration</th><th class="hide-s">Ground station</th><th>Messages</th><th class="hide-s">Last position</th><th>Last heard</th></tr></thead><tbody>
      ${t.flights.map((f) => `<tr><td class="id">${callLink("HFDL", f.i)}</td><td>${esc(aircraftAirline(f.i, names.airlines))}</td><td class="hide-s">${esc(f.reg || "")}</td><td class="hide-s">${esc(gsName[f.gs] || "")}</td><td>${num(f.n)}</td><td class="hide-s">${f.la === undefined ? "" : tracked.has(f.i) ? `<button type="button" class="show-flight" data-flight="${esc(f.i)}" title="Show this flight on the map">${f.la}, ${f.lo}</button>` : `${f.la}, ${f.lo}`}</td><td>${fmtTime(f.t)}</td></tr>`).join("")}
      </tbody></table></div>` : emptyLog()}
    </section>
    <section aria-labelledby="hship"><h2 id="hship" data-rail="Ships">Ship and coast station calls</h2>
      ${t.ships.length ? `<div class="scroll"><table><thead><tr><th>Time</th><th>MMSI</th><th>Flag</th><th>From</th><th>Call</th><th class="hide-s">kHz</th><th class="hide-s">Position</th></tr></thead><tbody>
      ${t.ships.map((s) => `<tr><td>${fmtTime(s.t)}</td><td class="id">${callLink("DSC", s.i)}</td><td>${esc(shipFlag(s, names.flags))}</td><td>${esc(s.from || "")}</td><td>${esc([s.cat, s.what].filter(Boolean).join(", "))}</td><td class="hide-s">${khz(s.f)}</td><td class="hide-s">${s.la == null ? "" : `${s.la.toFixed(2)}, ${s.lo.toFixed(2)}`}</td></tr>`).join("")}
      </tbody></table></div>` : emptyLog()}
    </section>`;
  view.querySelectorAll("[data-w]").forEach((b) => b.addEventListener("click", () => { location.hash = `#/traffic?w=${b.dataset.w}`; }));
  const cleanup = mountMap(view, { ...m, spots: [], squares: [], idents: [], air: [] }, {
    coast, land, ground, params, traffic: true, presets: TRAFFIC_VIEWS,
    flightInfo: (flight) => {
      const row = t.flights.find((x) => x.i === flight);
      return { operator: aircraftAirline(flight, names.airlines), reg: row?.reg, gs: gsName[row?.gs], n: row?.n };
    },
    loadFine: async () => {
      const [fineCoast, fineLand] = await Promise.all([load("coast-50m.json"), load("land-50m.json")]);
      return { coast: fineCoast, land: fineLand };
    },
    onParams: (p) => history.replaceState(null, "", `#/traffic?${new URLSearchParams(p)}`),
  });
  unmount = cleanup;
  view.querySelectorAll(".show-flight").forEach((b) => b.addEventListener("click", () => {
    cleanup.focusFlight(b.dataset.flight);
    view.querySelector(".traffic-map").scrollIntoView({ block: "start", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
  }));
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

const VIEWS = { now: showNow, map: showMap, activity: showActivity, pictures: showPictures, log: showLog, station: showStation, who: showWho, traffic: showTraffic };

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
  const key = `${name}|${name === "map" ? params.w || "24h" : name === "who" ? params.c || "" : name === "traffic" ? params.w || "24h" : ""}`;
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
