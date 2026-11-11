#!/usr/bin/env python3
"""python3 site/bin/test_hf_api.py"""
import base64
import gzip
import hashlib
import json
import os
import shutil
import struct
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
TMP = tempfile.mkdtemp(prefix="hf-api-")
TOKEN = base64.urlsafe_b64encode(os.urandom(32)).decode()
os.environ.update(HF_DB=os.path.join(TMP, "hf.sqlite"), HF_PICTURES=os.path.join(TMP, "pictures"), HF_DATA=os.path.join(TMP, "data"), HF_TOKEN_SHA256=hashlib.sha256(TOKEN.encode()).hexdigest())
import hf_api  # noqa: E402
import hf_warm  # noqa: E402
from http.server import HTTPServer  # noqa: E402

OBS = {"id": 1, "at_utc": 1800000000, "dial_hz": 14074000, "freq_hz": 14075000, "mode": "FT8", "kind": "spot", "ident": "K1ABC", "grid": "FN42", "lat": None, "lon": None, "loc_kind": "grid4", "snr_db": -7.0, "details": None}
STAY = {"id": 1, "start_utc": 1800000000, "end_utc": 1800000120, "dial_hz": 14074000, "mode": "FT8", "cause": "schedule", "noise_dbfs": -80.0, "peak_iq": 0.2, "decodes": 3, "valid_decodes": 3, "healthy": 1}


def batch(**kw):
    return {"station": "hf1", "grid": "FN20", "sent_utc": 1800000200, "observations": [], "listening": [], "gaps": [], **kw}


class Ingest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(("127.0.0.1", 0), hf_api.Handler)
        cls.server.conn = hf_api.connect(hf_api.DB)
        cls.url = f"http://127.0.0.1:{cls.server.server_port}"
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        shutil.rmtree(TMP, ignore_errors=True)

    def setUp(self):
        with self.server.conn as c:
            for t in ("observations", "listening", "gaps", "pictures", "meta", "heard", "spectrum"):
                c.execute(f"DELETE FROM {t}")
        shutil.rmtree(hf_warm.OUT, ignore_errors=True)
        shutil.rmtree(hf_api.PICTURES, ignore_errors=True)
        os.makedirs(hf_api.DATA, exist_ok=True)

    def post(self, body, token=TOKEN, path="/ingest", headers=None):
        hdrs = {"content-encoding": "gzip"} if headers is None else headers
        if token:
            hdrs = {**hdrs, "authorization": f"Bearer {token}"}
        req = urllib.request.Request(self.url + path, data=body, headers=hdrs, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                return r.status, json.loads(r.read())
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read())

    def send(self, value, **kw):
        return self.post(gzip.compress(json.dumps(value).encode()), **kw)

    def count(self, table):
        return self.server.conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]

    def test_a_good_batch_is_stored_and_a_repeat_adds_nothing(self):
        self.assertEqual(self.send(batch(observations=[OBS], listening=[STAY]))[0], 200)
        self.assertEqual(self.send(batch(observations=[OBS], listening=[STAY]))[0], 200)
        self.assertEqual((self.count("observations"), self.count("listening")), (1, 1))

    def test_a_later_verdict_replaces_a_stay(self):
        self.send(batch(listening=[STAY]))
        self.send(batch(listening=[{**STAY, "healthy": 0}]))
        self.assertEqual(self.server.conn.execute("SELECT healthy FROM listening").fetchone()[0], 0)

    def test_a_bad_token_is_refused_before_anything_is_read(self):
        self.assertEqual(self.send(batch(observations=[OBS]), token="wrong")[0], 401)
        self.assertEqual(self.send(batch(observations=[OBS]), token=None)[0], 401)
        self.assertEqual(self.count("observations"), 0)

    def test_an_unlisted_mode_is_refused_whole(self):
        status, _ = self.send(batch(observations=[OBS, {**OBS, "id": 2, "mode": "POCSAG"}]))
        self.assertEqual(status, 422)
        self.assertEqual(self.count("observations"), 0)

    def test_anything_above_30_mhz_is_refused(self):
        self.assertEqual(self.send(batch(observations=[{**OBS, "dial_hz": 50313000}]))[0], 422)
        self.assertEqual(self.send(batch(observations=[{**OBS, "freq_hz": 30000000}]))[0], 422)
        self.assertEqual(self.send(batch(listening=[{**STAY, "dial_hz": 144390000}]))[0], 422)

    def test_a_six_character_station_grid_is_refused(self):
        self.assertEqual(self.send({**batch(), "grid": "FN20xk"})[0], 422)

    def test_details_are_default_deny(self):
        row = {**OBS, "mode": "HFDL", "kind": "acars", "details": {"flight": "BA117", "text": "CREW NOTE", "to": "x"}}
        self.assertEqual(self.send(batch(observations=[row]))[0], 200)
        stored = self.server.conn.execute("SELECT details FROM observations").fetchone()[0]
        self.assertEqual(json.loads(stored), {"flight": "BA117"})

    def test_a_navtex_message_keeps_its_text_and_nothing_hidden_in_it(self):
        text = "ZCZC FA12\n" + "GALE WARNING " * 200 + "\nNNNN"
        ok = {**OBS, "dial_hz": 517000, "freq_hz": 517000, "mode": "NAVTEX", "kind": "message", "ident": "FA12", "grid": None, "loc_kind": "none",
              "details": {"station": "F", "subjectName": "Navigational warning", "complete": "yes", "text": text}}
        bell = {**ok, "id": 2, "details": {**ok["details"], "text": "ZCZC\x07"}}
        self.assertEqual(self.send(batch(observations=[ok, bell]))[0], 200)
        stored = [json.loads(r[0]) for r in self.server.conn.execute("SELECT details FROM observations ORDER BY id")]
        self.assertEqual(stored[0]["text"], text)
        self.assertNotIn("text", stored[1])

    def test_a_repeated_navtex_message_is_listed_once_with_its_best_copy(self):
        def msg(rid, age_s, complete, text):
            return self.recent(rid, age_s, dial_hz=517000, freq_hz=517000, mode="NAVTEX", kind="message", ident="FA12", grid=None, loc_kind="none", snr_db=None,
                               details={"station": "F", "subjectName": "Navigational warning", "complete": complete, "text": text})
        self.warm_with([msg(1, 9000, "yes", "WHOLE"), msg(2, 60, "no", "CUT")])
        m = self.data("activity.json")["navtex"]
        self.assertEqual(len(m), 1)
        self.assertEqual((m[0]["id"], m[0]["name"], m[0]["n"], m[0]["text"], m[0]["complete"]), ("FA12", "Boston", 2, "WHOLE", True))

    def test_beacons_heard_fill_their_cells_and_the_hours(self):
        now = int(time.time())
        def heard(rid, age_s, call, freq, snr, steps):
            return self.recent(rid, age_s, dial_hz=freq - 700, freq_hz=freq, mode="BEACON", kind="heard", ident=call, grid="KP20eh", loc_kind="grid6",
                               snr_db=snr, details={"steps": str(steps), "dash1": "12"})
        stay = {**STAY, "start_utc": now - 1800, "end_utc": now - 60, "dial_hz": 14_099_300, "mode": "BEACON", "cause": "wideband"}
        rows = [heard(1, 900, "OH2B", 14_100_000, 8.0, 2), heard(2, 300, "OH2B", 14_100_000, 14.0, 3), heard(3, 7200, "4X6TU", 14_100_000, 3.0, 1)]
        self.assertEqual(self.send(batch(observations=rows, listening=[stay]))[0], 200)
        hf_warm.main()
        b = self.data("activity.json")["beacons"]
        oh2b = next(x for x in b["beacons"] if x["call"] == "OH2B")["heard"]["14100000"]
        self.assertEqual((oh2b["n"], oh2b["snr"], oh2b["steps"]), (2, 14.0, 3))
        self.assertEqual(next(x for x in b["beacons"] if x["call"] == "4X6TU")["heard"], {}, "heard two hours ago, not in the last hour")
        self.assertEqual(b["listened"]["14100000"], 1740)
        self.assertEqual(b["listened"]["28200000"], 0)
        self.assertEqual(sum(n for n, _ in b["hours"][0]), 2, "OH2B and 4X6TU, each in its own hour")
        self.assertEqual(sum(s for _, s in b["hours"][0]), 1740)

    def test_without_a_beacon_channel_there_is_no_beacon_grid(self):
        self.warm_with([self.recent(1, 90)])
        self.assertIsNone(self.data("activity.json")["beacons"])

    def test_a_gzip_bomb_is_refused(self):
        bomb = gzip.compress(b" " * (hf_api.MAX_JSON + 1024))
        self.assertLess(len(bomb), hf_api.MAX_BODY)
        self.assertEqual(self.post(bomb)[0], 422)

    def test_junk_is_refused(self):
        self.assertEqual(self.post(b"not gzip")[0], 422)
        self.assertEqual(self.post(gzip.compress(b"{"))[0], 422)

    def warm_with(self, rows):
        self.assertEqual(self.send(batch(observations=rows))[0], 200)
        hf_warm.main()

    def data(self, name):
        with open(os.path.join(hf_warm.OUT, name)) as f:
            return json.load(f)

    def recent(self, rid, age_s, **kw):
        return {**OBS, "id": rid, "at_utc": int(time.time()) - age_s, **kw}

    def test_the_map_counts_stations_not_decodes(self):
        self.warm_with([self.recent(1, 90), self.recent(2, 75), self.recent(3, 60, ident="W2XYZ")])
        m = self.data("map-24h.json")
        self.assertEqual(m["squares"], ["FN42"])
        self.assertEqual(sorted(m["idents"]), ["K1ABC", "W2XYZ"])
        decodes = {m["idents"][r[4]]: r[5] for r in m["spots"]}
        self.assertEqual(decodes, {"K1ABC": 2, "W2XYZ": 1})

    def test_a_station_can_be_looked_up_and_a_second_run_does_not_count_twice(self):
        self.warm_with([self.recent(1, 500, snr_db=-20.0), self.recent(2, 100, snr_db=-3.0, grid="FN31")])
        hf_warm.main()
        row = self.data("who/K1.json")["stations"]["K1ABC"]
        self.assertEqual(len(row), 1)
        self.assertEqual(row[0][:2], ["FT8", "20 m"])
        self.assertEqual((row[0][4], row[0][5], row[0][6]), (2, -3, "FN31"))
        self.send(batch(observations=[self.recent(3, 30, snr_db=None, grid=None, loc_kind="none")]))
        hf_warm.main()
        row = self.data("who/K1.json")["stations"]["K1ABC"]
        self.assertEqual((row[0][4], row[0][5], row[0][6]), (3, -3, "FN31"))

    def test_the_receivers_status_reaches_the_now_file_and_bad_status_costs_nothing(self):
        self.assertEqual(self.send(batch(now={"at": 1800000000, "state": "monitoring", "label": "HFDL 11387.0 kHz", "ends_utc": 1800000100, "next_chart": ""}))[0], 200)
        with open(os.path.join(hf_api.DATA, "now.json")) as f:
            self.assertEqual(json.load(f)["label"], "HFDL 11387.0 kHz")
        self.assertEqual(self.send(batch(now={"state": "<script>"}, observations=[OBS]))[0], 200)
        self.assertEqual(self.count("observations"), 1)

    def test_airtime_health_and_traffic_are_built(self):
        t = int(time.time())
        stay = {**STAY, "start_utc": t - 300, "end_utc": t - 60}
        flight = self.recent(2, 90, mode="HFDL", kind="position", ident="UAL971", grid=None, lat=40.0, lon=-40.0, loc_kind="reported", snr_db=None, details={"flight": "UAL971", "gsId": "4", "fixUtc": t - 100})
        self.assertEqual(self.send(batch(listening=[stay], observations=[self.recent(1, 90), flight]))[0], 200)
        hf_warm.main()
        m = self.data("map-24h.json")
        self.assertEqual(m["bands"][m["air"][0][1]], "20 m")
        self.assertEqual(m["air"][0][2], 240)
        h = self.data("health.json")
        self.assertEqual(h["timeline"][0][2], "FT8")
        self.assertEqual(h["bands"][0]["label"], "20 m")
        self.assertEqual(h["checks"], {"passed": 1, "failed": 0})
        self.assertEqual(self.data("traffic.json")["flights"][0]["i"], "UAL971")

    def test_a_chart_gets_a_thumbnail_without_its_phasing_band(self):
        import zlib
        w, h = 1809, 400
        rows = b"".join(b"\0" + bytes([0 if y < 60 else 255]) * w for y in range(h))
        def chunk(k, b):
            return struct.pack(">I", len(b)) + k + b + struct.pack(">I", zlib.crc32(k + b))
        png = hf_api.PNG_MAGIC + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
        self.assertEqual(self.picture(png, width=w, lines=h)[0], 200)
        hf_warm.main()
        name = self.data("pictures.json")["pictures"][0]["thumb"]
        tw, trows = hf_warm.png_rows(os.path.join(hf_api.PICTURES, name))
        self.assertEqual(tw, 603)
        self.assertGreater(min(trows[0]), 200)

    def test_a_ship_call_is_shown_at_once_and_can_be_looked_up(self):
        self.warm_with([self.recent(1, 90, mode="DSC", kind="call", ident="477665200", grid=None, loc_kind="none", snr_db=None)])
        self.assertEqual(self.data("who/47.json")["stations"]["477665200"][0][0], "DSC")
        self.assertEqual([r["i"] for r in self.data("log.json")["rows"]], ["477665200"])

    def png(self, w, h):
        return hf_api.PNG_MAGIC + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", w, h) + b"\x08\x00\x00\x00\x00" + b"\x00" * 16

    def picture(self, body, **meta):
        m = {"id": 7, "start_utc": 1800000000, "end_utc": 1800000600, "mode": "WEFAX", "dial_hz": 8501100, "width": 1809, "lines": 1338, "station": "NMG", "scheduled_title": "Surface analysis", **meta}
        return self.post(body, path="/ingest/picture", headers={"x-hf-picture": json.dumps(m)})

    def test_spectrum_rows_are_stored_and_a_bad_one_costs_the_batch(self):
        good = {"m": 30000000, "lo": 1400, "d": "50" * 700}
        status, reply = self.send(batch(spectrum=[good]))
        self.assertEqual((status, reply["spectrum"]), (200, 1))
        for bad in ({"m": 30000000, "lo": 2900, "d": "50" * 700}, {"m": 0, "lo": 1400, "d": "50"}, {"m": 30000000, "lo": 1400, "d": "zz"}, {"m": 30000000, "lo": 1400, "d": ""}):
            self.assertEqual(self.send(batch(spectrum=[bad]))[0], 422, bad)
        self.assertEqual(self.count("spectrum"), 1)

    def test_the_spectrum_shows_only_what_stands_above_its_own_noise(self):
        now = 1800000000
        minute = now // 60 - 10
        cells = [80] * 700
        cells[100] = 160  # 40 dB over the floor
        cells[400] = 84   # 2 dB over: noise, and shown as quiet
        cells[300] = 0    # no reading
        with self.server.conn as c:
            c.execute("INSERT INTO spectrum VALUES (?,?,?)", (minute, 1000, bytes(cells)))
        out = hf_warm.build_spectrum(self.server.conn, now)
        self.assertEqual(out["rows"], 1)
        self.assertEqual(out["earliest_minute"], minute)
        self.assertEqual(len(out["palette"]), 256)
        over = hf_warm.above_floor(bytes(cells))
        self.assertEqual(over[100], 40)
        self.assertEqual(over[400], 2)
        self.assertIsNone(over[300])
        with open(os.path.join(hf_warm.OUT, "spectrum.png"), "rb") as f:
            self.assertEqual(hf_api.png_size(f.read()), (1440, 700))

    def test_a_picture_is_kept_under_a_name_built_here(self):
        self.assertEqual(self.picture(self.png(1809, 1338), scheduled_title="../../etc")[0], 200)
        self.assertEqual(os.listdir(hf_api.PICTURES), ["wefax-1800000000-8501100.png"])

    def test_a_picture_that_is_not_what_it_claims_is_refused(self):
        self.assertEqual(self.picture(b"<html>")[0], 422)
        self.assertEqual(self.picture(self.png(10, 10))[0], 422)
        self.assertEqual(self.picture(self.png(1809, 1338), mode="NAVTEX")[0], 422)


if __name__ == "__main__":
    unittest.main(verbosity=1)
