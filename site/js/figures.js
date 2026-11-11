// Figures and table parts that more than one page draws. Every number in them
// comes from the receiver's log; nothing here is made up to fill a gap.
import { esc, fmtTime, hours, khz, num } from "./util.js";

// A fixed color per mode, in the brand's categorical order. FT4 and SSTV fold into gray.
export const MODE_SLOT = { FT8: 1, HFDL: 2, DSC: 3, WSPR: 4, SELCAL: 5, JS8: 6, WEFAX: 7 };
export const modeColor = (m) => (MODE_SLOT[m] ? `var(--innerpulse-chart-${MODE_SLOT[m]})` : "var(--innerpulse-chart-other)");
export const modeChip = (m) => `<span class="mode" style="--c:${modeColor(m)}">${esc(m)}</span>`;

// The amateur bands below 30 MHz, in MHz.
export const HAM_BANDS = [[1.8, 2, "160 m"], [3.5, 4, "80 m"], [5.33, 5.41, "60 m"], [7, 7.3, "40 m"], [10.1, 10.15, "30 m"], [14, 14.35, "20 m"], [18.068, 18.168, "17 m"], [21, 21.45, "15 m"], [24.89, 24.99, "12 m"], [28, 29.7, "10 m"]];
export function bandOf(hz) {
  const mhz = hz / 1e6;
  const band = HAM_BANDS.find(([lo, hi]) => mhz >= lo && mhz <= hi);
  return band ? band[2] : "";
}

// Only these modes are in the lookup files: the ones with a station to name.
const LOOKUP_MODES = new Set(["FT8", "FT4", "WSPR", "JS8", "HFDL", "DSC"]);

export function callLink(mode, ident) {
  const clean = (ident || "").replace(/^\.+/, "");
  return LOOKUP_MODES.has(mode) && /[A-Z0-9]/.test(clean) ? `<a href="#/who?c=${encodeURIComponent(clean)}">${esc(clean)}</a>` : esc(ident || "");
}

// A report as a short meter beside its figure. The span is the range FT8 reports
// cover, -26 to +10 dB; anything outside it pins to an end.
function snrCell(s) {
  if (s == null) return "";
  const w = Math.max(4, Math.min(100, ((s + 26) / 36) * 100));
  return `<span class="snr"><i style="--w:${w.toFixed(0)}%"></i><b>${Math.round(s)} dB</b></span>`;
}

// Log rows. A time that repeats the row above is dimmed, so a burst of decodes
// from one slot reads as one group. `compact` drops the place column.
export function rowsTable(rows, { compact = false, sticky = false } = {}) {
  let before = null;
  const body = rows.map((r) => {
    const minute = Math.floor(r.t / 60);
    const same = minute === before;
    before = minute;
    const band = bandOf(r.f);
    const where = r.g || (r.la !== undefined ? `${r.la}, ${r.lo}` : "");
    return `<tr${same ? "" : ' class="first"'}><td class="${same ? "t same" : "t"}">${fmtTime(r.t)}</td><td>${modeChip(r.m)}</td><td class="f">${khz(r.f)}${band ? `<small>${band}</small>` : ""}</td><td class="id">${callLink(r.m, r.i)}</td>${compact ? "" : `<td class="hide-s">${esc(where)}</td>`}<td class="hide-s">${snrCell(r.s)}</td></tr>`;
  }).join("");
  return `<div class="scroll${sticky ? " stick" : ""}"><table class="rows"><thead><tr><th>Time</th><th>Mode</th><th>kHz</th><th>Who</th>${compact ? "" : `<th class="hide-s">Where</th>`}<th class="hide-s">SNR</th></tr></thead><tbody>${body}</tbody></table></div>`;
}

// Decodes per hour on air, one row per mode. One mode outruns the rest by a
// factor of a thousand, so the bars are on a log scale and say so.
export function modeBoard(byMode) {
  const rate = (m) => m.n / Math.max(m.monitored_s / 3600, 0.1);
  const top = Math.max(10, ...byMode.map(rate));
  const decades = Math.ceil(Math.log10(top));
  const at = (v) => (Math.log10(Math.max(v, 1)) / decades) * 100;
  const ticks = Array.from({ length: decades - 1 }, (_, i) => `<b style="left:${(((i + 1) / decades) * 100).toFixed(2)}%"></b>`).join("");
  const scale = Array.from({ length: decades + 1 }, (_, i) => `<span style="left:${((i / decades) * 100).toFixed(2)}%">${i < 3 ? 10 ** i : `${10 ** (i - 3)}k`}</span>`).join("");
  return `<div class="scroll"><table class="board"><thead><tr><th>Mode</th><th class="track-h"><div class="scale" aria-hidden="true">${scale}</div><span class="sr">Decodes per hour, log scale</span></th><th class="n">Per hour</th><th class="n hide-s">Decodes</th><th class="n hide-s">Stations</th><th class="n">On air</th></tr></thead><tbody>
    ${byMode.map((m, i) => `<tr><td>${modeChip(m.mode)}</td><td class="track"><div>${ticks}<i style="--w:${at(rate(m)).toFixed(1)}%;--c:${modeColor(m.mode)};--i:${i}"></i></div></td><td class="n strong">${num(Math.round(rate(m)))}</td><td class="n hide-s">${num(m.n)}</td><td class="n hide-s">${m.idents ? num(m.idents) : ""}</td><td class="n">${hours(m.monitored_s)}</td></tr>`).join("")}
  </tbody></table></div>
  <p class="note" style="margin-top:10px">Divided by the time actually spent on each mode. The bars are on a log scale: each tick is ten times the one before it.</p>`;
}

// The dial: every frequency the receiver sat on in the window, placed where it
// falls between 0 and 30 MHz. A stem's height is what was decoded there (log
// scale), the marks under the axis are how long it listened there, and the
// needle is where it was last tuned.
const DIAL = { w: 1200, h: 318, l: 46, r: 1186, top: 34, base: 226 };
const BUCKET_HZ = 50000;

export function dialFigure(health, last) {
  const rows = (health?.timeline || []).filter((r) => r[4] !== 0 && r[3] > 0 && r[3] < 30e6);
  if (!rows.length) return "";
  const points = new Map();
  const dwell = new Map();
  for (const [a, b, mode, dial, , decodes, cause] of rows) {
    const bucket = Math.round(dial / BUCKET_HZ);
    const key = `${mode}|${bucket}`;
    const p = points.get(key) || { mode, hz: dial, n: 0, s: 0 };
    p.n += decodes || 0;
    p.s += b - a;
    points.set(key, p);
    if (cause !== "wideband") dwell.set(bucket, (dwell.get(bucket) || 0) + (b - a));
  }
  const list = [...points.values()].sort((p, q) => p.n - q.n);
  const max = Math.max(10, ...list.map((p) => p.n));
  const decades = Math.ceil(Math.log10(max + 1));
  const x = (hz) => DIAL.l + (hz / 30e6) * (DIAL.r - DIAL.l);
  const y = (n) => DIAL.base - (Math.log10(n + 1) / decades) * (DIAL.base - DIAL.top);
  const grid = Array.from({ length: decades }, (_, i) => {
    const v = 10 ** (i + 1);
    const gy = y(v - 1).toFixed(1);
    return `<path class="dg" d="M${DIAL.l} ${gy}H${DIAL.r}"/><text class="dt" x="${DIAL.l - 8}" y="${Number(gy) + 4}" text-anchor="end">${v < 1000 ? v : `${v / 1000}k`}</text>`;
  }).join("");
  const mhz = Array.from({ length: 31 }, (_, i) => {
    const tx = x(i * 1e6).toFixed(1);
    const major = i % 5 === 0;
    return `<path class="dx" d="M${tx} ${DIAL.base}v${major ? 9 : 5}"/>${major ? `<text class="dt" x="${tx}" y="${DIAL.base + 44}" text-anchor="middle">${i}</text>` : ""}`;
  }).join("");
  const bands = HAM_BANDS.map(([lo, hi, name]) => {
    const [x0, x1] = [x(lo * 1e6), x(hi * 1e6)];
    const wide = Math.max(x1 - x0, 3);
    return `<rect class="db" x="${x0.toFixed(1)}" y="${DIAL.top - 10}" width="${wide.toFixed(1)}" height="${DIAL.base - DIAL.top + 10}"/><text class="dn" x="${(x0 + wide / 2).toFixed(1)}" y="${DIAL.base + 66}" text-anchor="middle">${name}</text>`;
  }).join("");
  const longest = Math.max(1, ...dwell.values());
  const sat = [...dwell.entries()].map(([bucket, s]) => {
    const h = 3 + 13 * Math.sqrt(s / longest);
    return `<rect class="dw" x="${(x(bucket * BUCKET_HZ) - 1.5).toFixed(1)}" y="${DIAL.base + 12}" width="3" height="${h.toFixed(1)}" rx="1"/>`;
  }).join("");
  const stems = list.map((p) => {
    const px = x(p.hz).toFixed(1);
    const py = y(p.n).toFixed(1);
    const band = bandOf(p.hz);
    const tip = `${p.mode} · ${khz(p.hz)} kHz${band ? ` (${band})` : ""} · ${p.n ? `${num(p.n)} decodes` : "nothing decoded"} · ${hours(p.s)} listening`;
    const delay = ((p.hz / 30e6) * 0.7).toFixed(2);
    return p.n
      ? `<g class="lp" style="--c:${modeColor(p.mode)};--d:${delay}s" data-tip="${esc(tip)}"><path d="M${px} ${DIAL.base}V${py}"/><circle cx="${px}" cy="${py}" r="4.6"/><circle class="hit" cx="${px}" cy="${py}" r="11"/></g>`
      : `<g class="lp none" style="--c:${modeColor(p.mode)};--d:${delay}s" data-tip="${esc(tip)}"><circle cx="${px}" cy="${DIAL.base}" r="3.4"/><circle class="hit" cx="${px}" cy="${DIAL.base}" r="9"/></g>`;
  }).join("");
  let needle = "";
  if (last?.dial_hz > 0 && last.dial_hz < 30e6) {
    const nx = x(last.dial_hz);
    const flip = nx > DIAL.r - 190;
    needle = `<g class="needle" style="--from:${(DIAL.l - nx).toFixed(0)}px"><path d="M${nx.toFixed(1)} ${DIAL.top - 16}V${DIAL.base + 10}"/><path class="cap" d="M${(nx - 5).toFixed(1)} ${DIAL.top - 22}h10l-5 8z"/><text x="${(nx + (flip ? -10 : 10)).toFixed(1)}" y="${DIAL.top - 13}" text-anchor="${flip ? "end" : "start"}">last tuned ${khz(last.dial_hz)} kHz · ${esc(last.mode)}</text></g>`;
  }
  const modes = [...new Set(list.map((p) => p.mode))].sort((a, b) => (MODE_SLOT[a] || 9) - (MODE_SLOT[b] || 9));
  return `<div class="scroll dial-scroll"><svg class="dial" viewBox="0 0 ${DIAL.w} ${DIAL.h}" role="img" aria-label="Decodes by frequency from 0 to 30 MHz over the last 24 hours, on a log scale, with the time spent listening at each frequency marked under the axis.">
      ${bands}${grid}<path class="da" d="M${DIAL.l} ${DIAL.base}H${DIAL.r}"/>${mhz}
      <text class="dt" x="${DIAL.r}" y="${DIAL.base + 44}" text-anchor="end" dx="-18">MHz</text>
      <text class="dt" x="${DIAL.l - 8}" y="${DIAL.base + 24}" text-anchor="end">time</text>
      ${sat}${stems}${needle}
    </svg></div>
    <p class="fig-read" id="dial-read" aria-live="off">Point at a stem for a reading.</p>
    <div class="key-row">${modes.map((m) => `<span><i style="background:${modeColor(m)}"></i>${esc(m)}</span>`).join("")}<span><i class="k-ring"></i>Listened, nothing decoded</span><span><i class="k-dwell"></i>Time spent tuned there</span></div>`;
}

// One line under a figure that says what the pointer is on. A tap does the same
// on a touch screen, where there is nothing to hover with.
export function wireReadout(figure, line, idle) {
  if (!figure || !line) return;
  let held = null;
  const show = (ev) => {
    const target = ev.target.closest?.("[data-tip]");
    if (target === held) return;
    held?.classList.remove("on");
    held = target;
    held?.classList.add("on");
    line.textContent = target ? target.dataset.tip : idle;
    line.classList.toggle("live", Boolean(target));
  };
  figure.addEventListener("pointerover", show);
  figure.addEventListener("pointerdown", show);
  figure.addEventListener("pointerleave", () => show({ target: figure.ownerDocument.body }));
}

// A table whose headings sort it, shown a page at a time. Each column is
// { label, value(row), html(row)?, num?, cls?, asc? }: `value` is what it sorts by
// (null always sorts last), `html` what the cell shows, `asc` a column whose first
// click sorts upward (names, times read as "earliest"). `onPick` makes rows act.
export function sortableTable(box, cols, rows, { sort = 0, desc = true, page = 100, onPick, picked, caption = "" } = {}) {
  let shown = page;
  const cmp = (col) => (a, b) => {
    const [x, y] = [col.value(a), col.value(b)];
    if (x == null || x === "") return y == null || y === "" ? 0 : 1;
    if (y == null || y === "") return -1;
    return (x < y ? -1 : x > y ? 1 : 0) * (desc ? -1 : 1);
  };
  let sorted = [];
  const draw = () => {
    sorted = [...rows].sort(cmp(cols[sort]));
    const head = cols.map((c, i) => `<th class="${c.cls || ""}${c.num ? " n" : ""}" aria-sort="${i === sort ? (desc ? "descending" : "ascending") : "none"}"><button type="button" class="th-sort" data-sort="${i}">${c.label}<i aria-hidden="true"></i></button></th>`).join("");
    const body = sorted.slice(0, shown).map((r, i) => `<tr${onPick ? ` data-pick="${i}" tabindex="0"` : ""}${picked && picked(r) ? ' class="picked"' : ""}>${cols.map((c) => `<td class="${c.cls || ""}${c.num ? " n" : ""}">${c.html ? c.html(r) : esc(c.value(r) ?? "")}</td>`).join("")}</tr>`).join("");
    box.innerHTML = `<div class="scroll frame" tabindex="0" role="region" aria-label="${esc(caption || "Table")}"><table class="sortable${onPick ? " pickable" : ""}">${caption ? `<caption class="sr">${esc(caption)}</caption>` : ""}<thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>
      ${sorted.length > shown ? `<p class="more-rows"><button type="button" data-more>Show ${num(Math.min(page, sorted.length - shown))} more</button><span>${num(shown)} of ${num(sorted.length)}</span></p>` : ""}`;
  };
  box.onclick = (ev) => {
    const s = ev.target.closest("[data-sort]");
    if (s) {
      const i = Number(s.dataset.sort);
      if (i === sort) desc = !desc;
      else { sort = i; desc = !cols[i].asc; }
      draw();
      return;
    }
    if (ev.target.closest("[data-more]")) { shown += page; draw(); return; }
    const tr = ev.target.closest("[data-pick]");
    if (tr && onPick && !ev.target.closest("a, button")) onPick(sorted[Number(tr.dataset.pick)]);
  };
  box.onkeydown = (ev) => {
    const tr = ev.target.closest?.("[data-pick]");
    if (tr && onPick && (ev.key === "Enter" || ev.key === " ")) { ev.preventDefault(); onPick(sorted[Number(tr.dataset.pick)]); }
  };
  draw();
  return { redraw: draw };
}

// Sixteen compass points, for a bearing in degrees.
const POINTS = ["N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW"];
export const compass = (deg) => POINTS[Math.round(deg / 22.5) % 16];
// A small arrow turned to a bearing, beside its compass point.
export const bearingCell = (deg) => (deg == null ? "" : `<span class="brg"><svg viewBox="-6 -6 12 12" aria-hidden="true"><path d="M0-5L3 3L0 1.4L-3 3Z" transform="rotate(${Math.round(deg)})"/></svg>${compass(deg)}</span>`);
