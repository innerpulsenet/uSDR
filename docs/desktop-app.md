# uSDR desktop: plan

A native client for the uSDR server, in Rust, that does everything the web page does and is
the better place to operate the receiver from. It is a client: the radio, the DSP and every
decoder stay in `usdr` (the server). Nothing in the app needs the RSP1A to be remote or
local; it needs a server address.

## Shape

```text
crates/proto     wire types: the binary FFT and audio frames, JSON events, REST bodies
                 (decoded tolerantly — unknown fields ignored, missing ones defaulted)
crates/desktop   usdr-desktop, an eframe/egui app on wgpu
  net.rs         websocket thread (stream) + REST thread (changes, coalesced per control)
  audio.rs       cpal output; drift-steered Catmull-Rom resampler; local WAV recorder
  spectrum.rs    the panadapter: trace, fill, holds, passband flag, markers, gestures
  waterfall.rs   GPU texture ring; history kept in dB so contrast and palette recolor all of it
  vfo.rs         the digit readout (wheel/click per digit, type-over, unit-aware entry)
  meter.rs       bar S-meter with ballistics, peak hold, floor caret and SNR span
  scope.rs       audio / MPX FFT + oscilloscope, eye diagram and slicer
  panels/        front panel applets, dock tabs, settings, connect screen
```

Why egui: one Rust codebase that builds on Kubuntu today and on Windows 11 (DX12/Vulkan
through wgpu) without a web view; immediate-mode painting suits a 25 fps instrument; and
custom painting (meshes, gradients, textures) gives full control of the look. egui 0.35 is
the newest release the installed toolchain (rustc 1.93) builds.

## Server changes

- **Presence.** Each websocket says what it is (`/ws?client=desktop&name=…`). The server
  keeps the list and broadcasts `{"type":"presence","clients":[…],"web":…}` on every change.
- **Web policy** (`--web`, `GET/POST /api/web`, saved): `shared` — page and app both drive
  the receiver and both follow the same state; `yield` — while an app is connected the page
  is view-only (a veil, and its POSTs refused with 409); `off` — the page is not served.
- **Shared memories** (`GET/POST /api/memories`, saved, broadcast): the memory bank is one
  bank for every client instead of per-browser localStorage.
- **AVG in status**, so every client shows the averaging that is actually on.

Everything else already syncs: tuning, mode, gain, span, zoom and the front end live on the
server and every change comes back to every client as a `status` event. Display preferences
(palette, dB range, holds) stay per client, as each SmartSDR client keeps its own panadapter.

## Parity map (web → app)

| Web | App |
|---|---|
| VFO readout, TS step, tune strip, «‹›» | VFO applet: digit readout (dimmed leading zeros, wheel/click per digit, type-over), step menu with mode raster, jog strip |
| Mode keys (13) | Mode grid, grouped Demod / Decode·Scan, raster per mode |
| Memory bank (8) | Memories applet, server-shared, more than eight |
| RF gain, AGC, IF AGC target, clip guard, tuner/IF AGC, LNA/IF readout | Gain applet with precision faders |
| Front end: notches, LO offset, IQ corr, bias-T, IF filter, spur notch | Front-end applet |
| S-meter, CH PWR/SNR/MARK/ERR/NOTCH | Meter applet + slice flag |
| PEAK, RNG, FILL, AVG, WF PEAK, palette, floor/ceiling | Display bar over the panadapter + right-click menu |
| Span, zoom, center | Display bar; Ctrl-wheel zoom at the cursor |
| Recorder (I/Q + JSON, voice, disc, play 20 s, replay, load I/Q) | Recorder applet, plus a local audio recorder |
| Scope (passband, audio, MPX, eye, slicer, disc) | Scope applet |
| Class log, decode log + inspector, Last Heard, Scan | Bottom dock tabs |
| P25 keys, device, ppm, calibrate | Radio applet and settings |
| Annunciators, stream health, clock | Masthead and status bar |

Known traps in the page that the app does not copy: the hard-coded IF BW labels (the app
shows `bandwidth_hz`), the fixed "LO +125k" label (the app shows `lo_offset_hz`), fixed
waterfall time labels (the app measures the row rate), and centering the zoomed view on
`freq_hz` (the app maps with each frame's own center and rate).

## Borrowed from other receivers

SmartSDR's slice flag (frequency, mode, filter, meter and controls at the carrier),
black-level/gain waterfall contrast with whole-history recolor, the noise floor held at a set
height with hysteresis (AetherSDR), SDR++'s digit readout, cursor-anchored zoom, a network
health readout that opens diagnostics, decode spots as labels on the spectrum, and a
multi-client badge that says who else is connected and who has control.

## Look

The innerpulse ink ground and type (Instrument Sans, JetBrains Mono for every measurement,
Syne for the wordmark alone), with uSDR's firmware & RF accent, amber, for the receiver's
own marks. Ember is kept for the mark, focus and *live* (recording). The connect screen
shows the mark received as six carriers into a waterfall. Motion follows the brand: one
draw-in, a beat only while waiting, and none at all with reduced motion on.

## Status

Merged as of 2026-11-02, with 42 headless UI tests (egui_kittest) driving the real widgets and
checking the exact requests they send, plus server and engine tests for the new routes.

Done since the first build:

- **Parity with the web page:** drag-to-tune strip, SQL MARK fader and meter caret, the slicer
  beside the eye, pager deviation marks, expandable decode rows that hold the reading position,
  native file dialogs, renameable memories.
- **Server features:** adjustable filters (sideband width and shift, AM/NFM/WFM width; digital
  modes keep theirs), an API version with feature list, and a second receiver (NFM, AM, USB,
  LSB, WFM) inside the span with its own audio stream.
- **The app:** receiver B as a second slice (its own flag, meter, audio and applet), span lock,
  passband edges that drag, spectrum styles (Ember, Classic, Glacier, Phosphor), a five-minute
  waterfall replay with UTC times, a band-plan strip, connection diagnostics with sparklines, a
  View menu (compact layout, text size, 60/30 fps).

## What is left

- **Windows 11:** CI that builds and tests Linux and Windows, a Windows build, an installer.
- **A GPU waterfall shader:** measured, it would not lower CPU (the frame wait dominates, and the
  30 fps setting halves that); worth doing only if waterfall recolors ever show up in a profile.
- **The web page** does not show receiver B or the filter controls; it follows both through the
  status (the passband it draws is the filtered one).
