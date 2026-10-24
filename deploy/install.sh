#!/usr/bin/env bash
# Install or update the Cuenca authority and stock Samba on an Ubuntu 24.04 host on the
# tailnet (spec rev 2.2, section 2a; BRO-3257). Idempotent: run it again after any change
# to this directory or to the binary. Run as root. See deploy/README.md.
#
#   sudo bash deploy/install.sh --binary ./cuenca --allow 100.82.213.74 --allow 100.71.190.125
#
# --binary PATH   the `cuenca` binary to install as /usr/local/bin/cuenca (optional on a re-run)
# --allow IP      an enrolled owner device's tailnet address (repeat; at least one)
# --bind ADDR     the agent API's address (default: `tailscale ip -4`, port 7419)
# --volume DIR    the volume Samba serves (default /srv/cuenca/volume)
# --retire-spike  remove the spike C container `cuenca-spike` (BRO-3234) before smbd binds :445
# --no-start      write everything but start nothing (containers without systemd; CI)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
binary=""
allow=()
bind=""
volume=/srv/cuenca/volume
home=/var/lib/cuenca
state=/var/lib/cuenca/state
retire_spike=0
start=1

die() { echo "install.sh: $*" >&2; exit 1; }
say() { echo "install.sh: $*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --binary) binary=$2; shift 2 ;;
        --allow) allow+=("$2"); shift 2 ;;
        --bind) bind=$2; shift 2 ;;
        --volume) volume=$2; shift 2 ;;
        --retire-spike) retire_spike=1; shift ;;
        --no-start) start=0; shift ;;
        *) die "unknown argument: $1" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "run as root"
[ ${#allow[@]} -gt 0 ] || die "name at least one enrolled owner device with --allow <tailnet address>"
for ip in "${allow[@]}"; do
    [[ $ip =~ ^100\.([0-9]{1,3})\.[0-9]{1,3}\.[0-9]{1,3}$ ]] && [ "${BASH_REMATCH[1]}" -ge 64 ] \
        && [ "${BASH_REMATCH[1]}" -le 127 ] || die "--allow $ip is not a tailnet IPv4 address (100.64.0.0/10)"
done
case "$volume" in /*) ;; *) die "--volume must be an absolute path" ;; esac
if [ -n "$binary" ]; then
    [ -f "$binary" ] && [ -x "$binary" ] || die "--binary $binary is not an executable file"
elif [ ! -x /usr/local/bin/cuenca ]; then
    die "no /usr/local/bin/cuenca yet: give --binary"
fi
if [ -z "$bind" ]; then
    command -v tailscale >/dev/null || die "tailscale is not installed; give --bind"
    ip4=$(tailscale ip -4 | head -n1)
    [ -n "$ip4" ] || die "tailscale has no IPv4 address; is the host on the tailnet?"
    bind="$ip4:7419"
fi

# Packages: Ubuntu's own. No community images, no third-party repositories.
missing=()
# samba-vfs-modules holds fruit and streams_xattr; it is only a Recommends of samba.
for p in samba samba-vfs-modules git; do dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p"); done
if [ ${#missing[@]} -gt 0 ]; then
    say "installing ${missing[*]} from Ubuntu's archive"
    DEBIAN_FRONTEND=noninteractive apt-get update -q
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends "${missing[@]}"
fi

# Accounts. cuenca runs the authority; cuenca-owner is Samba's one account. Neither has a
# shell. Group cuenca holds those two and nobody else: no agent account may read the volume.
getent group cuenca >/dev/null || groupadd --system cuenca
id cuenca >/dev/null 2>&1 || useradd --system --gid cuenca --home-dir "$home" --no-create-home \
    --shell /usr/sbin/nologin cuenca
id cuenca-owner >/dev/null 2>&1 || useradd --system --gid cuenca --home-dir /nonexistent \
    --no-create-home --shell /usr/sbin/nologin cuenca-owner
gid=$(getent group cuenca | cut -d: -f3)
extra=$(getent group cuenca | cut -d: -f4 | tr ',' '\n' | grep -vxE 'cuenca|cuenca-owner|' || true)
primary=$(getent passwd | awk -F: -v g="$gid" '$4 == g {print $1}' | grep -vxE 'cuenca|cuenca-owner' || true)
[ -z "$extra$primary" ] || die "group cuenca has other members ($(echo $extra $primary)); remove them first"

# Kernel: a hard link into main/ must be refused (spec 2a).
[ "$(cat /proc/sys/fs/protected_hardlinks 2>/dev/null || echo 0)" = 1 ] \
    || die "fs.protected_hardlinks is not 1"

# Directories. The authority's state is its own; the volume's modes are set by the
# authority itself on start (main/ and forks/ 2750, owner forks 2770).
install -d -o cuenca -g cuenca -m 0750 "$home"
install -d -o cuenca -g cuenca -m 0700 "$state"
install -d -o root -g root -m 0755 "$(dirname "$volume")"
install -d -o cuenca -g cuenca -m 2750 "$volume"
fstype=$(stat -f -c %T "$volume")
case "$fstype" in
    ext2/ext3|btrfs|xfs|tmpfs) ;;
    *) say "warning: the volume is on $fstype; the materialiser needs renameat2 exchange (ext4, btrfs, xfs)" ;;
esac

# Files.
if [ -n "$binary" ]; then install -o root -g root -m 0755 "$binary" /usr/local/bin/cuenca; fi
install -d -o root -g root -m 0755 /usr/local/lib/cuenca
install -o root -g root -m 0755 "$here/wait-tailnet" /usr/local/lib/cuenca/wait-tailnet
install -d -o root -g root -m 0755 /etc/cuenca
env_tmp=$(mktemp)
printf 'CUENCA_STATE=%s\nCUENCA_BIND=%s\nCUENCA_VOLUME=%s\n' "$state" "$bind" "$volume" >"$env_tmp"
install -o root -g cuenca -m 0640 "$env_tmp" /etc/cuenca/cuenca.env
rm -f "$env_tmp"
install -d -o root -g root -m 0755 /etc/systemd/system/smbd.service.d
sed "s|@VOLUME@|$volume|g" "$here/cuenca.service.in" >/etc/systemd/system/cuenca.service
chmod 0644 /etc/systemd/system/cuenca.service
install -o root -g root -m 0644 "$here/smbd-cuenca.conf" /etc/systemd/system/smbd.service.d/cuenca.conf

# smb.conf: rendered, checked by testparm, then installed. Ubuntu's own file is kept once.
smb_tmp=$(mktemp)
sed -e "s|@VOLUME@|$volume|g" -e "s|@HOSTS_ALLOW@|${allow[*]}|g" "$here/smb.conf.in" >"$smb_tmp"
testparm -s "$smb_tmp" >/dev/null 2>&1 || { testparm -s "$smb_tmp" >&2 || true; rm -f "$smb_tmp"; die "testparm refused the rendered smb.conf"; }
if [ -f /etc/samba/smb.conf ] && [ ! -f /etc/samba/smb.conf.pre-cuenca ] \
    && ! grep -q "^# Cuenca's volume over stock Samba" /etc/samba/smb.conf; then
    cp -p /etc/samba/smb.conf /etc/samba/smb.conf.pre-cuenca
fi
install -o root -g root -m 0644 "$smb_tmp" /etc/samba/smb.conf
rm -f "$smb_tmp"

# The owner's Samba password: generated here once, never printed. Hand it over out of band
# from /root/cuenca-owner.smbpass (root only), then delete that file.
if ! pdbedit -L -u cuenca-owner >/dev/null 2>&1; then
    (
        umask 077
        pw=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | head -c 24)
        printf '%s\n' "$pw" >/root/cuenca-owner.smbpass
        printf '%s\n%s\n' "$pw" "$pw" | smbpasswd -s -a cuenca-owner >/dev/null
    )
    say "cuenca-owner's Samba password is in /root/cuenca-owner.smbpass (root only)"
fi

if [ "$start" = 0 ]; then
    say "installed; --no-start, so nothing was started"
    exit 0
fi

if [ "$retire_spike" = 1 ] && command -v docker >/dev/null \
    && docker ps -a --format '{{.Names}}' | grep -qx cuenca-spike; then
    say "removing the spike container cuenca-spike (BRO-3234)"
    docker rm -f cuenca-spike >/dev/null
fi
if ss -Hltnp 'sport = :445' | grep -qv smbd; then
    die "something other than smbd listens on :445 ($(ss -Hltnp 'sport = :445' | head -n1)); --retire-spike removes the spike"
fi

systemctl daemon-reload
for unit in nmbd samba-ad-dc; do
    if systemctl list-unit-files "$unit.service" >/dev/null 2>&1; then
        systemctl disable --now "$unit" >/dev/null 2>&1 || true
        systemctl mask "$unit" >/dev/null 2>&1 || true
    fi
done
systemctl enable smbd cuenca >/dev/null 2>&1
systemctl restart smbd
systemctl restart cuenca
for unit in smbd cuenca; do
    for _ in $(seq 1 30); do systemctl is-active --quiet "$unit" && break; sleep 1; done
    systemctl is-active --quiet "$unit" || die "$unit did not start; journalctl -u $unit"
done
say "smbd and cuenca are active; the agent API is on $bind; run deploy/check.sh"
