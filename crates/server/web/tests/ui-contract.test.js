// Contract tests for the uSDR web UI.
//
// The front-panel restyle (see .hermes/plans/2026-09-09_000507-front-panel-realism.md
// §2) is allowed to change every pixel, but it must not change the DOM
// contract the inline script depends on. The page is one embedded file
// (include_str! in crates/server/src/main.rs:33), so a restyle that renames an
// id or drops a class fails *silently* at runtime — the script's guarded
// lookups just stop finding their elements. These tests pin the mechanical
// parts of that contract:
//
//   1. every literal id the script looks up still exists in the markup
//      (the one concatenated family is an explicit, documented allow-list);
//   2. the required class/attribute contracts keep their counts;
//   3. #freqReadout still builds 9 .fd[data-i=0..8] + 2 .fdot + .funit;
//   4. the S-meter JS constants still match the SVG viewBox and the ids the
//      needle code mutates;
//   5. no CSS rule targeting a canvas applies transform/filter/zoom, which
//      would break the scan-overlay offset math and blur the pixel mapping;
//   6. the 16 ids attached at boot without a null guard still exist (those
//      throw and kill the page when lost).
//
// Nothing here inspects pixels. Restyle freely; if one of these fails, the
// failure message names the exact id/selector that broke.
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
// Source readers. The page is styles -> markup -> one inline <script>.
// ---------------------------------------------------------------------------
function readPage() {
  const html = fs.readFileSync(PAGE, 'utf8');
  const styleStart = html.indexOf('<style>');
  const styleEnd = html.indexOf('</style>');
  const scriptStart = html.indexOf('<script', styleEnd);
  assert.ok(styleStart !== -1 && styleEnd !== -1 && scriptStart !== -1,
    'index.html must keep its <style> block and one inline <script> block');
  const scriptMatch = html.slice(scriptStart).match(/<script[^>]*>([\s\S]*?)<\/script>/);
  assert.ok(scriptMatch, 'index.html must keep an inline <script> with a body');
  return {
    html,
    css: html.slice(styleStart + '<style>'.length, styleEnd),
    markup: html.slice(styleEnd + '</style>'.length, scriptStart),
    script: scriptMatch[1],
  };
}

// Quote-aware tag scanner: attribute values may contain '>' (titles, arrows),
// so a naive /<[^>]*>/ truncates tags and loses attributes.
function scanTags(markup) {
  const tags = [];
  let i = 0;
  while (i < markup.length) {
    const lt = markup.indexOf('<', i);
    if (lt === -1) break;
    const next = markup[lt + 1] || '';
    if (!/[a-zA-Z/]/.test(next)) { i = lt + 1; continue; } // <!-- -->, <?
    if (next === '/' && !/[a-zA-Z]/.test(markup[lt + 2] || '')) { i = lt + 1; continue; }
    let j = lt + 1;
    let quote = null;
    while (j < markup.length) {
      const c = markup[j];
      if (quote) { if (c === quote) quote = null; }
      else if (c === '"' || c === "'") quote = c;
      else if (c === '>') break;
      j++;
    }
    const raw = markup.slice(lt, j + 1);
    const name = (raw.match(/^<\/?([a-zA-Z0-9:-]+)/) || [])[1] || '';
    tags.push({ raw, index: lt, name, attrs: parseAttrs(raw) });
    i = j + 1;
  }
  return tags;
}

function parseAttrs(raw) {
  const attrs = {};
  if (raw.startsWith('</')) return attrs;
  const body = raw.replace(/^<[^\s/>]+/, '').replace(/\/?>$/, '');
  const re = /([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*(?:=\s*("([^"]*)"|'([^']*)'|([^\s"'>]+)))?/g;
  let m;
  while ((m = re.exec(body))) {
    const value = m[3] !== undefined ? m[3]
      : m[4] !== undefined ? m[4]
        : m[5] !== undefined ? m[5] : '';
    attrs[m[1].toLowerCase()] = value;
  }
  return attrs;
}

function hasClass(tag, cls) {
  return (tag.attrs.class || '').split(/\s+/).includes(cls);
}

function markupIds(tags) {
  const ids = new Set();
  for (const t of tags) if (t.attrs.id) ids.add(t.attrs.id);
  return ids;
}

// Literal ids the inline script looks up, plus the literal *prefixes* it
// concatenates into ids.
function collectScriptIdRefs(script) {
  const literals = new Set();
  const dynamicPrefixes = new Set();
  for (const re of [
    /\$\(\s*(['"])([A-Za-z0-9_-]+)\1\s*\)/g,
    /getElementById\(\s*(['"])([A-Za-z0-9_-]+)\1\s*\)/g,
  ]) {
    for (const m of script.matchAll(re)) literals.add(m[2]);
  }
  for (const re of [
    /\$\(\s*(['"])([A-Za-z0-9_-]+)\1\s*\+/g,
    /getElementById\(\s*(['"])([A-Za-z0-9_-]+)\1\s*\+/g,
  ]) {
    for (const m of script.matchAll(re)) dynamicPrefixes.add(m[2]);
  }
  return { literals, dynamicPrefixes };
}

// The script builds scan-mode checkbox ids by concatenation:
//   $('scanMode' + m[0].toUpperCase() + m.slice(1))
// so the literal fragment `scanMode` is a prefix, not an id. Keeping the
// exception as an explicit allow-list (instead of silently skipping dynamic
// lookups) means any *new* concatenated id family fails this test until
// someone documents it here.
const DYNAMIC_ID_PREFIX_ALLOWLIST = ['scanMode'];
const SCAN_MODE_IDS = ['scanModeAm', 'scanModeNfm', 'scanModeP25', 'scanModeDmr'];

// The ids attached at boot with no null guard: `$('x').addEventListener(...)`.
function collectUnguardedAttachments(script) {
  const ids = new Set();
  for (const m of script.matchAll(/\$\(\s*(['"])([A-Za-z0-9_-]+)\1\s*\)\.addEventListener/g)) {
    ids.add(m[2]);
  }
  return ids;
}

// The 16 ids whose loss throws at boot and kills the page (plan §2.2).
const UNGUARDED_BOOT_IDS = [
  'sdrAgc', 'sdrGainRange', 'sdrRadioSel', 'sdrRateSel', 'sdrMinDb', 'sdrMaxDb',
  'volRange', 'keyPeak', 'keyAuto', 'keyFill', 'keyAvg', 'keyCenter', 'keySpur',
  'snapSel', 'ppmInput', 'scanThreshold',
];

// ---------------------------------------------------------------------------
// Stub DOM: enough for the real inline script's boot block to run (same shape
// as ui-behavior.test.js; duplicated so the two files stay independent).
// ---------------------------------------------------------------------------
function makeEl(id) {
  const classes = new Set();
  const el = {
    id,
    tagName: 'DIV',
    textContent: '',
    innerHTML: '',
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
  return {
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
}

function buildContext() {
  const byId = new Map();
  const document = {
    getElementById(id) {
      if (!byId.has(id)) byId.set(id, makeEl(id));
      return byId.get(id);
    },
    querySelectorAll() { return []; },
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
    setInterval: () => 0, clearInterval() {}, setTimeout: () => 0, clearTimeout() {},
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

  const ctx = vm.createContext(sandbox);
  const { script } = readPage();
  vm.runInContext(script, ctx, { filename: 'index.html inline script' });
  return ctx;
}

function run(ctx, code) {
  return vm.runInContext(code, ctx);
}

// Split generated span markup into {class, data-i} records, so attribute
// order inside the tag does not matter.
function parseSpans(html) {
  const spans = [];
  const re = /<span\b[^>]*>/g;
  let m;
  while ((m = re.exec(html))) {
    const attrs = parseAttrs(m[0]);
    spans.push({ cls: (attrs.class || '').split(/\s+/).filter(Boolean), dataI: attrs['data-i'] });
  }
  return spans;
}

// ---------------------------------------------------------------------------
// 1. Every literal id the script looks up exists in the markup.
// ---------------------------------------------------------------------------
test('every literal id the inline script looks up exists in the markup', () => {
  const { markup, script } = readPage();
  const ids = markupIds(scanTags(markup));
  const { literals, dynamicPrefixes } = collectScriptIdRefs(script);

  assert.ok(literals.size > 100, `id extraction looks wrong (found ${literals.size}); the script should reference well over 100 ids`);

  // A concatenated family must be a documented exception, not a silent skip.
  assert.deepStrictEqual([...dynamicPrefixes].sort(), [...DYNAMIC_ID_PREFIX_ALLOWLIST].sort(),
    'the script concatenates ids that are not in DYNAMIC_ID_PREFIX_ALLOWLIST — document the new family there and assert its concrete ids');

  const missing = [...literals].filter(id => !ids.has(id)).sort();
  assert.deepStrictEqual(missing, [],
    `ids referenced by the inline script but missing from the markup (a restyle must rename neither):\n  ${missing.join('\n  ')}`);

  // The concatenated family's concrete ids are the real contract.
  for (const id of SCAN_MODE_IDS) {
    assert.ok(ids.has(id), `#${id} must exist in the markup (built by $('scanMode' + …))`);
  }
});

// ---------------------------------------------------------------------------
// 2. Class / attribute contracts survive restyling.
// ---------------------------------------------------------------------------
test('key and panel class contracts keep their counts and data attributes', () => {
  const { markup } = readPage();
  const tags = scanTags(markup);

  const count = cls => tags.filter(t => hasClass(t, cls)).length;
  const withAttrs = (cls, attrs) => tags.filter(t => hasClass(t, cls) && attrs.every(a => t.attrs[a] !== undefined));

  assert.strictEqual(withAttrs('modebtn', ['data-mode']).length, 11,
    '.modebtn[data-mode] buttons (click → setSdrMode(dataset.mode)) must stay 11');
  assert.strictEqual(withAttrs('band-btn', ['data-freq', 'data-mode']).length, 8,
    '.band-btn[data-freq][data-mode] presets (click → tune + mode) must stay 8');
  assert.strictEqual(count('side-tab'), 5, '.side-tab dock tabs must stay 5');
  assert.ok(count('dsp-key') >= 7, `.dsp-key buttons must stay at least 7 (found ${count('dsp-key')})`);
  assert.strictEqual(count('fader-well'), 4, '.fader-well tracks must stay 4');
  assert.strictEqual(count('deck-bay'), 4, '.deck-bay panels must stay 4');
  const protoChips = withAttrs('proto-chip', ['data-proto']);
  assert.ok(protoChips.length >= 1, '.proto-chip[data-proto] decode filters must exist');
  assert.strictEqual(protoChips.length, count('proto-chip'),
    'every .proto-chip must keep its data-proto attribute (decode filter)');
});

test('required singleton contract elements exist in the markup', () => {
  const { markup } = readPage();
  const ids = markupIds(scanTags(markup));
  for (const id of ['freqReadout', 'sdrCanvasWrap', 'smeterSvg']) {
    assert.ok(ids.has(id), `#${id} must exist in the markup`);
  }
});

test('#sdrCanvasWrap keeps spectrum → waterfall → scan-overlay order', () => {
  const { markup } = readPage();
  const tags = scanTags(markup);
  const wrapIdx = tags.findIndex(t => t.attrs.id === 'sdrCanvasWrap');
  assert.ok(wrapIdx !== -1, '#sdrCanvasWrap must exist (it is the positioned parent of the canvases)');

  const open = tags[wrapIdx];
  let depth = 0;
  let innerStart = -1;
  let innerEnd = -1;
  for (let k = wrapIdx; k < tags.length; k++) {
    const t = tags[k];
    if (t.name.toLowerCase() !== open.name.toLowerCase()) continue;
    if (t.raw.startsWith('</')) {
      depth--;
      if (depth === 0) { innerEnd = t.index; break; }
    } else if (!t.raw.endsWith('/>')) {
      if (depth === 0) innerStart = t.index + t.raw.length;
      depth++;
    }
  }
  assert.ok(innerStart !== -1 && innerEnd !== -1, 'could not delimit #sdrCanvasWrap in the markup');
  const inner = markup.slice(innerStart, innerEnd);

  const order = ['sdrSpectrumCanvas', 'sdrWaterfallCanvas', 'sdrScanOverlayCanvas'];
  const positions = order.map(id => {
    const m = inner.match(new RegExp(`id\\s*=\\s*["']${id}["']`));
    assert.ok(m, `#${id} must be a direct child of #sdrCanvasWrap — the scan overlay is positioned from the waterfall's offsetTop/Left/Width/Height`);
    return m.index;
  });
  assert.ok(positions[0] < positions[1] && positions[1] < positions[2],
    `canvas order inside #sdrCanvasWrap must stay spectrum → waterfall → scan overlay (found ${order.join(' → ')} in positions ${positions.join(', ')})`);
});

// ---------------------------------------------------------------------------
// 3. The frequency readout is a hard contract (plan §2.4).
// ---------------------------------------------------------------------------
test('the real script builds the 9-digit frequency readout at boot', () => {
  const ctx = buildContext();
  const innerHTML = run(ctx, `document.getElementById('freqReadout').innerHTML`);
  assert.ok(typeof innerHTML === 'string' && innerHTML.length > 0,
    '#freqReadout must be populated by setupFreqReadout() during boot');

  const spans = parseSpans(innerHTML);
  const digits = spans.filter(s => s.cls.includes('fd'));
  const dots = spans.filter(s => s.cls.includes('fdot'));
  const units = spans.filter(s => s.cls.includes('funit'));

  assert.strictEqual(digits.length, 9,
    `#freqReadout must hold exactly 9 .fd digits (found ${digits.length}); renderFreq()/openFreqEntry() rely on that shape`);
  assert.deepStrictEqual(digits.map(d => d.dataI), ['0', '1', '2', '3', '4', '5', '6', '7', '8'],
    'the .fd spans must carry data-i 0..8 in DOM order (data-i is the decade index)');
  assert.strictEqual(dots.length, 2, `#freqReadout must hold exactly 2 .fdot separators (found ${dots.length})`);
  assert.strictEqual(units.length, 1, `#freqReadout must hold exactly 1 .funit (found ${units.length})`);

  const freqDigits = Array.from(run(ctx, `FREQ_DIGITS`));
  assert.strictEqual(freqDigits.length, 9, 'FREQ_DIGITS must have 9 decades');
  assert.strictEqual(freqDigits[0], 1e9, 'FREQ_DIGITS must start at 1 GHz (1e9)');
  assert.strictEqual(freqDigits[8], 1e1, 'FREQ_DIGITS must end at 10 Hz (1e1)');
  freqDigits.forEach((hz, i) => {
    assert.strictEqual(hz, 1e9 / Math.pow(10, i), `FREQ_DIGITS[${i}] must be the ${i}th decade (${1e9 / Math.pow(10, i)} Hz)`);
  });
});

// ---------------------------------------------------------------------------
// 4. S-meter geometry: JS constants and SVG must stay in sync (plan §2.6).
// ---------------------------------------------------------------------------
test('S-meter geometry constants match the SVG viewBox and the mutated ids exist', () => {
  const { markup, script } = readPage();
  const tags = scanTags(markup);
  const svg = tags.find(t => t.attrs.id === 'smeterSvg');
  assert.ok(svg, '#smeterSvg must exist in the markup');
  assert.strictEqual(svg.attrs.viewbox, '0 0 340 165',
    'a meter redraw must change the SVG viewBox and the JS constants in the same commit');

  const constValue = name => {
    const m = script.match(new RegExp(`const\\s+${name}\\s*=\\s*(-?[0-9.]+)`));
    assert.ok(m, `the inline script must still define const ${name}`);
    return Number(m[1]);
  };
  assert.strictEqual(constValue('METER_PIVOT_X'), 170, 'METER_PIVOT_X must match the dial pivot x');
  assert.strictEqual(constValue('METER_PIVOT_Y'), 205, 'METER_PIVOT_Y must match the dial pivot y');
  assert.strictEqual(constValue('METER_ANGLE_MIN'), -40, 'METER_ANGLE_MIN must match the printed arc');
  assert.strictEqual(constValue('METER_ANGLE_MAX'), 40, 'METER_ANGLE_MAX must match the printed arc');

  const ids = markupIds(tags);
  for (const id of ['meterNeedleGroup', 'meterPeakNeedle', 'meterAgcNeedle', 'meterMask', 'meterPeak', 'meterNum', 'meterClipLed']) {
    assert.ok(ids.has(id), `#${id} is mutated by the meter code and must stay in the markup`);
  }
});

// ---------------------------------------------------------------------------
// 5. Canvas geometry: no transform/filter/zoom on any canvas (plan §2.5).
// ---------------------------------------------------------------------------
test('no CSS rule targeting a canvas applies transform/filter/zoom', () => {
  const { css, markup } = readPage();
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const offenders = [];
  for (const m of stripped.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selector = m[1].trim();
    if (!/canvas\b/i.test(selector)) continue;
    const prop = m[2].match(/(?:^|;)\s*(transform|filter|zoom)\s*:/i);
    if (prop) offenders.push(`${selector} { ${prop[1]}: … }`);
  }
  assert.deepStrictEqual(offenders, [],
    `#sdrScanOverlayCanvas is positioned from the waterfall's offsetTop/Left/Width/Height, so transform/filter/zoom on a canvas breaks the overlay alignment and blurs the pixel mapping:\n  ${offenders.join('\n  ')}`);

  // Inline styles on the canvases are the same hazard.
  const inlineOffenders = scanTags(markup)
    .filter(t => t.name.toLowerCase() === 'canvas' && t.attrs.style)
    .filter(t => /(?:^|;)\s*(transform|filter|zoom)\s*:/i.test(t.attrs.style))
    .map(t => `${t.attrs.id || t.raw.slice(0, 40)} style="${t.attrs.style}"`);
  assert.deepStrictEqual(inlineOffenders, [],
    `canvases must not carry inline transform/filter/zoom:\n  ${inlineOffenders.join('\n  ')}`);
});

// ---------------------------------------------------------------------------
// 6. The 16 unguarded boot listeners still have their elements (plan §2.2).
// ---------------------------------------------------------------------------
test('the 16 unguarded boot-listener ids still exist', () => {
  const { markup, script } = readPage();
  const ids = markupIds(scanTags(markup));

  const missing = UNGUARDED_BOOT_IDS.filter(id => !ids.has(id));
  assert.deepStrictEqual(missing, [],
    `these ids are attached at boot with $('id').addEventListener and will throw, killing the page, if removed:\n  ${missing.join('\n  ')}`);

  // Keep the list honest: any new unguarded attachment must be documented.
  const attached = [...collectUnguardedAttachments(script)].sort();
  assert.deepStrictEqual(attached, [...UNGUARDED_BOOT_IDS].sort(),
    'the set of ids attached without a null guard changed — update UNGUARDED_BOOT_IDS (and treat a new one as a page-killing requirement)');
});

// ---------------------------------------------------------------------------
// 7. Ids the script wires by *argument*, not by a literal $('…') lookup.
//
// The drag helpers and the dock tab table receive ids as strings, so test 1's
// $('x')/getElementById('x') extraction never sees them: a restyle that drops
// one of these elements would kill the control silently. Each entry is here
// because it is the argument of a call that binds behavior to that element.
// ---------------------------------------------------------------------------
const ARGUMENT_WIRED_IDS = [
  // makeFaderDraggable(trackId, …) — the drag surface for each linear fader.
  'faderRfGainTrack', 'faderSqlTrack', 'faderSpanTrack', 'faderZoomTrack',
  // makeKnobDraggable(wrapId, …) / setKnobPointer(id, …) — the AF knob.
  'deckAfVolWrap', 'deckAfVolPointer', 'deckAfVolKnurls',
  // setAgcArc(id, …) — the illuminated AGC arc inside the S-meter.
  'meterAgcArc',
  // Dock tab ↔ panel table: [tabId, panelId] pairs wired in a loop.
  'tabClassifier', 'panelClassifier', 'tabDecode', 'panelDecode',
  'tabCalls', 'panelCalls', 'tabScan', 'panelScan',
];

test('ids wired by argument (drag helpers, knob pointers, dock tabs) still exist', () => {
  const { markup, script } = readPage();
  const ids = markupIds(scanTags(markup));

  const missing = ARGUMENT_WIRED_IDS.filter(id => !ids.has(id));
  assert.deepStrictEqual(missing, [],
    `these ids are passed as arguments to wiring calls, so removing the element breaks the control without any boot error:\n  ${missing.join('\n  ')}`);

  // Keep the list honest both ways: every id actually passed to those helpers
  // must be covered, so a new wired control cannot slip past this test.
  const passed = new Set();
  for (const re of [
    /makeFaderDraggable\(\s*(['"])([A-Za-z0-9_-]+)\1/g,
    /makeKnobDraggable\(\s*(['"])([A-Za-z0-9_-]+)\1/g,
    /setKnobPointer\(\s*(['"])([A-Za-z0-9_-]+)\1/g,
    /setAgcArc\(\s*(['"])([A-Za-z0-9_-]+)\1/g,
  ]) {
    for (const m of script.matchAll(re)) passed.add(m[2]);
  }
  for (const m of script.matchAll(/\[\s*'(tab[A-Za-z0-9_]+)'\s*,\s*'(panel[A-Za-z0-9_]+)'\s*\]/g)) {
    passed.add(m[1]);
    passed.add(m[2]);
  }
  const undocumented = [...passed].filter(id => !ARGUMENT_WIRED_IDS.includes(id)).sort();
  assert.deepStrictEqual(undocumented, [],
    'a newly wired element id is not covered by ARGUMENT_WIRED_IDS — add it so a restyle cannot remove it silently');
});
