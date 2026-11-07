# lookups.json: sources and gaps

All fetched 2026-11-06. Every table carries its own `source` block with the URL. The parsers are in `scratchpad/tools/` (`mid.py`, `natmarks.py`, `m49.py`, `mars.py`, `build.py`), and the raw downloads are in `scratchpad/dl/`.

| key | rows | source |
|---|---|---|
| `mid` | 294 rows, 292 distinct MIDs (201–775) | ITU GLAD "Table of Maritime Identification Digits" (itu.int/gladapp/Allocation/MIDs) |
| `mmsi_structure` | 15 rules | ITU-R M.585-10 (04/2026) PDF from itu.int |
| `coast_stations` | 25 (13 USCG records, 1 WLO, 12 CCG) | ITU MARS coast-station search, plus the CCG Radio Aids to Marine Navigation 2026, Part 2 |
| `icao24` | 207 blocks | tar1090 `flags.js`, which says it is generated from the Annex 10 Vol III Ch. 9 appendix |
| `registration_prefixes` | 233 | ICAO "Nationality marks" PDF (footer 1/6/26) |
| `airlines` | 6243 | FAA JO 7340.2P Chg 3, Ch. 3 §1 (ICAO 3-letter designators) |
| `acars_labels` | 175 | ACARS Hub `metadata.json` (Airframes.io) |
| `lookups_online` | 1 working link pattern | adsb.lol / tar1090 `?icao=` |
| `iso3166` | 248 | UN M49 table (it carries the ISO alpha-2/3 codes) |
| `iso639_2` | 487 | Library of Congress ISO 639-2 RA text file |

## Caveats

- **M.585 version.** M.585-10 (04/2026) is the version in force. M.585-9 (2022) is the one the Radio Regulations incorporate by reference. -10 also adds 979 (AMRD Group B), 8MID handheld, and the 12-character `97T` extended identity.
- **MID 306** is split among Bonaire/Sint Eustatius/Saba, Curaçao and Sint Maarten. Other MIDs (for example 209/210 Cyprus) share one row. Each MID gets its own entry.
- **Coast stations.** MARS registers each USCG MMSI twice: a "RADIO" broadcast record and an "RCC ... DSC-WATCH" record (for example 003669991 is Boston radio and RCC Boston). Both are kept. The MARS list view does not mark MF vs HF DSC, but the USCG search run with the HF band filter returned all 13 records. The CCG MMSIs and call signs match between MARS and the RAMN. The RAMN does not say which MCTS centers keep MF DSC versus VHF only, so the table doesn't say either.
- **ICAO24.** These are not checked against Annex 10 itself, which isn't freely available. tar1090 cites amendments through 88-A (2013), so any block changes after that are missing. 10 UK sub-territory ranges are tar1090 additions and are flagged `annex10:false`. They must be matched before the UK catch-all. tar1090 marks Solomon Islands 897000 with "flag typo?".
- **Airlines.** This is the whole FAA list, not a "top 250 long-haul" subset. No primary source ranks operators by HFDL traffic, so the table isn't ranked. Country strings are FAA's (for example Cathay Pacific is listed under CHINA).
- **ACARS labels** are a community decoder convention, not ARINC 618/620 text, which is paywalled. Source oddities: key `25` has an inner label of `2S`, and key `:}` has an empty inner label. Both are noted in the entries.
- **ISO 3166.** Names are UN M49 names. Taiwan (TW/TWN) is missing because M49 omits it.

## Unverified (left out)

- **USCG Guam MMSI** (claimed 003669994 elsewhere). MARS lists "GUAM" and "GUAM, MARIANA ISLANDS RADIO" with no MMSI, and NAVCEN is blocked (see below).
- **USCG Alaska stations other than Kodiak** (Adak, Juneau, Valdez) and the Hawaii NMO/KVM70 records. They are in MARS but have no MMSI there.
- **NAVCEN DSC pages** (navcen.uscg.gov node/211 and others) returned HTTP 403 to both curl and WebFetch, and dco.uscg.mil PDFs also returned 403. They were skipped rather than worked around.
- **NGA Pub 117** (msi.nga.mil) returned a 503 "apology" page and was skipped.
- **ACARS labels with an empty name in the source** (10, 32, 3B, 42, AE, BG) are omitted. Label 10 was explicitly requested.
- **"~250 most common long-haul operators"** has no authoritative ranking, so none was made.
- **Aircraft hex lookups.** ADS-B Exchange showed a CAPTCHA/challenge, airplanes.live loads a Cloudflare challenge script, and the FAA registry Mode S search returned HTTP 503. None of these is offered.
- **MARS ship lookup by MMSI.** There is no GET deep link: the search is a POST form with a session token. After about 8 scripted searches MARS began answering HTTP 403, so it should not be automated. Link to the search page instead.
