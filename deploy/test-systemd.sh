#!/usr/bin/env bash
# Run install.sh and the full check.sh under real systemd, with a real `cuenca` binary built
# for Linux, in a privileged ubuntu:24.04 container with Ubuntu's systemd. tailscale0 is a
# dummy interface at 100.64.0.1 and `tailscale` a stub that reports it. Not in CI (it needs
# --privileged and a Linux binary); run it before changing the units:
#   bash deploy/test-systemd.sh <path to a Linux cuenca binary>
set -euo pipefail
binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
here=$(cd "$(dirname "$0")" && pwd)
name=cuenca-deploy-test-$$
img=cuenca-deploy-test:local
docker build -q -t "$img" - >/dev/null <<'DOCKERFILE'
FROM ubuntu:24.04
RUN apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends \
    systemd systemd-sysv iproute2 curl smbclient ca-certificates
CMD ["/sbin/init"]
DOCKERFILE
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker run -d --name "$name" --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    --tmpfs /run --tmpfs /run/lock --tmpfs /srv/cuenca:exec,mode=755 \
    -v "$here":/deploy:ro -v "$binary":/cuenca-bin:ro "$img" >/dev/null
on_host() { docker exec "$name" bash -euo pipefail -c "$1"; }
for _ in $(seq 1 30); do on_host 'systemctl is-system-running 2>/dev/null | grep -qE "running|degraded"' && break; sleep 1; done
on_host 'ip link add tailscale0 type dummy && ip addr add 100.64.0.1/32 dev tailscale0 && ip link set tailscale0 up
    printf "#!/bin/sh\n[ \"\$1 \$2\" = \"ip -4\" ] && echo 100.64.0.1 && exit 0\necho stub >&2; exit 1\n" >/usr/local/bin/tailscale
    chmod 755 /usr/local/bin/tailscale'
on_host 'bash /deploy/install.sh --binary /cuenca-bin --allow 100.82.213.74 --allow 100.71.190.125' \
    | grep -v -E '^(Get|Selecting|Preparing|Unpacking|Setting up|Processing|Fetched|debconf|Created symlink|\(Reading)' || true
on_host 'bash /deploy/check.sh'
echo "--- the volume, as the authority made it"
on_host 'sleep 3; stat -c "%a %U:%G %n" /srv/cuenca/volume /srv/cuenca/volume/main /srv/cuenca/volume/forks /srv/cuenca/volume/.cuenca'
echo "--- the authority's own host is refused by Samba (hosts allow names only enrolled devices)"
on_host 'if smbclient -N -L //100.64.0.1 -m SMB3 >/tmp/smb 2>&1; then cat /tmp/smb; exit 1; fi
    grep -m1 "Denied connection from 100.64.0.1" /var/log/samba/log.smbd'
echo "--- control: with the host's own address allowed, cuenca-owner logs in; main/ and the root are read-only, .cuenca/ cannot be entered"
on_host 'bash /deploy/install.sh --allow 100.64.0.1 >/dev/null
    smb() { smbclient //100.64.0.1/cuenca -m SMB3 -U "cuenca-owner%$(cat /root/cuenca-owner.smbpass)" -c "$1" 2>&1 || true; }
    smb "ls" | grep -E "^  (main|forks) "
    smb "mkdir x" | grep NT_STATUS_ACCESS_DENIED
    smb "mkdir main/x" | grep NT_STATUS_ACCESS_DENIED
    smb "ls .cuenca/*" | grep NT_STATUS_ACCESS_DENIED
    if smbclient -N -L //100.64.0.1 -m SMB3 >/tmp/smb 2>&1; then echo "a guest listed shares"; exit 1; fi
    grep -E "NT_STATUS_(ACCESS_DENIED|LOGON_FAILURE)" /tmp/smb | head -n1
    bash /deploy/install.sh --allow 100.82.213.74 --allow 100.71.190.125 >/dev/null'
echo "--- the authority comes back after being killed (Restart=on-failure)"
on_host 'pid=$(systemctl show -p MainPID --value cuenca); kill -9 "$pid"; sleep 8; systemctl is-active cuenca
    [ "$(systemctl show -p MainPID --value cuenca)" != "$pid" ]'
echo "--- a second install keeps everything working"
on_host 'bash /deploy/install.sh --allow 100.82.213.74 --allow 100.71.190.125 >/dev/null && bash /deploy/check.sh | tail -n1'
echo "--- without tailscale0, smbd does not start (it waits, then fails)"
on_host 'ip link del tailscale0; systemctl set-environment CUENCA_WAIT_SECS=3
    if systemctl restart smbd 2>/dev/null; then echo "smbd started without tailscale0"; exit 1; fi
    journalctl -u smbd --no-pager | grep -m1 "wait-tailnet"'
echo "test-systemd: all passed"
