# HF directory, part 2: notes (fetched 2026-11-06)

All data is in `hf-directory-2.json`. Every entry has a source URL that was fetched today. Anything that couldn't be verified is listed under `unverified`.

## How each proposed list held up
- **ALE.** The amateur HFLINK channels are confirmed: 3596, 7102, 10145.5, 14109, 18106, 21096 and 28146 kHz USB are all HFN, the ALE texting/sounding net. I added the US-region HFL voice-calling ALE channels (1996, 3996, 5357, 5371.5, 7296, 10131, 14346, 18117.5, 21432.5, 24932, 28312.5) plus 1843, 24926 and 50162.5.
  The proposed HFGCS list is HFGCS *voice*, not ALE, and has no fetchable primary source (acc.af.mil returned 403). COTHEN and MARS ALE have no public official source. All three are left out and listed under unverified.
- **WSPR.** All 13 proposed HF/MF values match WSJT-X's built-in FrequencyList.cpp. Two caveats: 6 m 50.293 is Region 2/3 only, and 4 m 70.091 is Region 1 only. 60 m is *not* in WSJT-X's list, and neither 5.2872 nor 5.3647 has a primary source, so both are unverified.
- **JS8Call.** All 10 proposed values match the official repo's FrequencyList.cpp (js8call/js8call). The repo also lists 2 m 144.178.
- **SSTV.** 3.845, 7.171, 14.230, 21.340 and 28.680 MHz are confirmed by the ARRL band plan. The IARU R2 plan agrees except on 40 m, where it gives 7165. The sidebands (LSB below 10 MHz, USB above) come from the voice convention in the FCC question pool, not from a band plan.
  ISS 145.800 FM: the latest ARISS release (Nov 2025) says PD120, and ARRL's 2014 news says PD180. The mode is per event.
- **CW.** 3560, 7030 and 14060 kHz are confirmed as IARU R2 "CW QRP Center of Activity". The full per-band QRP and QRS list and the CW segments come from the Sept 2020 R2 plan. The ARRL band plan page doesn't break out CW segments except on 160 m and 10 m.
- **Feld Hell.** The club publishes *ranges*. 3.582, 7.082 and 10.142 fall inside them, and 14.063 is the club's preferred 20 m frequency. 14.082 is **not** on the list; the club gives 14.072-14.073.
- **Olivia.** From the Olivia Digital DXers Club's "Current (2023)" list, with center and dial (dial = center - 1.5 kHz). It's a club list; there is no standards-body list for Olivia.
- **Shortwave Radiogram.** The official site's bot challenge was skipped. WRMI's own B26 sheet (effective Nov 1, 2026) gives 7 weekly slots: Mon 0030 on 5050, Mon 0800 on 5850, Wed 1330 on 15770, Thu 0930 on 7780, Fri 0530 on 5850, Sat 2300 on 9455 and Sun 1530 on 9955.
  These are decoded by matching program blocks to the transmitter grid. Some blocks carry older "effective" dates inside the same sheet. Modes and WINB slots are unknown.
- **CHU.** **Off the air since June 22, 2026.** NRC's time-services page no longer lists shortwave, and the CHU page now returns 404. Don't add CHU as an active station.
- **NAVTEX HF.** 4209.5 kHz is confirmed as the international NAVTEX-type HF frequency (47 CFR 80.1135(e)). No station reachable from Connecticut was found on it: Canadian Atlantic NAVTEX is 518/490 only. US stations weren't checked because NAVCEN returned 403.

## Thin spots
Military and government ALE is empty. The Radiogram entries have no modes. The CHU format isn't verified, and US NAVTEX wasn't checked directly.
