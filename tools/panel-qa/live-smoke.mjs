// live-smoke.mjs — end-to-end smoke test against a REAL running server.
//
// Unlike panel-qa.mjs (which serves a static copy with a canned API stub),
// this starts the actual `usdr` binary, loads the real page in headless
// Chrome, and exercises the interactions through the real REST/WebSocket
// path. It uses a scratch config file and a private port so the operator's
// own settings and the running receiver are never disturbed.
//
// Usage:
//   cargo build --release
//   node tools/panel-qa/live-smoke.mjs [--port 8074] [--bin target/release/usdr]
//
// Requires an RTL-SDR the process can open (/dev/bus/usb/…). Without one the
// server still serves the page and the API, and the RF-dependent checks are
// reported SKIP rather than FAIL, so the harness stays useful in CI or inside
// a sandbox that has no USB device nodes.
import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const arg = (n, d) => { const i = process.argv.indexOf(n); return i >= 0 ? process.argv[i + 1] : d; };
const port = Number(arg('--port', 8074));
const bin = resolve(arg('--bin', 'target/release/usdr'));
const config = resolve('.hermes/qa/live-usdr.toml');

const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok, skipped: false, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? '  — ' + detail : ''}`);
};
const skip = (name, why) => {
  results.push({ name, ok: true, skipped: true, detail: why });
  console.log(`SKIP  ${name}  — ${why}`);
};

const server = spawn(bin, ['--bind', `127.0.0.1:${port}`, '--config', config], {
  cwd: process.cwd(), stdio: ['ignore', 'pipe', 'pipe'],
});
let serverLog = '';
server.stdout.on('data', d => { serverLog += d; });
server.stderr.on('data', d => { serverLog += d; });
const sleep = ms => new Promise(r => setTimeout(r, ms));

async function waitForServer() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/api/sdr/status`);
      if (r.ok) return await r.json();
    } catch {}
    await sleep(500);
  }
  throw new Error('server did not come up:\n' + serverLog.slice(-800));
}

const status = await waitForServer();
const radioPresent = !!status.serial && status.rate_hz > 0;
check('server serves /api/sdr/status', typeof status.mode === 'string',
  radioPresent ? `serial=${status.serial} tuner=${status.tuner}` : `no radio: ${status.error || 'unknown'}`);
if (radioPresent) {
  check('status carries a live span', status.rate_hz > 0 && status.min_freq_hz < status.max_freq_hz,
    `${(status.rate_hz / 1e6).toFixed(3)} MS/s ${(status.min_freq_hz / 1e6).toFixed(3)}–${(status.max_freq_hz / 1e6).toFixed(3)} MHz`);
} else {
  skip('status carries a live span', 'no RTL-SDR device reachable from this process');
}

// --- browser ---
const profile = mkdtempSync(join(tmpdir(), 'usdr-live-'));
const cdpPort = 9500 + Math.floor(Math.random() * 400);
const chrome = spawn('google-chrome', [
  '--headless=new', '--disable-gpu', '--no-sandbox', '--hide-scrollbars',
  `--user-data-dir=${profile}`, `--remote-debugging-port=${cdpPort}`,
  '--window-size=1600,1000', `http://127.0.0.1:${port}/`,
], { stdio: 'ignore' });

let target;
for (let i = 0; i < 80; i++) {
  try {
    const list = await (await fetch(`http://127.0.0.1:${cdpPort}/json/list`)).json();
    target = list.find(t => t.type === 'page' && t.webSocketDebuggerUrl);
    if (target) break;
  } catch {}
  await sleep(250);
}
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
let id = 0; const pending = new Map();
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); } };
const send = (method, params = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method, params })); return new Promise(r => pending.set(i, r)); };
const evalJs = async expr => {
  const r = await send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true });
  return r.result?.result?.value;
};
await send('Runtime.enable');
await sleep(4000); // boot, connect, let frames flow

check('page title is served from the embedded asset', (await evalJs('document.title')) === 'uSDR');
check('WebSocket connects', /connected/.test(await evalJs(`document.getElementById('conn').textContent`)),
  (await evalJs(`document.getElementById('conn').textContent`)).trim());

const freqText = (await evalJs(`document.getElementById('freqReadout').textContent`)).replace(/\s+/g, '');
check('frequency readout renders a frequency', /^\d{4}\.\d{3}\.\d{2}MHz$/.test(freqText), freqText);

const painted = await evalJs(`(() => {
  const c = document.getElementById('sdrSpectrumCanvas');
  const x = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
  let n = 0; for (let i = 0; i < x.length; i += 4) if (x[i] + x[i+1] + x[i+2] > 24) n++;
  return n;
})()`);
check('spectrum canvas is painting', painted > 500, `${painted} lit pixels`);

if (radioPresent) {
  const meter = await evalJs(`document.getElementById('meterNum').textContent`);
  check('S-meter readout is live', /CH .* dBFS/.test(meter) && !/CH —/.test(meter), meter);
} else {
  skip('S-meter readout is live', 'no radio: channel power is not measured');
}

// --- interactions ---
await evalJs(`document.querySelector('.band-btn[data-freq="144200000"]').click()`);
await sleep(1500);
const afterBand = (await evalJs(`document.getElementById('freqReadout').textContent`)).replace(/\s+/g, '');
check('band preset click retunes the receiver', afterBand.startsWith('0144.200'), afterBand);
check('band preset highlight follows the tune',
  await evalJs(`document.querySelector('.band-btn[data-freq="144200000"]').classList.contains('active')`));

// The click handler must acknowledge on the panel synchronously — a real radio
// responds to the key, not to the next status frame. Read the badge in the same
// evaluation, before any status frame can arrive and re-assert the server's
// (unacknowledged, in the no-radio case) mode.
const immediateMode = await evalJs(`(() => {
  document.querySelector('.modebtn[data-mode="am"]').click();
  return document.getElementById('vfoModeBadge').textContent;
})()`);
check('mode key acknowledges on the panel immediately', immediateMode === 'AM', immediateMode);

if (radioPresent) {
  await sleep(1200);
  check('mode key holds after the server acknowledges',
    (await evalJs(`document.getElementById('vfoModeBadge').textContent`)) === 'AM'
    && (await evalJs(`document.querySelector('.modebtn[data-mode="am"]').classList.contains('on')`)),
    await evalJs(`document.getElementById('vfoModeBadge').textContent`));
  const st = await fetch(`http://127.0.0.1:${port}/api/sdr/status`).then(r => r.json());
  check('server acknowledged the mode change', st.mode === 'am', `server mode=${st.mode}`);
} else {
  skip('server acknowledged the mode change', 'no radio: the worker is not running to apply it');
}

await evalJs(`document.getElementById('keyPalette').click()`);
await sleep(400);
check('palette popup opens', (await evalJs(`document.getElementById('paletteMenu').style.display`)) !== 'none');
await evalJs(`document.body.click()`);
await sleep(400);
check('palette popup closes on outside click', (await evalJs(`document.getElementById('paletteMenu').style.display`)) === 'none');

await evalJs(`document.getElementById('btnListen').click()`);
await sleep(600);
check('LISTEN toggles audio monitor', (await evalJs(`document.getElementById('listenBtnLabel').textContent`)) === 'MUTE',
  await evalJs(`document.getElementById('listenBtnLabel').textContent`));
await evalJs(`document.getElementById('btnListen').click()`);

await evalJs(`(() => {
  const d = document.querySelector('#freqReadout .fd[data-i="6"]');
  const r = d.getBoundingClientRect();
  d.dispatchEvent(new WheelEvent('wheel', { deltaY: -100, bubbles: true, cancelable: true,
    clientX: r.x + r.width / 2, clientY: r.y + r.height / 2 }));
})()`);
await sleep(1200);
const afterWheel = (await evalJs(`document.getElementById('freqReadout').textContent`)).replace(/\s+/g, '');
check('scrolling a digit tunes', afterWheel !== afterBand, `${afterBand} -> ${afterWheel}`);

const shot = await send('Page.captureScreenshot', { format: 'png' });
if (shot?.result?.data) {
  writeFileSync('.hermes/qa/live-smoke.png', Buffer.from(shot.result.data, 'base64'));
  console.log('screenshot: .hermes/qa/live-smoke.png');
}

ws.close();
chrome.kill();
server.kill('SIGTERM');
await sleep(800);
try { server.kill('SIGKILL'); } catch {}
try { rmSync(profile, { recursive: true, force: true }); } catch {}

const failed = results.filter(r => !r.ok);
const skipped = results.filter(r => r.skipped).length;
console.log(`\n${results.length - failed.length - skipped}/${results.length - skipped} live checks passed`
  + (skipped ? `, ${skipped} skipped (no radio)` : ''));
process.exit(failed.length ? 1 : 0);
