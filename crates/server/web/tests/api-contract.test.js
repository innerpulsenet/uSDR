// Client ↔ server contract tests for uSDR.
//
// The front panel is one embedded page (include_str! in main.rs:33) whose
// inline script talks to the Rust server over REST, WebSocket JSON and
// WebSocket binary frames. A visual pass may restyle the page, but the wire
// contract must not drift. These tests cross-check the client source against
// the server source — no build, no cargo, no browser:
//
//   1. every /api/... path the client calls is served by a .route(...) in
//      crates/server/src/main.rs (exact, dynamic-segment, or the documented
//      dynamic /api/sdr/ prefix used for the voice scan);
//   2. the dynamic scan sub-paths (scan/config, scan/control) resolve to real
//      /api/sdr/scan/* routes;
//   3. every s.<field> read inside updateSdrStatus() exists as a pub field of
//      struct SdrStatus in crates/server/src/sdr.rs (a restyle that renames a
//      status field breaks silently at runtime);
//   4. the WebSocket message handler still branches on the binary kind bytes
//      0x01 (audio PCM), 0x02 and 0x03 (FFT v2 bare / v3 classified), so no
//      frame type can be dropped without a test failing.
//
// Run: node --test crates/server/web/tests/*.test.js
'use strict';
const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const PAGE = path.join(__dirname, '..', 'index.html');
const MAIN_RS = path.join(__dirname, '..', '..', 'src', 'main.rs');
const SDR_RS = path.join(__dirname, '..', '..', 'src', 'sdr.rs');

// The client endpoints this page is documented to use (plan §2.7). Asserting
// them keeps the extraction below from passing vacuously if the script is ever
// restructured. Dynamic segments are normalised to {id}.
const REQUIRED_CLIENT_PATHS = [
  '/api/devices',
  '/api/sdr/tune', '/api/sdr/inspect', '/api/sdr/device', '/api/sdr/gain',
  '/api/sdr/rate', '/api/sdr/mode', '/api/sdr/frontend', '/api/sdr/spurs',
  '/api/sdr/ppm', '/api/sdr/zoom', '/api/sdr/avg', '/api/sdr/calibrate',
  '/api/sdr/status', '/api/sdr/calls',
  '/api/sdr/calls/{id}/audio.wav', '/api/sdr/capture.wav', '/api/sdr/replay',
];

function readSources() {
  const html = fs.readFileSync(PAGE, 'utf8');
  const scriptMatch = html.match(/<script[^>]*>([\s\S]*?)<\/script>/);
  assert.ok(scriptMatch, 'index.html must keep its inline <script>');
  return {
    script: scriptMatch[1],
    mainRs: fs.readFileSync(MAIN_RS, 'utf8'),
    sdrRs: fs.readFileSync(SDR_RS, 'utf8'),
  };
}

// Brace matcher that ignores braces inside strings, template literals and
// comments. The bodies it delimits here contain no regex literal with braces.
function blockAfter(src, openBraceIndex) {
  let depth = 0;
  let quote = null;
  let lineComment = false;
  let blockComment = false;
  for (let i = openBraceIndex; i < src.length; i++) {
    const c = src[i];
    const n = src[i + 1];
    if (lineComment) { if (c === '\n') lineComment = false; continue; }
    if (blockComment) { if (c === '*' && n === '/') { blockComment = false; i++; } continue; }
    if (quote) {
      if (c === '\\') { i++; continue; }
      if (c === quote) quote = null;
      continue;
    }
    if (c === '/' && n === '/') { lineComment = true; i++; continue; }
    if (c === '/' && n === '*') { blockComment = true; i++; continue; }
    if (c === "'" || c === '"' || c === '`') { quote = c; continue; }
    if (c === '{') depth++;
    else if (c === '}') { depth--; if (depth === 0) return src.slice(openBraceIndex, i + 1); }
  }
  throw new Error('unbalanced braces while extracting a function body');
}

function functionBody(src, signature) {
  const at = src.indexOf(signature);
  assert.notStrictEqual(at, -1, `the inline script must still define ${signature.trim()}`);
  const brace = src.indexOf('{', at);
  assert.notStrictEqual(brace, -1, `${signature.trim()} must have a body`);
  return blockAfter(src, brace);
}

// Literal /api/... paths used by the inline script. Query strings are dropped;
// template expressions become {…} so they can be matched against routes.
function collectClientApiPaths(script) {
  const all = new Set();
  for (const m of script.matchAll(/\/api\/[A-Za-z0-9_\-./${}]*/g)) all.add(m[0]);
  const prefixMarkers = new Set([...all].filter(p => p.endsWith('/')));
  const literals = [...all]
    .filter(p => !p.endsWith('/'))
    .map(p => p.replace(/\$\{[^}]*\}/g, '{id}'));
  return { literals: new Set(literals), prefixMarkers, raw: all };
}

function collectRoutes(mainRs) {
  const routes = new Set();
  for (const m of mainRs.matchAll(/\.route\(\s*"([^"]+)"/g)) routes.add(m[1]);
  return routes;
}

// Segment-wise match: a route segment {param} (or a normalised client {id})
// matches any single segment.
function routeCovers(clientPath, routePath) {
  const a = clientPath.split('/');
  const b = routePath.split('/');
  if (a.length !== b.length) return false;
  return a.every((seg, i) => seg === b[i] || /^\{[^}]+\}$/.test(seg) || /^\{[^}]+\}$/.test(b[i]));
}

// ---------------------------------------------------------------------------
// 1. Every client REST path has a server route.
// ---------------------------------------------------------------------------
test('every /api path used by the client is served by a main.rs route', () => {
  const { script, mainRs } = readSources();
  const routes = collectRoutes(mainRs);
  assert.ok(routes.size >= 20, `route extraction looks wrong (found ${routes.size} .route(...) entries)`);

  const { literals, prefixMarkers } = collectClientApiPaths(script);
  const clientPaths = [...literals].sort();
  assert.ok(clientPaths.length >= 15, `client API extraction looks wrong (found ${clientPaths.length} paths)`);
  for (const required of REQUIRED_CLIENT_PATHS) {
    assert.ok(clientPaths.includes(required),
      `the client no longer calls ${required} — if that is intentional, update REQUIRED_CLIENT_PATHS in this test`);
  }

  const uncovered = clientPaths.filter(p => ![...routes].some(r => routeCovers(p, r)));
  assert.deepStrictEqual(uncovered, [],
    'client API paths with no server route in crates/server/src/main.rs:\n'
    + `  uncovered: ${uncovered.join(', ') || '(none)'}\n`
    + `  client paths: ${clientPaths.join(', ')}\n`
    + `  server routes: ${[...routes].sort().join(', ')}`);

  // The dynamic-prefix form used by the scan must be declared explicitly.
  assert.ok([...prefixMarkers].includes('/api/sdr/'),
    `the client must still use the documented dynamic prefix '/api/sdr/' (found ${[...prefixMarkers].join(', ') || 'none'})`);
});

// ---------------------------------------------------------------------------
// 2. Dynamic scan endpoints resolve to real routes.
// ---------------------------------------------------------------------------
test('dynamic /api/sdr/ scan calls map to real scan routes', () => {
  const { script, mainRs } = readSources();
  const routes = collectRoutes(mainRs);

  const usesDynamicPrefix = /sdrPost\(\s*['"]\/api\/sdr\/['"]\s*\+/.test(script);
  assert.ok(usesDynamicPrefix,
    "the voice scan must still post through sdrPost('/api/sdr/' + path, …) — the prefix is part of the contract");

  const subPaths = [...new Set([...script.matchAll(/postScan\(\s*(['"])([^'"]+)\1/g)].map(m => m[2]))].sort();
  assert.ok(subPaths.length > 0, 'could not find any postScan(subPath, …) call sites');
  for (const sub of ['scan/config', 'scan/control']) {
    assert.ok(subPaths.includes(sub),
      `the scan must still call '${sub}' through the /api/sdr/ prefix (found: ${subPaths.join(', ')})`);
    assert.ok(routes.has('/api/sdr/' + sub),
      `crates/server/src/main.rs must still route /api/sdr/${sub} (routes: ${[...routes].sort().join(', ')})`);
  }
  const unmapped = subPaths.filter(sub => !routes.has('/api/sdr/' + sub));
  assert.deepStrictEqual(unmapped, [],
    `dynamic scan sub-paths with no /api/sdr/<sub> route:\n  ${unmapped.join('\n  ')}`);
});

// ---------------------------------------------------------------------------
// 3. updateSdrStatus only reads fields SdrStatus actually has.
// ---------------------------------------------------------------------------
test('updateSdrStatus reads only fields declared on struct SdrStatus', () => {
  const { script, sdrRs } = readSources();
  const body = functionBody(script, 'function updateSdrStatus(s)');

  const reads = new Set();
  for (const m of body.matchAll(/\bs\.([A-Za-z_$][A-Za-z0-9_$]*)/g)) reads.add(m[1]);
  assert.ok(reads.size >= 20, `status-field extraction looks wrong (found ${reads.size} s.<field> reads)`);

  const structAt = sdrRs.indexOf('pub struct SdrStatus {');
  assert.notStrictEqual(structAt, -1, 'crates/server/src/sdr.rs must still define pub struct SdrStatus');
  const structBody = blockAfter(sdrRs, sdrRs.indexOf('{', structAt));
  const fields = new Set([...structBody.matchAll(/pub\s+([a-z_][a-z0-9_]*)\s*:/g)].map(m => m[1]));
  assert.ok(fields.size >= 20, `SdrStatus field extraction looks wrong (found ${fields.size} pub fields)`);

  const unknown = [...reads].filter(f => !fields.has(f)).sort();
  assert.deepStrictEqual(unknown, [],
    'fields read off the status payload in updateSdrStatus() but not declared on SdrStatus:\n'
    + `  unknown: ${unknown.join(', ') || '(none)'}\n`
    + `  SdrStatus fields: ${[...fields].sort().join(', ')}`);
});

// ---------------------------------------------------------------------------
// 4. Wire format: the binary kind bytes stay wired.
// ---------------------------------------------------------------------------
test('the WebSocket handler still branches on binary kinds 0x01 / 0x02 / 0x03', () => {
  const { script } = readSources();
  const at = script.indexOf('ws.onmessage');
  assert.notStrictEqual(at, -1, 'the inline script must still install ws.onmessage');
  const handler = blockAfter(script, script.indexOf('{', at));

  assert.ok(/new\s+DataView\s*\(\s*ev\.data\s*\)/.test(handler),
    'the binary path must still read the frame through new DataView(ev.data)');
  assert.ok(/getUint8\s*\(\s*0\s*\)/.test(handler),
    'the kind byte must still be read with view.getUint8(0)');

  const kinds = new Set();
  for (const m of handler.matchAll(/\bkind\b[^;\n]*?\b0x0?([123])\b/g)) kinds.add(Number(m[1]));
  for (const kind of [1, 2, 3]) {
    assert.ok(kinds.has(kind),
      `the WebSocket handler must still branch on binary kind 0x0${kind} (found kinds: ${[...kinds].map(k => '0x0' + k).join(', ') || 'none'})`);
  }
});
