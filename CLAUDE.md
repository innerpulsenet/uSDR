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

## After changing the desktop app
`sh crates/desktop/packaging/linux/install.sh` rebuilds it and installs it in
`~/.local/bin`. Don't restart a desktop window Patrick has open; tell him to
reopen it.

## Running a server from this checkout
Only one process can hold the RSP. Stop the daemon first
(`ssh root@172.16.0.216 systemctl stop usdr`), run
`./target/release/usdr` (it reaches the RSP over SoapyRemote, per
`~/.config/usdr/usdr.toml`), and start the daemon again afterwards.

## Access
The server has no authentication. The container's nftables `clients` set
(`/etc/nftables.conf` in CT 216) is the only access control, and it admits
SCRIBE (`172.16.0.50`). Add a machine's address there before it can connect.
