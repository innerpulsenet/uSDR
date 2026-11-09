import { BAND_GROUPS, SPOT_SIZES, mountMap, STEPS } from "./map.js";
import { ago, esc, fmtClock, fmtTime, hours, khz, num } from "./util.js";

const view = document.getElementById("view");
const STALE_S = 20 * 60;
let unmount = null;
let coast = null;
let land = null;
let ground = null;

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

function rowsTable(rows) {
  return `<div class="scroll"><table><thead><tr><th>Time</th><th>Mode</th><th>kHz</th><th>Who</th><th class="hide-s">Where</th><th class="hide-s">SNR</th></tr></thead><tbody>
    ${rows.map((r) => `<tr><td>${fmtTime(r.t)}</td><td>${esc(r.m)}</td><td>${khz(r.f)}</td><td class="id">${esc(r.i || "")}</td><td class="hide-s">${esc(r.g || (r.la !== undefined ? `${r.la}, ${r.lo}` : ""))}</td><td class="hide-s">${r.s == null ? "" : `${Math.round(r.s)} dB`}</td></tr>`).join("")}
  </tbody></table></div>`;
}

async function showNow() {
  const s = await load("status.json");
  const total = s.by_mode.reduce((a, m) => a + m.n, 0);
  const flights = s.by_mode.find((m) => m.mode === "HFDL");
  const amateurs = s.by_mode.filter((m) => ["FT8", "FT4", "WSPR", "JS8"].includes(m.mode)).reduce((a, m) => a + m.idents, 0);
  const max = Math.max(1, ...s.by_mode.map((m) => m.n / Math.max(m.monitored_s / 3600, 0.1)));
  const lastLine = s.last ? `Last on ${esc(s.last.mode)} at ${khz(s.last.dial_hz)} kHz, ${ago(s.last.end_utc)}.` : "It has not reported a stretch of listening yet.";
  view.innerHTML = `
    <p class="eyebrow">Last 24 hours</p>
    <h1>One receiver, listening below 30 MHz.</h1>
    <p class="lede">It moves between amateur digital modes, aircraft datalink, ship calls and weather charts on its own schedule. ${lastLine}</p>
    <dl class="ip-readout">
      <div><dt>Decodes logged</dt><dd>${num(total)}</dd></div>
      <div><dt>Spent listening</dt><dd>${hours(s.monitored_s)}</dd></div>
      <div><dt>Amateur stations</dt><dd>${num(amateurs)}</dd></div>
      <div><dt>Aircraft on HFDL</dt><dd>${flights ? num(flights.idents) : "0"}</dd></div>
    </dl>
    ${s.unhealthy_s || s.lost_decodes ? `<p class="ip-notice ip-notice-warn" role="status"><svg class="ip-i sm" aria-hidden="true"><use href="brand/sprite.svg#ip-s-warn"/></svg><span>${s.unhealthy_s ? `${hours(s.unhealthy_s)} of listening failed its radio check and is left out of the figures. ` : ""}${s.lost_decodes ? `${num(s.lost_decodes)} decodes were lost to a full queue.` : ""}</span></p>` : ""}
    <section aria-labelledby="yield"><h2 id="yield">Decodes per hour on air</h2>
      <p class="note">Divided by the time actually spent on each mode, since the receiver can only do one at a time.</p>
      ${s.by_mode.length ? `<div class="bars">${s.by_mode.map((m) => {
        const rate = m.n / Math.max(m.monitored_s / 3600, 0.1);
        return `<div class="bar"><span>${esc(m.mode)}</span><i style="--w:${(rate / max * 100).toFixed(1)}%"></i><em>${num(Math.round(rate))}/h · ${hours(m.monitored_s)} on air</em></div>`;
      }).join("")}</div>` : emptyLog()}
    </section>
    <section aria-labelledby="latest"><h2 id="latest">Latest</h2>
      ${s.latest.length ? rowsTable(s.latest) : emptyLog()}
    </section>`;
}

function mapShell(w) {
  return `
    <p class="eyebrow">The map</p>
    <h1>Where it heard from.</h1>
    <div class="tools" style="margin-top:18px">
      <div class="chips"><span class="lbl">Window</span><button type="button" data-w="24h" aria-pressed="${w === "24h"}">24 hours</button><button type="button" data-w="7d" aria-pressed="${w === "7d"}">7 days</button></div>
      <div class="chips"><span class="lbl">Projection</span><button type="button" data-proj="eq">Flat</button><button type="button" data-proj="az">From the station</button></div>
      <div class="chips" id="presets"><span class="lbl">Go to</span></div>
    </div>
    <div class="tools"><div class="chips" id="bands"><span class="lbl">Band</span></div><div class="chips" id="modes"><span class="lbl">Mode</span></div></div>
    <div class="map-wrap" style="margin-top:6px">
      <div class="map-main">
        <div class="stage">
          <div class="zoomer"><button type="button" id="zin" aria-label="Zoom in">+</button><button type="button" id="zout" aria-label="Zoom out">−</button><button type="button" id="box" aria-pressed="false" aria-label="Drag a box to zoom to an area" title="Drag a box to zoom to an area">▭</button></div>
        </div>
        <div class="strip-wrap">
          <svg id="strip" class="strip" role="img" aria-label="Spots per hour. Hatched hours had no listening."></svg>
          <div class="range"><label for="from">From</label><input id="from" type="range" min="0" step="1"><label for="to">To</label><input id="to" type="range" step="1"></div>
          <p class="note" style="margin-top:2px"><span id="fromv" class="num"></span> to <span id="tov" class="num"></span>. Hatched hours had no listening at all, which is not the same as a quiet band.</p>
        </div>
      </div>
      <div class="side">
        <div class="card" id="readout"><h3>Under the pointer</h3><p class="note" style="margin-top:8px">Point at a dot or a flight. Scroll to zoom, drag to pan, or use ▭ to drag out an area (shift-drag does the same).</p></div>
        <div class="card" id="totals"></div>
        <div class="card"><h3>Reading the dots</h3>
          <div class="key">
            <div class="row" aria-label="Dot size is spots in that square">${SPOT_SIZES.map((s) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="${s.px / 2}" fill="var(--innerpulse-text-2)"/></svg>${s.label}</span>`).join("")}</div>
            <div class="row" aria-label="Dot color is the band group heard most in that square">${BAND_GROUPS.map((b, i) => `<span><svg class="swatch" viewBox="0 0 15 15" aria-hidden="true"><circle cx="7.5" cy="7.5" r="5" fill="var(--innerpulse-chart-${i + 1})"/></svg>${b}</span>`).join("")}</div>
          </div>
          <p class="note" style="margin-top:8px">One dot per grid square, at the square's center. Size is spots; color is the bands heard most there.</p>
          <div class="layers">
            <label class="ip-check"><input type="checkbox" data-layer="squares" checked><span>Amateur stations</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="tracks" checked><svg viewBox="0 0 18 12" aria-hidden="true"><path d="M1 10 L9 6" fill="none" stroke="var(--innerpulse-text-2)" stroke-width="1"/><path d="M17 2 L8.5 3.2 L10.6 6.5 L10.2 10.4Z" fill="var(--innerpulse-text)"/></svg><span>Aircraft positions</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="ground" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="4.5" y="1.5" width="9" height="9" fill="none" stroke="var(--innerpulse-text)" stroke-width="1.5"/></svg><span>HFDL ground stations</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="dsc" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x="6" y="3" width="6" height="6" transform="rotate(45 9 6)" fill="var(--innerpulse-text)"/></svg><span>Ship calls</span></label>
            <label class="ip-check"><input type="checkbox" data-layer="night" checked><svg viewBox="0 0 18 12" aria-hidden="true"><rect x=".5" y=".5" width="17" height="11" fill="#020305" stroke="var(--innerpulse-line-2)"/></svg><span>Night side, now</span></label>
          </div>
        </div>
      </div>
    </div>`;
}

async function showMap(params) {
  const w = params.w === "7d" ? "7d" : "24h";
  [coast, land, ground] = await Promise.all([coast || load("coast-110m.json"), land || load("land-110m.json"), ground || load("ground-stations.json")]);
  const data = await load(`map-${w}.json`);
  view.innerHTML = mapShell(w);
  view.querySelectorAll("[data-w]").forEach((b) => b.addEventListener("click", () => {
    location.hash = `#/map?w=${b.dataset.w}`;
  }));
  unmount = mountMap(view, data, {
    coast, land, ground, params,
    onParams: (p) => history.replaceState(null, "", `#/map?${new URLSearchParams(p)}`),
  });
}

function heatTable(rows, unit) {
  let max = 0;
  for (const r of rows) for (const [n, s] of r.cells) if (s >= 60) max = Math.max(max, n / (s / 3600));
  const cell = ([n, s], label, h) => {
    const at = `${label}, ${String(h).padStart(2, "0")}Z`;
    if (!s) return `<i class="out" title="${esc(at)}: not monitored"></i>`;
    const rate = n / (s / 3600);
    if (!n) return `<i class="q" title="${esc(at)}: monitored ${hours(s)}, nothing heard"></i>`;
    const step = Math.min(STEPS.length - 1, Math.floor((Math.log(rate + 1) / Math.log(max + 1)) * STEPS.length));
    return `<i style="background:${STEPS[step]}" title="${esc(at)}: ${num(Math.round(rate))} ${unit} per hour, ${hours(s)} monitored"></i>`;
  };
  return `<div class="scroll"><div class="heat">
    ${rows.map((r) => `<div class="rowlabel">${esc(r.label)}</div><div class="cells24">${r.cells.map((c, h) => cell(c, r.label, h)).join("")}</div>`).join("")}
    <div class="hours">${Array.from({ length: 24 }, (_, h) => `<span>${h % 3 === 0 ? String(h).padStart(2, "0") : ""}</span>`).join("")}</div>
  </div></div>`;
}

async function showActivity() {
  const a = await load("activity.json");
  const historyDays = a.first_listening_utc ? (Date.now() / 1000 - a.first_listening_utc) / 86400 : 0;
  const key = `<div class="heat-key"><span><i style="background:${STEPS[4]}"></i>busiest</span><span><i style="background:${STEPS[0]}"></i>few</span><span><i class="q"></i>listened, heard nothing</span><span><i class="out"></i>not monitored</span></div>`;
  const days = historyDays >= 1 ? `${Math.floor(historyDays)} days` : "less than a day";
  view.innerHTML = `
    <p class="eyebrow">Activity</p>
    <h1>When each band came alive.</h1>
    <p class="lede">The log so far covers ${days}, grouped by UTC hour over up to ${a.days} days. Every cell is divided by the time the receiver actually spent there.</p>
    <section aria-labelledby="h1"><h2 id="h1">Amateur decodes by band and hour</h2>${a.bands.length ? heatTable(a.bands, "decodes") + key : emptyLog()}</section>
    <section aria-labelledby="h2"><h2 id="h2">All modes by hour</h2>${a.modes.length ? heatTable(a.modes, "decodes") + key : emptyLog()}</section>
    <div class="twocol">
      <section aria-labelledby="h3"><h2 id="h3">Farthest squares</h2><p class="note">Measured between grid-square centers.</p>
        ${a.farthest.length ? `<div class="scroll"><table><thead><tr><th>km</th><th>Square</th><th>Who</th><th class="hide-s">Band</th><th>Heard</th></tr></thead><tbody>${a.farthest.map((f) => `<tr><td>${num(f.km)}</td><td>${esc(f.grid)}</td><td class="id">${esc(f.ident)}</td><td class="hide-s">${esc(f.band)}</td><td>${fmtTime(f.t)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">Distances need the station's grid square, which has not arrived yet.</p>`}
      </section>
      <section aria-labelledby="h4"><h2 id="h4">New squares today</h2>
        ${historyDays < 7 ? `<p class="empty">With ${days} of history almost every square is new, so this list waits for a week of data.</p>` : a.new_squares.length ? `<div class="scroll"><table><thead><tr><th>Square</th><th>First heard</th></tr></thead><tbody>${a.new_squares.map((n) => `<tr><td class="id">${esc(n.grid)}</td><td>${fmtTime(n.t)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">No new squares in the last 24 hours.</p>`}
      </section>
    </div>
    <section aria-labelledby="h5"><h2 id="h5">SELCAL calls</h2><p class="note">A SELCAL call is a two-pair code sent to wake one aircraft's radio. It carries no position.</p>
      ${a.selcal.length ? `<div class="scroll"><table><thead><tr><th>Time</th><th>Code</th><th>kHz</th></tr></thead><tbody>${a.selcal.map((c) => `<tr><td>${fmtTime(c.t)}</td><td class="id">${esc(c.code)}</td><td>${khz(c.dial_hz)}</td></tr>`).join("")}</tbody></table></div>` : `<p class="empty">No SELCAL calls logged.</p>`}
    </section>`;
}

async function showPictures() {
  const { pictures } = await load("pictures.json");
  view.innerHTML = `
    <p class="eyebrow">Pictures</p>
    <h1>Weather charts off the air.</h1>
    <p class="lede">Received as radiofax and kept as they arrived, noise included. Titles come from the transmitting station's timetable, because a chart does not state its own.</p>
    ${pictures.length ? `<div class="gallery">${pictures.map((p) => `<article class="pic">
      <a href="pictures/${encodeURIComponent(p.file)}"><img loading="lazy" src="pictures/${encodeURIComponent(p.file)}" alt="Radiofax chart received ${fmtTime(p.start_utc)}${p.scheduled_title ? `, scheduled as ${esc(p.scheduled_title)}` : ""}"></a>
      <div><b>${esc(p.scheduled_title || "Untitled picture")}</b><span>${fmtTime(p.start_utc)} · ${esc(p.station || "")} · ${khz(p.dial_hz + 1900)} kHz</span></div>
    </article>`).join("")}</div>` : `<p class="empty">No charts have been published yet. Only charts that pass a quality check and come from a service cleared for republishing appear here.</p>`}`;
}

async function showLog() {
  const { rows } = await load("log.json");
  const modes = [...new Set(rows.map((r) => r.m))].sort();
  view.innerHTML = `
    <p class="eyebrow">Log</p>
    <h1>The latest 500 entries.</h1>
    <div class="tools" style="margin-top:18px">
      <div class="chips"><label class="lbl" for="logmode">Mode</label><select id="logmode"><option value="">All</option>${modes.map((m) => `<option>${esc(m)}</option>`).join("")}</select></div>
      <div class="chips"><label class="lbl" for="logq">Search</label><input id="logq" type="search" placeholder="Callsign, flight, square"></div>
      <p class="note" id="logn" aria-live="polite"></p>
    </div>
    <div id="logrows"></div>`;
  const draw = () => {
    const m = view.querySelector("#logmode").value;
    const q = view.querySelector("#logq").value.trim().toUpperCase();
    const shown = rows.filter((r) => (!m || r.m === m) && (!q || `${r.i || ""} ${r.g || ""}`.toUpperCase().includes(q)));
    view.querySelector("#logn").textContent = `${num(shown.length)} of ${num(rows.length)}`;
    view.querySelector("#logrows").innerHTML = shown.length ? rowsTable(shown.slice(0, 500)) : rows.length ? `<p class="empty">No entries match.</p>` : emptyLog();
  };
  view.querySelector("#logmode").addEventListener("change", draw);
  view.querySelector("#logq").addEventListener("input", draw);
  draw();
}

function showStation() {
  view.innerHTML = `
    <p class="eyebrow">Station</p>
    <h1>How this log is made.</h1>
    <div class="prose">
      <p class="lede">A single software-defined receiver on a large wire loop, running unattended. It decodes one mode on one frequency at a time, and a schedule decides which.</p>
      <h2>What it listens to</h2>
      <p>Amateur digital modes (FT8, FT4, WSPR and JS8), HFDL aircraft datalink, DSC ship calls, SELCAL, radiofax weather charts and SSTV, all below 30 MHz.</p>
      <h2>Not monitored is not quiet</h2>
      <p>The receiver gives each mode a share of its time and moves on, so most frequencies are unattended at any given moment. Every stretch of listening is logged. Pages divide counts by that time and hatch out the hours with none. An empty cell and a hatched cell mean different things.</p>
      <h2>How a stretch is judged</h2>
      <p>Once an hour the receiver checks itself against a time-signal station. A stretch that failed the check or overloaded the receiver is left out of the figures.</p>
      <h2>What a blank map does not prove</h2>
      <p>A wire loop this long on the higher bands has a lobed pattern: strong in some directions, weak in others. A region with no spots does not show that the band was closed to it.</p>
      <h2>What is left out on purpose</h2>
      <p>The content of aircraft messages is not published. DSC ship calls appear six hours late and are a logbook, not a distress feed. If you are looking for someone in trouble at sea, contact a rescue coordination center.</p>
      <p>The station itself stays anonymous, and its position is given only as a four-character grid square. Radio amateurs heard on the air appear by their callsigns and locators, as they do on any public spotting network.</p>
      <h2>Aircraft positions</h2>
      <p>Aircraft queue position reports and sometimes send several together. Tracks are drawn in the order of each fix's own timestamp, and split where two fixes could not belong to one flight.</p>
    </div>`;
}

const VIEWS = { now: showNow, map: showMap, activity: showActivity, pictures: showPictures, log: showLog, station: showStation };

async function render() {
  if (unmount) { unmount(); unmount = null; }
  const { name, params } = route();
  const show = VIEWS[name] || showNow;
  document.querySelectorAll("nav a").forEach((a) => {
    if (a.dataset.view === (VIEWS[name] ? name : "now")) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  try {
    await show(params);
  } catch (e) {
    view.innerHTML = `<p class="empty">The data for this page could not be loaded (${esc(e.message)}).</p>`;
  }
}

async function freshness() {
  const out = document.getElementById("fresh");
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

let lastRoute = "";
function hashChanged() {
  const { name, params } = route();
  const key = `${name}|${name === "map" ? params.w || "24h" : ""}`;
  if (key !== lastRoute) { lastRoute = key; render(); }
}
addEventListener("hashchange", hashChanged);
hashChanged();
freshness();
setInterval(freshness, 60000);
