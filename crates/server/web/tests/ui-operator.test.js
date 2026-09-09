// Operator-contract tests — "one canonical visible control" grammar.
//
// Phase 0 of .hermes/plans/2026-09-09_111535-front-panel-operator-polish.md.
// These pin the interaction bugs verified in source at HEAD; they are EXPECTED
// TO FAIL until the Phase 2 fixes land. Do not weaken them to green.
//
//   1. spectrum dblclick tunes RX (tuneRxHz), not span-only tuneSdrHz
//      (index.html ~5735-5739: dblclick calls tuneSdrHz, click uses tuneRxHz)
//   2. snapSel default option is auto (matches snapOverrideHz = 0 at ~6624;
//      today `6250` carries `selected` at ~1803)
//   3. runSpurCheck finally writes #keySpurLabel, not key.textContent
//      (~6401 wipes #keySpurLabel + LED span)
//   4/5. LISTEN key keeps a stable label — never renamed to MUTE
//      (startAudio ~6291 writes 'MUTE' via a `lbl` alias for listenBtnLabel)
//
// NOTE on test 1: the plan draft anchors on the first "addEventListener('dblclick'"
// in the script, but that is the freq-entry dblclick (~4225), not the spectrum
// canvas handler. Anchoring on `cv.addEventListener('dblclick'` targets the
// spectrum bug so the failure is for the expected reason, not an incidental one.
//
// Run: node --test crates/server/web/tests/ui-operator.test.js
'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('path');
const PAGE = path.join(__dirname, '..', 'index.html');

function readPage() {
  const html = fs.readFileSync(PAGE, 'utf8');
  const styleEnd = html.indexOf('</style>');
  const scriptStart = html.indexOf('<script', styleEnd);
  const scriptMatch = html.slice(scriptStart).match(/<script[^>]*>([\s\S]*?)<\/script>/);
  return {
    html,
    markup: html.slice(styleEnd + '</style>'.length, scriptStart),
    script: scriptMatch[1],
  };
}

test('spectrum dblclick tunes RX (tuneRxHz), not span-only tuneSdrHz', () => {
  const { script } = readPage();
  const at = script.indexOf("cv.addEventListener('dblclick'");
  assert.ok(at !== -1, 'spectrum canvas dblclick handler must exist');
  const first = script.slice(at, at + 400);
  assert.match(first, /tuneRxHz\(/);
  assert.doesNotMatch(first, /tuneSdrHz\(/);
});

test('snapSel default option is auto (matches snapOverrideHz = 0)', () => {
  const { markup } = readPage();
  assert.match(markup, /id="snapSel"/);
  assert.match(markup, /<option value="auto"[^>]*id="snapOptAuto"[^>]*selected/);
  assert.doesNotMatch(markup, /value="6250" selected/);
});

test('runSpurCheck finally writes #keySpurLabel, not key.textContent', () => {
  const { script } = readPage();
  const fn = script.slice(script.indexOf('async function runSpurCheck'), script.indexOf('async function applySpurNotch'));
  assert.doesNotMatch(fn, /key\.textContent\s*=/);
  assert.match(fn, /keyLbl\.textContent|keySpurLabel/);
});

test('LISTEN / MUTE key initializes with MUTE default in markup and top cluster', () => {
  const { markup } = readPage();
  assert.match(markup, /id="btnListen"[^>]*title="[^"]*Mute[^"]*"/);
  assert.match(markup, /id="listenBtnLabel">MUTE<\/span>/);
});

test('startAudio/stopAudio toggle between MUTE and MUTED', () => {
  const { script } = readPage();
  const start = script.indexOf('function startAudio');
  const stop = script.indexOf('function stopAudio', start);
  assert.ok(start !== -1 && stop !== -1, 'startAudio/stopAudio must exist');
  const body = script.slice(start, script.indexOf('function onAudioFrame', stop));
  assert.match(body, /listenBtnLabel[\s\S]*?['"]MUTE['"]/);
  assert.match(body, /listenBtnLabel[\s\S]*?['"]MUTED['"]/);
});

test('toggleDockLayout calls resizeSdrCanvases and does not reference undefined resizeAllCanvases', () => {
  const { script } = readPage();
  const at = script.indexOf('function toggleDockLayout()');
  assert.ok(at !== -1, 'toggleDockLayout must exist');
  const body = script.slice(at, script.indexOf('const dlb =', at));
  assert.match(body, /resizeSdrCanvases\(/);
  assert.doesNotMatch(body, /resizeAllCanvases/);
});

test('drawSdrWaterfallRow auto-resizes canvas backing store on layout change', () => {
  const { script } = readPage();
  const at = script.indexOf('function drawSdrWaterfallRow(');
  assert.ok(at !== -1, 'drawSdrWaterfallRow must exist');
  const body = script.slice(at, script.indexOf('function drawWaterfallHud', at));
  assert.match(body, /targetW[\s\S]*?targetH/);
  assert.match(body, /sdrWfCanvas\.width\s*!==\s*targetW/);
  assert.match(body, /resizeSdrCanvases\(\)/);
});

