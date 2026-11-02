#!/bin/sh
# Build uSDR desktop and install it for this user: the binary in
# ~/.local/bin, the launcher and its icons under ~/.local/share. The window
# finds its icon through the launcher's name, which Wayland matches against
# the app id (net.innerpulse.usdr-desktop).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
id=net.innerpulse.usdr-desktop
cargo build --release -p usdr-desktop --manifest-path "$root/Cargo.toml"
install -Dm755 "$root/target/release/usdr-desktop" "$HOME/.local/bin/usdr-desktop"
for n in 16 24 32 48 64 128 256 512; do
    install -Dm644 "$here/../../assets/icon/usdr-$n.png" "$HOME/.local/share/icons/hicolor/${n}x${n}/apps/$id.png"
done
install -Dm644 "$here/../../assets/icon/usdr-icon.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/$id.svg"
install -Dm644 "$here/$id.desktop" "$HOME/.local/share/applications/$id.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$HOME/.local/share/applications" || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo "installed: ~/.local/bin/usdr-desktop and the uSDR launcher"
