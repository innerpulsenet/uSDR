# panel-qa — front-panel QA harness

The web client is a single file compiled into the server binary
(`crates/server/src/main.rs:33` — `include_str!("../web/index.html")`), so it
cannot be served without the Rust binary. This harness serves a copy over local
HTTP with a canned API stub, drives headless Chrome over CDP, and records the
panel's geometry, contrast and screenshots at several viewports.

It never touches the Rust server, the dongle, or the test suite.

## Usage

```sh
# measure + screenshot at the default viewports
node tools/panel-qa/panel-qa.mjs

# also capture 2x element crops (header, bays, meter, VFO, key rows)
node tools/panel-qa/panel-qa.mjs --crops

# compare a change against a stored baseline
node tools/panel-qa/panel-qa.mjs --out .hermes/qa/after
diff <(jq '.[] | {viewport, overflowX, hero, heroDigit, keySystems}' .hermes/qa/before/metrics.json) \
     <(jq '.[] | {viewport, overflowX, hero, heroDigit, keySystems}' .hermes/qa/after/metrics.json)

# other options
node tools/panel-qa/panel-qa.mjs --viewports 1600x900,1024x800
node tools/panel-qa/panel-qa.mjs --no-shots --json
```

Output defaults to `.hermes/qa/<timestamp>/`: one PNG per viewport (plus crops
with `--crops`) and `metrics.json`.

## Live smoke test

`live-smoke.mjs` is the other half: it starts the real `usdr` binary on a
private port with a scratch config, loads the real page, and drives the
interactions through the actual REST/WebSocket path — WebSocket connect,
frequency readout, canvas painting, band preset, mode key, palette popup,
LISTEN, and a digit scroll.

```sh
cargo build --release          # the page is embedded by include_str!
node tools/panel-qa/live-smoke.mjs
```

It needs an RTL-SDR the process can open (`/dev/bus/usb/…`). Without one the
server still serves the page and the API, and the RF-dependent checks report
SKIP instead of FAIL, so the harness is still useful in a sandbox or CI.
It never touches `~/.config/usdr/usdr.toml` and kills its own server.

## What it checks

| Field | Why it matters |
|---|---|
| `overflowX` | horizontal scroll is a layout bug (77 px at 1024 px as of 2026-09-09) |
| `rig`, `brand`, `deck`, `bays` | the faceplate's vertical budget and bay proportions |
| `hero`, `heroDigit` | the frequency readout must dominate the display well, and `.fd` must keep a fixed width or live digits jitter |
| `keySystems` | mode / band / DSP keys should converge on one cap geometry |
| `lowContrast` | engraved legends below WCAG AA (4.5:1) against their own surface |
| `smallTextUnder9px` | legibility floor for panel legends |

## Environment gotchas (all verified)

- Chrome refuses to start headless without an explicit `--user-data-dir`.
- The page must be served over HTTP. A `file://` load or a dead port produces
  `chrome-error://chromewebdata/` and a blank screenshot.
- `--window-size` is ~143 px taller than the resulting viewport, so the harness
  uses `Emulation.setDeviceMetricsOverride` instead.
- Node 22 has a global `WebSocket`, so CDP needs no npm dependency.
- The WebSocket never connects (the stub has no `/ws` upgrade), so the header
  shows "reconnecting"; that is expected and does not affect layout.
