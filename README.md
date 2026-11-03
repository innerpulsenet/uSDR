# uSDR

A single-radio spectrum inspector for RTL-SDR and SDRplay hardware (the RSP1A
runs as a [daemon beside its radio](#the-server-daemon-in-ct-216), or is
reached [over SoapyRemote](#remote-sdrplay-soapyremote)):
one radio, served to a web page and to a native desktop console
([Desktop app](#desktop-app)). It began as the SDR mode of
[`scannerd`](#provenance) lifted out on its own — a live FFT and waterfall over
the tuned span, plus a narrowband inspect chain that runs the same digital
classifiers and protocol decoders the full scanner uses.

There is no call database and no trunking follow — this exists to point a
receiver at a signal and find out what it is, to decode what it finds, and,
since the voice scanner arrived, to sweep a band for voice on its own.

## What it does

The interface is laid out like a receiver's front panel — audio, gain and
front-end switches on the left, the display in the middle, the S-meter, tuning
and recorder on the right — plus a log dock beside the case. There is no Po or
SWR: this is RX-only.

- **Every sample reaches the display.** Each frame is a Welch average of
  every half-overlapped segment since the last one, so the floor is smooth
  (0.2 dB of bin-to-bin ripple on noise, from 2 dB) and a 2 ms burst between
  frames is in the next frame instead of thrown away; full duty at 10 MS/s
  costs under 5% of a core. `WF PEAK` paints the waterfall from each frame's
  strongest segment instead, so pager, data and TDMA bursts show at full
  level. Waterfall rows keep the strongest bin per pixel when the FFT is
  wider than the screen, and the FFT size follows the screen's width.
- **Spectrum and waterfall** over spans from 200 kHz to 3.2 MHz on an
  RTL-SDR and 250 kHz to 10 MHz on an SDRplay, with peak hold, trace fill,
  auto or manual dB range, and a choice of twelve waterfall palettes on the
  page (`SPECTRUM` by default; `HAM` is the black → navy → blue → cyan →
  white ramp).
- **Tuning.** The big readout is the frequency being *received*, the way a
  radio's is — not the center of the span. Everything sets it: clicking
  anywhere on the spectrum or waterfall, double-clicking the spectrum (tunes RX
  and inspects that frequency, same as a click, recentered), scrolling a digit, the arrow keys,
  double-clicking the readout to type, scrolling the display, or clicking
  a peak marker or a logged decode.

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
  analyzer: one summary line per frame, and any frame expands to its detail —
  a `Frame` section with the protocol, kind and whether the checksum passed, a
  `Fields` section with every value the decoder recovered, and the raw data.

  Below the list is a **frame bytes pane** that follows the selected frame, the
  way a packet analyzer's third pane does. Byte payloads appear as an
  offset/hex/ASCII dump; POCSAG and FLEX codewords appear as *words* rather
  than bytes, because they are 20 and 21 bits wide and packing them into a byte
  dump would misalign every one after the first. The pane says plainly when a
  decoder did not hand back a raw frame, rather than showing an invented one.

  A filter box narrows the list by protocol, kind or text, and the bytes can be
  copied to the clipboard.

- **Last heard.** Finished voice calls are kept — the most recent sixteen —
  with their audio, so a call that went past can be played again. Each is
  listed with its protocol (P25, DMR, or an analog NFM/AM hit the scanner
  caught), frequency, duration, and identities where the protocol provides
  them.

  This exists because a decoded call and an audible one are not the same
  thing. The receivers vocode encrypted voice to silence rather than to noise,
  so a call can be reported in full and still play nothing. The list says which
  it was — `encrypted · ALG 0x84 — muted`, `no audio decoded`, or a play
  button — instead of leaving silence looking like a fault.

- **Zoom** on the spectrum and waterfall, 1× to 32×, centered on the frequency
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

- **Voice scanning.** `SCAN` walks a configured frequency range looking for
  voice, the way a scanner does — but it spends its slow hardware retunes on
  span moves instead of channel steps, and finds carriers the cheap way. See
  [Voice scanning](#voice-scanning).

- **A layout that holds still.** Every live readout is a fixed size and every
  growing list scrolls inside its own fixed-height box, so nothing on the page
  moves as text updates. The decode and classification logs prepend rows, so
  they also hold your scroll position against the new rows rather than sliding
  the content up under you.
- **Thirteen modes**, switchable from the screen:

  | Mode | What it does |
  |---|---|
  | `NFM` | Narrowband FM with the full digital classifier |
  | `AM` | Envelope-detected AM with AGC — HF airband and broadcast |
  | `USB` / `LSB` | Single sideband voice for HF: the 300–2800 Hz above (USB) or below (LSB) the suppressed carrier, the opposite side 50 dB down, with a speech AGC (fast attack, 400 ms hang, 15 dB/s release). Tunes on a 100 Hz raster; the passband is drawn beside the carrier |
  | `WFM` | 200 kHz broadcast FM, for listening, in stereo when the station sends a pilot: a PLL on the 19 kHz pilot regenerates the 38 kHz subcarrier, L and R are each de-emphasized (75 µs) and low-passed at 15 kHz with the pilot notched, and the separation blends toward mono as the stereo S/N (measured beside the pilot) falls from 44 to 26 dB, quickly on a fade and slowly back. Without a pilot it is the mono receiver it always was. The desktop app asks for stereo (`/ws?audio=stereo`); the page and older clients get mono |
  | `P25` | P25 Phase 1 C4FM decoded to voice through the IMBE vocoder |
  | `DMR` | DMR Tier II decoded to voice through the AMBE vocoder |
  | `NXDN` | NXDN48/NXDN96 conventional voice through the AMBE+2 vocoder; Type-C/D call assignments surface as trunking events |
  | `PACKET` | AX.25 at 1200 baud, POCSAG at 512/1200/2400, and FLEX, all at once |
  | `PAGER` | Whole-band POCSAG+FLEX sweep across the paging channels around the tune |
  | `FLEX` | Dedicated Motorola FLEX demodulator with symbol eye and discriminator scope |
  | `AUTO` | Every decoder above at once, on whatever is in the passband |
  | `SCAN` | Automatic voice scan across a configured range and mode set |

  `P25`, `DMR` and `NXDN` extract and equalize their own channel out of the
  span and decode it to audio in real time; call starts and ends,
  NAC/color-code/RAN and duration land in the decode log. `PACKET` runs
  every packet and paging decoder over the passband at the same time, so
  whatever is there decodes without being told in advance what it is.
  `GET /api/sdr/modes` serves this table — labels, audio/decode capability,
  bandwidth and scope kind per mode — so clients read what the server
  actually supports instead of hardcoding it.

  The passband drawn on the spectrum and the audio scope's frequency axis both
  follow the mode's real channel width.
- **Front panel controls.** One visible control per setting, grouped by what
  it does:
  - **AF · Audio**: `MONITOR` is the one audio switch, and `VOL` a straight
    fader (keyboard and scroll wheel work on it too).
  - **RF · Gain & SQL mark**: the RF gain and SQL mark faders, `AGC`, and the
    two gain-loop switches, `CLIP GUARD` and `IF AGC` (`TUNER AGC` on an
    RTL-SDR). `SQL MARK` draws a threshold on the spectrum and the S-meter
    only; `SCAN SQL` in the scan dock is the scanner gate.
  - **Front end**: `AM/FM NOTCH`, `DAB NOTCH`, `IQ CORR` and `BIAS-T` where
    the radio has them (hidden where it does not), `SPUR NOTCH`, `LO OFFSET`,
    and on an SDRplay the `IF FILTER` (AUTO, or one of the RSP's analog
    filters by hand). The VFO bar's `NOTCH` readout lists what is really
    notched. On an SDRplay the gain fader also shows where the gain is (LNA
    state and IF reduction, as the driver reports them), `IF AGC TARGET` sets
    the level the RSP's own AGC holds, and the span shows the RSP1A's ADC
    resolution at that rate (14 bits to 6 MS/s, falling to 8 at 10 MS/s, from
    SDRplay's specification). Measured here, the RSP's own I/Q correction made
    no difference above the floor: a −62 dBFS station's mirror read the noise
    floor (−88 dBFS) with it on or off, because uSDR corrects the image in
    software as well.
  - **Mode**: `DEMOD` keys (`NFM` `AM` `USB` `LSB` `WFM` `P25` `DMR` `NXDN`)
    pick the detector; `DECODE · SCAN` keys (`PACKET` `PAGER` `FLEX` `AUTO`
    `SCAN`) pick what the receiver does with it.
  - **Memory**: eight channels (frequency and mode). Click to recall; `STORE`
    and then a slot, or Shift-click a slot, saves the receive frequency and
    mode into it. The bank is kept by the server and shared with every client,
    the desktop app included; the browser keeps a copy for when the server has
    none.
  - **Faders** are straight precision sliders: one step per arrow key or wheel
    notch (1 dB of gain, one span, a quarter-octave of zoom), labeled with the
    scale they set. Gain commits at most every 150 ms while dragging, and a
    span change waits until the fader rests.
  - **Span · Zoom · Tune**: the span and zoom faders, `CENTER` and `ZOOM ±`,
    and a straight tuning strip — drag the scale, or scroll on it, and it
    moves one tuning step at a time with nothing coasting; the keys beside it
    step by one or ten.

  The ADC rate `<select>` lives under Advanced. The VFO badge reads `IF` for
  the channel filter width.
- **Listening.** Demodulated audio is streamed to the browser as raw PCM over
  the same WebSocket and played through Web Audio. Browsers only allow audio
  to begin inside a user gesture, so until the page has been clicked `MONITOR`
  says `CLICK TO START` rather than claiming a monitor that cannot be heard;
  after that it reads `ON` or `MUTED`, and `VOL` always shows the level.
  Analog audio is
  gated on FM quieting, measured in a 4–8 kHz band of the discriminator
  output: a carrier of about 9 dB or better opens the gate at once, a weaker
  one after 1.5 s, and an empty channel is silent rather than static.
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
- **Selected channel** panel beside the spectrum: protocol, modulation,
  confidence, RF level, deviation, and the peaks in the span, refreshed with
  every FFT frame. Two signal-to-noise figures are shown and named: *above
  floor* is the channel's power over the spectral noise floor (what the peak
  list and the meter use); *quieting* is the classifier's discriminator-based
  figure, which runs much lower on the same carrier.
- **Stream health** in the status strip: `STREAM OK`, or the count of
  dropped blocks and sample gaps since start when the receiver is not
  keeping up. Front-end options, calibration and the display range live in
  an **Advanced** drawer under the main controls; the results (class log,
  decode log, last heard, scan) sit directly below the receiver. The `⤢ tall`
  button, or `F`, gives them most of the viewport.
- **Last Heard diagnostics** retain each call's decoded WAV, channelized raw
  CS16LE I/Q, peak SNR, tuning error, protocol identities, encryption/key IDs,
  and every other end-of-call field the demodulator recovered.
- **Protocol Decode / FEC panel** showing accepted *and* rejected frame
  attempts, so a heuristic label is never presented as a successful decode.
- **Recorder.** The receiver keeps the last 20 seconds of the selected channel
  all the time, so a signal that went past can still be saved. `SAVE I/Q`
  writes the channel's complex baseband in the same format as a Last Heard
  call's IQ export — raw CS16LE (signed 16-bit I then Q, no header), named
  `usdr-capture-<time>-<MHz>MHz-<rate>sps-cs16le.iq` — with a `.json` beside it
  carrying `freqHz`, `iqRateHz`, `iqSamples`, `durationS`, `startedMs`, mode
  and radio. `SAVE AUDIO` and `SAVE DISC` write the demodulated audio and the
  discriminator as WAV. What the ring holds depends on the mode, and
  `GET /api/sdr/modes` says so (`capture`): NFM, PACKET, FLEX and AUTO keep all
  three; AM, P25, DMR, NXDN and SCAN keep the channel I/Q (their audio is at
  another rate, and is kept per call in Last Heard); WFM and PAGER keep
  nothing. The recorder offers only what the current mode can save, and says
  why when it cannot.
- **Deterministic replay**: push saved I/Q back through a fresh classifier and
  get the same decode events, with no radio attached. `LOAD I/Q…` takes a
  CS16LE `.iq` (with its `.json`, or keeping the `…sps-cs16le.iq` name that
  carries the rate) or an I/Q WAV.

### Decoders and classifiers included

Everything in `crates/engine` came across intact, so the inspector recognizes
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

Fedora 44:

```bash
sudo dnf install rust cargo SoapySDR-devel soapy-rtlsdr rtl-sdr-devel gcc alsa-lib-devel
```

Ubuntu 26.04 (`soapysdr0.8-module-remote` is the SoapyRemote client, needed
only for `--remote`):

```bash
sudo apt install rustup build-essential pkg-config libsoapysdr-dev soapysdr-tools soapysdr-module-rtlsdr soapysdr0.8-module-remote libasound2-dev
```

```bash
rustup default stable
```

The ALSA headers (`alsa-lib-devel`, `libasound2-dev`) are for the desktop
app's audio; `cargo build --release -p usdr-server` builds the server alone
without them.

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
| `--serial` | Radio serial; defaults to the first one found, USB before remote |
| `--remote` | SoapyRemote host with an SDRplay receiver (`172.16.0.216`, `host:55132`); repeatable, saved to the settings file |
| `--freq` | Center frequency in MHz |
| `--rate` | Span in MHz (0.2 – 3.2 on an RTL-SDR, to 10 on an SDRplay) |
| `--gain` | Tuner gain in dB; omit for automatic gain (a clip-guarded loop, see below) |
| `--config` | Settings file, default `~/.config/usdr/usdr.toml` |
| `--devices` | List visible radios, local and remote, and exit |
| `--web` | What the browser page may do: `shared` (default), `yield` (view-only while the desktop app is connected) or `off` (not served); saved to the settings file |
| `--voice-keys` | A private file of P25 voice keys; see [Authorized P25 voice keys](#authorized-p25-voice-keys) |
| `--p25-phase2` | A P25 Phase 2 traffic frequency and its system identifiers; repeatable, same section |

Tuning, span, gain and device changes made in the browser are written back to
the settings file, so a restart comes back up where you left off. Measured
frequency error per dongle, and whether its feedline needs the bias-T, live
separately in `~/.config/usdr/devices.toml`. A manual gain is kept per radio:
switching radios parks one radio's gain and takes up the other's, because a
gain in dB does not mean the same thing on two different front ends.

The server has no authentication. It binds to localhost by default and should
not be exposed to a network you do not control.

## The server daemon in CT 216

Day to day, the server runs in the RSP1A's own container (`rsp`, CT 216 on
vault, `172.16.0.216`; see [below](#the-server-container-216-on-vault)) as the
systemd unit `usdr`, and opens the radio on that container's USB with
`driver=sdrplay`. The I/Q never crosses the network: a client gets spectrum
frames and audio, about 2 Mbit/s against the 200 Mbit/s of 6 MS/s I/Q over
SoapyRemote. At 6 MS/s in WFM it takes about a quarter of one of the
container's two cores, and SDRplay's API service another 8%.

```bash
tools/deploy-server.sh                 # build for Debian 13, install, restart
usdr-desktop 172.16.0.216:8073         # or the page at http://172.16.0.216:8073
```

- **Build.** The container is Debian 13 (glibc 2.41), older than this host, so
  the script builds in a `rust:1.93-trixie` podman container into
  `target/trixie`, then copies the binary and
  [`crates/server/packaging/usdr.service`](crates/server/packaging/usdr.service)
  over SSH and restarts the unit.
- **Settings.** The daemon runs as the `usdr` user with its settings in
  `/var/lib/usdr/.config/usdr/` (`usdr.toml`, `devices.toml`). The first deploy
  seeds them from this host's `~/.config/usdr`, dropping `remotes`; later
  deploys leave them alone.
- **Access.** It binds `0.0.0.0:8073` with no authentication, so the
  container's nftables policy is the access control. SCRIBE is in the
  `clients` set (every port). The Windows 11 PC (`172.16.0.52`) is in the
  `viewers` set, which reaches port 8073 and nothing else. A machine that only
  runs the desktop app or the page goes in `viewers`.
- **Logs.** `ssh root@172.16.0.216 journalctl -u usdr -f`.

The SDRplay API lets one process have the radio at a time. To run a server
from this checkout instead (`cargo run` against the RSP over SoapyRemote, as
below), stop the daemon first, and start it again after:
`ssh root@172.16.0.216 systemctl stop usdr`.

## Remote SDRplay (SoapyRemote)

The RSP1A can also be used from another machine over the network with
[SoapyRemote](https://github.com/pothosware/SoapyRemote). It lives in its own
container on the Proxmox host `vault`, which runs `SoapySDRServer`, and this
machine opens `driver=remote`. That proxies the whole Soapy API — both gain
elements, the notch and bias-T settings, the sample rates and the stream.
Nothing above `crates/radio` knows the radio is remote beyond the host name.
The RSP's 16-bit samples go over the wire as they are (`CS16`, 8 MB/s at
2 MS/s, 40 MB/s at 10) and become floats here, so none of the 14-bit ADC's
range is lost on the way. The stream runs over TCP, because SoapyRemote's UDP
default has no retransmission and a lost datagram is a hole in the samples.

```bash
./target/release/usdr --remote 172.16.0.216
```

`--remote` is saved, so later runs find the RSP on their own. With no RTL-SDR
plugged in it is the radio the server opens; with one, pick it in the
Advanced drawer's device list, where it shows as `RSP1A @ 172.16.0.216`. Only
SDRplay receivers are looked for on a remote host; on the server's own USB both
RTL-SDRs and SDRplay receivers are. An RTL-SDR's I2C faults
show up only on the stderr of the process that owns it (see
[`i2c wr failed=-9`](#i2c-wr-failed-9-and-tuning-that-does-not-take)), and
across the network that process is the server.

### The server: container 216 on vault

`rsp` (CT 216, `172.16.0.216`) is an unprivileged Debian 13 container that
does nothing else. It runs SDRplay's API 3.15 service, SoapySDRPlay3 (built
from source, at the commit MARIO had), Debian's `soapyremote-server`, and the
[`usdr` daemon](#the-server-daemon-in-ct-216).

- **USB.** The container bind-mounts `/dev/bus/usb` and may open USB devices
  (`lxc.cgroup2.devices.allow: c 189:* rwm` in `/etc/pve/lxc/216.conf`). A
  udev rule on vault, `/etc/udev/rules.d/66-sdrplay-rsp1a.rules`, makes the
  RSP1A's node world-writable so the container's unprivileged root can claim
  it. Every other node stays root-only, so the RTL dongles passed to MARIO are
  out of the container's reach.
- **Access.** SoapyRemote has no authentication. The server binds to
  `172.16.0.216:55132`, and the container's nftables policy
  (`/etc/nftables.conf`) drops all input except loopback, replies, ping and
  SCRIBE (`172.16.0.50`). The stream uses ephemeral ports, so the rule admits
  the address rather than a port. To add a SoapyRemote client, add its address
  to the `clients` set. The `viewers` set admits port 8073 (the uSDR daemon)
  alone. Other LAN hosts, vault included, cannot reach the server or
  SSH. `pct enter 216` on vault still gets a shell.

```bash
ssh root@172.16.0.216 systemctl status sdrplay soapyremote-server
```

<a id="the-driver-patch"></a>**The driver patch.** SoapySDRPlay3 in CT 216 carries
[`tools/soapysdrplay3-live-gain.patch`](tools/soapysdrplay3-live-gain.patch),
which keeps what SDRplay's API reports through its events (the IF AGC's gain
reduction, and power overload) and answers `readSetting` for `live_ifgr`,
`live_lna_gr`, `overload` and `overload_count`. The daemon reads them where
they exist and does without them where they do not. Applied 2026-11-03; the
sources and the library from before it are in
`/root/src/SoapySDRPlay3/usdr-backup/`. To apply it to a fresh tree:

```bash
scp tools/soapysdrplay3-live-gain.patch root@172.16.0.216:/root/src/SoapySDRPlay3/
```

```bash
ssh root@172.16.0.216 'cd /root/src/SoapySDRPlay3 && patch -p1 < soapysdrplay3-live-gain.patch && cd build && make && make install && systemctl restart soapyremote-server usdr'
```

```bash
SoapySDRUtil --find="driver=remote,remote=tcp://172.16.0.216:55132,remote:driver=sdrplay"
```

The RSP was passed through to the MARIO VM until 2026-11-02, and it stopped
streaming there; see
[`Init() failed`](#init-failed-sdrplay_api_fail-and-the-rsp-never-streams).
A container shares vault's kernel, so the RSP's isochronous transfers go
through vault's own USB stack, with no emulated controller in between.

### What changes with an RSP

**Gain.** The RSP has two *reduction* controls: an IF stage, 20–59 dB and
exact, and an LNA state whose reduction depends on the band. SoapySDRPlay3's
own "overall gain" sums the two raw numbers and calls larger *less* gain, so
the radio layer builds its own scale instead: decibels above the deepest
reduction it uses, so higher is more gain, as on the RTL. The RF GAIN fader's
range comes from the radio and follows the band (0–96 dB at 100 MHz, 0–58 dB
at 460 MHz). The first 20 dB of reduction comes off the IF stage, which costs
no noise figure. Past that the LNA steps in, because a signal strong enough
to need more is strong enough to be driving the LNA toward overload. Once the
LNA has nothing left, the IF takes the rest. Automatic gain starts there,
with the LNA wide open, rather than at 40% of the range: on the RSP's scale
that is a deep LNA state, where SDRplay measures a 40–50 dB noise figure.

The LNA reductions are SDRplay's nominal per-band values, cross-checked
against the gain SDRplay measured for each LNA state in its RSP1A technical
information. Where the two disagree about a state (the 420–1000 MHz band past
state 3, the top state at 60–420 MHz, and the top two above 1 GHz), the state
is not used, so those bands reach less deep, never wrongly. Like librtlsdr's
R820T table the scale is nominal: measured steps differ by a few dB. A
crossing into another band rewrites the gain for the new band's table.
Another SDRplay model, with no table here, moves only its IF stage.

**IF AGC.** The "tuner AGC" switch becomes `IF AGC` on an RSP: automatic gain
handed to the RSP's own IF loop, which holds the ADC at its -30 dBFS setpoint.
That loop moves only the IF stage, so the LNA follows it: once a second, an
IF gain reduction of 50 dB or more (or an overload report) takes the LNA
deeper by as much as the IF is over 40 dB, in one step, and an IF under
30 dB for three seconds gives back a state. The IF settles near 40 dB, the
same take-over level manual and clip-guarded gain use. Without this, a strong
local station drove the IF to the end of its range with the LNA wide open,
and the overdriven front end painted that station across the band; see
[intermodulation](#one-strong-station-shows-up-all-over-the-fm-band).

The IF AGC's own readings need a [patched driver](#the-driver-patch): the
stock SoapySDRPlay3 drops the API's gain-change and overload events, and its
IF gain readback is the last value set by hand. Without the patch the IF is
shown as unknown under the IF AGC and the LNA is not stepped.

**Two stages on screen.** For an RSP the Gain card draws the LNA as the steps
the band has (each as wide as its decibels) and the IF reduction on its 20–59
dB range, with the 30–50 dB window shaded while the LNA follows the IF AGC.
An `OVERLOAD` chip shows while the radio reports one; past overloads since
the radio opened are counted on the note's hover.

**Bias-T, AM/FM notch, DAB notch.** In the Advanced drawer, shown only for a
radio that has them, and read back from the driver rather than echoed. Bias-T
is saved per radio in `devices.toml`, because it belongs to the feedline: on
MARIO's it powers an inline preamp, without which the receiver is deaf, so it
is on for the RSP1A. A radio with no entry opens with it off.

**Spans** come from the radio: 250 kHz, 500 kHz, 1 MHz (the rates the RSP
decimates to in hardware) and 2–10 MHz. Measured from the container, every
span ran with no dropped, lagged or lost samples. The worker thread, which
carries the spectrum and every channel chain, uses about 22% of a core at
6 MS/s in NFM; at 10 MS/s it measured 40% before the channel chain was made
cheaper. The channel decimator computes only the samples it keeps when the
decimation is deep (every 48 kHz mode), which took NFM's chain from 124 to
40 ms per second of signal at 6 MS/s and WFM's from 107 to 54;
`cargo run --release -p scannerd-dsp --example bench_chain` times the stages. The analog filter for a whole span is the RSP's
widest that fits under it (1.536 MHz at 2.048 MS/s, 6 MHz at 6 MS/s). Tuning reaches down to the
RSP's 1 kHz floor, below the 1 MHz bound kept for other radios.

**Blocks.** SoapySDRPlay3 and SoapyRemote hand over about 1000 samples per
read. The radio thread gathers them into 20 ms deliveries, matching what the
RTL path gets from its USB buffer size, so the 32-deep subscriber queues keep
their 640 ms of slack. A driver overflow, where samples were lost before they
reached this process, starts a new epoch and counts as a dropped block. It is
no longer one of the three read errors that reopen the device.

## HTTP API

| Route | Purpose |
|---|---|
| `GET /api/devices` | Radios the drivers can see (with `host` for a remote one), and which one is in use |
| `GET /api/sdr/status` | Current tuning, span, gain, and sample-loss counters (`droppedBlocks`, `laggedBlocks`, plus `lostSamples`/`gapEvents` from the block timeline metadata) |
| `GET /api/sdr/modes` | Every accepted mode with label, capability (audio/decode), bandwidth and scope kind |
| `POST /api/sdr/tune` | `{"freq_hz": …}` — retune the span |
| `POST /api/sdr/inspect` | `{"freq_hz": …}` — move the narrowband inspect chain |
| `POST /api/sdr/rate` | `{"rate_hz": …}` |
| `POST /api/sdr/gain` | `{"gain_db": …}` or `null` for AGC |
| `POST /api/sdr/mode` | `{"mode": "nfm" \| "am" \| "usb" \| "lsb" \| "wfm" \| "p25" \| "dmr" \| "nxdn" \| "packet" \| "pager" \| "flex" \| "auto" \| "scan"}` |
| `POST /api/sdr/scan/config` | `{"start_hz": …, "stop_hz": …, "modes": ["am","nfm","p25","dmr"], "threshold_db": …, "resume_delay_s": …}` — voice-scan configuration |
| `POST /api/sdr/scan/control` | `{"action": "pause" \| "resume" \| "skip" \| "forget"}`, or `{"action": "unskip", "freq_hz": …}` to re-test one learned skip |
| `POST /api/sdr/frontend` | `{"lo_offset": bool, "clip_guard": bool, "tuner_agc": bool}`, and on an SDRplay `"bias_t"`, `"rf_notch"`, `"dab_notch"`, `"iq_correction"` (bool), `"agc_setpoint"` (−60..0 dBFS, the IF AGC target) and `"if_bandwidth"` (Hz from the radio's list; 0 returns the analog IF filter to following the span) |
| `POST /api/sdr/processing` | Any of `{"nr": "off" \| "low" \| "medium" \| "high", "auto_notch": bool, "blanker": 0..3, "agc": "fast" \| "medium" \| "slow"}`; fields left out keep their setting, and the answer is the whole set in force. Saved, and carried in the status as `processing`. See [Noise and AGC](#noise-and-agc) |
| `POST /api/sdr/avg` | `{"mode": "off" \| "slow" \| "deep"}` — extra spectrum averaging (about 0.5 s or 2 s) |
| `POST /api/sdr/zoom` | `{"zoom": 1..32}` — display magnification |
| `POST /api/sdr/display` | `{"bins": n}` — the spectrum's width in device pixels; the FFT follows it (a power of two, 1024–4096 at 1×: a wide screen can raise it, a narrow one never lowers it for other viewers) |
| `POST /api/sdr/ppm` | `{"ppm": …}` — crystal correction |
| `POST /api/sdr/calibrate` | Derive ppm from the tuned reference carrier |
| `POST /api/sdr/spurs` | `{"offsets_hz": [...]}` — offsets from center to notch |
| `POST /api/sdr/device` | `{"serial": "…"}` |
| `GET /api/sdr/p25/key?frequency_hz=` | Whether a P25 channel key is configured there, with its algorithm and key ID — never the key |
| `POST /api/sdr/p25/key` | `{"frequency_hz": …, "action": "save", "entry": {"algorithm", "key_id", "key"}}` or `"action": "remove"`; accepted only from the server's own page |
| `GET /api/sdr/calls` | Recent voice calls, newest first (P25, DMR, and scanner-caught AM/NFM) |
| `GET /api/sdr/calls/{id}/audio.wav` | That call's audio |
| `GET /api/sdr/calls/{id}/raw.iq` | That call's channelized raw I/Q (`CS16LE`, interleaved I/Q; frequency, rate, and format are returned as `x-usdr-*` headers) |
| `GET /api/sdr/capture.wav?kind=` | `voice`, `discriminator`, `iq`, or `span` (raw device I/Q at the span rate; the ring is demand-armed — the first request starts the fill, so re-request for the full 12 s) |
| `POST /api/sdr/replay?frequency_hz=` | Decode a posted I/Q WAV (≤ 16 MiB) |
| `GET /api/version` | `{"name", "version", "api", "features"}` — the API revision (2) and what this server supports; the websocket `hello` carries the same as `server` |
| `POST /api/sdr/filter` | `{"low_hz": …, "high_hz": …}` (edges from the carrier) or `{"reset": true}`, with an optional `"mode"` (default: the current one) — that mode's filter, saved per mode. Sideband takes both edges (width and shift), AM/NFM/WFM a centered width; digital modes refuse it. `GET /api/sdr/modes` gives each mode's `filter` range, and the status carries `filter_low_hz`, `filter_high_hz` and `filter_custom` |
| `POST /api/sdr/rx2` | `{"freq_hz": …, "mode": "nfm" \| "am" \| "usb" \| "lsb" \| "wfm"}` starts or moves a second receiver inside the span, `{"off": true}` stops it. A listener only, with no decoders; its audio arrives as `[0x04][u8 receiver][u32 rate][i16…]` frames, which the web page skips, and the status carries `rx2` |
| `GET /api/web` | Who is connected (`clients`: kind, name, address, since) and the web policy |
| `POST /api/web` | `{"policy": "shared" \| "yield" \| "off"}` |
| `GET /api/memories` | The shared memory bank, or `null` before anyone has stored one |
| `POST /api/memories` | `{"memories": [{"name", "freq_hz", "mode"}, …]}` (at most 64) — replaces the bank and tells every client |
| `GET /ws` | Binary FFT and audio frames, and JSON `hello`, `status`, `decode`, `presence` and `memories` events. `?client=desktop&name=<host>` identifies the desktop app |

## Desktop app

`crates/desktop` is a native client for this server, `usdr-desktop`: the
same receiver, with a console built for it instead of a web page. It runs on
Linux today and is written to build for Windows 11 too (wgpu draws through
Vulkan or DX12; cpal plays through PipeWire/ALSA or WASAPI). The radio, the
DSP and every decoder stay in the server; the app streams the spectrum,
audio, decodes and status, and sends the operator's changes back.

```bash
cargo build --release -p usdr-desktop
```

```bash
./target/release/usdr-desktop 127.0.0.1:8073
```

`crates/desktop/packaging/linux/install.sh` installs it for the current user
with a launcher and icons (the launcher is what gives the window its icon
under Wayland); `install.sh --uninstall` removes them again. The icon set
(SVG, PNGs from 16 to 1024 px, and the Windows `.ico`) is drawn by
`crates/desktop/packaging/icon/make_icon.py`; the small sizes are laid out
on their own pixel grid. `usdr-desktop --probe host:port` connects without a window,
decodes four seconds of the stream and prints what it saw.

The window is laid out by what each control acts on:

- **The receiver bar**, across the top, is the receiver being driven: A or B, the readout and
  tuning, the mode, the filter, the audio and the S-meter. Switch to B (click its flag, or `B`)
  and the same controls drive B; `M` and `R` mute and record whichever is driven, and `L|R`
  puts A in the left ear and B in the right. Memories sit under it as one-click chips.
- **The display toolbar**, above the spectrum, holds what the display shows: span and zoom,
  averaging, holds, spots, the band plan, range, style and the waterfall (folded into one
  DISPLAY menu when the window is narrow). HOLD is on the
  waterfall itself; held, a timeline beside it drags back through the last five minutes.
- **The right column** is one column: RF gain, front end, noise and AGC (when the server
  offers it), the recorder (with the channel's level over the last 20 s, so a burst can
  be seen before it is saved), the P25 key in P25, the radio, and the memory bank (folded;
  open it to store, rename or clear).
- **The dock** keeps the logs on the left and the scope always in view on the right, with
  the views the mode offers (passband, audio, MPX, discriminator) as keys over it. A playing
  call or replay shows its progress and a stop key where it was started.

What it has that the page does not:

- **The panafall.** A trace that rises at once and falls slowly, a fill
  whose heat follows level, peak hold and the server's max hold, decode
  spots as labels that stack instead of overlapping, and a slice flag at the
  carrier with the frequency, mode, filter, a meter and what the classifier
  hears. Five styles (Ember, Midnight, Classic, Glacier, Phosphor) each
  bring their waterfall palette. The waterfall paints what was measured:
  each frame's own row (AVG and the trace's bin smoothing never reach it),
  every screen column the strongest bin under it, bins as crisp columns when
  zoomed in, and each row the moment it arrives. It keeps its history in dB,
  so a palette, black level or contrast change recolors all of it (a palette
  crossfades), and a retune slides it under the frequencies it was recorded
  at. Its time marks are measured from the rows, and the palette's ramp runs
  down the gutter beside them. Drag the display to pan, drag the dB scale to
  set the range, drag the grip under the spectrum to resize the two, and
  drag the grips on the passband's edges to set the filter.
- **A second receiver.** Receiver B listens anywhere inside the span (NFM,
  AM, USB, LSB or WFM) with its own flag, passband and meter; middle-click
  or the context menu puts it there. The page skips B's audio.
- **Noise and AGC.** The [receive processing](#noise-and-agc) has its card
  here; the page has no controls for it.
- **The readout.** Every digit is a dial (wheel, or click its upper or lower
  half); typing starts entry and reads `146.52`, `146520`, `146520000`,
  `146.520.000`, `7.2m` and `540k`.
- **Audio** through the sound card with a resampler that follows the
  server's clock (it reads the drift in ppm), a balance control, playback of
  the 20 s buffer and of Last Heard calls in place of the live audio, and a
  local recorder of what you hear (`R`).
- **Captures** saved straight to disk: I/Q as raw CS16LE with a JSON sidecar,
  audio and discriminator as WAV. Drop a `.iq` and its `.json` on the window
  to decode it on the server.
- **Honest status.** Stream health, ADC clipping, frame rate, throughput,
  API round trip, the audio queue and clock drift, and who else is connected.

The page and the app stay in step through the server: every change either
makes comes back to both as a `status` event, and the memory bank is the
server's, not each browser's. Display preferences (palette, range, holds)
stay with each client, so two operators can look at the same receiver
differently.
The app's Settings set the web policy: **Shared**, **View-only** (the page
follows along under a veil and the server refuses its changes with 409 while
an app is connected) or **Off** (the page is not served).

Its look is the innerpulse Carbon ground (brushed steel on cool blacks, a
steel wordmark, glass panels lit from above), with Ink as the alternative,
and uSDR's own accent, amber, for the receiver's marks; ember is kept for
the mark, focus and recording. Motion follows the desktop's animation setting
(KDE's, today) and can be reduced in Settings. The plan it was built to is
`docs/desktop-app.md`.

The client's per-frame cost has a benchmark of its own: it drives the
panafall and the whole window headless and reports CPU time, tessellation,
vertices and texture bytes per frame for each case (live, idle, palette
change and crossfade, retune, held and scrubbed waterfall, a full decode log).
Measured here, a frame of the whole window costs about 1 ms of CPU.

```bash
cargo test -p usdr-desktop --release bench -- --ignored --nocapture --test-threads=1
```

## Architecture

```text
RTL-SDR, or SDRplay over SoapyRemote (SoapySDR)
        │
        ▼
crates/radio    device ownership, tuning, calibrated ppm, bounded IQ fan-out
        │
        ├───────────────┐
        ▼               ▼
crates/dsp         crates/engine
filters, NCO,      demodulators, FEC, protocol parsers,
spectrum, AGC      classifier, noise reduction
        │               │
        └───────┬───────┘
                ▼
crates/server   one SDR worker thread, REST/WebSocket API, embedded web client
                │
                ▼  HTTP + WebSocket
crates/proto    the wire formats, for clients other than the page
                │
                ▼
crates/desktop  usdr-desktop, the native console
```

DSP runs on a standard thread; Tokio handles network I/O only. The IQ
subscriber has a bounded queue, so a slow classifier drops its own blocks
rather than back-pressuring the radio thread; `dropped_blocks` and
`lagged_blocks` in the status report when that happens.

## Front end

Every block is passed through `dsp::FrontEnd` before the spectrum measures it
or the inspect chain mixes out of it: DC offset and LO leakage removal, and IQ
image correction. The impulse blanker is off by default — it keys on
wideband magnitude, so it eats the leading edge of exactly the strong
narrowband bursts PACKET exists to decode — and can be turned on for
ignition or power-line noise (see [Noise and AGC](#noise-and-agc)).

Measured on a quiet band at 2.048 MS/s, this flattens the center of the span to
**0.04–1.7 dB** above the noise floor. The DC spur is, in practice, gone.

### Spur masking

The peak list ignores the tuner's own artifacts — DC/LO leakage and the
RTL2832's ±fs/4 images — within half a channel either side.

This is not theoretical. Surveying the VHF/UHF/700/800 bands here without it
returned 48 strong "signals", and the ones at the top all sat at the same
*baseband* offset and moved with the tuner, which is the definition of a spur.
With masking the same survey returns 6 candidates, and clicking one inspects a
signal rather than noise.

### LO offset (on by default for an SDRplay, off for an RTL-SDR)

The plan depends on the radio. Until the operator sets `LO OFFSET` it follows
the radio: on for an SDRplay, off for an RTL-SDR.

**SDRplay (zero-IF).** The RSP has no fs/4 spur, only DC, and the DC is the
problem: measured on the RSP1A at 5 MS/s, with its own DC correction on and
the software blocker after it, the LO still sits on a steady hump 8–9 dB over
the floor and 1 dB up 15 kHz out. With the LO on the channel, that hump rides
along under whatever is tuned. So the LO is parked just past the channel —
the passband's far edge plus 50 kHz, at least 100 kHz, at most 15% of the
span — and everything else is shown: 4.8 MHz of a 5 MS/s span in NFM, 4.7 in
WFM. Measured at the receive frequency after a retune, the excess over the
floor falls from 8–9 dB to 0.0 ± 0.3 dB. The residue, now a fixed distance
from the receive frequency, is notched out of the display like a spur. The
software DC tracker also starts from the first block's mean after a retune
instead of from zero.

**RTL-SDR.** Parking the tuner off-center so its DC spur falls outside the
displayed window is the textbook next step, and here it is mostly not worth
it.

Offsetting by the obvious quarter-span lands the display center straight on the
RTL2832's own **±fs/4 spur**, which measured **11 dB** above the floor —
five times worse than the ~2 dB of DC residue it was meant to remove. The
offset is therefore placed midway between DC and that spur, with a window
narrow enough to keep both outside it. That works — center excess drops to
0.7 dB and the worst spur in the window to 2.8 dB — but it costs **80% of the
span** (2.048 MHz becomes 410 kHz).

Since the DC blocker already flattens the center to about 2 dB, the full span
is the better default. Turn the offset on when a weak signal sits close enough
to the center that even that residue is in the way.

### Noise and AGC

Optional processing, saved with the settings and reported in the status, set
through `POST /api/sdr/processing` or the desktop app's **Noise and AGC**
card. Noise reduction and the auto-notch work on the audio you hear, never on
what the decoders see:

- **Noise reduction** (off, low, medium, high; off by default) for NFM, AM,
  USB and LSB: a short-time Wiener filter whose noise estimate comes from
  each bin's minimum over the last 1.5 s, so it tracks a changing band
  without needing a pause, with a floor so weak speech is never gated away.
  Taking the noise out would leave the voice quieter than the AGC set it
  (measured on weak voice 2 dB over the hiss: 6 dB down at low, 13 at
  medium, 20 at high), so while speech is present the lost level is given
  back, up to 18 dB, and eased off again after the over; a soft limiter
  rounds the restored peaks. It adds about 16 ms of latency at 48 kHz, and
  none when off.
- **Auto-notch** (on by default) hunts heterodynes in the same voice modes;
  NFM always had it, and AM and sideband now do too.
- **Impulse blanker** (0 off to 3 aggressive; off by default) in the front
  end, ahead of the spectrum.
- **AGC speed** (fast, medium, slow) for AM and sideband audio. Medium is the
  behavior from before the setting existed. Sideband's AGC always has 60 dB
  of gain to give; past that it follows the band's noise floor, up to 90 dB,
  stopping where the hiss between overs would come within 20 dB of the
  speech. A weak signal on a quiet band plays at full level; a noisy band
  keeps the 60.
- **AF gain** in the desktop app runs to 200: 100 is unity (the middle mark),
  and past it is up to +12 dB more, behind a 2 ms look-ahead limiter so the
  boost never clips.

### Automatic gain and the clip guard

`AGC` is **not** the R820T's own AGC. The tuner's AGC drives the LNA and mixer
to full gain regardless of what is on the antenna; measured here it left the
ADC at 0.997 of full scale with the noise floor 30 dB up, and everything but
the strongest carrier gone. That is the receiver "going deaf after a bit of
tuning around", and a restart only fixed it because the restart re-applied
the saved manual gain.

Automatic gain is instead a loop on manual gain: it starts at 40% of the
tuner's range and walks the gain down 2 dB within two seconds of the ADC
clipping. Going up depends on headroom: while the peak sample is under a
quarter of full scale it steps 3 dB every two seconds, so a quiet band
reaches full sensitivity in about twenty seconds; inside that last 12 dB it
creeps 1 dB per eight quiet seconds and settles just under the clip point.
Every value it writes is one of the R820T's 29 real gain settings (0, 0.9,
1.4 … 48.0, 49.6 dB) — librtlsdr rounds any other request down, so a loop
stepping in whole decibels would write values the tuner cannot take. The
gain control shows the level it is at (`auto · 43 dB`), and the status strip
turns red with `ADC CLIPPING` while the converter is overloaded in any mode.

The driver does not report a failed tuner write: on this dongle the first
attempt of nearly every gain write stalls on the I2C bus and SoapyRTLSDR
reports success anyway, so the receiver used to sit at whatever gain it
had while the display claimed the new one. The radio layer now watches the
driver's own stderr for those faults, retries a write that did not take,
and reports one that keeps failing so the receiver reopens itself.

**Tuner AGC (off by default)**, in the Advanced drawer, hands automatic
gain to the R820T's own AGC instead of the guarded loop. It is the only way
to the tuner's VGA stage: librtlsdr fixes the VGA at 16.3 dB under manual
gain and at 26.5 dB under its AGC. Measured on the 929 MHz paging band, full
manual gain left the ADC at 0.06 of full scale — four effective bits, and
weak pages gone — while tuner AGC put the peak at 0.2 with the floor 10 dB
higher and the pages back. On a strong band it overloads, which the
`ADC CLIPPING` warning shows; it is the operator's call per band.

With a pre-amplifier ahead of the dongle the right manual gain is low but
not zero. Measured on 151–155 MHz here: at 0 dB the ADC peak was 0.1 of
full scale and the strongest carriers read 10 dB over the floor; at 8 dB the
peak was 0.5 and the same band read 22 dB, because at 0 dB the converter's
own quantization was setting the floor; at 16 dB it clipped. The guarded loop
finds that point on its own — it walks down out of clipping and only climbs
while the peak is under half of full scale — and settles at 8.7 dB there.

**Clip guard (off by default)** is the same loop under a hand-set gain: it
never exceeds the gain you chose, and walks down under it when the ADC clips.
It is off by default because a gain set by hand should stay where it was
put, and watching the number move on its own is alarming rather than
helpful. It is available as a checkbox for anyone who wants it.

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

The full engine test suite passes unchanged (`cargo test -p scannerd-engine --release`).

## Voice scanning

`SCAN` is a scanner in the classic sense: you give it a frequency range and a
set of voice modes (AM, NFM, P25, DMR), and it sweeps that range on its own,
stopping to hold whatever it can actually decode, playing it when MONITOR is
on, and filing each call into Last Heard with its frequency and mode.

What it does not do is step the channel raster. An RTL-SDR's tuner faults
above one control write per ~250 ms, so a 40 MHz range walked at 6.25 kHz
steps is half an hour per sweep, nearly all of it spent listening to empty
channels. The scanner instead spends its hardware retunes on **span moves**
and finds carriers the cheap way:

- The wideband FFT computed for the display every frame covers the whole span
  — up to 3.2 MHz on an RTL-SDR, 10 MHz on an SDRplay — at once. Peak detection over that spectrum (the same
  local-maximum search the display uses, against its own noise floor and with
  the tuner's spurs masked) produces the candidate carriers inside the
  configured range.
- The narrowband inspect chain can re-mix onto any channel inside the current
  span **without touching the tuner**. Each candidate is centered by spectral
  centroid, then test-decoded for about 1.2 s with the receivers for the
  selected modes, all at once — the same arbitration `AUTO` uses.
- Candidates are not tested one at a time. Up to **four parallel channels**
  (configurable, 1–8) each carry their own receivers, and every sweep frame
  the idle ones take the strongest carriers in the window. A birdie's test
  dwell no longer serializes in front of the short call two channels away,
  which matters most on bands like airband, where a transmission can be over
  in a few seconds. When channels prove calls — in the same block or while
  another call is already held — each holds its own: every held call records
  its own audio while the pool keeps sweeping around it, the newest call
  takes the speaker, and the window only moves once the last of them has
  dropped.
- Only when the current window's candidates are exhausted does the tuner move,
  to the next overlapping window up the range, wrapping at the top.

A candidate earns a lock on evidence, not on power alone:

- **P25 / DMR** — the channel receiver found frame sync *and* a call started.
  A receiver that locks and produces only grant and control traffic is a
  **trunked control channel**: it decodes beautifully and carries no voice, so
  it is parked out for half an hour and the skip is explained in the decode
  log. A carrier that locks but never talks is retested after a minute.
- **NFM / AM** — a hysteresis squelch on the channel SNR (default 8 dB,
  configurable) says a carrier is present, and energy through the gated voice
  audio says someone is talking. The two analog paths cross-check each other:
  FM is near-constant-envelope, so the AM detector hears almost nothing from
  it, and the discriminator hears almost nothing from AM — whichever path
  carries voice is what the channel was.

Each held call records on its own: digital calls exactly as in the P25/DMR
modes, analog calls bracketed by their slot's squelch with the hang time
trimmed off the tail — a blip of squelch with no voice behind it is filed
nowhere. A call drops a couple of seconds after it goes quiet
(configurable), releasing its slot back to the sweep; overlapping calls
finish independently and each lands in Last Heard tagged with its mode —
`P25`, `DMR`, `NFM`, `AM` — and its frequency. On the display, dashed amber
brackets mark candidates under test, solid green brackets mark held calls
recording in the background, and the filled passband is the call on the
speaker.

The operator is always senior to the scanner: tuning by hand pauses the sweep
(the SCAN panel says so), and Pause, Resume, Skip and Forget-skips are on the
panel next to the range. The panel also lists every learned skip — frequency,
reason, time left — with a per-entry re-test button, because a memory that
cannot be inspected cannot be trusted. Carriers that keep failing the test are
treated as what they usually are, birdies and persistent hash: after a few
strikes (configurable) they park for a long stretch and stop costing the sweep
its dwell time, and they stay in the list — plainly labeled — until re-tested. Configuration — range, modes, threshold, resume
delay — persists in `usdr.toml`, and a restart with `scan` as the saved mode
comes straight back up scanning. Learned skips survive reconfiguration and
the sweep wrapping around; only a server restart forgets them.

Not in this first version: following a trunked control channel's voice grants
(the engine already surfaces grants, so this is the natural next step), NXDN
as a scan mode, and a manual per-channel lockout list.

## Troubleshooting

### `Init() failed: sdrplay_api_Fail`, and the RSP never streams

This was the RSP1A passed through to the MARIO VM. From at least 2026-10-31,
SDRplay's API service segfaulted in `Init()` at the first stream, every time.
The guest logged `xhci_hcd … TRB DMA ptr not part of current TD` errors from
QEMU's emulated xHCI controller at the same moment. Enumeration, gain,
settings and tuning all worked, because none of them touch the stream.

The RSP streams over high-bandwidth isochronous USB (3 × 1024 bytes per
microframe), which is the weakest part of QEMU's USB passthrough. Ruled out
in turn: the guest kernel (the previous one failed the same way), stale QEMU
state (a cold VM restart did not help), the QEMU update (nothing USB-related
in it), and the hardware (the same RSP streams cleanly from a container on
the same host). SoapySDRPlay3 built for the RSP's bulk mode stopped the crash
but stalled instead. The RSP worked in this VM in September, under vault's
previous kernel. That points at vault's 7.0.14-22 kernel together with
passthrough, but it is not proven.

The fix was moving the RSP out of the VM into its own container; see
[the server](#the-server-container-216-on-vault). If it is ever passed to a
VM again, expect this.

### One strong station shows up all over the FM band

Seen 2026-11-03: WRKI on 95.1, the closest and strongest station, was heard
on 102.1 and on humps across the band, and RDS on them decoded as I-95. Not
an image (the LO is never 3.5 MHz from the receive frequency) and not an
alias (at 6 MS/s 95.1 folds to 101.1): it was intermodulation in an
overdriven front end. Products land at 2×A − B for strong pairs (2 × 95.1 −
88.1 = 102.1), twice as wide as a station because the doubling doubles the
deviation, and RDS survives it because its coding is differential.

The cause was the IF AGC with the LNA wide open, plus the bias-T preamp:
daemon start splits 76 dB of gain to LNA state 0, and the IF AGC then held
it there. Measured across 99.85–105.55 MHz:

| Gain | LNA | IF | Floor | 102.1 |
|---|---|---|---|---|
| manual 32 dB | 57 dB reduction | 27 dB | -83.9 dBFS | clear |
| manual 76 dB | 0 dB | 40 dB | -33.1 dBFS, API reports overload | -24.1 dBFS, the only peak left |
| IF AGC, LNA following | 38 dB (settled in 2 s) | 39 dB | -81.5 dBFS | clear, real stations 2–5 dB further over the floor than at 32 dB |

Fixed by the LNA following the IF AGC (see
[What changes with an RSP](#what-changes-with-an-rsp)). If it comes back,
the Gain card shows it: the `OVERLOAD` chip, and the IF bar at its end in
red. With the LNA already at its deepest state there, the preamp is adding
more than the RSP can take; turn the bias-T off to check.

### On an RSP, every station sits beside its line, and calibrate reads hundreds of ppm

Fixed. SoapySDRPlay3 stores a new frequency before asking SDRplay's API to
apply it. When the API refuses (another update in flight) or times out, the
driver only logs a warning on the radio's host and returns as if it had
worked; its readback is the stored value, and a repeat request for the same
Hz is skipped. Over SoapyRemote the warning never arrives. On the RSP1A this
left the hardware about 47 kHz below what the display labeled, so every
station sat off its VFO line and a calibration read −236 ppm.

After every SDRplay tune or ppm change, the radio layer now writes the
frequency again 1 Hz up at 300 ms and back at 1.5 s, so each write is a
change the driver acts on. If calibrate still refuses, it names the measured
offset and says to tune away and back.

The refusal that kept recurring, found on 2026-11-03 once the driver ran
inside the daemon and its warnings reached the journal, was
`sdrplay_api_Update(Tuner_FrF) Error: sdrplay_api_OutOfRange` on every tune
across 60 MHz. The API rejects a tune into a band that lacks the LNA state
in use. The RSP1A has 10 states from 60 MHz to 1 GHz, 7 below 60 MHz and 9
above 1 GHz, and the gain split uses state 8 at FM gains. So any hop from FM
to an HF memory (7.241, 14.298) stranded the tuner, and a calibration taken
afterwards saved −148 ppm against a true +25. Measured: at LNA state 8 the
60.5 → 59.5 MHz hop was refused every time; at state 3 it took every time.
Four changes close it:

- Before an SDRplay tune, the LNA state comes down to the deepest the new band
  has. Under IF AGC the band change then puts back the state the chosen gain
  makes there, so returning to FM restores state 8.
- If a tune is still refused (a model with no known state count), it is
  retried at LNA state 0, which every band has.
- The stderr watcher now counts SoapySDRPlay3's refusals as faults. A refused
  tune is retried a hertz further on, because the driver has already stored
  the refused frequency and would otherwise skip a repeat as unchanged. A
  tune that never lands shows "tuner did not accept" and puts the display
  back where the radio is.
- CALIBRATE refuses while that error shows, and refuses a step of more than
  50 ppm from a correction already in use. From 0 the full ±200 ppm is open.

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

The meter is a straight segmented bar rather than a needle: it shows one
quantity, so it gets one scale. Around the bar it draws only what the receiver
measured — the peak, the channel's noise floor (the caret under the bar) with
the gap from it to the signal shaded as the SNR, and the `SQL MARK` on the same
scale, filled while the level is at or above it. The readout gives the S-unit
reading on the same law (`S7`, `S9+12`), the level in dBFS, the SNR and floor,
and how far automatic gain has the front end down from its maximum, from the
gain the radio reports.

The color belongs to the scale, not to the reading: the bar carries the
gradient and a mask hides the part not reached. Painting it on the fill instead
made the red section start partway along whatever the fill happened to be, so a
mid-scale signal showed half a red bar. Red now begins at S9 and nowhere else —
nothing below −60 dBFS shows any, and above it the red grows with how far over
the reading is.

Both readouts are smoothed, faster on the way up than on the way down, so the
last digit settles without the bar being slow to answer a signal. The peak
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
behavior, not drift.

### The frequency reads low, and worse the higher you tune

An RTL dongle's crystal is not exact, and its error is *proportional* to
frequency — so a receiver a few tens of ppm out is unnoticeable on an FM
broadcast channel and badly wrong at 1 GHz. This build ships with no
correction applied until one is measured.

The status carries a live `freq_error_hz`: the power-weighted center of the
tuned channel against where the receiver put it. Tune to a carrier whose exact
frequency you know — a P25 control channel, a commercial repeater, a broadcast
station — let the reading settle, and press **calibrate**. The correction is
worked out from the measured error and stored against that dongle's serial in
`~/.config/usdr/devices.toml`, so it survives restarts and follows the dongle
rather than the machine. It can also be typed in directly.

Two things worth knowing about the measurement:

- **A wide channel is a poor reference.** The centroid of a 200 kHz broadcast
  channel is pulled about by program content and by neighbors bleeding in —
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
quiet centers found nothing above 2 dB. If the screen is full of stationary
signals, **turn the gain down first** — a wide-open front end on an RTL dongle
makes its own images out of whatever strong transmitter is nearby. The LO
offset also moves the display window off the worst of them, at the cost of
span.

### The passband is offset from the click when the LO offset is on

Fixed. The driver reports where the *local oscillator* is, and with the LO
parked off-center that is not the middle of the window on screen — it is the
offset above it. Taking one for the other shifted the whole frequency axis by
the offset, so a click landed a few hundred kHz from the passband it produced,
and only when the LO offset was enabled.

The confirmed oscillator frequency and the confirmed display center are now
kept apart: the axis, peak positions and channel level follow the display
center, while the channel extractors — which work on the raw span — follow the
oscillator. Measured at 154 MHz with a 300 kHz offset, clicks at 25%, 50% and
75% across now land within **0 kHz** of the frequency clicked, with the offset
on and off.

### Clicking the spectrum lands beside the signal

Fixed. The click handler measured the bordered wrapper around the canvas while
the marker was drawn in canvas coordinates *inside* that border, and the
backing store was sized from the wrapper too. The error was zero at the center
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
24-bit fields most Tier III CSBKs place at the front of it, labeled
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

`crates/dsp`, `crates/engine` and `crates/radio` started as copies from the
`scannerd` project (commit `5dda1c4`) and have been developed here since:
hot-path CPU passes and a cheaper channel decimator, FLEX and POCSAG
weak-signal fixes, USB and LSB, adjustable filters, P25 keyed decryption,
noise reduction and audio AGC speeds in the engine; and in the radio, a
`Cmd::ClipGuard(bool)` so the automatic gain guard can be switched off, gain
writes that are retried and snapped to the tuner's table, the tuner's own
AGC as an option, SDRplay receivers over SoapyRemote, and re-asserted SDRplay
tunes. `crates/server/src/sdr.rs` grew from that project's SDR worker: its
per-device calibration lookup reads a local `devices` module instead of the
`scannerd-store` crate, which dropped the SQLite and Opus dependencies along
with it, and it coalesces tuner writes, waits for a busy radio at startup,
and reopens the dongle after a USB fault rather than sitting on a dead
waterfall (see [Troubleshooting](#troubleshooting)).

`crates/server/src/main.rs` is new — a single-mode server in place of the
multi-mode original — as are `crates/proto` and `crates/desktop`.

The web client keeps the original's classifier, decode-log, and I/Q replay
JavaScript, and is otherwise new: the transceiver-style layout, the
digit-tuning readout, the spectrum/waterfall renderers, the audio scope, and a
WebSocket client that handles `hello`, `fft`, `decode`, `status`, `presence`
and `memories`. The frequency preset buttons the original carried are gone; the
readout replaced them.

Upstream `scannerd` was not modified. Changes made here have not been sent
back to it.

## Authorized P25 voice keys

In the web interface, select **P25**, tune the frequency you want to monitor,
and open **P25 channel key** below the receiver controls. Choose ADP/RC4,
DES-OFB, AES-256-OFB, or AES-128-OFB, enter the over-the-air **key ID in hex**,
and enter the full hexadecimal key. **Save key** applies the key at the next
P25 voice block without restarting. Saving again replaces the channel key;
**Remove channel key** removes it.

Web-entered keys are scoped to the displayed frequency, rounded to the nearest
Hz. They are saved in a separate private file alongside the selected settings
file (`usdr.p25-keys.json` for `usdr.toml`, mode `0600` on Unix), and restored
on startup. The browser receives only configured/not-configured status, key
type, and key ID; it never receives the saved key value. The entry field clears
after submission and when changing channels. Keys are not saved in browser
storage or the ordinary settings TOML.

A web-configured channel key overrides startup-file keys on that frequency.
Its type and key ID must match the call. Removing it restores startup-file key
lookup; if neither source has a matching key, encrypted voice stays muted.

The command-line key-file option remains available:

Start uSDR with `--voice-keys /path/to/keys.toml` to load operator-supplied keys
before any receivers start. Without a matching algorithm and over-the-air key
ID, encrypted calls remain muted. Key IDs are not frequencies or talkgroup IDs.
The key file is separate from browser settings, is never exposed by the web API,
and must be private on Unix (`chmod 600 /path/to/keys.toml`). Restart after edits.

```toml
[[keys]]
protocol = "p25"
algorithm = 0xaa
key_id = 0x1234
key = "0001020304" # public example only; replace with your actual key
```

| Algorithm | ALGID | Key length in hex digits |
| --- | --- | --- |
| Motorola ADP / RC4 | `0xaa` | 10 |
| DES-OFB | `0x81` | 16 (including DES parity bits) |
| AES-256-OFB | `0x84` | 64 |
| AES-128-OFB | `0x89` | 32 |

The three algorithms supported by boatbod OP25 (ADP, DES-OFB, AES-256) are
covered by byte-for-byte reference vectors for Phase 1 and Phase 2. AES-128 is
an additional uSDR algorithm and is not part of that OP25 comparison.

Existing boatbod OP25 JSON key files are also accepted by `--voice-keys`:

```json
{"0x1234": {"algid": "0xaa", "key": [0, 1, 2, 3, 4]}}
```

JSON IDs, algorithms, and bytes can use decimal numbers or hexadecimal strings.
Short JSON key arrays are padded with leading zeros as in OP25. TOML hex keys
require the full length, including leading zeros. Invalid files fail startup
without printing key material. Oversized keys are rejected rather than truncated.

If a key still produces no intelligible audio, check the received ALGID and KID,
key length/leading zeros, and whether clear P25 reception works at the same
signal quality. A configured key cannot be authenticated by P25 voice encryption;
the `decrypted` flag means the key was applied, not proof that it is correct.
For an interoperability report, include phase, ALGID, KID and an authorized
capture if available; do not put real keys in issues. Use the dedicated key form or private key file.
The reference revision and reproduction procedure are in
[the crypto test notes](tools/p25-crypto/README.md).

For a Phase 2 traffic frequency, also supply the system identifiers and the
**zero-based** timeslot (0 or 1), then select P25 mode and tune that frequency:

```sh
usdr --voice-keys /path/to/keys.json \
  --p25-phase2 '851.0125,0xbee00,0x123,0x293,0'
```

The fields are MHz, WACN, system ID, NAC, and slot. Decimal integers and `0x`
hexadecimal integers are accepted. Repeat `--p25-phase2` for other frequencies;
only one selected timeslot per frequency is supported. Other frequencies retain
Phase 1 reception. These settings select a receiver; they are not key material.
Phase 2 keys use the same key-file format as Phase 1. Unknown encryption state
is muted until a PTT or repeated ESS establishes the call state.

This provides explicitly configured Phase 2 reception, **not automatic Phase 2
trunk-following parity** with OP25. Live RF interoperability still needs an
authorized capture or a radio test; synthetic I/Q tests cover the connected
receiver path and missing-key muting.
