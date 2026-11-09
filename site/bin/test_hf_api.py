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
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
TMP = tempfile.mkdtemp(prefix="hf-api-")
TOKEN = base64.urlsafe_b64encode(os.urandom(32)).decode()
os.environ.update(HF_DB=os.path.join(TMP, "hf.sqlite"), HF_PICTURES=os.path.join(TMP, "pictures"), HF_TOKEN_SHA256=hashlib.sha256(TOKEN.encode()).hexdigest())
import hf_api  # noqa: E402
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
            for t in ("observations", "listening", "gaps", "pictures", "meta"):
                c.execute(f"DELETE FROM {t}")

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

    def test_a_gzip_bomb_is_refused(self):
        bomb = gzip.compress(b" " * (hf_api.MAX_JSON + 1024))
        self.assertLess(len(bomb), hf_api.MAX_BODY)
        self.assertEqual(self.post(bomb)[0], 422)

    def test_junk_is_refused(self):
        self.assertEqual(self.post(b"not gzip")[0], 422)
        self.assertEqual(self.post(gzip.compress(b"{"))[0], 422)

    def png(self, w, h):
        return hf_api.PNG_MAGIC + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", w, h) + b"\x08\x00\x00\x00\x00" + b"\x00" * 16

    def picture(self, body, **meta):
        m = {"id": 7, "start_utc": 1800000000, "end_utc": 1800000600, "mode": "WEFAX", "dial_hz": 8501100, "width": 1809, "lines": 1338, "station": "NMG", "scheduled_title": "Surface analysis", **meta}
        return self.post(body, path="/ingest/picture", headers={"x-hf-picture": json.dumps(m)})

    def test_a_picture_is_kept_under_a_name_built_here(self):
        self.assertEqual(self.picture(self.png(1809, 1338), scheduled_title="../../etc")[0], 200)
        self.assertEqual(os.listdir(hf_api.PICTURES), ["wefax-1800000000-8501100.png"])

    def test_a_picture_that_is_not_what_it_claims_is_refused(self):
        self.assertEqual(self.picture(b"<html>")[0], 422)
        self.assertEqual(self.picture(self.png(10, 10))[0], 422)
        self.assertEqual(self.picture(self.png(1809, 1338), mode="SSTV")[0], 422)


if __name__ == "__main__":
    unittest.main(verbosity=1)
