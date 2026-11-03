#!/bin/sh
# Build the uSDR server for the radio host and (re)start it there.
#
# The server runs as a systemd daemon in CT 216 ("rsp", Debian 13) beside the
# RSP1A; the desktop app runs elsewhere and connects to it. The container's
# glibc is older than a current desktop distribution's, so the binary is built
# in a Debian 13 podman container rather than on this host.
#
#   tools/deploy-server.sh          build, install, restart
#   tools/deploy-server.sh --no-build   install what target/trixie already has
#
# The first run also creates the usdr user and seeds its settings from this
# host's ~/.config/usdr (minus the SoapyRemote line: the radio is local
# there). Later runs never touch the daemon's settings.
set -eu
host=${USDR_HOST:-root@172.16.0.216}
root=$(cd "$(dirname "$0")/.." && pwd)
bin="$root/target/trixie/release/usdr"

if [ "${1:-}" != "--no-build" ]; then
    podman run --rm \
        -v "$root":/src:Z \
        -v "$HOME/.cargo/registry":/usr/local/cargo/registry:Z \
        -e CARGO_TARGET_DIR=/src/target/trixie \
        -w /src docker.io/library/rust:1.93-trixie \
        sh -c 'apt-get update -qq && apt-get install -y -qq libsoapysdr-dev pkg-config >/dev/null &&
               cargo build --release -p usdr-server'
fi

scp -q "$bin" "$host:/usr/local/bin/usdr.new"
scp -q "$root/crates/server/packaging/usdr.service" "$host:/etc/systemd/system/usdr.service"

# Seed the daemon's settings once.
if ! ssh "$host" test -e /var/lib/usdr/.config/usdr/usdr.toml; then
    seed=$(mktemp -d)
    trap 'rm -rf "$seed"' EXIT
    for f in usdr.toml devices.toml; do
        [ -e "$HOME/.config/usdr/$f" ] && grep -v '^remotes *=' "$HOME/.config/usdr/$f" > "$seed/$f"
    done
    ssh "$host" 'id usdr >/dev/null 2>&1 || useradd --system --home-dir /var/lib/usdr --shell /usr/sbin/nologin usdr
                 install -d -o usdr -g usdr /var/lib/usdr /var/lib/usdr/.config /var/lib/usdr/.config/usdr'
    for f in "$seed"/*; do
        [ -e "$f" ] && scp -q "$f" "$host:/var/lib/usdr/.config/usdr/"
    done
    ssh "$host" 'chown -R usdr:usdr /var/lib/usdr'
fi

ssh "$host" 'mv /usr/local/bin/usdr.new /usr/local/bin/usdr && chmod 755 /usr/local/bin/usdr &&
             systemctl daemon-reload && systemctl enable -q usdr && systemctl restart usdr'

# Up when the API answers.
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if curl -sf -m 2 "http://${host#*@}:8073/api/version" >/dev/null; then
        echo "uSDR server running on http://${host#*@}:8073"
        exit 0
    fi
    sleep 1
done
echo "uSDR server did not answer; see: ssh $host journalctl -u usdr -n 50" >&2
exit 1
