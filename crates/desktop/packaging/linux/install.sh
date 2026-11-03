#!/bin/sh
# Build uSDR desktop and install it for this user: the binary in
# ~/.local/bin, the launcher and its icons under ~/.local/share. The window
# finds its icon through the launcher's name, which Wayland matches against
# the app id (net.innerpulse.usdr-desktop).
#
#   install.sh             build and install
#   install.sh --uninstall remove what install.sh put in place
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
id=net.innerpulse.usdr-desktop
bin="$HOME/.local/bin/usdr-desktop"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor"

refresh() {
    command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
    command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin" "$apps/$id.desktop" "$icons/scalable/apps/$id.svg"
    for n in 16 24 32 48 64 128 256 512; do rm -f "$icons/${n}x${n}/apps/$id.png"; done
    refresh
    echo "removed uSDR desktop"
    exit 0
fi

cargo build --release -p usdr-desktop --manifest-path "$root/Cargo.toml"
install -Dm755 "$root/target/release/usdr-desktop" "$bin"
for n in 16 24 32 48 64 128 256 512; do
    install -Dm644 "$here/../../assets/icon/usdr-$n.png" "$icons/${n}x${n}/apps/$id.png"
done
install -Dm644 "$here/../../assets/icon/usdr-icon.svg" "$icons/scalable/apps/$id.svg"
# The launcher names the binary by its full path: a desktop session's PATH
# does not always include ~/.local/bin.
mkdir -p "$apps"
sed "s|^Exec=usdr-desktop|Exec=$bin|" "$here/$id.desktop" > "$apps/$id.desktop"
chmod 644 "$apps/$id.desktop"
refresh
echo "installed: $bin and the uSDR launcher"
