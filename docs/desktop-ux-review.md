# uSDR desktop: UX review

Reviewed 2026-11-02 against `remote-sdrplay` at a62c016, running the release build
against the live server (RSP1A on 172.16.0.216) at 1600×980 and 1200×760, both
grounds, every dock tab, and the connect screen. No code was changed. Screenshots
are in `/tmp/claude-1000/-home-patrickh-usdr/1d7e29c3-8bc5-4b3c-bb88-bb91aaa960e7/scratchpad/shots/`
(`01-main` WFM at 5 MHz, `02-nfm` NOAA at 2 MHz, `03`–`05` the dock tabs,
`06-ink`, `07-narrow`, `08-connect`).

Not seen live, because Wayland gives no way to inject input: the settings, help
and diagnostics windows, the right-click menu, hover states, receiver B, the held
waterfall, and the Classic / Glacier / Phosphor styles. Those were reviewed from
the code only. The server was put back to 100.5 MHz WFM at 5 MHz afterwards.

**Acted on, same day:** every item in sections 1 to 4 is implemented in the
working tree (uncommitted); the two deliberate deviations are a 10 px floor for
scale numerals rather than 10.5, and stock egui rows kept inside menus.

## Verdict

This is already a serious instrument. The receiver bar reads as one band, the
house widgets (keys with tally LEDs, chip keys, faders, chips) give the panel one
voice, the S-meter has real ballistics, the waterfall pans on retune, the slice
flag is the right idea, and the connect splash is the best piece of brand work in
it. The data honesty is exemplary: nothing is invented, and the empty and
degraded states say what is going on.

What holds it back is a short list of visible defects (one of them a text
overlap on the hero element), a handful of stock egui widgets that break the
house voice, and the fact that nothing *moves* when the operator acts. The
motion that exists is ambient (the meter, the waterfall, the mark); the
operator's own actions (tune, step, fold, switch tab) snap. That is the gap
between "well made" and "feels expensive", and it is cheap to close.

## 1. Defects (fix first)

1. **Slice flag text overlaps its receiver letter.** The bottom line
   (`WFM broadcast · SNR 14.5 dB`) runs under the `A` in the corner, on every
   screenshot. [spectrum.rs:1530](../crates/desktop/src/spectrum.rs:1530) paints
   the letter at the right edge and the line at the left with no truncation.
   Fix: make the letter a small tab at the top-left of the card (where the ember
   index tick already is), and truncate the bottom galley to the card width.
   Same for B's flag.

2. **The readout's unit contradicts its format.** `0.162.475.000 MHz`: dots as
   group separators plus a MHz unit reads as a malformed decimal. The README
   describes the house format as `146.520 000`. Fix in
   [vfo.rs:108](../crates/desktop/src/vfo.rs:108): one real decimal point after
   the MHz group, a thin gap (not a dot) between the kHz and Hz groups, and no
   separator after the dimmed GHz zero.

3. **The SIGNAL bay truncates the one thing it exists to say.** At the default
   1600 px fit the classification shows as `WFM broadcast · Wideband…` and
   `Analog Voice…` ([bar.rs:1497](../crates/desktop/src/bar.rs:1497)). Fix: show
   the protocol alone on that line, modulation on hover, and give the bay a
   second line when there is no `err` to show.

4. **Ink ground is almost indistinguishable from Carbon.** Measured on the
   chrome: the means differ by about five levels per channel. The visible
   changes are the weave disappearing and steel text going flat, which reads as
   "something broke", not "a warmer ground". Either push Ink's surfaces apart
   (lift `s[1..3]`, warm the hairlines, let the amber selection show) or drop
   the setting. My call: keep it and make it earn its place.

5. **Zoom is not anchored at the cursor.** `docs/desktop-app.md` promises
   "Ctrl-wheel zoom at the cursor"; the code zooms around the receiver
   ([spectrum.rs:1250](../crates/desktop/src/spectrum.rs:1250),
   `App::set_zoom`). Operators zoom into what they are pointing at. Fix: when
   the cursor is more than a channel from the receiver, tune to the snapped
   cursor frequency in the same gesture, then zoom. Or correct the doc.

6. **The two-row bar wastes its second row.** Under 1500 px the WIDTH fader
   stretches to ~900 px and the meter to 760 px
   ([bar.rs:1038](../crates/desktop/src/bar.rs:1038)). Nine hundred pixels for a
   2.5–25 kHz range is a slab of empty track. Cap the filter bay at ~380 px and
   let AUDIO sit beside it; let the meter cap at ~420.

7. **The classifier's detail line wraps mid-field** (`Dev:` on one line, `Peak`
   on the next). [dock.rs:321](../crates/desktop/src/dock.rs:321) wraps one long
   mono string. Lay it as three lines: name + modulation + confidence; the
   measurements; the details, each truncated.

8. **Menu chips have no menu affordance.** `SPAN 5M`, `RANGE AUTO`,
   `STYLE EMBER`, `WATERFALL` are egui `menu_button`s with no `▾`, while
   `DISPLAY ▾` and the step selector have one
   ([toolbar.rs:366](../crates/desktop/src/toolbar.rs:366)). One `menu_chip`
   widget with the chevron, styled like `chip_key`.

9. **The waterfall's time axis is nearly empty.** A 300 px waterfall at 25 rows/s
   shows one mark (`−5s`). Marks every 5 s are too sparse at that row rate
   ([spectrum.rs:836](../crates/desktop/src/spectrum.rs:836)). Pick the mark
   interval with `nice_step` from seconds-per-pixel, aiming at one label per
   ~70 px.

10. **`FILLING` in ember reads as an alarm.** The recorder's buffer-is-full
    chip uses the live tone, next to a red waterfall
    ([panels.rs:705](../crates/desktop/src/panels.rs:705)). Buffering is a
    steady state, not a recording. Use the OK tone; keep ember for REC.

11. **`RF GAIN` is said twice**, as the card title and as the fader label
    ([panels.rs:412](../crates/desktop/src/panels.rs:412),
    [panels.rs:587](../crates/desktop/src/panels.rs:587)). Rename the card
    `GAIN`.

## 2. UX improvements

1. **Band plan with names.** The strip is 3 px and unlabeled; the name is only on
   hover ([spectrum.rs:785](../crates/desktop/src/spectrum.rs:785)). Make it
   5 px and paint the band name inside it wherever the band is wider than about
   90 px. This is the single biggest legibility gain for a sweep across a band.

2. **Ember palette on wide spans.** At 5 MHz over FM broadcast the whole
   waterfall is a wall of red (`01-main`): the ramp's low stops put the noise
   floor in dark red, so there is little tonal room between floor and signal.
   Keep ink until ~0.30, a steeper lift to amber and paper at the top, and
   consider a default contrast range of 65 dB. The NOAA shot (`02-nfm`) shows
   the ramp works once the floor sits below the first stop.

3. **Hover readout on the trace.** The hover line and tooltip exist; add a dot on
   the trace at the hover column and a hairline from it to the dB axis. Reading
   a level off the display becomes one glance instead of a tooltip.

4. **Empty states in the dock.** `0 frames` and `0 call(s) recorded` leave a
   large blank. One muted line each: what arrives here and how to make it happen
   (for Decodes: "Frames decoded on receiver A appear here · AUTO runs every
   decoder on the channel").

5. **Status bar diet.** `clock −167 ppm`, `FFT 2048`, `crystal −28.26 ppm` are
   diagnostics an operator reads once. Keep fps, link rate and audio queue; move
   the rest to the diagnostics window (which already has the room) and the
   connection chip's hover.

6. **Window title.** It is always `uSDR`. When live, set it to
   `162.475 NFM · uSDR` through a viewport command, so the taskbar and alt-tab
   say what the receiver is on.

7. **Trace continuity across a span change.** `Panafall::push` resets the
   smoothed trace and the peak hold when the rate changes, so the trace blinks.
   Resample the old trace onto the new axis for the first frames.

8. **One minimum text size.** Bay labels are 9.5 px mono, helpers 10.0, chips
   10.5. On a 27-inch 1080p at 100 % the 9.5 is at the edge. Make 10.5 the floor
   everywhere; the size menu covers people who want smaller.

9. **First-run discoverability.** The shortcuts are good and `?` finds them, but
   nothing says so. One toast after the first successful connect
   ("? shows the shortcuts and gestures"), shown once and remembered in prefs.

## 3. Graphics and motion upgrades

Each gated by `app.reduce_motion()` (snap when reduced), each driven by the
existing `anim::more` clock, none ambient. The brand's rule stands: the mark
draws in once and beats only while waiting; everything else moves only when the
operator acts. In order of payoff:

1. **Odometer roll on the readout.** When a digit changes by a step, slide the
   old glyph out and the new one in, vertically, in the direction of the step,
   ~110 ms eased. Keep the last shown string and a per-cell start time in `Vfo`;
   paint with a clip rect per cell. The readout is the hero; this one change
   makes the whole app feel like an instrument.

2. **Marker glide on tune.** The carrier line, passband and flag jump to the
   new frequency; ease the drawn x over ~140 ms toward the target while the
   waterfall pans underneath. [spectrum.rs:1005](../crates/desktop/src/spectrum.rs:1005).

3. **Applet fold.** The chevron rotates but the body snaps. Size the body by the
   same `animate_bool` value and clip it
   ([widgets.rs:41](../crates/desktop/src/widgets.rs:41)).

4. **Dock tab underline.** Animate the amber bar's x and width between tabs
   ([dock.rs:40](../crates/desktop/src/dock.rs:40)).

5. **Toasts slide in.** They fade out but appear abruptly; an 8 px rise with a
   fade-in on arrival ([app.rs:1252](../crates/desktop/src/app.rs:1252)).

6. **Trace afterglow.** A faint, wide stroke of a slower-decaying EMA of the
   trace under the live one, like phosphor persistence. Distinct from peak hold
   (a line) and max hold; gives the panadapter depth. Off in Classic.

7. **Waterfall palette crossfade on style change.** Blend the two LUTs over six
   to eight steps and re-upload each; only on change, so it costs nothing in
   steady state.

8. **Connect splash.** Already right. Soften the hard hairline at the split with
   a short fade under the panadapter.

9. **Receiver B switch.** The violet wash on the bar is good; animate its alpha
   and the index tick's slide when A/B changes, so the switch is seen before it
   is read.

## 4. One control language

The house vocabulary (`key`, `chip_key`, `action`, `primary`, `Fader`, `chip`)
covers most of the panel. Stock egui widgets still show up with different
radii, fonts and hover fills:

| Stock widget | Where | Replace with |
|---|---|---|
| `ComboBox` | step, IF filter, device, palette, P25 algorithm, audio device | a `selector` widget: mono text, house frame, `▾` |
| `DragValue` | PPM, seven scan fields | a `numeric` skin on the house frame |
| `ui.button` | Clear ×2, context menus, stop playback | `action` |
| `small_button` | copy, IQ, WAV, re-test, text size −/+/reset | `action` at chip height |
| `checkbox` / `radio` | View menu, settings, scan modes, connect on start | fine in menus and settings; use `key` for the scan modes |

Two new widgets and a sweep of about thirty call sites.

## 5. What is good and should not change

- The receiver bar as one band that follows the active receiver.
- Tuning snaps to the mode's raster, with Alt to escape, and the hover readout
  shows where a click will land.
- The waterfall pans on retune and keeps five minutes to scroll back through.
- The S-meter's ballistics and the floor caret; the slice flag's mini meter.
- The decode log keeping the reading position with a "back to newest" pill.
- Every custom widget reports `widget_info`, so the kittest suite and screen
  readers see real controls.
- Honest text everywhere: `encrypted — muted`, `nothing is listening at`,
  `WFM keeps no buffer`.

## 6. Suggested order

1. Defects 1–11: about half a day, all small, mostly in `bar.rs`, `vfo.rs`,
   `spectrum.rs` and `toolbar.rs`. Run the 42 kittest tests after; the readout
   format and the bar fits have tests that will need their expectations updated.
2. UX 1–7: a day. The band plan labels and the palette retune are the visible
   ones; do them first and look at `01-main` again.
3. Motion 1–5: a day, each a self-contained change with a reduced-motion gate.
   Add a kittest for the odometer that runs with reduced motion on, so the
   logic is covered without timing.
4. Control language: a day, mechanical.
5. Motion 6–9 as taste allows.
