# HF monitoring station: build plan

Drafted 2026-11-08. Phases 1 to 5 are deployed as of 2026-11-09, with rows uploading to arm2. The site went public the same day.

## Goal

Turn the receiver into an unattended HF monitoring station with a public
logbook. The daemon decides what to listen to, keeps a permanent record of what
it heard and when it was listening, and publishes that to a site with history,
pictures and zoomable SVG maps. Claude comes last, as a planner that writes the
monitoring schedule.

**In scope:** FT8, FT4, WSPR, JS8, HFDL, DSC, SELCAL, and WEFAX and SSTV
pictures. NAVTEX, SITOR and RTTY can follow. DSC is the ship-position layer; AIS
is VHF and stays out.

**Out of scope:** pagers, digital voice, and anything above 30 MHz. The uplink
enforces this with a default-deny allowlist, so a new decoder cannot leak into
the public log by accident.

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Who owns the radio | The schedule, except when you are using it | One tuner and one decoder; touching a control pauses the schedule |
| Where the schedule runs | Daemon, CT 216 | The desktop may be closed |
| Where history lives | Short-term on CT 216, permanent on arm2 | CT 216 has 2.5 GB free; arm2 has 25 GB |
| Host | arm2, beside `dx.innerpulse.net` | Same subject, same deploy pattern; a job every few minutes would stack on the stats cron bursts on arm1 |
| Site backend | Python standard library and SQLite | Matches `dx-fcc-api`; arm2 has no PHP or Rust toolchain |
| Serving | Pre-built JSON slices, no per-request SQL | Same rule the stats site uses; keeps the host quiet |
| Name | `hf.innerpulse.net` | Free as an A record; confirm no other record type uses it |
| Site source | `site/` in this repo | The upload format and the daemon change together |
| API key | Desktop only, never on CT 216 or arm2 | The daemon has no authentication |
| Storage | SQLite on arm2, no remote database | Small single-writer workload; a remote database adds a network dependency and credentials for no gain |
| Station identity | Anonymous, no callsign anywhere public | Your call; can be revisited later |
| Certificate | Let's Encrypt through certbot | Same as the other sites |
| Launch | Public once Phase 5 passes | Your call |

## What the code gives us

- `Reporter::run` (`crates/server/src/main.rs:453`) already subscribes to every
  decode whether or not a client is connected. The station log uses the same
  hook.
- `spot_of` and `text_of` in `pskreporter.rs` already turn decodes into call,
  grid, frequency, SNR, mode and time. Its 5-minute and 1-hour limits are upload
  etiquette, so the log must capture before them.
- `hfdata.rs` runs **one** decoder on receiver A. `sub_rx.rs` has no decoders.
  Monitoring is time-shared until that changes.
- `post_sdr_mode` saves the mode to settings (`main.rs:1088`). The scheduler must
  call `SdrHandle::tune` and `set_mode` directly, so a hop never overwrites your
  settings.
- `hf-directory.json` holds per-chart WEFAX times, frequency windows by UTC, and
  HFDL ground stations with coordinates. It is the timetable.
- `scan.rs` is the model for the scheduler: a clock-injected state machine that
  returns actions and is testable without hardware.
- HFDL events carry flight, position, ICAO and ground station. DSC carries MMSI,
  category, nature and lat/lon when sent. SELCAL carries only a code.
- WEFAX rows are held on the server. SSTV rows are only streamed to clients. PNGs
  are currently made by the desktop (`fax.rs`, `sstvview.rs`).
- The daemon has no TLS client. `wsprnet.rs` is plain HTTP.

## Shape

```text
 CT 216 (daemon)                         arm2
 ┌────────────────────────────┐          ┌────────────────────────────────┐
 │ schedule.rs ──tune/mode──► radio      │ hf_api.py  127.0.0.1:8767      │
 │ decoders ──► stationlog.rs │  HTTPS   │   SQLite (outside web root)    │
 │              SQLite, PNGs  ├─────────►│ warm timer ──► JSON slices     │
 │ uplink (queue + cursor)    │  batches │ Apache ──► static site + JSON  │
 └──────────▲─────────────────┘          └───────────────▲────────────────┘
            │ plan JSON                                  │ proxied A record
 usdr-desktop (Claude planner, key)              Cloudflare ──► visitors
```

## Phase 1: station log (daemon)

New `crates/server/src/stationlog.rs`, with `rusqlite` (bundled) in the daemon.

- Subscribe like the reporter. Keep only `valid` events, under 30 MHz, on the
  allowlist. Move `spot_of` and `text_of` somewhere both modules can use.
- Write on a dedicated thread behind a bounded channel so the radio loop never
  waits. Count overflow and broadcast `Lagged` drops, and record them as gaps.
- Add the **listening log**. Without it an empty map cannot be told apart from a
  radio that was elsewhere.

```text
observations(id, at_utc, dial_hz, freq_hz, mode, kind, ident, grid,
             lat, lon, loc_kind, snr_db, public_json, private_json)
listening(id, start_utc, end_utc, dial_hz, mode, cause, noise_dbfs,
          peak_iq, decodes, valid_decodes, healthy)
gaps(id, at_utc, kind, dropped)
```

The `pictures` table arrives with Phase 3 and `healthy` with Phase 2, each as a
schema migration, so nothing sits empty until it has a writer.

`loc_kind` is one of `reported` (HFDL, DSC), `grid4`, `grid6`, `ground_station`
or `none`, so nothing is drawn more precisely than it was received. `private_json`
holds ACARS free text and never leaves CT 216. `cause` is `schedule` or `operator`.

- Anonymity starts here. `ident` is the sending station only, and any observation
  from the station's own callsign is left out. FT8-style message text can name the
  station it is answering, so raw amateur message text goes in `private_json`, never
  `public_json`.
- Read routes: `/api/station/summary`, `/observations`, `/listening`.
- Retention on CT 216: 14 days of rows, pictures until uploaded plus 7 days, and
  a hard size cap that deletes oldest first.

**Done when:** an hour of FT8 matches the decode log row for row, data survives a
daemon restart, drop counters stay at zero, and memory stays well under the unit's
512 MB limit. Deploy with `tools/deploy-server.sh` and confirm with `--probe`.

## Phase 2: scheduler (daemon, desktop controls)

New `crates/server/src/schedule.rs`, clock-injected like `scan.rs`. The daemon
reads `hf-directory.json` with `include_str!`.

Priority, highest first:

1. Operator hold: any client POST to tune, mode or rate pauses the schedule for
   a set time, and the radio stays where you left it.
2. A picture in progress is never interrupted.
3. WEFAX reservations from the timetable, with lead time for the phasing signal.
4. Health checks.
5. Planned slots, then probes.

| Group | Modes | Rule |
|---|---|---|
| Timetabled | WEFAX, NAVTEX | Calendar; choose the frequency by UTC window and past copy |
| Continuous | HFDL, FT8/FT4, WSPR, JS8 | Short probes are meaningful; rank by yield per frequency and hour |
| Sparse | DSC, SELCAL | Probes prove nothing; long dwells; judged only over weeks |

- **Antenna:** the 1000 ft loop receives across HF, so no band is ruled out up front.
  Time of day and measured yield decide. It also delivers strong signals, so a slot
  where `peak_iq` shows clipping counts as unhealthy, and gain settings are recorded
  per band.
- **Scoring without AI:** each mode has a share of the air. Within it, the frequency
  comes from a yield table by mode, dial and hour of day over the 14 days the log
  keeps, with every fifth pick rechecking the least-heard frequency.
- **Health:** every slot records noise floor and `peak_iq`. A regular check on a
  known signal (WWV, frequency chosen by hour) catches a stranded tuner. Failed
  slots are marked unhealthy and left out of all statistics, and repeated failure
  is reported in status.
- **Protocol:** add `schedule: Option<ScheduleStatus>` to status. This is a `proto`
  change, so the desktop is rebuilt with it.
- **Desktop:** an HF WATCH view in the Scan tab with the on/off switch, what is
  running and what is next, and a notice when the schedule takes the radio back. A
  sixth dock tab would squeeze the scope at the smallest window size.

Check during implementation that opening the desktop does not post a tune or mode
by itself (`app.rs:542`), or every launch would count as an operator hold.

**Done when:** state-machine tests cover holds, reservations and pictures in
progress; a 72-hour soak with manual interruptions leaves no undetected dead
periods; and `tools/tune-roundtrip.py` passes after the soak.

## Phase 3: pictures on the daemon

- Keep SSTV rows server-side as WEFAX rows already are, and write PNGs when a
  picture ends so nothing depends on the desktop.
- Quality gate: minimum line count plus a noise measure, so partial and static
  frames are not kept.
- Name WEFAX charts from the directory by station and UTC time, labeled as
  scheduled, since the picture does not state its own title.

**Done when:** a timetabled WEFAX chart and a live SSTV picture both arrive as
correct PNGs with no client connected. This touches `hfdata.rs`, so run
`tools/hf-live-check.py` on both.

## Phase 4: uplink and backend (CT 216, arm2)

**Uplink.** A `ureq` client with rustls on a blocking task, queueing the way
`wsprnet.rs` does. Gzipped JSON batches about once a minute. Observation ids make
uploads idempotent, and the cursor advances only on success. Token is generated
on the host and stored with the unit's credentials, never in the repo or terminal.
Batches identify the station by a generic id, never a callsign.

**arm2.**

- `~/www/hf` from `site/`, deployed by a new `tools/deploy-site.sh`.
- `bin/hf_api.py` on `127.0.0.1:8767` (confirm free; 8765 and 8766 are dx's) with a
  memory-capped systemd unit modeled on `dx-fcc-api`.
- SQLite in WAL mode outside the document root.
- A timer every few minutes, at low priority, writes JSON slices by time window
  and mode. Pages never query SQLite.
- Raw rows for a year, hourly rollups indefinitely, full pictures for a year and
  thumbnails indefinitely.

**Storage.** Measured on the live log: about 134 bytes a row, and 159 FT8 rows a minute
on 40 m at night. That is about 30 MB a day if FT8 ran around the clock, so real use
will be well below it. Pictures are a guess of up to 25 MB
a day. Against 25 GB free, SQLite on arm2 is enough. Oracle Autonomous Database and
MySQL HeatWave are available but not used, because they add a network dependency
and credentials, and pictures, the larger share, wouldn't go in them anyway.

Alert when arm2 falls below 8 GB free. Then, in order: shorten full-size picture
retention, move pictures to R2, and only if rows are still the problem, archive raw
rows older than a year to Oracle. The hourly rollups stay small either way.

**DNS.** `hf.innerpulse.net` → `193.122.136.83`, proxied. arm2 accepts ports 80 and 443
only from Cloudflare, so a DNS-only stage cannot work: Let's Encrypt reaches the webroot
through the proxy, and that path answers even before the origin has a certificate. Cloudflare
does not cache JSON by default. A Cache Rule for the slices is worth adding at launch; picture
files are immutable and can cache for a long time.

**Access.** `/etc/apache2/hf-access.conf` on arm2 holds `Require all granted` since launch
on 2026-11-09. To close the site again, list addresses there and reload Apache; use a /64 for
IPv6, because a home computer's temporary address rotates. Ingest stays open to the token.

**Done when:** a day of data arrives with no gaps or duplicates, a forced outage of
arm2 catches up by itself, and ingest rejects bad tokens, unlisted modes and
anything above 30 MHz.

## Phase 5: the site

Static HTML, CSS and JS on the innerpulse brand kit in Carbon, copied into `site/brand`,
linked with `dx.innerpulse.net`.

**Pages:** Now, Map, Activity, Traffic (aircraft and ship calls), Pictures, Log with
station lookup, and Station (how it works, what was monitored, and noise by band).

**Map.**

- SVG, with Natural Earth 110m, 50m and 10m simplified at build time by a new
  `tools/gen-site-map.py`, swapped in by zoom level. No tile service.
- Wheel zoom, drag to pan, drag a box to zoom to an area, presets (North America,
  Caribbean, South America, Europe), and the view kept in the URL.
- Two projections: Miller with filled land, and azimuthal centered on the station,
  where distance and bearing read directly.
- **One dot per 4-character square, at the square's center.** Size is spots in decades,
  color is one of three band groups (the most the brand's chart rules allow where marks
  can touch). Close in, each square's true extent is drawn, so a dot never claims more
  precision than a square. Only positions a decoder actually reported get a point of
  their own.
- Layers: amateur squares, HFDL tracks and ground stations, DSC positions, great
  circle paths, day/night line.
- HFDL tracks are ordered by each fix's own `fixUtc`, never by `at_utc`: aircraft
  queue reports and send several at once, so the time heard can run backward along
  a route. Rows from before 2026-11-09 05:56 UTC have no `fixUtc` and get no track.
- The viewport is the filter: charts and totals recompute in the browser from
  loaded JSON, so area zoom costs the server nothing.
- Time scrubber over 24 hours and 7 days. Keep to about 2,000 SVG nodes per view.
- No text over marks, and honor `prefers-reduced-motion`.

**Activity.** Band-by-hour heat maps normalized by time monitored, stations per
monitored hour, farthest stations, new squares, SELCAL log, and band openings
defined as a rise over that band's own baseline.

**Data honesty.** "Not monitored" looks different from "quiet". Every number comes
from the log. Stale data says how old it is. Empty states say so; no sample data,
ever. A loop this long has a lobed pattern on the higher bands, so a region with no
spots doesn't prove the band was closed to it. The Station page says so.

**Before going public:**

| Content | Rule |
|---|---|
| HFDL | Flight, position, ground station. Never ACARS free text |
| DSC | Published as logged, with no delay; the Station page says it is not a 24/7 distress monitor |
| SSTV | Held for approval by default; anyone can transmit any image. Approved or declined at `/station/pictures` on the daemon, which only the LAN can reach |
| WEFAX | Every chart that passes the quality check publishes automatically |
| Station identity | No callsign in pages, JSON, picture names or site source |
| Station location | 4-character grid at most; the map and paths center on that square |
| SSTV pictures | Review for the station's own callsign as part of approval |

The legal side of repeating non-broadcast traffic deserves a check before launch.

**Done when:** map and charts work at 1440 and 390 px wide with real data, and the
publication rules are confirmed against the live API output, including a search of
every public file for the station's callsign. The site goes public at this point.

## Wideband: several decoders from one capture

Added 2026-11-09. Today the receiver runs one HF decoder and the schedule hops it
between dials, so most of the 8 MHz span feeds only the waterfall. On the first day
40 m FT8 got about 2.4 hours of air and the map covered 11 hours of 24. One capture
can carry many of the digital channels at once; the schedule would then choose a
slice of spectrum instead of a single frequency.

Takes effect after Phase 5 and before Phase 6. The Claude planner should reason about
slices, and yields measured while hopping are not directly comparable with these.

| Slice | Center | What fits inside it | Span of the amateur channels |
|---|---|---|---|
| Low (night) | About 6.9 MHz | FT8, FT4, WSPR, JS8 on 80, 40, 30 m; DSC 4, 6, 8 MHz; HFDL 5.5, 6.6, 8.9, 10 MHz; SELCAL; WWV 5 and 10 MHz | 3.57 to 10.14 MHz |
| Middle (day) | About 17.6 MHz | FT8, FT4, WSPR, JS8 on 20, 17, 15 m; DSC 16 MHz; HFDL 15 and 17.9 MHz; WWV 15 MHz | 14.07 to 21.14 MHz |
| High (openings) | About 24.6 MHz | FT8, FT4, WSPR, JS8 on 15, 12, 10 m | 21.07 to 28.18 MHz |

An 8 MHz capture reaches about 4 MHz either side of center, less the filter's roll-off.
DSC 12 MHz, HFDL 11.3, 13.3 and 21.9 MHz, and FT8 on 17 m together with 10 m do not fit
in these slices, so they stay on hops or wait for a turn. The slices may move once real
yields are in.

**Stage 1 measured, 2026-11-09.**

- **Filters:** the RSP1A datasheet lists separate 2 to 12 and 12 to 30 MHz filters, so no slice
  may cross 12 MHz. Usable width at an 8 MHz span is 7.16 MHz, 3.58 either side of center.
- **Overload:** the receiver has been at an 8 MHz span throughout, so the soak already measured
  this. 13 hours, 03 to 16 UTC, on every band: no stay clipped and the API reported no overload.
  The worst peak was 0.93 of full scale on 14.08 MHz, typically 0.15. Evening hours, when
  broadcasters are loudest, have not been seen yet.
- **CPU:** decoding about 36 FT8 messages from one 14 MHz slot costs 0.3 to 0.5 core-seconds, in
  a burst under a second long, against a 0.145-core baseline on CT 216's two cores. Nine channels
  are about 4 core-seconds per 15 s slot. CPU is not the limit, though channel filters at 8 MS/s
  are still to be measured.

**Stage 2 done, 2026-11-09.** FT8 channels on 80, 40 and 30 m run from the span
(`crates/server/src/wideband.rs`), set by `USDR_WIDE_FT8` and `USDR_WIDE_LOG` in a systemd
drop-in on CT 216. On 15 shared daytime slots they matched the main decoder on all 70
stations it heard, plus 3 more. Three channels cost 0.12 of a core with no dropped blocks.
A channel runs whenever the tuner happens to cover its dial, and its rows are marked
`wideband`. Busy evening slots have not been compared yet.

**Stage 3 in progress, 2026-11-09.** Channels now take any mode, listed in `USDR_WIDE` as
`FT8+7074000` (logging) or `FT4?14080000` (shadow), on two worker threads. Compared with the
main decoder on the same dial and time: FT8 404 of 416 stations with 14 extra, DSC 9 of 9 with
1 extra, HFDL 31 of 33 with 2 extra, WSPR 20 of 22 with 7 extra. Those four modes log on 39
channels' worth of dials at about 0.7 of a core with no dropped blocks.

**Stage 4 live, 2026-11-09.** The schedule holds a slice of spectrum for 15 minutes, on the
side of the 12 MHz filter split that suits the sun at the station's grid: above it by day,
below it at night. FT4 and JS8 are off the channel list, leaving 29 channels for FT8, WSPR,
DSC and HFDL. First 20 minutes on the day side: 785 distinct FT8 stations, no dropped blocks,
no clipping, no restarts. The first night on the low side is still the real overload test.

**All-day spectrum, 2026-11-09.** `crates/server/src/overview.rs` keeps one row a minute on a
fixed 10 kHz grid, the 98th percentile of ten short FFTs a second. Rows go up with the batch;
arm2 keeps 15 days and draws the last 24 hours for the Activity page, as height over the local
noise. Frequencies the tuner did not cover stay hatched. The contrast still needs tuning on a
full day of real rows.

**Review pass, 2026-11-09.** Measured on 147 FT8 slots recorded off 20, 17 and 15 m
(`cargo run --release -p scannerd-engine --example ft8_corpus <dir>`; `USDR_FT8_DUMP` now
records the wideband channels too):

- The slot decoder searches up to four times, each after the last one's decodes are taken
  out, each search with its own share of the time. Candidates belief propagation cannot
  settle are tried as CQs with a CQ's fixed bits given, and by an ordered-statistics decoder,
  both screened: a standard message between whole callsigns, few contradicted bits. 3712
  decodes before, 4394 after (+18%), and none from the same slots played backwards.
- Leaving repeats of a strong candidate out of the list decoded 1% fewer, and the soft
  bits' scale made no difference from 14 to 40. Neither was changed.
- The bounded pool exists: every slot decoder shares it, at lowered priority, with a budget
  from what is left of the slot. A channel whose last slot is still decoding is counted as
  dropped, as before.
- Channels take the span in 4096-sample tiles and share the corrected block instead of
  copying it. With AVX2 (`-C target-cpu=x86-64-v3`, set in `tools/deploy-server.sh`) the two
  workers went from 43% to 28% of a core on the night slice, back to back on CT 216. Fat LTO
  doubled their cost and is not used.
- Dials within 60 kHz of each other (each band's FT8 and WSPR pair) share one mixer and the
  first halvings, then a short chain each. The two workers went from 28% to 21% of a core,
  back to back on the night slice; they were at 43% before this pass.
- DSC: a call with exactly one unreadable character has it filled from the ECC, when that
  is also the code word the character's own soft bits favor most. The call says so
  (`recovered` in its fields, and in its summary in place of "ECC ok").
- Not done: WSPR ordered-statistics decoding and a third WSPR pass (the spots go to
  wsprnet, and there is no reference decoder here to check new ones against), and JS8
  subtraction (off the channel list).

**Stage 5: more channels, 2026-11-10.** Added to `USDR_WIDE` on CT 216, cheapest first, 50
channels in all, every one logging:

- **SELCAL** on 5598, 6628, 8825 and 8906 kHz, inside both night slices. 11396 and 13306 kHz
  are in no slice (the night slices reach up to 10.54 MHz, the day slice starts at 13.93) and
  stay on hops. In its first 15 minutes the 8825 channel heard three aircraft (DE-LQ, PS-AJ,
  FS-JL) while the main decoder on 8906 heard none, and the codes were on the site's SELCAL
  list with the next batch.
- **FT4** on 80, 40, 30, 20, 17, 15, 12 and 10 m. As shadows against the main decoder on
  14.080 MHz for 76 slots: all 76 of its stations and one more, 881 of 902 messages in the
  same slot, most of the rest a slot apart on one side or the other. Logging since. A logging
  channel's FT4 decodes go to PSKReporter as FT8's do.
- **60 and 160 m:** FT8 and WSPR on 5357 and 5364.7 kHz, and WSPR on 1836.6 kHz, which only
  the 5.4 MHz slice reaches.
- **Radiofax**, one frequency per station a slice holds: NMF 6340.5, DDK3 7880 and NMG
  8503.9 kHz at night, NMG 17146.4 kHz by day. The stations send on several frequencies at
  once; with a channel on each of them the first afternoon filed NMF's 16:00 chart twice.

CPU on the 6.855 MHz slice, 180 s each, back to back, the main decoder on SELCAL:

| Channels | Wideband workers | Slot decoder pool | Dropped blocks |
|---|---|---|---|
| The 31 from Stage 4 | 0.25 core | 0.08 | 0 |
| + SELCAL, FT4, 60 m and 160 m (46) | 0.34 | 0.17 | 0 |
| + radiofax (50, three of them in this slice) | 0.38 | 0.18 | 0 |

A radiofax channel costs about 0.015 of a core. On the 24.62 MHz day slice the workers went
from 0.125 to 0.16 of a core and the slot pool from 0.23 to 0.41: FT4 decodes every 7.5 s.

**Charts on channels.** WEFAX is a channel mode (`WEFAX+9108100`, the dial 1.9 kHz below the
listed frequency). A channel starts a chart on its own tone and files it like the main
receiver's. When a timetabled chart's frequency is a channel inside the held slice, the
schedule leaves the chart to it: the main decoder carries on, and the slice stays until the
chart's time is up, or longer while the picture is still coming in, up to the picture overrun.
A radio check or another chart that would move the span waits. A slice chosen in the 15
minutes before such a chart is the one that holds it, and when the chart comes due in
another slice on the same side of the split, the slice moves to it rather than the main
decoder going to the chart, unless that would cut off a picture or a radio check. The
receiver's own radiofax path was checked after the change on DWD's 16:36 surface analysis,
1296 rows, filed against its timetable entry. The first chart a channel received
was NMF's 16:00 iceberg analysis, 920 rows, untimetabled, which the picture check passed
(row correlation 0.70). The first evening showed a gap: DWD's 18:00 chart on 13880.6 kHz,
outside every slice, held the main receiver until 18:19:38, and NMG's 18:20 chart, due a
second later with a channel on 17144.5 kHz, went to the main receiver on 8502.0 kHz instead,
which heard no start tone. The finished chart still counted as the slot in progress. A chart
whose picture is over, or never began, no longer holds a chart back from its channel.
After the fix, DWD's 22:00 surface analysis went to the 7878.1 kHz channel and was filed
against its timetable entry (1296 rows), and the 6338.6 kHz channel filed two NMF charts
the same evening.

**NAVTEX.** The directory lists only 518 kHz, which no slice reaches, so NAVTEX is a
timetabled main-receiver visit rather than a channel: dial 517.0 kHz, 30 s before the
station's slot to 11 minutes after it, with all channels dark meanwhile. Boston, Chesapeake
and Yarmouth take turns, one per 4-hour cycle (Boston 00:50 and 12:50 UTC, Chesapeake 06:10
and 18:10, Yarmouth 11:20 and 23:20), so six visits, about an hour a day. Charts win: a
chart on the main receiver goes first, a channel chart in the slice ends a visit, and a
visit is passed over while a channel is receiving a chart. The log keeps each message by its
id with its text (up to 4000 characters); the site lists each message once from its best
copy. The first two visits on 2026-11-10 copied nothing: a manual one 22:38–22:52
(Sydney NS and San Juan slots), noise -81 dBFS, and the schedule's Yarmouth visit
23:19–23:31, noise -84 dBFS. Boston's 00:50 visit, the nearest station, copied nothing either
(-84 dBFS), so the visits came out of the schedule on 2026-11-11: the receiver's reach below
1 MHz is unproven, and an hour a day of dark channels bought nothing. The log and the site
still take NAVTEX, so a message copied by hand is kept and listed; the site's section stays
hidden until one is. Bringing the visits back is a revert of the schedule part of 479a74d,
once something has shown 518 kHz arriving.

**JS8, 2026-11-11.** JS8 channels on 1842, 3578, 7078, 10130, 14078, 18104, 21078, 24922
and 28078 kHz, the JS8Call defaults; a slice holds three of them. On the 5.4 MHz night slice,
180 s each, back to back, with the main decoder on DSC 2185.8 kHz: the wideband workers
stayed at 0.39 to 0.40 of a core and the FT8/FT4 slot pool at 0.29 to 0.30, and the JS8
decoder's own thread added 0.13 for its three channels. No dropped blocks. In the first
14 minutes the 7078 kHz channel logged 348 frames and 3578 kHz 23. A 10-minute side-by-side
on 7078 kHz had the main receiver copy 62 frames, mostly Normal speed, some Fast.

**NCDXF/IARU beacons, 2026-11-11.** Eighteen beacons take turns on 14.100, 18.110, 21.150,
24.930 and 28.200 MHz, ten seconds each, so the clock alone says which one is on. A
`BEACON` channel's dial sits 700 Hz below the frequency (`crates/engine/src/beacon.rs`,
`wideband.rs`). Per slot the detector finds the carrier within ±60 Hz, matches its
envelope against the scheduled callsign's Morse at 22 WPM over starts up to 1.5 s late,
and counts the call only when the match stands 6 standard deviations clear of the same
match in the noise 250 Hz either side and fits better than the other seventeen calls.
It then measures the dashes in turn (100 W, 10 W, 1 W, 100 mW); the steps heard are the
dashes heard in a row. Strength is over the noise in 2500 Hz, as FT8 reports it. Each
heard slot is one `BEACON` row at the beacon's locator. The Activity page shows the 18
by 5 grid for the last hour and how many of the 18 each frequency heard each hour of the
last day. The project's pages give no timing inside a slot, so the detector finds the
callsign's start and each dash's itself; `lateMs` in each row says where the call began.
Only the day slices hold the five frequencies (14.1 to 21.15 MHz in the 17.6 MHz slice,
21.15 to 28.2 MHz in the 24.6 MHz one), so the beacons are heard by day only. A 14.100 MHz
recording at 02:40 UTC heard nothing: 20 m was closed.

**What has to change.**

- **Channels from the span.** A narrow channel filter per decoder, one `HfDecoder` per
  channel. The AIS path in `sdr.rs` already takes both its channels from the span and
  is the model; `sub_rx.rs` has no decoders today.
- **FT8 and FT4 bursts.** Every channel's slot ends at the same instant. Decodes go through
  a bounded pool on CT 216's two cores, with a time budget per slot. A channel that
  misses its budget is counted and recorded as a gap, never silently dropped.
- **The schedule.** A slot becomes a slice plus a channel list. Timetabled charts run as
  one more channel when their frequency is inside the slice and otherwise take the radio as
  they do now. Operator holds and pictures in progress keep their priority.
- **Radio check.** WWV sits inside the low and middle slices, so the check can run
  continuously from a channel instead of costing a hop.
- **Log.** One `listening` row per channel, so rows overlap in time and yield per
  dial stays correct. A new column tells a slice's rows apart from hopped ones.
- **Site.** "Time on air" becomes channel-hours beside receiver-hours, and
  "decodes per hour on air" stays per channel. Both have to be labeled so the figures
  stay honest.
- **Uplink and backend.** Rows already carry dial and mode, so these should need no
  change beyond the new column.

**Stages.**

1. **Measure before building.** Confirm the RSP1A's front-end filter edges; from memory it
   switches at 12 MHz, which is why no slice crosses it. Then record clipping and noise floor
   at 8 MHz on each slice, day and night, on the real antenna, and bench three FT8 channels
   on CT 216 for CPU. If the low slice clips at any usable gain, narrow the slice or stop here.
2. **FT8 on 80, 40 and 30 m together**, behind a setting, with decode counts compared to
   hopped slots on the same dials.
3. **The other amateur modes and DSC** on the same capture, then HFDL.
4. **The schedule picks slices**, the log and site show overlapping channels, then the middle
   and high slices.

The 72-hour soak is not a gate for this or anything else (decided 2026-11-09). It runs
over the following days, and whatever it finds gets adjusted then.

**Done when:** a slice holds every planned channel for 72 hours with no clipped or
unhealthy stretches beyond what hopping shows, no channel misses its decode budget, FT8
decodes per dial per hour are at least what a dedicated hop gets, and the site's figures
say what they measure. Run `tools/tune-roundtrip.py` and `tools/hf-live-check.py` per
the working rules.

## Phase 6: Claude planner (desktop)

Only after a few weeks of Phase 2 data, so it has real yield numbers to reason from.

- `crates/desktop/src/analyst.rs` on its own thread, following `net.rs`. A direct
  POST to `/v1/messages`, as there is no official Rust SDK. Enable TLS in `ureq`.
- Model `claude-opus-5-5`. Forced `tool_choice` returns a 400 on it, so the plan comes
  back through `output_config.format`. Handle `stop_reason: "refusal"` and enable the
  server-side fallback.
- Inputs: the yield table, the directory, your goals in plain language, and
  `web_search` for schedule changes. `web_fetch` stays off.
- Tools: read yield data, read listening history, and run a bounded probe.
- Output: the same plan JSON the Phase 2 scorer produces. The daemon validates it
  against known modes and frequencies and rejects anything else, so Claude can never
  do more than you could from the UI.
- Decoded text is untrusted input. Anyone can transmit an instruction.
- Key from an environment variable or OS keychain, not `desktop.toml`. A
  Console key is billed separately from a Claude plan.
- Show the plan and the reasoning; you apply it.

Later uses can share this module: identify-a-signal, and captions for the site.

**Done when:** a generated plan beats the plain scorer on measured valid decodes per
monitored hour over two comparable weeks. If it doesn't, keep the scorer.

## Risks

| Risk | Mitigation |
|---|---|
| A stranded tuner looks like a quiet band | Per-slot health, known-signal checks, unhealthy slots excluded |
| Hopping exposes retune bugs | 72-hour soak and `tune-roundtrip.py` before anything depends on it |
| The log slows the radio loop | Separate thread, bounded channel, counted drops |
| CT 216 fills up | Hard size cap, oldest first |
| arm2 serves live sites | Low-priority warm job, memory cap, no per-request SQL |
| False decodes reach the public log | `valid` only; `DMR`-style noise false positives are out of scope, but apply the same rule to any new mode |
| Offensive SSTV image | Approval queue by default |
| Overload from the large loop | Clipped slots marked unhealthy; gain recorded per band |
| Anonymity is imperfect | PSKReporter uploads carry the callsign and locator, so a determined person could match them to the site's spots. Accepted for now; turning reporting off closes it |

## Settled 2026-11-08

1. **Ships:** DSC is the ship layer. Everything stays below 30 MHz.
2. **Antenna:** 1000 ft loop of 12-gauge wire; all HF bands are candidates.
3. **Identity:** the station stays anonymous for now.
4. **Launch:** public after Phase 5, with a certbot certificate.
5. **Storage:** SQLite on arm2, with the fallback order under Phase 4.
