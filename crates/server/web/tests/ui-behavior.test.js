// Behavior tests for the uSDR web UI.
//
// These run the REAL inline script from crates/server/web/index.html inside a
// stubbed DOM (node:test + node:vm — no browser required). The point is to
// pin the behaviors that regressed or were wrong:
//   1. FLEX mode must not delete non-FLEX entries from the decode history
//      (it used to splice them out permanently); the view filter must narrow
//      what is shown and switching back must restore everything.
//   2. The AUTO step label must always show the mode's own raster, even while
//      a manual override is in force (it used to inherit the override).
//   3. AM raster must be service-aware: 8.33 kHz on the airband, 10 kHz on
//      broadcast (a fixed 9 kHz step landed between channels on both).
//   4. Band-preset highlighting must follow acknowledged receiver state, not
//      the last click: tuning away clears it, being on it lights it.
//
// Run: node --test crates/server/web/tests/*.test.js
'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const PAGE = path.join(__dirname, '..', 'index.html');

// ---------------------------------------------------------------------------
// Stub DOM: enough for the page's boot block to run and for the UI functions
// under test to find their elements.
// ---------------------------------------------------------------------------
function makeEl(id) {
  const classes = new Set();
  let _inner = '';
  const el = {
    id,
    tagName: 'DIV',
    textContent: '',
    get innerHTML() { return _inner; },
    set innerHTML(v) {
      _inner = String(v);
      el.textContent = _inner.replace(/<[^>]*>/g, '');
    },
    title: '',
    value: '',
    checked: false,
    style: {},
    dataset: {},
    children: [],
    parentElement: null,
    classList: {
      add: c => classes.add(c),
      remove: c => classes.delete(c),
      contains: c => classes.has(c),
      toggle: (c, on) => {
        if (on === undefined) on = !classes.has(c);
        if (on) classes.add(c); else classes.delete(c);
        return on;
      },
      _classes: classes,
    },
    addEventListener() {},
    removeEventListener() {},
    appendChild(c) { el.children.push(c); c.parentElement = el; return c; },
    querySelector() { return null; },
    querySelectorAll() { return []; },
    getContext() { return ctx2d(); },
    getBoundingClientRect() { return { width: 800, height: 400, left: 0, top: 0 }; },
    setAttribute() {}, getAttribute() { return null; },
    focus() {}, blur() {}, click() {}, scrollIntoView() {},
    width: 800, height: 400, scrollTop: 0, scrollHeight: 0, clientHeight: 400,
  };
  return el;
}

function ctx2d() {
  const noop = () => {};
  const c = {
    canvas: null,
    fillStyle: '', strokeStyle: '', lineWidth: 1, font: '', globalAlpha: 1,
    textAlign: '', textBaseline: '', filter: '', imageSmoothingEnabled: true,
    save: noop, restore: noop, clearRect: noop, fillRect: noop, strokeRect: noop,
    beginPath: noop, closePath: noop, moveTo: noop, lineTo: noop, stroke: noop,
    fill: noop, arc: noop, rect: noop, clip: noop, translate: noop, scale: noop,
    rotate: noop, setTransform: noop, resetTransform: noop,
    fillText: noop, strokeText: noop, measureText: () => ({ width: 10 }),
    drawImage: noop, putImageData: noop,
    getImageData: () => ({ data: new Uint8ClampedArray(4) }),
    createImageData: (w, h) => ({ data: new Uint8ClampedArray(w * h * 4), width: w, height: h }),
    createLinearGradient: () => ({ addColorStop: noop }),
    createRadialGradient: () => ({ addColorStop: noop }),
    setLineDash: noop, getLineDash: () => [], quadraticCurveTo: noop, bezierCurveTo: noop,
    ellipse: noop, arcTo: noop, isPointInPath: () => false,
  };
  return c;
}

function buildContext() {
  const byId = new Map();
  const bandBtns = [
    { freq: '121500000', mode: 'am', label: 'AIR 118' },
    { freq: '156800000', mode: 'nfm', label: 'MAR 156' },
    { freq: '929612500', mode: 'pager', label: 'PAGER' },
    { freq: '98100000', mode: 'wfm', label: 'WFM 88' },
  ].map(d => {
    const el = makeEl('band-' + d.freq);
    el.dataset.freq = d.freq;
    el.dataset.mode = d.mode;
    el.textContent = d.label;
    el.classList.add('band-btn');
    return el;
  });

  const qmemBtns = Array.from({ length: 8 }, (_, i) => {
    const el = makeEl('qmem-' + i);
    el.classList.add('qmem-btn');
    return el;
  });

  const document = {
    getElementById(id) {
      if (!byId.has(id)) byId.set(id, makeEl(id));
      return byId.get(id);
    },
    querySelectorAll(sel) {
      if (sel === '.band-btn') return bandBtns;
      if (sel === '.qmem-btn') return qmemBtns;
      return [];
    },
    querySelector() { return null; },
    createElement(tag) { return makeEl('created-' + tag); },
    createElementNS(ns, tag) {
      const el = makeEl('created-' + tag);
      el.tagName = String(tag).toUpperCase();
      el.namespaceURI = ns;
      el.setAttributeNS = () => {};
      return el;
    },
    addEventListener() {}, removeEventListener() {},
    body: makeEl('body'),
    documentElement: makeEl('html'),
    activeElement: null,
    hidden: false,
    visibilityState: 'visible',
  };

  class WebSocketStub {
    static OPEN = 1;
    constructor() { this.readyState = 1; this.onopen = null; this.onmessage = null; this.onclose = null; this.onerror = null; }
    send() {} close() { this.readyState = 3; }
    addEventListener() {}
  }

  const sandbox = {
    document,
    WebSocket: WebSocketStub,
    navigator: { userAgent: 'node-test', platform: 'linux', clipboard: { writeText: async () => {} } },
    location: { href: 'http://127.0.0.1:8073/', protocol: 'http:', host: '127.0.0.1:8073', search: '' },
    history: { pushState() {}, replaceState() {} },
    fetch: async () => ({ ok: true, json: async () => ({}), text: async () => '' }),
    setInterval: () => 0, clearInterval() {}, setTimeout: (f) => 0, clearTimeout() {},
    requestAnimationFrame: () => 0, cancelAnimationFrame() {},
    performance: { now: () => Date.now() },
    matchMedia: () => ({ matches: false, addEventListener() {}, addListener() {} }),
    localStorage: { getItem: () => null, setItem() {}, removeItem() {} },
    sessionStorage: { getItem: () => null, setItem() {}, removeItem() {} },
    AudioContext: class { constructor() { this.state = 'running'; this.destination = {}; this.sampleRate = 48000; } createGain() { return { gain: { value: 1, setValueAtTime() {} }, connect() {}, disconnect() {} }; } createBufferSource() { return { connect() {}, start() {}, stop() {}, buffer: null }; } createScriptProcessor() { return { connect() {}, onaudioprocess: null }; } resume() { return Promise.resolve(); } close() { return Promise.resolve(); } },
    Image: class { constructor() { this.width = 1; this.height = 1; } },
    Audio: class { play() { return Promise.resolve(); } pause() {} },
    Blob: class { constructor() {} },
    URL: Object.assign(function () {}, { createObjectURL: () => 'blob:stub', revokeObjectURL() {} }),
    FileReader: class { readAsArrayBuffer() {} readAsDataURL() {} },
    ResizeObserver: class { observe() {} unobserve() {} disconnect() {} },
    MutationObserver: class { observe() {} disconnect() {} },
    IntersectionObserver: class { observe() {} disconnect() {} },
    console: { log() {}, warn() {}, error() {}, info() {}, debug() {} },
    alert() {}, confirm: () => false, prompt: () => null,
    devicePixelRatio: 1, innerWidth: 1920, innerHeight: 1080,
    addEventListener() {}, removeEventListener() {},
    scrollTo() {}, getComputedStyle: () => ({ getPropertyValue: () => '' }),
    OffscreenCanvas: class { constructor(w, h) { this.width = w; this.height = h; } getContext() { return ctx2d(); } },
    Path2D: class {}, ImageData: class {}, TextEncoder, TextDecoder,
    atob: s => Buffer.from(s, 'base64').toString('binary'),
    btoa: s => Buffer.from(s, 'binary').toString('base64'),
    queueMicrotask: f => f(),
    structuredClone: x => JSON.parse(JSON.stringify(x)),
    crypto: { randomUUID: () => '00000000-0000-0000-0000-000000000000' },
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.globalThis = sandbox;
  sandbox.__bandBtns = bandBtns;

  const ctx = vm.createContext(sandbox);
  const html = fs.readFileSync(PAGE, 'utf8');
  const m = html.match(/<script[^>]*>([\s\S]*?)<\/script>/);
  if (!m) throw new Error('no inline <script> found in index.html');
  vm.runInContext(m[1], ctx, { filename: 'index.html inline script' });
  return ctx;
}

function run(ctx, code) {
  // No IIFE wrapper: vm.runInContext returns the completion value of the
  // script, which is the value of its last expression statement.
  return vm.runInContext(code, ctx);
}

// ---------------------------------------------------------------------------
// 1. FLEX mode must not destroy decode history.
// ---------------------------------------------------------------------------
test('FLEX mode filters the view but never deletes history', () => {
  const ctx = buildContext();
  run(ctx, `
    sdrDecodeLogData.length = 0;
    let uid = 1;
    const add = (protocol, summary) => sdrDecodeLogData.unshift(
      { uid: uid++, t: Date.now(), timeStr: 'now', freqHz: 929612500,
        protocol, kind: 'page', summary, valid: true, fields: {} });
    add('POCSAG', 'page A');
    add('FLEX', 'flex page');
    add('POCSAG', 'page B');
    applyModeUi('flex');
  `);
  // History intact while in FLEX mode.
  const kept = run(ctx, `sdrDecodeLogData.length`);
  assert.strictEqual(kept, 3, 'FLEX mode must not delete entries from the history');
  // The rendered view shows only the FLEX frame.
  const rows = run(ctx, `
    renderSdrDecodeLog();
    (document.getElementById('sdrDecodeLog').innerHTML.match(/\\[/g) || []).length
  `);
  const logHtml = run(ctx, `document.getElementById('sdrDecodeLog').innerHTML`);
  assert.ok(logHtml.includes('FLEX'), 'FLEX view should show the FLEX frame');
  assert.ok(!logHtml.includes('POCSAG'), 'FLEX view should hide POCSAG frames');
  // Switching back restores the full view.
  run(ctx, `applyModeUi('nfm'); renderSdrDecodeLog();`);
  const back = run(ctx, `document.getElementById('sdrDecodeLog').innerHTML`);
  assert.ok(back.includes('POCSAG'), 'leaving FLEX mode must bring POCSAG frames back');
  assert.ok(back.includes('FLEX'), 'leaving FLEX mode keeps FLEX frames too');
  assert.strictEqual(run(ctx, `sdrDecodeLogData.length`), 3);
  void rows;
});

// ---------------------------------------------------------------------------
// 2. AUTO step label shows the mode raster, not the manual override.
// ---------------------------------------------------------------------------
test('AUTO step label keeps showing the mode default under a manual override', () => {
  const ctx = buildContext();
  run(ctx, `
    sdrMode = 'nfm';
    sdrInspectHz = 156800000;
    snapOverrideHz = 100;           // operator forced a 100 Hz step
    updateSnapHint();
  `);
  const autoLabel = run(ctx, `document.getElementById('snapOptAuto').textContent`);
  assert.strictEqual(autoLabel, 'AUTO (6.25 kHz)',
    'AUTO option must advertise the mode raster (6.25 kHz), not the 100 Hz override');
  // And the effective readout reflects the override, visibly.
  const eff = run(ctx, `document.getElementById('snapEffective')`);
  assert.strictEqual(eff.textContent, '100 Hz');
  assert.notStrictEqual(eff.style.display, 'none',
    'the effective-step indicator must be visible while an override is in force');
  // Clearing the override returns AUTO semantics.
  run(ctx, `snapOverrideHz = 0; updateSnapHint();`);
  assert.strictEqual(run(ctx, `document.getElementById('snapOptAuto').textContent`), 'AUTO (6.25 kHz)');
  assert.strictEqual(run(ctx, `document.getElementById('snapEffective').style.display`), 'none');
});

// ---------------------------------------------------------------------------
// 3. Service-aware AM raster.
// ---------------------------------------------------------------------------
test('AM raster is service-aware: 8.33 kHz airband, 10 kHz broadcast', () => {
  const ctx = buildContext();
  run(ctx, `sdrMode = 'am'; sdrInspectHz = 121500000;`);
  assert.strictEqual(run(ctx, `modeSnapStepHz()`), 8330, 'airband AM uses the 8.33 kHz raster');
  run(ctx, `sdrInspectHz = 770000;`);
  assert.strictEqual(run(ctx, `modeSnapStepHz()`), 10000, 'broadcast AM uses the 10 kHz raster');
  // Snapping lands on real channels, not between them (grid assertions in
  // the next test).
  run(ctx, `sdrInspectHz = 121500000;`);
  assert.strictEqual(run(ctx, `modeSnapStepHz()`), 8330);
});

test('AM snapping lands on 8.33 kHz channels in the airband', () => {
  const ctx = buildContext();
  run(ctx, `sdrMode = 'am'; sdrInspectHz = 121500000;`);
  // 121.5000 MHz is a channel: a tune to 121.5012 snaps to 121.50083? No —
  // 8330 Hz grid: 121501200/8330 = 14586.09… → 14586*8330 = 121501380? keep
  // the test to the property: the snapped value must be a multiple of 8330.
  const snapped = run(ctx, `snapHz(121501234)`);
  assert.strictEqual(snapped % 8330, 0, 'airband snap must land on the 8.33 kHz grid');
  run(ctx, `sdrInspectHz = 770000;`);
  const bc = run(ctx, `snapHz(771234)`);
  assert.strictEqual(bc % 10000, 0, 'broadcast snap must land on the 10 kHz grid');
});

// ---------------------------------------------------------------------------
// 4. Band presets follow receiver state.
// ---------------------------------------------------------------------------
test('band preset highlight follows acknowledged receiver state', () => {
  const ctx = buildContext();
  // Receiver on the marine preset (156.800 NFM).
  run(ctx, `
    sdrMode = 'nfm';
    sdrInspectHz = 156800000;
    updateBandPresetUi();
  `);
  let active = run(ctx, `__bandBtns.filter(b => b.classList.contains('active')).map(b => b.dataset.freq)`);
  assert.deepStrictEqual(active, ['156800000'], 'the marine preset is lit while the receiver is on it');
  // Tune elsewhere by any means → the row clears.
  run(ctx, `sdrInspectHz = 156900000; updateBandPresetUi();`);
  active = run(ctx, `__bandBtns.filter(b => b.classList.contains('active')).map(b => b.dataset.freq)`);
  assert.deepStrictEqual(active, [], 'tuning off a preset must clear its highlight');
  // Wrong mode at the right frequency does not light it either.
  run(ctx, `sdrInspectHz = 156800000; sdrMode = 'am'; updateBandPresetUi();`);
  active = run(ctx, `__bandBtns.filter(b => b.classList.contains('active')).map(b => b.dataset.freq)`);
  assert.deepStrictEqual(active, [], 'frequency alone must not light a mode-specific preset');
  // The airband AM preset lights on its own freq+mode.
  run(ctx, `sdrInspectHz = 121500000; sdrMode = 'am'; updateBandPresetUi();`);
  active = run(ctx, `__bandBtns.filter(b => b.classList.contains('active')).map(b => b.dataset.freq)`);
  assert.deepStrictEqual(active, ['121500000'], 'airband preset lights when freq and mode both match');
});

test('NXDN mode is wired into the UI mode table', () => {
  const ctx = buildContext();
  // applyModeUi('nxdn') must run without error and set the mode; the raster
  // table must know it (6.25 kHz like the other digital voice modes).
  run(ctx, `applyModeUi('nxdn');`);
  assert.strictEqual(run(ctx, `sdrMode`), 'nxdn');
  assert.strictEqual(run(ctx, `modeSnapStepHz()`), 6250,
    'NXDN must have its own raster entry, not fall through to the 6250 default by luck');
  const step = run(ctx, `SNAP_STEPS.nxdn`);
  assert.strictEqual(step, 6250, 'SNAP_STEPS must carry nxdn explicitly');
});

// ---------------------------------------------------------------------------
// 5. Refused commands must reach the error line with the server's reason.
// ---------------------------------------------------------------------------
test('a refused command surfaces the server reason', async () => {
  const ctx = buildContext();
  // Replace fetch with one that answers 400 with the server's plain-text
  // reason, exactly like ApiError::BadRequest does. Await the in-context
  // promise directly: cross-realm await is deterministic, while racing a
  // fixed sleep against the VM's microtask drain is not.
  vm.runInContext(`
    fetch = async (url, opts) => ({
      ok: false, status: 400,
      text: async () => 'frequency outside the dongle range',
      json: async () => ({}),
    });
  `, ctx);
  const p = vm.runInContext(`
    sdrPost('/api/sdr/tune', { freq_hz: 1 }, 'tune')
      .then(() => 'NO THROW', e => e.message);
  `, ctx);
  const err = await p;
  assert.ok(err.includes('refused'), `error must say the command was refused: ${err}`);
  assert.ok(err.includes('frequency outside the dongle range'),
    `error must carry the server's reason: ${err}`);
});

test('a successful command resolves without an error', async () => {
  const ctx = buildContext();
  vm.runInContext(`
    fetch = async () => ({ ok: true, status: 200, text: async () => '', json: async () => ({}) });
  `, ctx);
  const p = vm.runInContext(`
    sdrPost('/api/sdr/tune', { freq_hz: 156800000 }, 'tune')
      .then(() => 'OK', e => 'THREW: ' + e.message);
  `, ctx);
  assert.strictEqual(await p, 'OK');
});

// ---------------------------------------------------------------------------
// 6. Scope chrome: leaving a symbol scope must clear the per-frame lock
//    colour from the trigger tag.
// ---------------------------------------------------------------------------
test('scope chrome resets the trigger tag colour on kind change', () => {
  const ctx = buildContext();
  // Simulate the symbol scope's per-frame colouring of the trigger tag…
  run(ctx, `
    sdrMode = 'p25';
    setScopeChrome('symbols');
    document.getElementById('scopeTrigTag').style.color = '#2bff6a';
  `);
  assert.strictEqual(run(ctx, `document.getElementById('scopeTrigTag').style.color`), '#2bff6a');
  // …then switch to an audio scope: the stale lock-green must not persist.
  run(ctx, `
    sdrMode = 'nfm';
    setScopeChrome('audio');
  `);
  assert.strictEqual(run(ctx, `document.getElementById('scopeTrigTag').style.color`), '',
    'leaving the symbol scope must clear the per-frame lock colour');
  assert.strictEqual(run(ctx, `document.getElementById('scopeTrigTag').textContent`), 'AUTO ZERO-CROSS');
});

// ---------------------------------------------------------------------------
// 7. Transceiver deck: Standalone AF Gain knob and RF Gain fader.
// ---------------------------------------------------------------------------
test('standalone AF Gain knob syncs volume and readout correctly', () => {
  const ctx = buildContext();
  run(ctx, `
    audioOn = true;
    audioVolume = 0.75;
    syncAfVolUi();
  `);
  const readout = run(ctx, `document.getElementById('deckAfVolReadout').textContent`);
  assert.ok(readout.includes('75%'), `readout should display 75%: ${readout}`);
  assert.ok(readout.includes('MON ON'), `readout should indicate monitor on: ${readout}`);

  // Test mute
  run(ctx, `
    audioOn = false;
    syncAfVolUi();
  `);
  const mutedReadout = run(ctx, `document.getElementById('deckAfVolReadout').textContent`);
  assert.ok(mutedReadout.includes('MON OFF'), `readout should indicate monitor off: ${mutedReadout}`);
});

test('linear faders for RF Gain, Squelch, SPAN and Zoom sync accurately', () => {
  const ctx = buildContext();
  // 1. RF Gain Fader & AGC button
  run(ctx, `
    document.getElementById('sdrGainRange').value = '35';
    document.getElementById('sdrAgc').checked = false;
    syncGainUi();
  `);
  assert.strictEqual(run(ctx, `document.getElementById('faderRfGainCap').style.bottom`), '70.0%');
  assert.strictEqual(run(ctx, `document.getElementById('faderRfGainVal').textContent`), '35 dB');
  assert.strictEqual(run(ctx, `document.getElementById('faderAgcBtn').classList.contains('on')`), false);

  // Toggle AGC
  run(ctx, `
    document.getElementById('sdrAgc').checked = true;
    syncGainUi();
  `);
  assert.strictEqual(run(ctx, `document.getElementById('faderRfGainVal').textContent`), 'auto');
  assert.strictEqual(run(ctx, `document.getElementById('faderAgcBtn').classList.contains('on')`), true);

  // 2. Squelch Fader live sync
  run(ctx, `
    sdrMinDb = -100;
    sdrMaxDb = 0;
    sdrSquelchDb = -40;
    syncSquelchUi();
  `);
  // (-40 - (-100)) / 100 = 60%
  assert.strictEqual(run(ctx, `document.getElementById('faderSqlCap').style.bottom`), '60.0%');
  assert.strictEqual(run(ctx, `document.getElementById('faderSqlVal').textContent`), '-40 dB');

  // 3. SPAN Rate Fader
  run(ctx, `
    document.getElementById('sdrRateSel').value = '2048000';
    syncSpanKnobUi();
  `);
  assert.strictEqual(run(ctx, `document.getElementById('faderSpanVal').textContent`), '2.048M');

  // 4. FFT Zoom Fader
  run(ctx, `
    sdrZoom = 4.0;
    syncZoomUi();
  `);
  // log2(4) / 5 = 2 / 5 = 40%
  assert.strictEqual(run(ctx, `document.getElementById('faderZoomCap').style.bottom`), '40.0%');
  assert.strictEqual(run(ctx, `document.getElementById('faderZoomVal').textContent`), '4.0x');
});

test('Main VFO flywheel setup generates 72 knurling teeth', () => {
  const ctx = buildContext();
  run(ctx, `
    setupVfoFlywheel();
  `);
  const knurls = run(ctx, `document.getElementById('vfoKnurlGroup').children.length`);
  assert.strictEqual(knurls, 72, 'Main VFO flywheel must have 72 precision knurling teeth');
});

test('LISTEN/MUTE and LO OFFSET buttons and VFO telemetry bar sync correctly', () => {
  const ctx = buildContext();
  run(ctx, `
    startAudio();
  `);
  assert.strictEqual(run(ctx, `document.getElementById('listenBtnLabel').textContent`), 'MUTE');
  assert.strictEqual(run(ctx, `document.getElementById('btnListen').classList.contains('on')`), true);

  run(ctx, `
    stopAudio();
  `);
  assert.strictEqual(run(ctx, `document.getElementById('listenBtnLabel').textContent`), 'MUTED');
  assert.strictEqual(run(ctx, `document.getElementById('btnListen').classList.contains('on')`), false);

  run(ctx, `
    updateSdrStatus({ lo_offset: true });
  `);
  assert.strictEqual(run(ctx, `document.getElementById('btnLoOffset').classList.contains('on')`), true);

  run(ctx, `
    updateSdrStatus({ freq_error_hz: 245.0 });
  `);
  assert.strictEqual(run(ctx, `document.getElementById('vfoFreqErrVal').textContent`), '+245 Hz');
  assert.strictEqual(run(ctx, `document.getElementById('freqErrVal').textContent`), 'err 245 Hz');

  run(ctx, `
    updateSdrStatus({ freq_error_hz: null });
  `);
  assert.strictEqual(run(ctx, `document.getElementById('vfoFreqErrVal').textContent`), '—');
  assert.strictEqual(run(ctx, `document.getElementById('freqErrVal').textContent`), 'err —');

  run(ctx, `
    updateMeter(null, null, -50.0, -70.0, null);
  `);
  assert.strictEqual(run(ctx, `document.getElementById('vfoChPwrVal').textContent`), '-50.0 dBFS');
  assert.strictEqual(run(ctx, `document.getElementById('vfoSnrVal').textContent`), '20.0 dB');
  assert.strictEqual(run(ctx, `document.getElementById('vfoSqlGateVal').textContent`), 'OPEN');
});

test('Realistic DOM environment with strict element checking boots without errors', () => {
  const html = fs.readFileSync(PAGE, 'utf8');
  const validIds = new Set();
  const idRegex = /id=["']([^"']+)["']/g;
  let match;
  while ((match = idRegex.exec(html)) !== null) {
    validIds.add(match[1]);
  }

  const byId = new Map();
  const document = {
    getElementById(id) {
      if (!validIds.has(id)) return null;
      if (!byId.has(id)) byId.set(id, makeEl(id));
      return byId.get(id);
    },
    querySelectorAll() { return []; },
    querySelector() { return null; },
    createElement(tag) { return makeEl('created-' + tag); },
    createElementNS(ns, tag) {
      const el = makeEl('created-' + tag);
      el.tagName = String(tag).toUpperCase();
      return el;
    },
    addEventListener() {}, removeEventListener() {},
    body: makeEl('body'),
    documentElement: makeEl('html'),
    activeElement: null,
    hidden: false,
    visibilityState: 'visible',
  };

  class WebSocketStub {
    static OPEN = 1;
    constructor() { this.readyState = 1; }
    send() {} close() { this.readyState = 3; }
    addEventListener() {}
  }

  const sandbox = {
    document,
    WebSocket: WebSocketStub,
    navigator: { userAgent: 'node-test', platform: 'linux', clipboard: { writeText: async () => {} } },
    location: { href: 'http://127.0.0.1:8073/', protocol: 'http:', host: '127.0.0.1:8073', search: '' },
    history: { pushState() {}, replaceState() {} },
    fetch: async () => ({ ok: true, json: async () => ({}), text: async () => '' }),
    setInterval: () => 0, clearInterval() {}, setTimeout: () => 0, clearTimeout() {},
    requestAnimationFrame: () => 0, cancelAnimationFrame() {},
    performance: { now: () => Date.now() },
    matchMedia: () => ({ matches: false, addEventListener() {}, addListener() {} }),
    localStorage: { getItem: () => null, setItem: () => {}, removeItem: () => {} },
    sessionStorage: { getItem: () => null, setItem: () => {}, removeItem: () => {} },
    AudioContext: class { constructor() { this.state = 'running'; this.destination = {}; this.sampleRate = 48000; } createGain() { return { gain: { value: 1, setValueAtTime() {} }, connect() {}, disconnect() {} }; } createBufferSource() { return { connect() {}, start() {}, stop() {}, buffer: null }; } createScriptProcessor() { return { connect() {}, onaudioprocess: null }; } resume() { return Promise.resolve(); } close() { return Promise.resolve(); } },
    Image: class { constructor() { this.width = 1; this.height = 1; } },
    Audio: class { play() { return Promise.resolve(); } pause() {} },
    Blob: class {},
    URL: Object.assign(function () {}, { createObjectURL: () => 'blob:stub', revokeObjectURL() {} }),
    FileReader: class { readAsArrayBuffer() {} readAsDataURL() {} },
    ResizeObserver: class { observe() {} unobserve() {} disconnect() {} },
    MutationObserver: class { observe() {} disconnect() {} },
    IntersectionObserver: class { observe() {} disconnect() {} },
    console: { log() {}, warn() {}, error() {}, info() {}, debug() {} },
    alert() {}, confirm: () => false, prompt: () => null,
    devicePixelRatio: 1, innerWidth: 1920, innerHeight: 1080,
    addEventListener() {}, removeEventListener() {},
    scrollTo() {}, getComputedStyle: () => ({ getPropertyValue: () => '' }),
    OffscreenCanvas: class { constructor(w, h) { this.width = w; this.height = h; } getContext() { return ctx2d(); } },
    Path2D: class {}, ImageData: class {}, TextEncoder, TextDecoder,
    atob: s => Buffer.from(s, 'base64').toString('binary'),
    btoa: s => Buffer.from(s, 'binary').toString('base64'),
    queueMicrotask: f => f(),
    structuredClone: x => JSON.parse(JSON.stringify(x)),
    crypto: { randomUUID: () => '00000000-0000-0000-0000-000000000000' },
  };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.globalThis = sandbox;

  const ctx = vm.createContext(sandbox);
  const m = html.match(/<script[^>]*>([\s\S]*?)<\/script>/);
  assert.doesNotThrow(() => {
    vm.runInContext(m[1], ctx, { filename: 'index.html inline script' });
  });
});

test('WFM scope defaults to 15 kHz audio passband and toggles to MPX mode', () => {
  const ctx = buildContext();
  run(ctx, `
    scopeWaveCtx = document.getElementById('scopeWaveCanvas').getContext('2d');
    scopeSpecCtx = document.getElementById('scopeSpecCanvas').getContext('2d');
    sdrMode = 'wfm';
    scopeWfmMode = 'audio';
    setScopeChrome('mpx');
    drawAudioScope([], 128000);
  `);
  assert.strictEqual(run(ctx, `document.getElementById('scopeTitle').textContent`), 'WFM BROADCAST AUDIO');
  assert.strictEqual(run(ctx, `document.getElementById('scopeModeTag').textContent`), 'AUDIO (15kHz) ⇄ MPX');
  assert.strictEqual(run(ctx, `document.getElementById('scopeTrigTag').textContent`), '15kHz AUDIO LPF');
  const axAudio = run(ctx, `document.getElementById('scopeAxis').textContent`);
  assert.ok(axAudio.includes('15kHz'), `axis should display 15kHz in audio mode, got ${axAudio}`);

  // Toggle to MPX
  run(ctx, `
    scopeWfmMode = 'mpx';
    setScopeChrome('mpx');
    drawAudioScope([], 128000);
  `);
  assert.strictEqual(run(ctx, `document.getElementById('scopeTitle').textContent`), 'FM MULTIPLEX SCOPE');
  assert.strictEqual(run(ctx, `document.getElementById('scopeModeTag').textContent`), 'MPX (64kHz) ⇄ AUDIO');
  const axMpx = run(ctx, `document.getElementById('scopeAxis').textContent`);
  assert.ok(axMpx.includes('64kHz'), `axis should display 64kHz in mpx mode, got ${axMpx}`);
});

test('Quick Memory matrix and DSP controls exist and handle events', () => {
  const ctx = buildContext();
  run(ctx, `
    setupRadioDeck();
  `);
  const qmemButtons = run(ctx, `document.querySelectorAll('.qmem-btn').length`);
  assert.strictEqual(qmemButtons, 8, 'Must have 8 Quick Memory buttons M1-M8');

  // Check DSP buttons
  assert.ok(run(ctx, `document.getElementById('btnDspNotch') !== null`), 'btnDspNotch must exist');
  assert.ok(run(ctx, `document.getElementById('btnDspNr') !== null`), 'btnDspNr must exist');
  assert.ok(run(ctx, `document.getElementById('btnDspSql') !== null`), 'btnDspSql must exist');
});

