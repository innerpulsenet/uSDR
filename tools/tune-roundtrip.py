#!/usr/bin/env python3
"""Live check: after leaving broadcast FM and coming back, is the tuner where
the server says it is?

    tools/tune-roundtrip.py [host:port]        (default 172.16.0.216:8073)

The fault this guards against has come back several times, each time by a
different road (a tune the driver stored but never applied, a mode change that
rebuilt the channel for a new LO offset without moving the tuner, a span change
that did the same): the server's idea of the LO and the hardware's part
company, every signal sits beside its passband by the difference, and broadcast
audio is rough because the channel filter is half off the station. None of it
shows in a unit test, because the radio is what disagrees.

So this drives the real receiver through the hops that have stranded it (80 m
in LSB and back, with the mode sent before the tune and after it, with no
pause, across a span change, NFM and back) and after each return measures
where the broadcast stations in the span actually sit against the 200 kHz
raster they are licensed on (US: odd tenths of a megahertz). A tuner off by an
LO offset's 50 kHz or more puts every station off by the same; a healthy one
puts their median within a few kilohertz (a station's own spectrum wanders
that much with its program). The stereo pilot on the check station must lock
too, which it cannot with the carrier far from the channel's middle.

It retunes the receiver (and leaves it on the check station in WFM), so run it
when nobody is listening for something else. Needs the `websockets` module.
Exit status 0 when every return is on frequency.
"""
import asyncio
import json
import math
import statistics
import struct
import sys
import time
import urllib.request

import websockets

BASE = sys.argv[1] if len(sys.argv) > 1 else "172.16.0.216:8073"
SPAN_CENTER = 99.1e6
STATION = 100.5e6  # strong, with a pilot, and its spectrum sits on its carrier
HF_CENTER, HF_RX = 5.0e6, 3.7e6
LIMIT_KHZ = 15.0


def post(path, body):
    req = urllib.request.Request(
        f"http://{BASE}{path}", data=json.dumps(body).encode(), headers={"Content-Type": "application/json"}
    )
    return json.load(urllib.request.urlopen(req, timeout=5))


def status():
    return json.load(urllib.request.urlopen(f"http://{BASE}/api/sdr/status", timeout=3))


async def mean_spectrum(frames):
    """The mean of `frames` spectrum frames (linear power) and their axis."""
    acc, meta, k = None, None, 0
    url = f"ws://{BASE}/ws?client=desktop&id=990&name=tune-roundtrip"
    async with websockets.connect(url, max_size=None) as ws:
        while k < frames:
            m = await asyncio.wait_for(ws.recv(), 10)
            if not isinstance(m, bytes) or m[0] in (1, 4, 5):
                continue
            center, rate = struct.unpack_from("<dd", m, 1)
            off = 1 + 28 + 2 + 1 + 2 + 4 + 4
            (scope_len,) = struct.unpack_from("<I", m, off)
            off += 4 + 2 * scope_len
            (bins,) = struct.unpack_from("<I", m, off)
            off += 12
            lin = [10 ** (v / 10) for v in struct.unpack_from(f"<{bins}f", m, off)]
            if acc is None or len(acc) != bins or meta != (center, rate):
                acc, meta, k = lin, (center, rate), 1
            else:
                acc = [a + b for a, b in zip(acc, lin)]
                k += 1
    return meta, [a / k for a in acc]


def station_offsets(meta, p):
    """(channel MHz, centroid offset kHz, dB over the floor) per strong station."""
    center, rate = meta
    n = len(p)
    freq = lambda i: center - rate / 2 + (i + 0.5) * rate / n
    floor = sorted(p)[n // 6]
    out = []
    khz = 88_100
    while khz * 1e3 < center + rate / 2 - 150e3:
        fc = khz * 1e3
        if fc > center - rate / 2 + 150e3:
            idx = [i for i in range(n) if abs(freq(i) - fc) <= 95e3]
            w = [max(p[i] - floor, 0.0) for i in idx]
            tot = sum(w)
            if idx and tot / len(idx) > 100 * floor:
                cen = sum(wi * freq(i) for wi, i in zip(w, idx)) / tot
                out.append((khz / 1e3, (cen - fc) / 1e3, 10 * math.log10(tot / len(idx) / floor)))
        khz += 200
    return out


def measure(tag):
    meta, p = asyncio.run(mean_spectrum(50))
    offs = [o for _, o, _ in station_offsets(meta, p)]
    st = status()
    pilot = (st.get("stereo") or {}).get("pilot")
    err = statistics.median(offs) if len(offs) >= 3 else float("nan")
    ok = len(offs) >= 3 and abs(err) <= LIMIT_KHZ and pilot is True and st["mode"] == "wfm"
    print(
        f"{'ok  ' if ok else 'FAIL'} {tag:<34} rx {st['inspect_hz'] / 1e6:8.4f} {st['mode']:>4} "
        f"span {st['rate_hz'] / 1e6:g}M LO {st.get('lo_offset_hz', 0) / 1e3:g}k · "
        f"stations {err:+6.1f} kHz off their raster ({len(offs)}) · pilot {pilot}",
        flush=True,
    )
    return ok


def fm(mode_first=False, pause=0.0):
    steps = [("/api/sdr/tune", {"freq_hz": SPAN_CENTER}), ("/api/sdr/inspect", {"freq_hz": STATION})]
    mode = ("/api/sdr/mode", {"mode": "wfm"})
    for path, body in ([mode] + steps) if mode_first else (steps + [mode]):
        post(path, body)
        time.sleep(pause)


def hf(mode_first=False, pause=0.0):
    steps = [("/api/sdr/tune", {"freq_hz": HF_CENTER}), ("/api/sdr/inspect", {"freq_hz": HF_RX})]
    mode = ("/api/sdr/mode", {"mode": "lsb"})
    for path, body in ([mode] + steps) if mode_first else (steps + [mode]):
        post(path, body)
        time.sleep(pause)


def main():
    start_rate = status()["rate_hz"]
    results = []
    fm()
    time.sleep(4)
    results.append(measure("broadcast FM, to begin with"))

    hf()
    time.sleep(3)
    fm()
    time.sleep(4)
    results.append(measure("80 m and back, tune then mode"))

    hf(mode_first=True)
    time.sleep(3)
    fm(mode_first=True)
    time.sleep(4)
    results.append(measure("80 m and back, mode then tune"))

    hf()
    time.sleep(0.3)
    fm()
    time.sleep(4)
    results.append(measure("80 m and back, at once"))

    hf()
    post("/api/sdr/rate", {"rate_hz": 2e6})
    time.sleep(4)
    post("/api/sdr/rate", {"rate_hz": start_rate})
    fm()
    time.sleep(5)
    results.append(measure("80 m at 2 MS/s and back"))

    post("/api/sdr/mode", {"mode": "nfm"})
    time.sleep(2)
    post("/api/sdr/mode", {"mode": "wfm"})
    time.sleep(4)
    results.append(measure("NFM and back, no tune"))

    if all(results):
        print("the tuner came back on frequency every time")
        return 0
    print("the tuner did NOT come back on frequency: see the FAIL lines; journalctl -u usdr on the radio host")
    return 1


if __name__ == "__main__":
    sys.exit(main())
