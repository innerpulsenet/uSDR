#!/usr/bin/env node
// panel-qa.mjs — front-panel QA harness for the uSDR web client.
//
// The client is one embedded file (crates/server/web/index.html, compiled in by
// include_str!), so it cannot be served without the Rust binary. This harness
// serves a copy over local HTTP with a canned API stub, drives headless Chrome
// over CDP, and for each viewport records:
//   - a screenshot (PNG)
//   - panel geometry (deck bays, frequency hero, keys, meter, header)
//   - contrast ratios of the engraved legends against their surfaces
//   - horizontal overflow (a real bug at 1024 px as of 2026-09-09)
//
// It never touches the Rust server, the dongle, or the repository's tests.
//
// Usage:
//   node tools/panel-qa/panel-qa.mjs [--page crates/server/web/index.html]
//        [--out .hermes/qa/<timestamp>] [--viewports 1600x900,1440x900,1280x800,1024x800]
//        [--no-shots] [--json]
//
// Environment facts this harness exists to encode (all verified):
//   - Chrome needs an explicit --user-data-dir or it refuses to start headless.
//   - The page must be served over HTTP; a file:// load or a dead port yields
//     chrome-error://chromewebdata/ and a blank screenshot.
//   - Headless --window-size is ~143 px taller than the resulting viewport, so
//     this harness uses Emulation.setDeviceMetricsOverride instead.
//   - Node 22 has a global WebSocket, so no npm dependency is needed for CDP.

import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { readFileSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : dflt;
};
const flag = name => args.includes(name);

const pagePath = resolve(opt('--page', 'crates/server/web/index.html'));
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const outDir = resolve(opt('--out', join('.hermes', 'qa', stamp)));
const viewports = opt('--viewports', '1600x900,1440x900,1280x800,1024x800')
  .split(',').map(v => {
    const [w, h] = v.split('x').map(Number);
    return { w, h };
  });
const wantShots = !flag('--no-shots');
const wantCrops = flag('--crops');
const jsonOnly = flag('--json');

mkdirSync(outDir, { recursive: true });

// ---------------------------------------------------------------------------
// Canned API stub: enough for the page to paint a representative panel.
// ---------------------------------------------------------------------------
const STATUS = {
  serial: '00000001', tuner: 'Rafael Micro R820T', freq_hz: 156800000, rate_hz: 2048000,
  gain_db: null, gain_now_db: 33.0, ppm: 0.0, min_freq_hz: 155776000, max_freq_hz: 157824000,
  fft_size: 2048, peak_iq: 0.31, inspect_hz: 156800000, inspect_rate_hz: 48000,
  dropped_blocks: 0, lagged_blocks: 0, lost_samples: 0, gap_events: 0,
  mode: 'nfm', audio_rate_hz: 48000, lo_offset: false, lo_offset_hz: 0,
  clip_guard: true, tuner_agc: false, zoom: 1.0, bandwidth_hz: 12500,
  spurs_hz: [], freq_error_hz: 42.0, error: null,
};
const DEVICES = { devices: [{ serial: '00000001', label: 'Generic RTL2832U OEM', tuner: 'Rafael Micro R820T' }] };
const MODES = { modes: [
  { id: 'nfm', label: 'NFM', bandwidth_hz: 12500 }, { id: 'am', label: 'AM', bandwidth_hz: 8000 },
  { id: 'wfm', label: 'WFM', bandwidth_hz: 180000 }, { id: 'p25', label: 'P25', bandwidth_hz: 12500 },
] };
const CALLS = { calls: [] };

const html = readFileSync(pagePath, 'utf8');
const server = createServer((req, res) => {
  const url = req.url.split('?')[0];
  const json = body => {
    res.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
    res.end(JSON.stringify(body));
  };
  if (url === '/' || url === '/index.html') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
    res.end(html);
  } else if (url === '/api/sdr/status') json(STATUS);
  else if (url === '/api/devices') json(DEVICES);
  else if (url === '/api/sdr/modes') json(MODES);
  else if (url === '/api/sdr/calls') json(CALLS);
  else if (url.startsWith('/api/')) json({ ok: true });
  else { res.writeHead(404); res.end('not found'); }
});
await new Promise(r => server.listen(0, '127.0.0.1', r));
const baseUrl = `http://127.0.0.1:${server.address().port}/index.html`;

// ---------------------------------------------------------------------------
// CDP plumbing
// ---------------------------------------------------------------------------
const profile = join(tmpdir(), `usdr-panel-qa-${process.pid}`);
const port = 9200 + Math.floor(Math.random() * 600);
const chrome = spawn('google-chrome', [
  '--headless=new', '--disable-gpu', '--no-sandbox', '--hide-scrollbars',
  `--user-data-dir=${profile}`, `--remote-debugging-port=${port}`, baseUrl,
], { stdio: 'ignore' });

const sleep = ms => new Promise(r => setTimeout(r, ms));

async function findTarget() {
  for (let i = 0; i < 80; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const t = list.find(t => t.type === 'page' && t.webSocketDebuggerUrl);
      if (t) return t;
    } catch {}
    await sleep(250);
  }
  throw new Error('chrome did not expose a page target');
}

const target = await findTarget();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let msgId = 0;
const pending = new Map();
ws.onmessage = ev => {
  const m = JSON.parse(ev.data);
  if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
};
const send = (method, params = {}) => {
  const id = ++msgId;
  ws.send(JSON.stringify({ id, method, params }));
  return new Promise(r => pending.set(id, r));
};

await send('Runtime.enable');
await send('Page.enable');

// ---------------------------------------------------------------------------
// What we measure inside the page
// ---------------------------------------------------------------------------
const MEASURE = `(() => {
  const box = el => { if (!el) return null; const b = el.getBoundingClientRect();
    return { x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height) }; };
  const css = el => { if (!el) return null; const c = getComputedStyle(el);
    return { font: c.fontSize, weight: c.fontWeight, tracking: c.letterSpacing,
             color: c.color, bg: c.backgroundColor, radius: c.borderRadius, shadow: c.textShadow }; };
  const q = s => document.querySelector(s);
  const qa = s => [...document.querySelectorAll(s)];
  const hero = q('#freqReadout');
  const digits = hero ? hero.querySelectorAll('.fd') : [];
  return {
    viewport: { w: innerWidth, h: innerHeight },
    docHeight: document.documentElement.scrollHeight,
    overflowX: document.documentElement.scrollWidth - innerWidth,
    rig: box(q('.rig')), brand: box(q('.rig-brand')), screen: box(q('.screen')),
    deck: box(q('.deck-console')),
    bays: qa('.deck-bay').map(b => ({ cls: b.className, box: box(b) })),
    hero: box(hero), heroDigits: digits.length,
    heroDigit: digits.length ? css(digits[0]) : null,
    heroDigitBox: digits.length ? box(digits[0]) : null,
    meter: box(q('.meter-housing')), smeterSvg: box(q('#smeterSvg')),
    flywheel: box(q('.vfo-flywheel-container')),
    keySystems: {
      mode: qa('.modebtn').slice(0, 1).map(b => ({ box: box(b), css: css(b) }))[0] || null,
      band: qa('.band-btn').slice(0, 1).map(b => ({ box: box(b), css: css(b) }))[0] || null,
      dsp: qa('.dsp-key').slice(0, 1).map(b => ({ box: box(b), css: css(b) }))[0] || null,
    },
    keyCounts: { mode: qa('.modebtn').length, band: qa('.band-btn').length, dsp: qa('.dsp-key').length },
    legends: qa('.bay-title, .fader-title, .fader-sublabel, .brand-metric-label').map(e => ({
      t: e.textContent.trim().slice(0, 28), css: css(e),
      // nearest painted ancestor background, so contrast is measured against
      // the surface the legend is actually engraved on
      bg: (() => { let n = e; while (n && n !== document.body) {
        const c = getComputedStyle(n).backgroundColor;
        if (c && c !== 'rgba(0, 0, 0, 0)' && c !== 'transparent') return c; n = n.parentElement; }
        return getComputedStyle(document.body).backgroundColor; })(),
    })),
    // 8.5 px is the documented legibility floor for panel legends, so count
    // strictly below it; 8.5 px itself is by design.
    smallTextUnder9px: qa('.screen *').filter(e => parseFloat(getComputedStyle(e).fontSize) < 8.5
      && e.children.length === 0 && (e.textContent || '').trim()).length,
  };
})()`;

// sRGB relative luminance / WCAG contrast, computed harness-side.
const lin = c => { c /= 255; return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4); };
const luminance = ([r, g, b]) => 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
const parseColor = s => {
  const m = String(s).match(/rgba?\(([^)]+)\)/);
  if (!m) return null;
  const [r, g, b, a = 1] = m[1].split(',').map(Number);
  return a === 0 ? null : [r, g, b];
};
const contrast = (fg, bg) => {
  const a = parseColor(fg), b = parseColor(bg);
  if (!a || !b) return null;
  const l1 = luminance(a), l2 = luminance(b);
  return +(((Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05))).toFixed(2);
};

// ---------------------------------------------------------------------------
// Run every viewport
// ---------------------------------------------------------------------------
const results = [];
for (const vp of viewports) {
  await send('Emulation.setDeviceMetricsOverride', {
    width: vp.w, height: vp.h, deviceScaleFactor: 1, mobile: false,
  });
  await sleep(1200); // relayout + rAF paint
  const r = await send('Runtime.evaluate', { expression: MEASURE, returnByValue: true });
  const m = r.result?.result?.value ?? {};
  const shot = wantShots
    ? await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false })
    : null;
  const file = `${vp.w}x${vp.h}.png`;
  if (shot?.result?.data) writeFileSync(join(outDir, file), Buffer.from(shot.result.data, 'base64'));

  // Element crops: a full-page screenshot downscales the panel details away.
  // These are captured at 2x so engraved legends and the meter dial can be
  // reviewed at their real size.
  if (wantCrops) {
    const CROPS = {
      header: '.rig-brand',
      frontend: '.bay-frontend',
      meter: '.bay-meter',
      vfo: '.vfo-container',
      modekeys: '.mode-keypad',
      bandkeys: '.band-matrix',
      dspkeys: '.dsp-keypad',
    };
    for (const [name, sel] of Object.entries(CROPS)) {
      const box = await send('Runtime.evaluate', {
        expression: `(() => { const e = document.querySelector(${JSON.stringify(sel)});
          if (!e) return null; const b = e.getBoundingClientRect();
          return { x: b.x, y: b.y, w: b.width, h: b.height }; })()`,
        returnByValue: true,
      });
      const b = box.result?.result?.value;
      if (!b || b.w < 2 || b.h < 2) continue;
      const crop = await send('Page.captureScreenshot', {
        format: 'png',
        clip: { x: b.x, y: b.y, width: b.w, height: b.h, scale: 2 },
      });
      if (crop?.result?.data) {
        writeFileSync(join(outDir, `crop-${vp.w}-${name}.png`), Buffer.from(crop.result.data, 'base64'));
      }
    }
  }

  const lowContrast = (m.legends || [])
    .map(l => ({ t: l.t, ratio: contrast(l.css.color, l.bg) }))
    .filter(l => l.ratio !== null && l.ratio < 4.5);

  results.push({
    viewport: `${vp.w}x${vp.h}`,
    overflowX: m.overflowX,
    docHeight: m.docHeight,
    rig: m.rig, brand: m.brand, deck: m.deck,
    bays: (m.bays || []).map(b => ({ cls: b.cls.replace(/deck-bay ?/, ''), w: b.box.w, h: b.box.h })),
    hero: m.hero, heroDigit: m.heroDigit, heroDigitBox: m.heroDigitBox, heroDigits: m.heroDigits,
    meter: m.smeterSvg, flywheel: m.flywheel,
    keySystems: m.keySystems, keyCounts: m.keyCounts,
    smallTextUnder9px: m.smallTextUnder9px,
    lowContrast,
    screenshot: shot?.result?.data ? join(outDir, file) : null,
  });
}

writeFileSync(join(outDir, 'metrics.json'), JSON.stringify(results, null, 2));
ws.close();
chrome.kill();
server.close();
try { rmSync(profile, { recursive: true, force: true }); } catch {}

if (jsonOnly) {
  console.log(JSON.stringify(results, null, 2));
} else {
  console.log(`panel-qa: ${pagePath}`);
  console.log(`out:      ${outDir}\n`);
  for (const r of results) {
    console.log(`── ${r.viewport}  overflowX=${r.overflowX}px  doc=${r.docHeight}px`);
    console.log(`   rig ${r.rig?.w}x${r.rig?.h}  brand h=${r.brand?.h}  deck ${r.deck?.w}x${r.deck?.h}`);
    console.log(`   bays ${r.bays.map(b => `${b.cls}:${b.w}`).join(' ')}`);
    console.log(`   hero ${r.hero?.w}x${r.hero?.h}  digit ${r.heroDigit?.font} ${r.heroDigitBox?.w}px  (${r.heroDigits} digits)`);
    console.log(`   keys mode ${r.keySystems.mode?.box?.w}x${r.keySystems.mode?.box?.h}@${r.keySystems.mode?.css?.font}`
      + ` | band ${r.keySystems.band?.box?.w}x${r.keySystems.band?.box?.h}@${r.keySystems.band?.css?.font}`
      + ` | dsp ${r.keySystems.dsp?.box?.w}x${r.keySystems.dsp?.box?.h}@${r.keySystems.dsp?.css?.font}`);
    console.log(`   meter ${r.meter?.w}x${r.meter?.h}  smallText<9px=${r.smallTextUnder9px}`
      + `  lowContrast=${r.lowContrast.length ? r.lowContrast.map(l => `${l.t}:${l.ratio}`).join(' ') : 'none'}`);
    if (r.screenshot) console.log(`   shot ${r.screenshot}`);
    console.log('');
  }
}
