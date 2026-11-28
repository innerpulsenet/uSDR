# uSDR desktop: design review

Reviewed 2026-11-23 on `hf-station-schedule` (b5d5aad). I looked at the app's own GPU renders
(`shots` and the `qa_renders_*` tests) at 1100, 1280, 1400, 1600, 1900 and 2560 px wide, with
Carbon, Ink and Night, receiver B, settings, the map, and the HF views. I also read the theme,
receiver bar, dock and widget code. No code was changed. The screenshots are in
`output/design-review-2026-11-23/`.

**Acted on, 2026-11-24:** every item below is implemented. Four differ from what the
review proposed:

- **The display bar moves up into the masthead, not into the spectrum well.** It folds to
  fit the room. The dial scale and the steady ADC/STREAM lamps move to the status bar; a
  fault (NO RADIO, ADC CLIP) still shows in the masthead. Below about 1200 px the bar stays
  in its own row over the display. The waterfall gains about 30 px at 1280 px and wider.
- **The mode keys stay as the selector,** under their LCD zone, as the bar's design asks.
  The LCD's dim echo of the mode list is replaced by the mode's description.
- **Wide windows get a larger text size instead of capped zones.** The text size follows
  the window: 1.0 up to about 2200 px, then up to 1.3. Setting it by hand turns that off,
  and View › Text size › auto turns it back on.
- **The decode filters and dock tabs get the foot lamp too,** not only the AVG segments
  and settings choices.

What this review can't judge: the waterfall *content*. The shots harness feeds synthetic
diagonal stripes, so the waterfall's color, contrast and palettes aren't assessed here. I also
didn't see hover or press states, or motion, because the renders are stills.

## Verdict

The design reads as one instrument. Since the November 2 review the plate, glass keys, LCD
faces, the slice flag, the band ruler in the masthead and the token system (`theme::ty`, the
`R_*` radii, the `LIP_*` lips) have made it hang together. The data honesty is still exemplary:
every empty state says what's missing and why. Accessibility is better than most egui apps.
The house keys report their state to AccessKit, focus draws on the key, and motion follows the
desktop's setting.

Four things keep it from feeling finished:

1. **Amber means too many things.** The theme says amber is the receiver's own accent: the
   VFO, the passband and the trace. In practice it also fills every "on" key. In the 1600 px
   NFM shot I count about 17 lit amber keys outside the LCD, so the receiver's color no longer
   points at anything.
2. **The waterfall gets less than half the window.** The waterfall is your first priority, but
   247 px of fixed chrome sit above it. It gets 47% of the height at 1600×980 and only 37% at
   1280×800.
3. **Everything is set the same way.** Almost every label is JetBrains Mono in caps at 10 to
   11.5 px. Hierarchy comes from color alone, and the few sans headings look out of place.
4. **A short list of real defects.** The worst is that the LCD's cell captions don't share a
   baseline.

## 1. Defects (fix first)

1. **The LCD captions sit at two heights.** RX, FREQUENCY, AUDIO A and MODE are at y≈73.
   SIGNAL, FILTER and STEP are about 5 px lower, at y≈78 (see `09-` and `10-lcd-*-2x.png`).
   The cause is that SIGNAL, FILTER and STEP share a horizontal row with something taller
   (the 22 px S-reading, the shape key, AUTO), and egui centers the caption vertically in that
   row.
   - Where: [bar.rs:246](../crates/desktop/src/bar.rs:246), [bar.rs:868](../crates/desktop/src/bar.rs:868)
     and the signal row near [bar.rs:1222](../crates/desktop/src/bar.rs:1222).
   - Fix: give every cell a fixed caption strip (one height, top-aligned), and put the
     right-hand items (AUTO, SHARP▾, CAL) in that strip at caption size. The S-reading moves
     down to the meter's line.
   - On an instrument whose whole idea is "one LCD", this is the most visible flaw.

2. **The decode log says FT8 has no decoder.** In FT8 (and in any data mode not listed),
   the DECODES tab on ALL shows: "This mode has no decoder: pick a digital mode…". Meanwhile
   the LCD says DATA and "DECODING · 8 heard" (`04-ft8-1100.png`). The match in
   [dock.rs:302](../crates/desktop/src/dock.rs:302) only lists AUTO and the VHF digital modes.
   - Fix: use the HF-data wording for any mode where `is_data` is true. For example: "FT8
     decodes are under HF DATA; this list holds frames from the other decoders."

3. **The step reads "6.25k Hz" and "100k Hz".** `fmt_short_hz` already adds the k, and
   [bar.rs:256](../crates/desktop/src/bar.rs:256) adds "Hz" after it.
   - Fix: split the prefix into the unit, so it reads `6.25 kHz` and `100 Hz`.
   - The memory row (`146.04 NFM`) and the filter presets (`2.4k`) can keep their short form,
     because they have no unit beside them.

4. **The recorder says the same thing twice.** With no buffer, "FT8 keeps no buffer"
   appears both in the header line and centered in the plot
   ([panels.rs:261](../crates/desktop/src/panels.rs:261), [panels.rs:716](../crates/desktop/src/panels.rs:716)).
   Keep the plot's copy and leave the header to the OFF tag.

5. **The empty spectrum is half drawn.** The "waiting for the first spectrum frame" gradient
   fills the top 40% of the well and stops before the axis gutter. The waterfall half below it
   is bare black. This shows at every size and is clearest at 2560 px (`05-b-2560.png`).
   - Fix: fill the trace area out to the axis, then draw the waterfall area as an empty
     waterfall with its time axis.

6. **The startup toast covers working controls.** The "Press ? for the shortcuts…" toast
   sits at the bottom right, over the scope. At 1280×800 it also covers the recorder's SAVE
   keys. It shows in every render.
   - Fix: anchor toasts to the bottom edge of the spectrum well, which is the one area that
     is always there and never holds a control. Or put them in the status bar's empty middle.

7. **Exclusive choices are announced as checkboxes.** Mode chips, OFF/LOW/MED/HIGH and the
   filter presets go through `chip_key_named`, which reports `WidgetType::Checkbox`
   ([widgets.rs:411](../crates/desktop/src/widgets.rs:411)). A screen reader then says "NFM,
   checkbox, checked" for a set where only one can be chosen. Pass `RadioButton` from the
   call sites that build exclusive rows.

## 2. Amber: give it back one meaning

Here is what is amber in the 1600 px NFM shot:

- the whole LCD
- the A key and the recalled memory
- the NFM mode key
- AVG **OFF**
- PEAK, FILL, SPOTS and BAND PLAN
- AGC and IF AGC
- LO 150k and IQ CORR
- BLANKER **OFF**
- the DECODES tab and the ALL filter
- PASSBAND and 10 Hz
- the slice flag, the passband and the cursor

Two of those are "OFF" choices lit like "on". When everything is amber, the receiver's own
marks (the flag, the passband, the VFO) lose their pull. A WARN-lemon BIAS-T beside amber
keys is also hard to tell apart.

What I propose, which needs no new colors:

- **Amber means receiver A.** Keep it for the LCD, the slice, the passband, and the keys
  that *set the receiver*: the mode, the recalled memory, and the A/B selector. Violet
  already plays this role for B.
- **Plain toggles light in steel.** For on/off display and processing switches (PEAK, FILL,
  SPOTS, BAND PLAN, AGC, IF AGC, LO, IQ CORR), make "on" a brighter cap plus the key's tally
  LED. `key()` already takes a `tally` color, so this is a choice at the call sites, not a new
  widget.
- **Selected tabs and segments get a lit underline, not a fill.** This covers dock tabs,
  filters, AVG, 5/10/25 Hz and OFF/LOW/MED/HIGH. A selected "OFF" then stops looking like
  something is on.
- **Caution keeps lemon** (BIAS-T, OVERLOAD). Once amber is rarer, lemon will read clearly.

This is the biggest change to how the app looks for the least code. Try it on the display bar
first and compare screenshots.

## 3. Vertical space: give the waterfall the height

Fixed height above the spectrum well at 1600×980:

| Item | Height |
|---|---|
| Masthead | 50 px |
| LCD | 110 px |
| Key row (A/B, memories, mute, modes, tuning) | 37 px |
| Display bar (span, zoom, avg, overlays, range, style) | 37 px |

That leaves 463 px for the spectrum and waterfall, which is 47% of the window. At 1280×800
the spectrum trace is about 110 px tall and the waterfall about 150 px (`03-laptop-1280.png`).

There are two duplications to remove:

- **The mode is shown three times.** The LCD's MODE cell shows the big "NFM", a dim row
  `NFM AM USB LSB WFM DATA`, and then the key row below repeats `NFM AM USB LSB WFM DATA` as
  keys.
  - Fix: make the LCD's dim row the mode selector itself (touch a word to choose it, which is
    how the filter shape and SPAN LOCK already work), then drop the mode chips from the key
    row. That frees about 220 px of the key row.
- **The display bar duplicates context menus.** SPAN, the zoom keys, CENTER and AVG can move
  into the key row's freed space. PEAK, FILL, SPOTS, BAND PLAN, RANGE, STYLE and WATERFALL
  are view settings that rarely change while operating, and they're already in the
  right-click menu. They can go in a slim strip inside the top edge of the spectrum well,
  shown on hover or always at 60% opacity, the way the dBFS axis sits inside the well now.

Doing both gives the waterfall back about 37 px at every size, which is about 8% more
waterfall at 1600 and 12% more at 1280. This is the change that best serves "waterfall
first".

## 4. Typography: mono for measurements, sans for names

Today JetBrains Mono in caps does every job: LCD captions, key legends, panel headings (GAIN,
FRONT END), table headers, row labels (NOISE RED., BLANKER) and status. Instrument Sans only
shows up in prose (empty states, toasts) and in two scope titles, "Passband" and "FM
multiplex". There, sans bold beside mono keys looks pasted in.

The rule I'd adopt:

- **Mono** for anything that is a measurement or a legend printed on a control: figures,
  units, key caps, LCD captions. That's where the instrument character comes from, and it
  should stay.
- **Instrument Sans 600, sentence case, about 12 px** for the names of things: panel
  headings ("Gain", "Front end", "Noise and AGC"), side-panel row labels ("Noise reduction",
  not "NOISE RED."), and dock section titles. This gives the side panel a second voice, so
  the hierarchy no longer depends on how gray a label is.
- **Drop titles that repeat a key.** "Passband" sits right beside the PASSBAND|AUDIO segment.
  Let the segment be the title.

The type scale is otherwise sound. The 10 px floor holds, and contrast is fine on Carbon:
`text_3` on the plate measures 4.7:1 and `text_2` measures 7.6:1.

## 5. Layout details

- **The side panel has no shared label column.** Values start at different x positions: the
  NOISE RED./BLANKER segments at about +93 px, the IF FILTER dropdown at +73, DEVICE and PPM
  elsewhere (`11-side-panel.png`). Give each panel one label width, measured from its longest
  label, so every control starts on the same line.
- **The disabled RF GAIN row contradicts itself.** The label is dimmed (AGC owns the gain),
  but "auto · 0 dB" is at full amber. Dim the value too, or say who owns it in the label
  ("RF gain · set by AGC").
- **The masthead shows the address twice.** "RSP1A @ 172.16.0.216" sits at left and the
  connection chip shows `host:8073` at right. Keep the device name at left and let the chip
  own the address.
- **Wide windows stretch instead of composing.** At 2560 px the LCD cells grow in
  proportion. The S-meter becomes about 700 px wide, the filter graphic is stranded at the
  left of a 400 px cell, and every label stays at 10 px (`05-b-2560.png`).
  - Fix: cap the meter, mode, filter and step cells at their 1900 px widths, and give the
    extra width to the frequency figures and the band ruler.
  - Also consider setting `pixels_per_point` from window width (1.0 below 1900, 1.15 at 2560)
    when the View menu's text size is on Auto.
- **The dock map is squeezed by an idle scope.** With the map open, half the dock is a scope
  saying "no signal yet" (`08-map.png`), and the map's help line is cut off.
  - Fix: let the map, HFDL and other table views take the full dock width, with the scope
    folding to a small tab, at least while the scope has nothing to show.

## 6. Grounds and settings

- **Night squeezes the text levels together.** Mapping every color to its luma in red is the
  right idea, but measured contrast falls to about 4.6:1 for primary text and about 2.3:1 for
  `text_3`. That makes CAL, SPAN LOCK, axis figures and "level over the floor" fade out
  (`06-night.png`).
  - Fix: give Night its own ramp. Use a pure-black ground and remap luma with a lift on the
    low end, so the three text levels stay distinct, then apply the global dimming the eye
    needs. Dark adaptation needs the whole screen dim, not low contrast.
- **The Ground choice is crowded.** It is a segmented row about 680 px wide whose keys hold
  whole descriptions ("Carbon — brushed steel on cool blacks") in mono (`07-settings.png`).
  Make the keys say just Carbon / Ink / Night, with the description as one sans line under
  the row for the selected ground.
- **Settings still has stock egui chrome.** Its title bar is a centered sans "Settings" and a
  stock ×. The checkboxes are about 8 px squares with no check mark, and the unchecked state
  barely shows on the plate. Draw the title as a panel heading on the plate, and use the house
  `key` with a tally (or the LCD tag) for the three booleans.

## What I'd do, in order

1. Fix the defects in section 1. They're small, and item 1 is the most visible.
2. Narrow what amber means (section 2), checked with before-and-after shots at 1600 px.
3. Take back the vertical space (section 3).
4. Apply the type rule to the side panel and dock headings (section 4), which also fixes the
   label column in section 5.
5. The wide-window, Night and settings items as they come up.

Section 3 changes how the window is operated, not just how it looks, so I'd mock it from a
live capture before building it.

## Second pass, 2026-11-28

Looked at the same GPU renders again, and this time at a muted QA window on the live daemon
(FT8 on 20 m, 8 MHz span, 1600×980), so the waterfall, the FT8 view and a full memory row
were real. First read is right: the amber frequency draws the eye, then the trace and
waterfall, and the purpose is clear in a glance. What was found and changed:

| Finding | Severity | Change |
|---|---|---|
| The scope's two foot readouts ran into each other in a narrow dock ("level -14 dB.0 ms/div" at 1100 px). | Moderate | The time base is drawn only where it clears the level. |
| Memory keys cut their own figures at 1100 px ("146.04." under the next key, the free slot over slot 3). | Moderate | A slot is never narrower than its number and frequency; fewer slots sit in a narrow row and share its width, the rest go to the +N menu. |
| Memory names that said nothing: the frequency again ("99.1 99.1", "7.241 7.2…") or one or two letters and an ellipsis. | Minor | A name that repeats the frequency, or is cut to under three letters, is left off. The whole name stays in the hover text. |
| The side panel still had two voices for names: fader names in mono caps (RF GAIN, IF AGC TARGET) beside sans row labels (IF filter, Noise reduction). | Minor | Stacked faders are named in the sans, sentence case, here and in the Display menus (Zoom, Floor, Ceiling, Depth, Black level, Contrast range). Readings stay mono. |
| Sentences under controls were set in lowercase mono. | Minor | Panel notes are sans sentences ("Noise reduction, notch and AGC act on NFM, AM, USB and LSB."). Text on an LCD face stays mono. |
| Status bar readings each had their own form: "25 fps", "834 KiB/s" with no name, "API 1.0 ms", lowercase "audio". "STREAM 26 drop · 0 gap" read as jargon. | Minor | One form, a dim name and a brighter figure: FPS, NET, API, AUDIO. The stream lamp says "STREAM 26 DROPPED"; gaps and lost samples are in its hover text. |
| A toast lay across the Settings window's rows. | Minor | While Settings is open, toasts sit at the foot of the window. |

Looked at and left alone:

- **Lit steel switches** (LO, IQ CORR, IF AGC) are told from unlit ones by a brighter cap and
  rim only. It reads on Carbon and the "3 ON" count backs it up; a tally lamp on each would
  bring back the clutter the last pass removed.
- **Caps mono legends and "A · B" readings** are the instrument's idiom here, chosen on
  purpose, and they stay on keys, LCD captions and readings.
- **The waterfall's share** (about 225 px of 980 with a 230 px dock) is the dock height the
  user set; the dock is resizable.
- **Night**, the grounds, Settings' layout, the HF views and the map held up.
