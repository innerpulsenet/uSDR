#!/bin/sh
# Copies site/ to arm2: pages to ~/www/hf, the backend to ~/hf-site (outside the
# web root), and restarts the ingest API. Generated JSON and received pictures
# on the host are left alone.
set -eu
cd "$(dirname "$0")/.."
python3 tools/gen-site-map.py
python3 site/bin/test_hf_api.py 2>/dev/null
ssh arm2 'mkdir -p ~/www/hf/data ~/www/hf/pictures ~/hf-site/bin'
rsync -a site/index.html site/css site/js arm2:www/hf/
rsync -a site/data/coast-110m.json site/data/ground-stations.json arm2:www/hf/data/
rsync -a --exclude test_hf_api.py --exclude __pycache__ site/bin/ arm2:hf-site/bin/
ssh arm2 'python3 -m py_compile ~/hf-site/bin/*.py && rm -rf ~/hf-site/bin/__pycache__ && sudo -n systemctl restart hf-site-api && sudo -n systemctl start hf-site-warm'
echo deployed
