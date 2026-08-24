# uSDR

A single-radio spectrum inspector for RTL-SDR hardware: one dongle, one web
page. It is the SDR mode of [`scannerd`](#provenance) lifted out on its own —
a live FFT and waterfall over the tuned span, plus a narrowband inspect chain
that runs the same digital classifiers and protocol decoders the full scanner
uses.

There is no scanning, no call recording, no database, and no voice or data
mode. This exists to point a receiver at a signal and find out what it is.

## What it does

The interface is laid out like an IC-7300 front panel: status strip, S/Po
meter, a large tuning readout, spectrum over waterfall, and an audio scope,
all on one LCD-style screen.

- **Spectrum and waterfall** over spans from 200 kHz to 3.2 MHz, using the
  `HAM` black → navy → blue → cyan → white waterfall ramp, with peak hold,
  trace fill, and auto or manual dB range.
- **Tuning.** The big readout is the frequency being *received*, the way a
  radio's is — not the centre of the span. Everything sets it: clicking
  anywhere on the spectrum or waterfall, scrolling a digit, the arrow keys,
  double-clicking to type, scrolling the display, or clicking a peak marker or
  a logged decode.

  Tuning **snaps to the channel raster the mode implies**, so pointing at a
  signal lands on the frequency it is actually allocated to:

  | Mode | Raster | Why |
  |---|---|---|
  | `WFM` | 100 kHz | Broadcast FM as the dial is marked — 99.1, 99.2, 99.3 |
  | `NFM` | 6.25 kHz | The narrowbanded land-mobile raster |
  | `P25` / `DMR` | 6.25 kHz | The raster the digital channels sit on |
  | `PACKET` | 2.5 kHz | Divides both the 10 kHz APRS raster — 144.390 lands on it exactly — and 12.5 kHz paging channels |

  The **Step** control above the readout overrides that. `Auto` follows the
  mode's raster and says which it is using; anything else is a fixed step from
  100 Hz to 200 kHz, shown in amber so a manual step is never mistaken for the
  mode's own.

  Hold **Alt** while clicking or scrolling to tune off the raster. The hover
  readout shows where a click would land, not where the cursor is. Typed
  entry and the digit dials are never snapped: those are already exact.

  The two gestures do the two things a radio does with its display:

  - **Turning the dial** — scrolling a digit, the arrow keys, or scrolling the
    spectrum — keeps the tuned frequency in the middle and scrolls the spectrum
    underneath it, the way a VFO knob behaves. The passband marker does not
    move.
  - **Clicking a signal** leaves the span where it is and moves the marker onto
    what was clicked, because the operator is looking at that span and does not
    want it to jump. The span only shifts if the passband would otherwise hang
    off an edge, and then by the least it takes: measured with a 200 kHz WFM
    channel in a 2.4 MHz span, a click at 4% across moved the span 48 kHz.

  Shift-click moves the span without retuning, double-click does both, and
  `CENTER` puts the span back around the receive frequency.

  The waterfall is **panned** rather than wiped when the span moves, so its
  history stays under the frequencies it was recorded at. Tuning 200 kHz in a
  2.4 MHz span slides it 94 pixels — measured against a marker painted into the
  history — and only a span change or a jump bigger than the screen clears it.
- **A packet list for decoded frames.** The decode log reads like a protocol
  analyser: one summary line per frame, and any frame expands to its detail —
  a `Frame` section with the protocol, kind and whether the checksum passed, a
  `Fields` section with every value the decoder recovered, and the raw data.

  Below the list is a **frame bytes pane** that follows the selected frame, the
  way a packet analyser's third pane does. Byte payloads appear as an
  offset/hex/ASCII dump; POCSAG and FLEX codewords appear as *words* rather
  than bytes, because they are 20 and 21 bits wide and packing them into a byte
  dump would misalign every one after the first. The pane says plainly when a
  decoder did not hand back a raw frame, rather than showing an invented one.

  A filter box narrows the list by protocol, kind or text, and the bytes can be
  copied to the clipboard.

- **Last heard.** Finished P25 and DMR calls are kept — the most recent
  sixteen — with their audio, so a call that went past can be played again.
  Each is listed with its identities, colour code or NAC, and duration.

  This exists because a decoded call and an audible one are not the same
  thing. The receivers vocode encrypted voice to silence rather than to noise,
  so a call can be reported in full and still play nothing. The list says which
  it was — `encrypted · ALG 0x84 — muted`, `no audio decoded`, or a play
  button — instead of leaving silence looking like a fault.

- **Zoom** on the spectrum and waterfall, 1× to 32×, centred on the frequency
  being received. This is not the same picture stretched: the receiver narrows
  the displayed window *and* raises the FFT size with it, so the detail is
  real. Measured at 2.4 MS/s, 1× gives 2,344 Hz per bin and 8× gives **36.6 Hz**
  — a 64× improvement — with 32× reaching 9.2 Hz across a 75 kHz window.

- **`AUTO`** runs the classifier, both digital voice receivers and every packet
  and paging decoder on the same channel simultaneously, so a signal is
  identified and decoded without being told what it is. A receiver that has
  locked outranks the classifier's heuristic for the label, and whichever voice
  receiver is carrying a call is what you hear.

  It used to cost about a core and drop blocks at 2.4 MS/s; after the work in
  [Classifier cost](#classifier-cost) it runs at a third of that with none
  dropped. If it ever does fall behind it says so rather than decoding patchily
  — *AUTO cannot keep up at 2.400 MS/s — dropping blocks; try a narrower span*.

- **A layout that holds still.** Every live readout is a fixed size and every
  growing list scrolls inside its own fixed-height box, so nothing on the page
  moves as text updates. The decode and classification logs prepend rows, so
  they also hold your scroll position against the new rows rather than sliding
  the content up under you.
- **Five modes**, switchable from the screen:

  | Mode | What it does |
  |---|---|
  | `NFM` | Narrowband FM with the full digital classifier |
  | `WFM` | 200 kHz broadcast FM, 75 µs de-emphasis, for listening |
  | `P25` | P25 Phase 1 C4FM decoded to voice through the IMBE vocoder |
  | `DMR` | DMR Tier II decoded to voice through the AMBE vocoder |
  | `PACKET` | AX.25 at 1200 baud, POCSAG at 512/1200/2400, and FLEX, all at once |
  | `AUTO` | Every decoder above at once, on whatever is in the passband |

  `P25` and `DMR` extract and equalise their own channel out of the span and
  decode it to audio in real time; call starts and ends, NAC/colour code and
  duration land in the decode log. `PACKET` runs every packet and paging
  decoder over the passband at the same time, so whatever is there decodes
  without being told in advance what it is.

  The passband drawn on the spectrum and the audio scope's frequency axis both
  follow the mode's real channel width.
- **Listening.** Demodulated audio is streamed to the browser as raw PCM over
  the same WebSocket and played through Web Audio. Press LISTEN to start it —
  browsers only allow audio to begin inside a user gesture.
- **Front-end correction** on the raw span before anything measures or
  demodulates it: DC offset removal, LO leakage suppression, and IQ image
  correction. See [Front end](#front-end) for what this is worth measured, and
  for the LO offset option.
- **A scope that changes instrument with the mode.** A 0–4 kHz audio spectrum
  says nothing useful about C4FM, so the panes show whatever the mode is
  actually working with:

  | Mode | Left pane | Right pane |
  |---|---|---|
  | `NFM` / `PACKET` | Audio spectrum 0–4 kHz with its own waterfall, CTCSS and AFSK mark/space marked | Audio waveform, level and time/div |
  | `WFM` | The whole FM multiplex to 64 kHz, with the 19 kHz pilot, stereo subcarrier and 57 kHz RDS marked | Audio waveform |
  | `P25` / `DMR` | Eye diagram, two symbols wide, against the four nominal C4FM levels | Constellation of the recovered symbols, with a level histogram down the edge |

  All of it is real: the server picks the trace per mode — decimated audio,
  the pre-de-emphasis multiplex at 128 kHz, or the channel discriminator at
  10 samples per symbol — and says which it is sending. Nothing is simulated.
- **Digital Signal Classifier** reporting modulation, symbol rate, protocol,
  RF level, SNR, and deviation, refreshed with every FFT frame.
- **Protocol Decode / FEC panel** showing accepted *and* rejected frame
  attempts, so a heuristic label is never presented as a successful decode.
- **Rolling 20-second capture** of voice audio, discriminator output, and
  complex I/Q, downloadable as WAV.
- **Deterministic replay**: push a saved I/Q WAV back through a fresh
  classifier and get the same decode events, with no radio attached.

### Decoders and classifiers included

Everything in `crates/engine` came across intact, so the inspector recognises
P25 (Phase 1 and 2, conventional and trunked control), DMR Tier II/III
including Connect Plus, Capacity Plus and Hytera XPT control telemetry, NXDN
Type-C and Type-D/IDAS, Motorola SmartNet/SmartZone, LTR, Passport, POCSAG
512/1200/2400, FLEX, APRS/AX.25, NOAA SAME, MDC-1200, CTCSS and DCS, plus
frame-sync-qualified identification for ProVoice, EDACS/ESK, X2-TDMA, dPMR,
M17, YSF and D-STAR.

Labels the classifier calls "probable" are modulation and clock candidates,
not validated protocol frames. On a quiet band with no antenna the classifier
will happily fit a name to noise — check the SNR and the FEC panel before
believing a label.

## Requirements

Fedora 44 package names; adjust for other distributions:

```bash
sudo dnf install rust cargo SoapySDR-devel soapy-rtlsdr rtl-sdr-devel gcc
```

Confirm the driver sees the dongle before starting the server:

```bash
SoapySDRUtil --find
```

## Build and run

```bash
cargo build --release
```

```bash
./target/release/usdr --freq 162.55
```

Then open <http://127.0.0.1:8073>.

| Flag | Meaning |
|---|---|
| `--bind` | Listen address, default `127.0.0.1:8073` |
| `--serial` | Dongle serial; defaults to the first one found |
| `--freq` | Centre frequency in MHz |
| `--rate` | Span in MHz (0.2 – 3.2) |
| `--gain` | Tuner gain in dB; omit for hardware AGC |
| `--config` | Settings file, default `~/.config/usdr/usdr.toml` |
| `--devices` | List visible dongles and exit |

Tuning, span, gain and device changes made in the browser are written back to
the settings file, so a restart comes back up where you left off. Measured
frequency error per dongle lives separately in `~/.config/usdr/devices.toml`.

The server has no authentication. It binds to localhost by default and should
not be exposed to a network you do not control.

## HTTP API

| Route | Purpose |
|---|---|
| `GET /api/devices` | Dongles the driver can see, and which one is in use |
| `GET /api/sdr/status` | Current tuning, span, gain, and sample-loss counters |
| `POST /api/sdr/tune` | `{"freq_hz": …}` — retune the span |
| `POST /api/sdr/inspect` | `{"freq_hz": …}` — move the narrowband inspect chain |
| `POST /api/sdr/rate` | `{"rate_hz": …}` |
| `POST /api/sdr/gain` | `{"gain_db": …}` or `null` for AGC |
| `POST /api/sdr/mode` | `{"mode": "nfm" \| "wfm" \| "p25" \| "dmr" \| "packet" \| "auto"}` |
| `POST /api/sdr/frontend` | `{"lo_offset": bool, "clip_guard": bool}` |
| `POST /api/sdr/zoom` | `{"zoom": 1..32}` — display magnification |
| `POST /api/sdr/ppm` | `{"ppm": …}` — crystal correction |
| `POST /api/sdr/calibrate` | Derive ppm from the tuned reference carrier |
| `POST /api/sdr/spurs` | `{"offsets_hz": [...]}` — offsets from centre to notch |
| `POST /api/sdr/device` | `{"serial": "…"}` |
| `GET /api/sdr/calls` | Recent digital voice calls, newest first |
| `GET /api/sdr/calls/{id}/audio.wav` | That call's audio |
| `GET /api/sdr/capture.wav?kind=` | `voice`, `discriminator`, or `iq` |
| `POST /api/sdr/replay?frequency_hz=` | Decode a posted I/Q WAV (≤ 16 MiB) |
| `GET /ws` | JSON stream of `fft`, `decode`, and `status` events |

## Architecture

```text
RTL-SDR (SoapySDR)
        │
        ▼
crates/radio    device ownership, tuning, calibrated ppm, bounded IQ fan-out
        │
        ├───────────────┐
        ▼               ▼
crates/dsp         crates/engine
filters, NCO,      demodulators, FEC, protocol parsers,
spectrum, AGC      classifier
        │               │
        └───────┬───────┘
                ▼
crates/server   one SDR worker thread, REST/WebSocket API, embedded web client
```

DSP runs on a standard thread; Tokio handles network I/O only. The IQ
subscriber has a bounded queue, so a slow classifier drops its own blocks
rather than back-pressuring the radio thread; `dropped_blocks` and
`lagged_blocks` in the status report when that happens.

## Front end

Every block is passed through `dsp::FrontEnd` before the spectrum measures it
or the inspect chain mixes out of it: DC offset and LO leakage removal, and IQ
image correction. The impulse blanker is deliberately left off — it keys on
wideband magnitude, so it eats the leading edge of exactly the strong
narrowband bursts PACKET exists to decode.

Measured on a quiet band at 2.048 MS/s, this flattens the centre of the span to
**0.04–1.7 dB** above the noise floor. The DC spur is, in practice, gone.

### Spur masking

The peak list ignores the tuner's own artefacts — DC/LO leakage and the
RTL2832's ±fs/4 images — within half a channel either side.

This is not theoretical. Surveying the VHF/UHF/700/800 bands here without it
returned 48 strong "signals", and the ones at the top all sat at the same
*baseband* offset and moved with the tuner, which is the definition of a spur.
With masking the same survey returns 6 candidates, and clicking one inspects a
signal rather than noise.

### LO offset (off by default)

Parking the tuner off-centre so its DC spur falls outside the displayed window
is the textbook next step, and here it is mostly not worth it.

Offsetting by the obvious quarter-span lands the display centre straight on the
RTL2832's own **±fs/4 spur**, which measured **11 dB** above the floor —
five times worse than the ~2 dB of DC residue it was meant to remove. The
offset is therefore placed midway between DC and that spur, with a window
narrow enough to keep both outside it. That works — centre excess drops to
0.7 dB and the worst spur in the window to 2.8 dB — but it costs **80% of the
span** (2.048 MHz becomes 410 kHz).

Since the DC blocker already flattens the centre to about 2 dB, the full span
is the better default. Turn the offset on when a weak signal sits close enough
to the centre that even that residue is in the way.

### Clip guard (off by default)

The radio layer can walk the tuner gain down when the ADC clips and back up
when it clears. That is right for an unattended scanner and wrong here: a gain
set by hand should stay where it was put, and watching the number move on its
own is alarming rather than helpful. It also costs I2C traffic once a second on
a clipping signal, which is exactly what provokes the tuner faults described
below. It is available as a checkbox for anyone who wants it.

## Classifier cost

The classifier, not the protocol receivers, was where the time went. Profiling
found three scans that re-examined work they had already done, and fixing them
cut every mode by two to five times:

| Mode | Before | After |
|---|---|---|
| `NFM` | 92% | **19%** |
| `P25` | 21% | 14% |
| `DMR` | 29% | 16% |
| `AUTO` | 102%, dropping blocks | **33%, none dropped** |

Each retained buffer holds a tail so a frame straddling two calls is not lost,
and each scan restarted at the head of that tail:

- **P25 frame detection** was 45% of all CPU on its own. Its buffer keeps
  ~8,700 samples while roughly 1,000 arrive per call, so it re-correlated
  ~7,400 positions per push against the same 24-symbol sync it had already
  rejected them on. It now resumes where the previous scan stopped; a position
  is only marked finished once its window has fully arrived, so one waiting on
  data is retried rather than skipped.
- **DMR acquisition** did the same while unlocked, sweeping the whole retained
  buffer and classifying every position twice, normal and inverted. The marker
  resets whenever the fit that judged those positions changes — on a successful
  acquire or a loss of lock — because an old rejection is only valid against
  the fit that made it.
- **Legacy family detection** sweeps three baud rates across eight phases of a
  0.65 s history, and did it on every channel whether or not anything was
  there. It now runs only when there is something above the noise floor. That
  is a correctness improvement as much as a saving: those sweeps searching
  noise are what produced confident labels on empty channels.

All 302 engine tests pass unchanged.

## Troubleshooting

### `i2c wr failed=-9`, and tuning that does not take

```text
rtlsdr_demod_write_reg failed with -9
r82xx_write: i2c wr failed=-9 reg=17 len=1
r82xx_set_freq: failed=-9
```

`-9` is `LIBUSB_ERROR_PIPE`: the dongle stalled its USB control endpoint while
librtlsdr was writing tuner registers (`reg=17` is frequency, `reg=05` is
gain). It is a hardware fault — stock `rtl_tcp` reproduces it on the same
dongle, with nothing in `dmesg` — and it gets worse as a dongle warms up. A
tired one measured here went from failing about one retune in thirty to
failing eight out of eight over a long session.

The receiver is built to ride that out rather than react to it:

- **Every tune is retried** up to three times, a few milliseconds apart. Most
  stalls clear on the second attempt. In an 80-tune storm on a flaky dongle
  this absorbed every fault: one I2C glitch, zero failed tunes.
- **A failed tune is not remembered as done.** The frequency the radio is on is
  only advanced when the driver accepts it, so asking for the same frequency
  again is a real retry. Recording the request optimistically is what made
  clicking a signal need several attempts before anything moved.
- **A failed tune never tears the stream down.** A tuner that will not move
  still delivers samples at the frequency it is on, and killing the receiver
  over it costs the audio and the waterfall to fix nothing.
- **Reopening the device is a last resort**, not a reflex. It costs the stream,
  and on a tired dongle it buys about one successful retune. It needs six
  failures across thirty seconds *and* nothing having tuned successfully in all
  that time, then backs off from 2 s to 30 s if it is not helping, and says so
  rather than retrying forever.
- **The display never claims an unconfirmed frequency.** The frequency axis,
  peak positions and channel extraction all follow what the tuner confirmed. If
  a tune does not take, the display stays where the radio actually is and says
  `tuner did not accept 103.7000 MHz — still on 98.5000`, instead of
  relabelling itself and putting every station a megahertz off.

If tuning fails persistently anyway, it is physical. Nothing in software fixes
it: measured on a wedged dongle, closing and reopening the device bought one
retune, a USB re-enumeration bought nothing, and a `USBDEVFS_RESET` bought
about four before it wedged again. **Unplug it and let it cool.** A different
port, a shorter cable, and somewhere with airflow all help it stay healthy.

### The passband covers half the screen

It should not any more. Two things caused it.

The LO offset costs 80% of the span, and a 200 kHz broadcast channel simply
does not fit in what is left of a 2.048 MHz span — the passband filled the
display and there was nothing either side of it to look at. The offset is now
declined automatically whenever the mode's channel would take more than a third
of the remaining window, and the full span is shown instead. It still applies
in the narrowband modes, where the channel is a few percent of the window.

A channel that legitimately covers a lot of the span — WFM in a narrow span —
is now drawn as edge brackets with a faint tint rather than a solid block, so
the signal inside it stays visible.

The receive frequency is also clamped to the window actually on screen rather
than to the wider band the ADC captures, so it can no longer sit somewhere the
display does not reach.

### What the S-meter actually measures

Power in the tuned channel, in dBFS, measured by summing the spectrum bins
across the current mode's passband. That is deliberately taken from the
spectrum rather than from the classifier, because the classifier only runs in
the narrowband modes — reading it there left WFM, P25 and DMR showing a
default value rather than a level.

The scale is a real law rather than a decorative one: 6 dB per S-unit below S9
and 20 dB per step above it, with S9 placed at −60 dBFS so that S9+60 lands on
0 dBFS, the top of the ADC. The tick labels are positioned at the dB values
they represent, so S1–S9 occupies the first 44% of the bar and the overrange
the rest. Even spacing would put every label somewhere the needle never agrees
with.

The colour belongs to the scale, not to the reading: the bar carries the
gradient and a mask hides the part not reached. Painting it on the fill instead
made the red section start partway along whatever the fill happened to be, so a
mid-scale signal showed half a red bar. Red now begins at S9 and nowhere else —
nothing below −60 dBFS shows any, and above it the red grows with how far over
the reading is.

Both readouts are smoothed, faster on the way up than on the way down, so the
last digit settles without the needle being slow to answer a signal. The peak
marker is not smoothed: it jumps to a new maximum immediately and decays back,
which is the whole point of it.

**It is relative, not calibrated.** An RTL dongle has no calibrated reference,
its tuner gain steps are discrete and unevenly spaced, and the front end clips
before the meter tops out. Measured here, a 36 dB gain change moved the reading
30 dB — monotonic and usable for comparing one signal against another, and not
S-units in the dBm sense. Do not read absolute field strength off it.

The reading rises with channel bandwidth, because a wider channel collects more
energy: on one broadcast carrier the same signal reads −31 dBFS through a
12.5 kHz P25 channel and −17 dBFS through a 200 kHz WFM one. That is correct
behaviour, not drift.

### The frequency reads low, and worse the higher you tune

An RTL dongle's crystal is not exact, and its error is *proportional* to
frequency — so a receiver a few tens of ppm out is unnoticeable on an FM
broadcast channel and badly wrong at 1 GHz. This build ships with no
correction applied until one is measured.

The status carries a live `freq_error_hz`: the power-weighted centre of the
tuned channel against where the receiver put it. Tune to a carrier whose exact
frequency you know — a P25 control channel, a commercial repeater, a broadcast
station — let the reading settle, and press **calibrate**. The correction is
worked out from the measured error and stored against that dongle's serial in
`~/.config/usdr/devices.toml`, so it survives restarts and follows the dongle
rather than the machine. It can also be typed in directly.

Two things worth knowing about the measurement:

- **A wide channel is a poor reference.** The centroid of a 200 kHz broadcast
  channel is pulled about by programme content and by neighbours bleeding in —
  measured here it disagreed by 88 ppm between two adjacent stations. A
  narrowband carrier gives a far steadier figure.
- **The discriminator's DC is worse still.** It looks like the obvious
  measurement and is dominated by modulation.

If a calibration appears not to take, check the log. The correction is a tuner
write like any other and can be refused by a dongle with a stalled I2C bus; it
is retried, and reports `frequency correction failed` when it genuinely could
not be applied rather than storing a number that never reached the hardware.

### A signal stays in the same spot on screen when I retune

It is not a signal. It is being generated inside the receiver.

A real transmission sits at a fixed *frequency*, so retuning moves it across
the display. Anything the receiver makes itself — the tuner's DC and LO
leakage, the RTL2832's ±fs/4 image, and mixer products from front-end overload
— is fixed relative to the local oscillator, so it follows the tuner and
appears nailed to one screen position no matter how far you move.

That difference is the only reliable test, and **SPUR CHK** performs it: it
nudges the LO by an eighth of a span, compares before and after, and keeps
every bin that did not move.

What it finds is then **notched**, in two places:

- **The display** is interpolated across each spur — a straight line between
  the clean bins either side — so the waterfall stops showing a signal that is
  not on the air. Measured against a strong carrier used as a stand-in, this
  takes a **10.2 dB** feature down to **1.1 dB**, and restores it when cleared.
- **The channel** gets a real notch filter for any spur that lands inside the
  passband being decoded, because a spur sitting on a channel corrupts the
  decode and not just the picture. It is applied at the inspect chain's output
  rate, where it costs a few operations per sample rather than the tens of
  millions per second the same filter would cost across the whole span.

The key reads `NOTCH n` while notches are active; press it again to clear them.
The notched offsets are drawn as dashed red `NOTCH` lines so it is always
visible what has been removed — a notch you cannot see is a good way to lose a
real signal without noticing.

The display notch is cosmetic by design: the energy is real, it is just not on
the air. A transmission that happens to sit on a notched offset is narrowed
rather than erased, and since the offsets are fixed relative to the LO, the
notch follows the tuner — so re-run the check after a large retune.

Measured on this hardware, two independent methods agree on a stationary
feature at about **+588 kHz** at 2.048 MS/s. The check's threshold is 4 dB, so
it flags what is prominent enough to be mistaken for a signal and ignores the
2–3 dB bumps below that.

They get much worse with gain. At 44 dB into populated bands a survey returned
48 strong "signals" whose top entries were all spurs; the same survey at AGC on
quiet centres found nothing above 2 dB. If the screen is full of stationary
signals, **turn the gain down first** — a wide-open front end on an RTL dongle
makes its own images out of whatever strong transmitter is nearby. The LO
offset also moves the display window off the worst of them, at the cost of
span.

### The passband is offset from the click when the LO offset is on

Fixed. The driver reports where the *local oscillator* is, and with the LO
parked off-centre that is not the middle of the window on screen — it is the
offset above it. Taking one for the other shifted the whole frequency axis by
the offset, so a click landed a few hundred kHz from the passband it produced,
and only when the LO offset was enabled.

The confirmed oscillator frequency and the confirmed display centre are now
kept apart: the axis, peak positions and channel level follow the display
centre, while the channel extractors — which work on the raw span — follow the
oscillator. Measured at 154 MHz with a 300 kHz offset, clicks at 25%, 50% and
75% across now land within **0 kHz** of the frequency clicked, with the offset
on and off.

### Clicking the spectrum lands beside the signal

Fixed. The click handler measured the bordered wrapper around the canvas while
the marker was drawn in canvas coordinates *inside* that border, and the
backing store was sized from the wrapper too. The error was zero at the centre
of the span and grew toward the edges — about ±1.6 kHz at a 2.048 MHz span,
which is why it looked slightly off most of the time but fine in the middle.
Hit testing now measures the canvas itself, and backing stores are sized from
their own box and scaled by `devicePixelRatio`, so a click round-trips to
within a fraction of a pixel of where the marker draws.

### `UNKNOWN_CSBK` on a Motorola Capacity Max control channel

`decode_csbk` checks the CRC before reporting anything, so an unknown CSBK is a
correctly received, FEC-corrected frame whose opcode this build has no name for
— a coverage gap, not corruption. It now names itself,
`UNKNOWN_CSBK op=0x2C mfid=0x00`, and expanding the frame shows the opcode,
manufacturer ID, FEC corrections and the raw bytes.

Opcodes added after checking them against DSD-FME's `dmr_csbk.c` rather than
from memory: `UU_V_REQ` (0x04), `UU_ANS_RSP` (0x05), `CT_CSBK` (0x07),
`P_ACKD` (0x22), `P_ACKU` (0x23), and Motorola's `MOTO_DATA_CHANNEL` (0x29 with
MFID 0x10). The unit-to-unit requests also decode their target and source.

Worth knowing about 0x29: **DSD-FME does not know what it is either.** It sits
in a section of `dmr_csbk.c` headed "misc discovered but not uncovered CSBKs",
where it is printed as a data-channel announcement with its eight payload bytes
dumped, and the comment records SDRTrunk's guess that it may be a data revert
channel. This build matches that — it names the opcode and surfaces the
payload, and says the interpretation is unconfirmed rather than asserting one.

For any opcode still unnamed, the frame's payload is shown along with the two
24-bit fields most Tier III CSBKs place at the front of it, labelled
`candidateTargetId` and `candidateSourceId`. They are candidates because that
layout is a convention of the opcodes we can identify, not a guarantee for one
we cannot — if they match real IDs on the system, the layout is confirmed.

### The classifier names a protocol on an empty band

Labels are only as good as the signal. With no antenna the classifier will fit
a name to noise — it reported `M17 · 4-FSK @ 4800 Bd` at 0.015 dB SNR during
testing. Check the SNR figure and the Protocol Decode/FEC panel before
believing a label; the panel shows rejected frame attempts alongside accepted
ones for exactly this reason.

### `peak IQ` sits at 1.00

The front end is clipping. Turn AGC off and set a manual gain until the peak
sits below about 0.9; hardware AGC on an RTL dongle will happily saturate the
ADC on a strong nearby signal.

## Provenance

`crates/dsp` and `crates/engine` are unmodified copies from
the `scannerd` project (commit
`5dda1c4`). `crates/radio` is the same copy plus one addition: a
`Cmd::ClipGuard(bool)` so the automatic gain guard can be switched off, which
upstream has no way to do. `crates/server/src/sdr.rs` is that project's SDR worker with two
changes: its per-device calibration lookup now reads a local `devices` module
instead of the `scannerd-store` crate, which dropped the SQLite and Opus
dependencies along with it; and it coalesces tuner writes, waits for a busy
radio at startup, and reopens the dongle after a USB fault rather than sitting
on a dead waterfall (see [Troubleshooting](#troubleshooting)).

`crates/server/src/main.rs` is new — a single-mode server in place of the
multi-mode original.

The web client keeps the original's classifier, decode-log, and I/Q replay
JavaScript verbatim, and is otherwise new: the IC-7300-style layout, the
digit-tuning readout, the spectrum/waterfall renderers, the audio scope, and a
trimmed WebSocket client that handles only `hello`, `fft`, `decode`, and
`status`. The frequency preset buttons the original carried are gone; the
readout replaced them.

Upstream `scannerd` was not modified. Changes made here have not been sent
back to it.
