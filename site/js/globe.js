// The globe on the Now page: the last day's stations on a slowly turning Earth,
// with the last hour's replayed as paths arriving at the station's square. Every
// mark is a real square from the log; nothing here is invented or interpolated.
import { bandGroup, BAND_GROUPS, gridCenter } from "./map.js";
import { calm } from "./motion.js";

const RAD = Math.PI / 180;
const TILT = 24 * RAD;
const TURN_DEG_S = 2.4;
const FRAME_MS = 1000 / 30;
const ARC_S = 1.3;
const FADE_S = 1.1;
const MAX_PATHS = 7;
const PATH_EVERY_MS = 620;

const unit = (lon, lat) => {
  const [l, p] = [lon * RAD, lat * RAD];
  return [Math.cos(p) * Math.sin(l), Math.sin(p), Math.cos(p) * Math.cos(l)];
};

function subsolar(ms) {
  const d = ms / 86400000 - 10957.5;
  const g = (357.529 + 0.98560028 * d) * RAD;
  const q = 280.459 + 0.98564736 * d;
  const ecl = (q + 1.915 * Math.sin(g) + 0.02 * Math.sin(2 * g)) * RAD;
  const e = 23.439 * RAD;
  const dec = Math.asin(Math.sin(e) * Math.sin(ecl));
  const ra = Math.atan2(Math.cos(e) * Math.sin(ecl), Math.cos(ecl)) / RAD;
  const gmst = (18.697374558 + 24.06570982441908 * d) * 15;
  return [(((ra - gmst + 180) % 360) + 360) % 360 - 180, dec / RAD];
}

// Kept between visits, so a refresh of the page does not snap the Earth back.
let resumeLon = null;

export function mountGlobe(canvas, map, land) {
  const ctx = canvas.getContext("2d");
  // The dark chart set, by value: the tokens are light-dark() pairs, which a canvas can't read.
  const palette = {
    ocean: "#0d1014", land: "#20242b", line: "rgba(227,230,233,.07)", rim: "rgba(227,230,233,.22)", text: "#e3e6e9",
    groups: ["#00a3b8", "#e1664a", "#865ce7", "#8f949b"],
  };

  const rings = land.map((ring) => ring.map(([lon, lat]) => unit(lon, lat)));
  const graticule = [];
  for (let lon = -180; lon < 180; lon += 30) graticule.push(Array.from({ length: 37 }, (_, i) => unit(lon, -90 + i * 5)));
  for (let lat = -60; lat <= 60; lat += 30) graticule.push(Array.from({ length: 73 }, (_, i) => unit(-180 + i * 5, lat)));

  const squares = new Map();
  let newest = 0;
  for (const [sq, , b, , id, n, , t] of map.spots) {
    const s = squares.get(sq) || { ids: new Set(), groups: new Array(BAND_GROUPS.length + 1).fill(0), t: 0 };
    s.ids.add(id);
    s.groups[bandGroup(map.bands[b])] += n;
    s.t = Math.max(s.t, t);
    newest = Math.max(newest, t);
    squares.set(sq, s);
  }
  const dots = [...squares].map(([sq, s]) => {
    const [lon, lat] = gridCenter(map.squares[sq]);
    return { v: unit(lon, lat), lon, lat, r: s.ids.size >= 20 ? 3 : s.ids.size >= 5 ? 2.2 : s.ids.size >= 2 ? 1.6 : 1.1, group: s.groups.indexOf(Math.max(...s.groups)), recent: s.t >= newest - 60 };
  }).sort((a, b) => a.r - b.r);
  const recent = dots.filter((d) => d.recent);
  const home = map.station_grid ? gridCenter(map.station_grid) : null;
  const homeV = home ? unit(...home) : null;

  let lon0 = resumeLon ?? (home ? home[0] : -40) + 12;
  let W = 0, R = 0, cx = 0, cy = 0;
  function resize() {
    const size = canvas.clientWidth;
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    canvas.width = canvas.height = Math.round(size * ratio);
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    W = size;
    R = size / 2 - 3;
    cx = cy = size / 2;
  }

  // Rotate a unit vector into view: [screen x, screen y, toward the viewer].
  function view(v, sinL, cosL, lift = 1) {
    const x = v[0] * cosL - v[2] * sinL;
    const z0 = v[0] * sinL + v[2] * cosL;
    const y = v[1] * Math.cos(TILT) - z0 * Math.sin(TILT);
    const z = v[1] * Math.sin(TILT) + z0 * Math.cos(TILT);
    return [x * lift, y * lift, z];
  }

  function trace(points, sinL, cosL, close) {
    let drawn = false, any = false;
    ctx.beginPath();
    for (const v of points) {
      let [x, y, z] = view(v, sinL, cosL);
      if (z >= 0) any = true;
      else if (close) {
        const k = 1 / (Math.hypot(x, y) || 1);
        x *= k; y *= k;
      } else { drawn = false; continue; }
      if (drawn) ctx.lineTo(cx + R * x, cy - R * y);
      else { ctx.moveTo(cx + R * x, cy - R * y); drawn = true; }
    }
    if (close) ctx.closePath();
    return any;
  }

  const slerp = (a, b, t) => {
    const w = Math.acos(Math.max(-1, Math.min(1, a[0] * b[0] + a[1] * b[1] + a[2] * b[2])));
    if (w < 1e-4) return a;
    const [s, r] = [Math.sin((1 - t) * w) / Math.sin(w), Math.sin(t * w) / Math.sin(w)];
    return [s * a[0] + r * b[0], s * a[1] + r * b[1], s * a[2] + r * b[2]];
  };

  let paths = [];
  let lastPath = 0;
  let cursor = 0;
  const order = [...recent].sort(() => Math.random() - 0.5);

  function draw(nowMs) {
    // A canvas with no room yet (a hidden tab, a page still laying out) has nothing to draw on.
    if (!(R > 0)) return;
    const sinL = Math.sin(lon0 * RAD), cosL = Math.cos(lon0 * RAD);
    ctx.clearRect(0, 0, W, W);
    ctx.save();
    ctx.beginPath();
    ctx.arc(cx, cy, R, 0, Math.PI * 2);
    const sea = ctx.createRadialGradient(cx - R * 0.35, cy - R * 0.45, R * 0.1, cx, cy, R);
    sea.addColorStop(0, "#161a20");
    sea.addColorStop(1, palette.ocean);
    ctx.fillStyle = sea;
    ctx.fill();
    ctx.clip();

    ctx.strokeStyle = palette.line;
    ctx.lineWidth = 1;
    for (const line of graticule) { trace(line, sinL, cosL, false); ctx.stroke(); }

    ctx.fillStyle = palette.land;
    for (const ring of rings) if (trace(ring, sinL, cosL, true)) ctx.fill();

    // The night side, from where the sun is overhead right now.
    const [sunLon, sunLat] = subsolar(Date.now());
    const anti = unit(sunLon + 180, -sunLat);
    const axisA = Math.abs(anti[1]) > 0.9 ? [1, 0, 0] : [0, 1, 0];
    const u = [anti[1] * axisA[2] - anti[2] * axisA[1], anti[2] * axisA[0] - anti[0] * axisA[2], anti[0] * axisA[1] - anti[1] * axisA[0]];
    const un = Math.hypot(...u);
    const e1 = u.map((c) => c / un);
    const e2 = [anti[1] * e1[2] - anti[2] * e1[1], anti[2] * e1[0] - anti[0] * e1[2], anti[0] * e1[1] - anti[1] * e1[0]];
    const cap = Array.from({ length: 120 }, (_, i) => {
      const a = (i / 120) * Math.PI * 2;
      return [Math.cos(a) * e1[0] + Math.sin(a) * e2[0], Math.cos(a) * e1[1] + Math.sin(a) * e2[1], Math.cos(a) * e1[2] + Math.sin(a) * e2[2]];
    });
    if (trace(cap, sinL, cosL, true) || view(anti, sinL, cosL)[2] > 0) {
      ctx.fillStyle = "rgba(2,3,5,.5)";
      ctx.fill();
    }

    for (const d of dots) {
      const [x, y, z] = view(d.v, sinL, cosL);
      if (z <= 0.02) continue;
      ctx.fillStyle = palette.groups[d.group];
      ctx.globalAlpha = 0.45 + 0.55 * Math.min(1, z * 2.2);
      ctx.beginPath();
      ctx.arc(cx + R * x, cy - R * y, d.r, 0, Math.PI * 2);
      ctx.fill();
    }
    ctx.globalAlpha = 1;

    if (!calm && homeV && order.length) {
      if (nowMs - lastPath > PATH_EVERY_MS && paths.length < MAX_PATHS) {
        lastPath = nowMs;
        // Only one that can be seen; a path behind the Earth would say nothing.
        for (let tries = 0; tries < order.length; tries++) {
          const d = order[cursor++ % order.length];
          if (view(d.v, sinL, cosL)[2] > 0.15) { paths.push({ d, born: nowMs }); break; }
        }
      }
      paths = paths.filter((p) => (nowMs - p.born) / 1000 < ARC_S + FADE_S);
      ctx.lineWidth = 1.1;
      ctx.lineCap = "round";
      for (const p of paths) {
        const age = (nowMs - p.born) / 1000;
        const drawnTo = Math.min(1, age / ARC_S) ** 0.7;
        const alpha = age < ARC_S ? 0.9 : 0.9 * (1 - (age - ARC_S) / FADE_S);
        ctx.strokeStyle = palette.groups[p.d.group];
        ctx.globalAlpha = Math.max(0, alpha);
        ctx.beginPath();
        let open = false;
        for (let i = 0; i <= 40 * drawnTo; i++) {
          const t = i / 40;
          const [x, y, z] = view(slerp(p.d.v, homeV, t), sinL, cosL, 1 + 0.16 * Math.sin(Math.PI * t));
          if (z < -0.05) { open = false; continue; }
          if (open) ctx.lineTo(cx + R * x, cy - R * y);
          else { ctx.moveTo(cx + R * x, cy - R * y); open = true; }
        }
        ctx.stroke();
        const ring = Math.max(0, age / (ARC_S + FADE_S));
        const [x, y, z] = view(p.d.v, sinL, cosL);
        if (z > 0) {
          ctx.globalAlpha = Math.max(0, 0.8 * (1 - ring));
          ctx.beginPath();
          ctx.arc(cx + R * x, cy - R * y, 2 + 13 * ring, 0, Math.PI * 2);
          ctx.stroke();
        }
      }
      ctx.globalAlpha = 1;
    }

    if (homeV) {
      const [x, y, z] = view(homeV, sinL, cosL);
      if (z > 0) {
        ctx.strokeStyle = palette.text;
        ctx.fillStyle = palette.text;
        ctx.lineWidth = 1.4;
        ctx.beginPath();
        ctx.arc(cx + R * x, cy - R * y, 6, 0, Math.PI * 2);
        ctx.stroke();
        ctx.beginPath();
        ctx.arc(cx + R * x, cy - R * y, 1.8, 0, Math.PI * 2);
        ctx.fill();
      }
    }
    ctx.restore();

    ctx.strokeStyle = palette.rim;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.arc(cx, cy, R, 0, Math.PI * 2);
    ctx.stroke();
  }

  let raf = 0, last = 0, visible = true;
  function frame(now) {
    raf = requestAnimationFrame(frame);
    if (!visible || document.hidden || now - last < FRAME_MS) return;
    const dt = last ? Math.min(0.2, (now - last) / 1000) : 0;
    last = now;
    lon0 += TURN_DEG_S * dt;
    resumeLon = lon0;
    draw(performance.now());
  }

  const size = new ResizeObserver(() => { resize(); draw(performance.now()); });
  size.observe(canvas);
  resize();
  draw(performance.now());
  let seen = null;
  if (!calm) {
    seen = new IntersectionObserver(([e]) => { visible = e.isIntersecting; });
    seen.observe(canvas);
    raf = requestAnimationFrame(frame);
  }
  return () => { cancelAnimationFrame(raf); size.disconnect(); if (seen) seen.disconnect(); };
}
