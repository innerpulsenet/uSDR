# HF monitoring station: build plan

Drafted 2026-11-08. Phase 1 is built and Phase 2 is built and soaking.

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

**DNS.** Create `hf.innerpulse.net` → `193.122.136.83` as DNS-only, issue the
Let's Encrypt certificate with certbot as on the other sites, then turn on the
proxy. The zone is on SSL strict, so the origin certificate has to
be valid before proxying. Add a Cache Rule for the JSON slices, which Cloudflare
does not cache by default; picture files are immutable and can cache for a long
time.

**Done when:** a day of data arrives with no gaps or duplicates, a forced outage of
arm2 catches up by itself, and ingest rejects bad tokens, unlisted modes and
anything above 30 MHz.

## Phase 5: the site

Static HTML, CSS and JS in the innerpulse Carbon look with the shared masthead,
linked with `dx.innerpulse.net`.

**Pages:** Now, Map, Activity, Pictures, Log, and Station (how it works and what
was monitored).

**Map.**

- SVG, with Natural Earth 110m, 50m and 10m simplified at build time by a new
  `tools/gen-site-map.py`, swapped in by zoom level. No tile service.
- Wheel zoom, drag to pan, drag a box to zoom to an area, presets (North Atlantic,
  Caribbean, Europe, Pacific), and the view kept in the URL.
- Two projections: equirectangular, and azimuthal centered on the station, where
  distance and bearing read directly.
- **Stations are drawn as squares, not pins.** Grid fields at world zoom, 4-character
  squares when closer. Only positions a decoder actually reported get a point.
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
| DSC | Published as logged history, delayed, and clearly not a live distress feed |
| SSTV | Held for approval by default; anyone can transmit any image |
| WEFAX | NOAA charts automatically; other services only after checking their terms |
| Station identity | No callsign in pages, JSON, picture names or site source |
| Station location | 4-character grid at most; the map and paths center on that square |
| SSTV pictures | Review for the station's own callsign as part of approval |

The legal side of repeating non-broadcast traffic deserves a check before launch.

**Done when:** map and charts work at 1440 and 390 px wide with real data, and the
publication rules are confirmed against the live API output, including a search of
every public file for the station's callsign. The site goes public at this point.

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

## Later: several decoders at once

An 8 MHz span holds 7.074, 10.136 and 14.074 MHz together. Running one decoder per
channel would multiply coverage, at the cost of CPU on CT 216's two cores and
possibly dynamic range on HF. Worth measuring once the log shows what time-sharing
misses. With a loop this size, test for clipping at the wide span first.

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
