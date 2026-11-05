#!/usr/bin/env python3
"""Live check of an HF data mode on the deployed daemon: tune it, listen,
and print what the decoder makes of the air.

    tools/hf-live-check.py MODE DIAL_HZ SECONDS [host:port] [--pgm out.pgm]

MODE is one of ft8 ft4 wefax rtty navtex psk31; DIAL_HZ is the suppressed
carrier (the dial), as the BANDS menus list it. The socket connects as a
desktop client, so radiofax rows arrive too: with --pgm they are written as
a gray PGM picture when the run ends. Every decode prints as it lands, and
the decoder's reading (lock, audio frequency, SNR, PSK31 channels, fax
state) prints whenever it changes, with the station reporter's state at the
end.

It retunes the receiver and leaves it where it was asked to go, so run it
when nobody is listening to something else. Needs the `websockets` module.
Exit status 0 when anything decoded.
"""
import asyncio
import json
import struct
import sys
import time
import urllib.request

import websockets

args = [a for a in sys.argv[1:] if not a.startswith("--")]
pgm = None
if "--pgm" in sys.argv:
    pgm = sys.argv[sys.argv.index("--pgm") + 1]
    args = [a for a in args if a != pgm]
if len(args) < 3:
    print(__doc__)
    sys.exit(2)
MODE, DIAL, SECS = args[0], float(args[1]), float(args[2])
BASE = args[3] if len(args) > 3 else "172.16.0.216:8073"


def post(path, body):
    req = urllib.request.Request(
        f"http://{BASE}{path}",
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "x-usdr-client": "desktop hf-live-check"},
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read() or b"{}")


def get(path):
    with urllib.request.urlopen(f"http://{BASE}{path}", timeout=10) as r:
        return json.loads(r.read())


async def main():
    # Already there, nothing is sent: a mode or tune request rebuilds the
    # decoder, which would end a radiofax picture or a PSK31 channel midway.
    st = get("/api/sdr/status")
    here = st.get("mode") == MODE and abs(st.get("inspect_hz", 0) - DIAL) < 1
    if not here:
        post("/api/sdr/mode", {"mode": MODE})
        await asyncio.sleep(0.5)
        # The span centered on the dial, then the receiver on it.
        post("/api/sdr/tune", {"freq_hz": DIAL})
        await asyncio.sleep(0.5)
        post("/api/sdr/inspect", {"freq_hz": DIAL})
    print(f"{MODE} at {DIAL / 1e3:.1f} kHz for {SECS:.0f} s")
    decodes = 0
    rows = {}
    width = 0
    last_reading = None
    end = time.time() + SECS
    async with websockets.connect(f"ws://{BASE}/ws?client=desktop&name=hf-live-check", max_size=None) as ws:
        while time.time() < end:
            try:
                msg = await asyncio.wait_for(ws.recv(), timeout=max(0.1, end - time.time()))
            except asyncio.TimeoutError:
                break
            if isinstance(msg, bytes):
                if msg and msg[0] == 0x06:
                    _, rx, image, line, w = struct.unpack_from("<BBIIH", msg)
                    width = w
                    rows.setdefault(image, {})[line] = msg[12 : 12 + w]
                continue
            m = json.loads(msg)
            if m.get("type") == "decode":
                e = m["event"]
                decodes += 1
                f = e.get("fields", {})
                extra = " ".join(f"{k}={f[k]}" for k in ("df", "snr", "channel", "id") if k in f)
                print(f"{time.strftime('%H:%M:%S', time.gmtime())} [{e['protocol']} {e['kind']}] {e['summary'][:160]!r} {extra}")
            elif m.get("type") in ("status", "hello"):
                st = m.get("sdr", m)
                r = st.get("digital")
                if r != last_reading and r is not None:
                    last_reading = r
                    chans = r.get("channels") or []
                    ch = " ".join(f"{c['audio_hz']:.0f}/{c['snr_db']:+.0f}{'*' if c['active'] else ''}" for c in chans)
                    print(
                        f"  reading: locked={r.get('locked')} hz={r.get('center_hz')} snr={r.get('snr_db')} "
                        f"rev={r.get('reverse')} fax={r.get('fax_state')} rows={r.get('fax_lines')} {ch}"
                    )
    st = get("/api/station")
    print("reporter:", json.dumps(st.get("reporter")))
    if rows:
        image = max(rows)
        got = rows[image]
        n = max(got) + 1
        print(f"fax picture {image}: {len(got)} of {n} rows, {width} wide")
        if pgm:
            with open(pgm, "wb") as f:
                f.write(f"P5 {width} {n} 255\n".encode())
                for i in range(n):
                    f.write(got.get(i, bytes([128]) * width))
            print("wrote", pgm)
    print(f"{decodes} decodes")
    sys.exit(0 if decodes or rows else 1)


asyncio.run(main())
