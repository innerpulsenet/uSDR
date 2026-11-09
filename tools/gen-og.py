#!/usr/bin/env python3
"""Draws the unchanging part of the share card, and the digits for its figures,
with headless Chrome and the site's own fonts. The backend's timer plots the real
stations onto this and writes data/og.png. Run it when the card's design changes."""
import base64, json, os, re, subprocess, tempfile, zlib
from PIL import Image

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "site", "bin")
W, H = 1200, 630
GLOBE = {"cx": 908, "cy": 342, "r": 352, "lon0": -44, "lat0": 34}
STATS = [{"x": 56, "y": 452}, {"x": 300, "y": 452}]
CHARS = "0123456789,"
CELL = (46, 74)

index = open(os.path.join(ROOT, "site", "index.html")).read()
wordmark = re.search(r'<path d="([^"]+)"/></svg></span>', index).group(1)
land = open(os.path.join(ROOT, "site", "data", "land-110m.json")).read()
fonts = f"""
@font-face {{ font-family: IS; src: url(file://{ROOT}/site/brand/instrument-sans-latin.woff2); font-weight: 400 700; }}
@font-face {{ font-family: JB; src: url(file://{ROOT}/site/brand/jetbrains-mono-latin.woff2); font-weight: 400 800; }}
* {{ margin: 0; box-sizing: border-box; }}
html, body {{ width: {W}px; height: {H}px; overflow: hidden; }}
"""
bars = '<rect x="-103" y="-23" width="22" height="46" rx="4"/><rect x="-66.2" y="-87.74" width="22" height="175.48" rx="4"/><rect x="-29.4" y="-67.21" width="22" height="134.41" rx="4"/><rect x="7.4" y="-53" width="22" height="106" rx="4"/><rect x="44.2" y="-93.16" width="22" height="186.33" rx="4"/><rect x="81" y="-23" width="22" height="46" rx="4"/>'
card = f"""<!doctype html><meta charset=utf-8><style>{fonts}
body {{ background: #0b0d10; color: #e3e6e9; font-family: IS; position: relative; }}
body::before {{ content: ""; position: absolute; inset: 0; background: radial-gradient(90% 90% at 76% 50%, rgba(255,179,71,.05), transparent 60%), radial-gradient(120% 100% at 50% 0, transparent 40%, rgba(0,0,0,.5)); }}
canvas {{ position: absolute; inset: 0; }}
.brand {{ position: absolute; left: 56px; top: 52px; display: flex; align-items: center; gap: 14px; }}
h1 {{ position: absolute; left: 56px; top: 150px; width: 480px; font: 700 62px/1.04 IS; letter-spacing: -.03em; }}
.lab {{ position: absolute; font: 500 14px/1 JB; letter-spacing: .13em; color: #8f949b; text-transform: uppercase; }}
.url {{ position: absolute; left: 56px; bottom: 44px; font: 400 19px/1 JB; color: #b9bdc3; }}
</style>
<canvas id=c width={W} height={H}></canvas>
<div class=brand><svg viewBox="-110 -110 220 220" width=52 height=52><g fill="#f43a2a">{bars}</g></svg>
<svg viewBox="4.5 -70 413.2 92.5" height=38><defs><linearGradient id=s gradientUnits=userSpaceOnUse x1=0 y1=-69 x2=0 y2=0><stop offset=0 stop-color=#fbfcfd /><stop offset=.38 stop-color=#cfd3d8 /><stop offset=.54 stop-color=#8b9097 /><stop offset=.6 stop-color=#b9bdc3 /><stop offset=.84 stop-color=#e3e6e9 /><stop offset=1 stop-color=#9da2a9 /></linearGradient></defs><path d="{wordmark}" fill="url(#s)"/></svg></div>
<h1>One receiver, listening below 30&nbsp;MHz.</h1>
<div class=lab style="left:{STATS[0]['x']}px;top:{STATS[0]['y'] - 26}px">Stations, 24 h</div>
<div class=lab style="left:{STATS[1]['x']}px;top:{STATS[1]['y'] - 26}px">Grid squares</div>
<div class=url>hf.innerpulse.net</div>
<script>
const land={land}, G={json.dumps(GLOBE)}, X=document.getElementById("c").getContext("2d"), R=Math.PI/180;
const P=(lo,la,clamp)=>{{const l=(lo-G.lon0)*R,p=la*R,p0=G.lat0*R;let x=Math.cos(p)*Math.sin(l),y=Math.cos(p0)*Math.sin(p)-Math.sin(p0)*Math.cos(p)*Math.cos(l);const z=Math.sin(p0)*Math.sin(p)+Math.cos(p0)*Math.cos(p)*Math.cos(l);if(z<0&&clamp){{const k=1/Math.hypot(x,y);x*=k;y*=k}}return [G.cx+G.r*x,G.cy-G.r*y,z]}};
X.beginPath();X.arc(G.cx,G.cy,G.r,0,7);const g=X.createRadialGradient(G.cx-120,G.cy-150,30,G.cx,G.cy,G.r);g.addColorStop(0,"#191d24");g.addColorStop(1,"#0c0e11");X.fillStyle=g;X.fill();X.save();X.clip();
X.strokeStyle="rgba(227,230,233,.07)";X.lineWidth=1;
for(let lo=-180;lo<180;lo+=20){{X.beginPath();let d=0;for(let la=-90;la<=90;la+=3){{const[a,b,z]=P(lo,la);if(z<0){{d=0;continue}}d?X.lineTo(a,b):X.moveTo(a,b);d=1}}X.stroke()}}
for(let la=-60;la<=70;la+=10){{X.beginPath();let d=0;for(let lo=-180;lo<=180;lo+=3){{const[a,b,z]=P(lo,la);if(z<0){{d=0;continue}}d?X.lineTo(a,b):X.moveTo(a,b);d=1}}X.stroke()}}
X.fillStyle="#242932";for(const r of land){{let any=0;X.beginPath();r.forEach(([lo,la],i)=>{{const[a,b,z]=P(lo,la,1);if(z>=0)any=1;i?X.lineTo(a,b):X.moveTo(a,b)}});X.closePath();if(any)X.fill()}}
X.restore();X.strokeStyle="rgba(227,230,233,.24)";X.lineWidth=1.2;X.beginPath();X.arc(G.cx,G.cy,G.r,0,7);X.stroke();
document.title="ready";
</script>"""
digits = f"""<!doctype html><meta charset=utf-8><style>{fonts}
body {{ background: #000; color: #fff; display: flex; }}
span {{ width: {CELL[0]}px; height: {CELL[1]}px; font: 500 60px/{CELL[1]}px JB; text-align: center; letter-spacing: -.02em; }}
</style>{''.join(f'<span>{c}</span>' for c in CHARS)}"""


def shot(html, path, size):
    with tempfile.NamedTemporaryFile("w", suffix=".html", delete=False, dir="/tmp") as f:
        f.write(html)
    with tempfile.TemporaryDirectory() as profile:
        subprocess.run(["google-chrome", "--headless=new", "--no-sandbox", "--disable-gpu", "--hide-scrollbars", "--allow-file-access-from-files", f"--user-data-dir={profile}", f"--window-size={size[0]},{size[1]}", "--virtual-time-budget=4000", f"--screenshot={path}", f"file://{f.name}"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=90)
    os.remove(f.name)


with tempfile.TemporaryDirectory() as tmp:
    a = os.path.join(tmp, "base.png")
    shot(card, a, (W, H))
    base = Image.open(a).convert("RGB")
    assert base.size == (W, H), base.size
    with open(os.path.join(OUT, "og-base.bin"), "wb") as f:
        f.write(zlib.compress(base.tobytes(), 9))
    b = os.path.join(tmp, "digits.png")
    shot(digits, b, (CELL[0] * len(CHARS), CELL[1]))
    atlas = Image.open(b).convert("L")
glyphs = {}
for i, ch in enumerate(CHARS):
    cell = atlas.crop((i * CELL[0], 0, (i + 1) * CELL[0], CELL[1]))
    box = cell.getbbox() or (0, 0, CELL[0], CELL[1])
    x0, x1 = box[0], box[2]
    columns = x1 - x0
    glyphs[ch] = {"w": columns, "advance": 37 if ch != "," else 26, "x": x0, "a": base64.b64encode(zlib.compress(cell.crop((x0, 0, x1, CELL[1])).tobytes(), 9)).decode()}
json.dump({"width": W, "height": H, "globe": GLOBE, "stats": STATS, "cell_h": CELL[1], "glyphs": glyphs}, open(os.path.join(OUT, "og-assets.json"), "w"), separators=(",", ":"))
print("og-base.bin", os.path.getsize(os.path.join(OUT, "og-base.bin")), "og-assets.json", os.path.getsize(os.path.join(OUT, "og-assets.json")))
