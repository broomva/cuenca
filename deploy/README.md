# Deploying the authority

One Ubuntu 24.04 host on the tailnet runs the Cuenca authority (`cuenca serve`) and stock Samba, which serves the authority's volume to the owner's Finder and Files app. That host is the NUC once it is a Linux node (BRO-3209). The spec, rev 2.2 §2a, sets every Samba setting used here. This directory holds:

| File | What it is |
|---|---|
| `install.sh` | Installs or updates everything, idempotently. Run it as root |
| `check.sh` | Checks an installed host (one line per check; exit 1 if any fails) |
| `cuenca.service.in` | The authority's systemd unit, run as `cuenca:cuenca` |
| `smbd-cuenca.conf` | A drop-in for Ubuntu's `smbd.service`: start only once `tailscale0` has its address |
| `smb.conf.in` | Samba's configuration: tailnet only, one account, server-side modes, `fruit` + `streams_xattr` |
| `tailnet-addr.sh` | Sourced by both scripts: an `--allow` / `hosts allow` address must be one tailnet address, written canonically |
| `wait-tailnet` | Waits for `tailscale0`'s IPv4 address (both units run it before starting) |
| `test-container.sh` | CI: install twice in `ubuntu:24.04`, check that nothing moved, and check that `check.sh` catches each planted fault |
| `test-systemd.sh` | Before changing the units: the real binary under Ubuntu's systemd in a privileged container, with a dummy `tailscale0`. It runs the full `check.sh`, shows that Samba refuses the host's own address (with a control that logs in as `cuenca-owner` and finds `main/` read-only), restarts after a kill, and shows that `smbd` waits for `tailscale0` |

## What the host looks like

| Path | Owner, mode | What |
|---|---|---|
| `/usr/local/bin/cuenca` | root, 0755 | The binary (`cuenca serve` is the authority) |
| `/var/lib/cuenca/state/` | cuenca, 0700 | The log, blobs, keys file and landing's git repositories |
| `/var/lib/cuenca/` | cuenca, 0750 | The authority user's home (`gh`'s login lives under it) |
| `/srv/cuenca/volume/` | cuenca:cuenca, 2750 | The volume: `main/<location>/` and `forks/<fork>/` read-only, `forks/owner-*/` writable by `cuenca-owner` |
| `/run/cuenca/admin.sock` | cuenca, dir 0700 | Owner acts (`cuenca admin`) |
| `/etc/cuenca/cuenca.env` | root:cuenca, 0640 | `CUENCA_STATE`, `CUENCA_BIND` (tailnet address, port 7419), `CUENCA_VOLUME` |

There are two accounts, and neither has a shell. `cuenca` runs the authority. `cuenca-owner` is Samba's one account. Both are in group `cuenca`, and nobody else is: `install.sh` and `check.sh` refuse a third member, so no agent account can read the volume. The volume's filesystem must support `renameat2` exchange: ext4, btrfs or xfs.

## Install

1. Put the host on the tailnet (`tailscale up`), and note both tailnet addresses (IPv4 and IPv6) of each of the owner's enrolled devices, the Mac and the iPhone: `tailscale ip <device>` prints them. A device can reach Samba over either one (a MagicDNS name resolves to both), so name both.
2. Build the binary for the host's architecture (`linux/amd64` for the NUC; a Mac with Apple silicon builds `arm64` unless told), from a checkout at the commit to deploy:
   ```bash
   docker run --rm --platform linux/amd64 -v "$PWD":/src:ro -v "$PWD/target-linux":/target -w /src -e CARGO_TARGET_DIR=/target \
     rust:1 cargo build --release --locked -p cuenca-cli        # → target-linux/release/cuenca
   file target-linux/release/cuenca                             # x86-64 for the NUC
   ```
3. Install, naming each enrolled device:
   ```bash
   sudo bash deploy/install.sh --binary ./cuenca --allow <mac ipv4> --allow <mac ipv6> --allow <iphone ipv4> --allow <iphone ipv6>
   ```
   The script installs `samba`, `samba-vfs-modules` (which holds `fruit` and `streams_xattr`), `git`, `gh` and `curl` from Ubuntu's archive, holding their services until Cuenca's `smb.conf` is in place, creates the accounts and directories, renders the unit and `smb.conf` (it checks `smb.conf` with `testparm` first, and keeps Ubuntu's own file once as `smb.conf.pre-cuenca`), masks `nmbd`, and starts `smbd` and `cuenca`. The host's `smb.conf` becomes Cuenca's alone: any other share on it is dropped, so use a host that serves nothing else over SMB. Each `--allow` address must be written canonically (as `tailscale ip` prints it), and `--volume` must be a new directory outside `/var/lib/cuenca`. On a host that still runs spike C, add `--retire-spike` to remove the `cuenca-spike` container, which holds `:445`.
4. Hand the owner the Samba password, out of band. The script generates it once and never prints it. It is in `/root/cuenca-owner.smbpass` (root only). Read it on the host, store it in the owner's password manager, then `sudo rm /root/cuenca-owner.smbpass`.
5. Log the authority's `gh` in for landing (the shared login, for now; spec rev 2.2 §6):
   ```bash
   sudo -u cuenca -H gh auth login --with-token < token-file
   ```
6. Check it: `sudo bash deploy/check.sh` (all lines `ok`). Then, from outside the host:
   ```bash
   nc -z -w 3 <tailnet ip> 445 && echo open        # from an enrolled device: open
   nc -z -w 3 <public ip> 445 || echo closed       # from anywhere: closed
   ```
   In Finder, Go → Connect to Server → `smb://<tailnet ip>/cuenca`, as `cuenca-owner`.

Re-running `install.sh` with the same arguments changes nothing except restarting both services. Without `--binary` it keeps the installed binary.

## Operate

- **Owner acts** run on the host, as the authority's user:
  ```bash
  sudo -u cuenca cuenca admin --socket /run/cuenca/admin.sock key mint <principal>
  sudo -u cuenca cuenca admin --socket /run/cuenca/admin.sock location add <name> --git <url>   # or --dir <path>
  ```
- **Logs:** `journalctl -u cuenca -f`, `journalctl -u smbd`, and `/var/log/samba/`.
- **Enroll a device:** re-run `install.sh` with its two addresses added as `--allow`.
- **Remove a device:** re-run `install.sh` without its two `--allow`, which restarts `smbd` and so ends every live session. A session from an address no longer allowed cannot reconnect. Revoke the device's agent keys too, if it had any (`key revoke`).
- **Upgrade:** build the new binary, then `install.sh --binary ./cuenca <same --allow list>`.

## Recover

- **A service is down:** `systemctl status cuenca smbd`. Both wait up to 120 s for `tailscale0` at start, and both are `Restart=on-failure`, so once `tailscale up` gives `tailscale0` its address they come back within one restart interval.
- **After a reboot:** both units are enabled and come back on their own. `check.sh` confirms it.
- **The volume looks wrong:** the volume is derived from the log. Stop `cuenca`, move the volume aside, and start `cuenca`. It rebuilds `main/` and every agent fork. Owner forks are written from Finder, so move them back from the old volume before deleting it, and the next snapshot picks them up.
- **Lost state:** restore the log and blobs from backup (BRO-3258).
