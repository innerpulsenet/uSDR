# uSDR — working rules

## Where things run
- **Server** (`crates/server`, with `engine`, `radio`, `proto`, `dsp`): a systemd
  daemon, unit `usdr`, in CT 216 (`rsp`, `172.16.0.216`, Debian 13) on vault,
  beside the RSP1A on that container's own USB. Clients connect to
  `172.16.0.216:8073`.
- **Desktop app** (`crates/desktop`): runs on the machine you're working on
  (SCRIBE now, a Windows 11 PC later) and connects to the daemon.
- One workspace on purpose: `proto` is the wire format both sides share, so a
  protocol change lands on both in one commit.

## After changing server-side code
Run `tools/deploy-server.sh` once tests pass. It builds in a `rust:1.93-trixie`
podman container (the container's glibc is older than SCRIBE's, so a binary
built here won't run there), copies the binary and
`crates/server/packaging/usdr.service`, and restarts the unit. It never touches
the daemon's settings (`/var/lib/usdr/.config/usdr/`). Then check it:
`usdr-desktop --probe 172.16.0.216:8073`, and
`ssh root@172.16.0.216 journalctl -u usdr -n 50` on failure.

Server-side means anything under `crates/server`, `engine`, `radio`, `dsp`, or
`proto`. A `proto` change also means rebuilding the desktop app.

After touching anything that moves the tuner (the LO offset, the mode, rate or
tune handling in `crates/server/src/sdr.rs`, `crates/radio/src/device.rs`), run
`tools/tune-roundtrip.py` against the deployed daemon: it hops to 80 m and back
several ways and checks the broadcast stations still sit on their raster. The
radio and the driver are what strand a tune, so no unit test sees it. It
retunes the receiver and leaves it on 100.5 MHz WFM.

## After changing the desktop app
`sh crates/desktop/packaging/linux/install.sh` rebuilds it and installs it in
`~/.local/bin`. Don't restart a desktop window Patrick has open; tell him to
reopen it.

To look at the running app without touching his: a copy of
`~/.config/usdr-desktop/desktop.toml` with `muted = true`, then
`USDR_DESKTOP_PREFS=<copy> USDR_QA_SIZE=1600x980 USDR_QA_FPS=1 usdr-desktop
172.16.0.216:8073`. `USDR_QA_FPS` prints, every two seconds it paints, the
frame rate, the audio queue and underruns, what asked for motion, and how often
the hidden-window loop release ran. A window that opens under a fullscreen
window gets no frames from the compositor and prints nothing.

## Running a server from this checkout
Only one process can hold the RSP. Stop the daemon first
(`ssh root@172.16.0.216 systemctl stop usdr`), run
`./target/release/usdr` (it reaches the RSP over SoapyRemote, per
`~/.config/usdr/usdr.toml`), and start the daemon again afterwards.

## Access
The server has no authentication. The container's nftables policy
(`/etc/nftables.conf` in CT 216) is the only access control. The `clients` set
(SCRIBE, `172.16.0.50`) gets every port, including SSH and SoapyRemote. The
`viewers` set (the Windows 11 PC, `172.16.0.52`) gets port 8073 only. A new
machine that just runs the app or the page goes in `viewers`. Patrick has
asked Claude to make these firewall edits itself; check the file with `nft -c`
before loading it.
