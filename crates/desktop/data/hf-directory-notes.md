# HF station directory: notes (fetched 2026-11-06)

`hf-directory.json` was built by `build_dir.py`, which parses the source files saved in `dl/`, so every entry traces back to a file that was fetched. Times are UTC (HHMM). Frequencies are in kHz as the source prints them.

## Sources (all fetched 2026-11-06)

| Section | Source | Source's own date |
|---|---|---|
| NMF, NMG, NMC, NOJ, KVM70 schedules | https://tgftp.nws.noaa.gov/fax/hfmarsh.txt, hfgulf.txt, hfreyes.txt, hfak.txt, hfhi.txt | NMF/NMC/NOJ: "effective Sep 04 2018, information dated Mar 16 2023". KVM70: effective Feb 11 2020. NMG: effective Apr 03 2012, information dated Dec 12 2024 (file modified 2026-03-18). |
| Cross-check, plus GYA, Canadian and DWD entries | https://www.weather.gov/media/marine/rfax.pdf (Worldwide Marine Radiofacsimile Broadcast Schedules) | March 7, 2025 |
| Radiofax block times, NIK ice patrol, USCG SITOR, NAVTEX weather times | https://www.weather.gov/marine/uscg_broadcasts | undated |
| DWD fax | https://www.dwd.de/EN/specialusers/shipping/broadcast_en/broadcast_fax_092023.pdf (German original sendeplan_fax_092023.pdf), linked from the live DWD broadcast page | file is "092023" |
| DWD RTTY | broadcast_rtty_1_092023.pdf and broadcast_rtty_2_092023.pdf on the same DWD page | "092023" |
| Canadian NAVTEX, Iqaluit DSC and NBDP | CCG Radio Aids to Marine Navigation 2026, Part 2: https://www.canada.ca/en/canadian-coast-guard/corporate/publications/radio-aids-marine-navigation/facilities-information.html | RAMN 2026 |
| DSC distress frequencies | RAMN 2026 general section: .../radio-aids-marine-navigation/general.html | page modified 2026-08-28 |
| VCO fax discontinued | RAMN 2026 ice and weather section: .../environment-climate-change-marine-ice-warning-forecast-programs.html | page modified 2026-10-30 |
| HFDL | https://github.com/szpajder/dumphfdl/blob/master/etc/systable.conf | table version 52, committed 2023-12-14 |
| WWV/WWVH | nist.gov pages: radio-station-wwv (updated 2023-07-20), radio-station-wwvh (updated 2026-06-22), wwv-and-wwvh-digital-time-code (format and tone table, updated 2024-09-03), broadcast-outages; NIST SP 432 (2002) http://tf.nist.gov/general/pdf/1383.pdf for station-ID minutes; NWS SCN 18-102 for the end of storm warnings | as listed |
| DRM | https://www.drm.org/broadcast-schedule/ (schedule JSON embedded in the page; B26 season, updated 2026-10-26); RRI's English winter page checked directly for 9570 kHz | 2026-10-26 |
| PACTOR | 47 CFR 97.221 through the eCFR API | current |
| STANAG 4285 | https://www.sigidwiki.com/wiki/STANAG_4285 (community wiki, not an official source) | — |

## Caveats

- **The US radiofax schedules are old but nothing newer exists.** Every NWS file and the March 2025 PDF give the same schedules. None has a newer "effective" date than 2018 (NMF/NMC/NOJ), 2020 (KVM70) or 2012 (NMG).
- **Durations.** NWS publishes start times only, so `minutes` is left out. The gap to the next start is an upper bound, not a published duration. DWD's `minutes` comes from its "Laufzeit" (running time) column, which the English PDF labels "DD-time".
- **`valid_utc`.** A paired value such as "00/12" was split between the two broadcast times, in the order the source gives them.
- **Tuning.** US radiofax frequencies are assigned frequencies: tune the dial 1.9 kHz below. USCG SITOR: 1.7 kHz below.
- **NOJ.** The top channel is 12412.5 kHz in hfak.txt and rfax.pdf, but 12410.6 kHz on the USCG broadcasts page. 12410.6 is the carrier (12412.5 minus 1.9).
- **KVM70.** The source prints 120/570 for four products. These look like typos for 576, and they are listed in `ioc_anomalies_in_source`.
- **NMG.** The weather.gov page says New Orleans fax can be preempted, because it shares transmitters with HF voice.
- **GYA (Northwood).** The only source is the GYA section of the NWS PDF, dated Jan 2022. It gives no LPM/IOC (left null). The map-area line is cut off in the source. It is marked `partial: true`.
- **DWD.** The fax PDF has no per-frequency hours. Fax pauses for voice on 5905/6180 kHz at 0555, 1155, 1555 and 1955.
- **NAVTEX, US stations.** Times are the USCG weather-broadcast slots from weather.gov. Two of the six slots per station carry Notices to Mariners instead of the forecast (`nm_repeat_utc`), and warnings can go out at any time. All six are 518 kHz only. The USCG's own NAVTEX page (navcen.uscg.gov) refused automated access with HTTP 403, so it was not used.
- **NAVTEX, Canadian stations.** Included are Yarmouth U/V (the closest, 518/490), Sydney Q/J, Moisie C/D, Robin Hood Bay O and Cartwright X. Iqaluit T/S is open only from about mid-May to mid-December. Times come from RAMN 2026 Part 2. Pacific stations were left out.
- **HFDL.** The table is version 52. The live table is broadcast by the ground stations, so a decoder can confirm whether it is still 52. Station 12 is not in the table. Riverhead NY (station 4) is the local one.
- **DSC.** The distress list (2187.5 plus the five HF channels) is from CCG RAMN 2026. Only Iqaluit VFF (MMSI 003160023, seasonal) is listed as a nearby HF DSC coast station. Halifax (VCS, 003160016) and Sydney (VCO, 003160017) are VHF Ch70 DSC only, so they are not usable on HF.
- **WWV.** The tone table is NIST's current one.
  - Station-ID minutes (WWV 0/30, WWVH 29/59) come from the 2002 SP 432 figures.
  - Marine storm warnings ended 2019-01-31.
  - GPS-status segments (SP 432: WWV 14–15, WWVH 43–44) do not appear on any current NIST page. The current tone table puts tones in those minutes, so treat them as gone (unverified).
  - The HamSCI test signal runs at minute 8 on WWV and minute 48 on WWVH. NIST also lists a 500 Hz tone for WWVH minute 48.
  - WWVH 5 MHz is running at 5 kW, down from 10 kW, because of an equipment failure.
  - WWV 25 MHz is experimental.
  - The most recent outage: 2026-10-19, 2011–2034 UTC, on all frequencies.
- **DRM.**
  - No B26 DRM broadcast targets North America.
  - `drm` holds the confirmed Europe/Africa/Latin America entries of 10 kW or more. These are RRI 9720, 9570, 5955, 6110 and 9500 (to Argentina/Brazil), Music 4 Joy 13730/7225/9810 (Tue and Thu), and TDF/RFI 9565.
  - `drm_other` holds the Asia/Pacific, unconfirmed and low-power entries.
  - Reception in Connecticut is not verified for any entry.
  - The BBC DRM service for Europe ended with B26.
- **SITOR-B.** USCG NMF (0140 and 1630), NMC, NMO and NRV from weather.gov, and CCG Iqaluit NBDP on 8416.5 at 0330 and 1530 (seasonal).

## Unverified or left out

These are also in the JSON under `unverified`.

- **CFH Halifax radiofax.** Not in rfax.pdf or RAMN 2026, and no official schedule was found.
- **VCO Sydney radiofax (4416 / 6915.1 kHz).** It is still printed in rfax.pdf, with information dated 2014. But RAMN 2026 says it was **discontinued 1 Oct 2024**, so treat it as off air.
- **Iqaluit, Resolute and Inuvik radiofax.** Discontinued in 2019.
- **USCG HF DSC watch stations and MMSIs (e.g. 003669995).** navcen.uscg.gov and dco.uscg.mil returned 403. NGA Pub 117 (msi.nga.mil) returned 503.
- **Routine DSC calling channels.** No ITU source could be fetched (404).
- **WINB, KTWR, AIR shortwave, Radio Kuwait and Vatican DRM.** Not in the B26 schedule. drmrx.org returned 403.
- **Winlink RMS channel list.** https://winlink.org/RMSChannels exists but is behind a bot challenge, so it was not read or copied.
- **STANAG 4285.** No specific stations are listed. The only general statement comes from the community wiki ("1.89–22.7 MHz"; it points to the Utility DXers Forum logs).
