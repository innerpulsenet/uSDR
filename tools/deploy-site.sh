#!/bin/sh
# Copies site/ to arm2: pages to ~/www/hf, the backend to ~/hf-site (outside the
# web root), and restarts the ingest API. Generated JSON and received pictures
# on the host are left alone.
set -eu
cd "$(dirname "$0")/.."
python3 tools/gen-site-map.py
python3 site/bin/test_hf_api.py 2>/dev/null
ssh arm2 'mkdir -p ~/www/hf/data ~/www/hf/pictures ~/hf-site/bin'
# Browsers can be told by the CDN to keep scripts and styles for hours, so their
# URLs carry a hash of the site: a deploy that changes anything gets new URLs, and
# the page itself is never cached. The copy in the repo stays unstamped.
version=$(find site/index.html site/css site/js site/brand -type f | LC_ALL=C sort | xargs cat | sha256sum | cut -c1-10)
stamped=$(mktemp -d)
trap 'rm -rf "$stamped"' EXIT
cp -r site/index.html site/css site/js "$stamped/"
sed -i -E "s#href=\"((brand/[^\"]+|css/hf)\.css)\"#href=\"\1?v=$version\"#; s#src=\"js/app\.js\"#src=\"js/app.js?v=$version\"#" "$stamped/index.html"
sed -i -E "s#from \"(\./[a-z]+\.js)\"#from \"\1?v=$version\"#" "$stamped"/js/*.js
test "$(grep -c "?v=$version" "$stamped/index.html")" -eq 5
rsync -a "$stamped/index.html" "$stamped/css" "$stamped/js" arm2:www/hf/
rsync -a site/favicon.ico site/favicon.svg site/apple-touch-icon.png site/brand arm2:www/hf/
rsync -a site/data/coast-110m.json site/data/land-110m.json site/data/coast-50m.json site/data/land-50m.json site/data/lookups-site.json site/data/ground-stations.json arm2:www/hf/data/
rsync -a --exclude test_hf_api.py --exclude __pycache__ site/bin/ arm2:hf-site/bin/
ssh arm2 'python3 -m py_compile ~/hf-site/bin/*.py && rm -rf ~/hf-site/bin/__pycache__ && sudo -n systemctl restart hf-site-api && sudo -n systemctl start hf-site-warm'
echo deployed
