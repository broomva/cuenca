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
# The image ships a policy-rc.d; remove it so install.sh's own hold of the stock starts runs.
on_host 'rm -f /usr/sbin/policy-rc.d
    sysctl -qw net.ipv6.conf.all.disable_ipv6=0 net.ipv6.conf.default.disable_ipv6=0
    ip link add tailscale0 type dummy && ip addr add 100.64.0.1/32 dev tailscale0
    ip -6 addr add fd7a:115c:a1e0::1/128 dev tailscale0 nodad && ip link set tailscale0 up
    printf "#!/bin/sh\ncase \"\$1 \$2\" in \"ip -4\") echo 100.64.0.1;; \"ip -6\") echo fd7a:115c:a1e0::1;; *) echo stub >&2; exit 1;; esac\n" >/usr/local/bin/tailscale
    chmod 755 /usr/local/bin/tailscale'
allow="--allow 100.82.213.74 --allow fd7a:115c:a1e0::2 --allow 100.71.190.125"
on_host "bash /deploy/install.sh --binary /cuenca-bin $allow" \
    | grep -v -E '^(Get|Selecting|Preparing|Unpacking|Setting up|Processing|Fetched|debconf|Created symlink|\(Reading)' || true
on_host 'printf "ghp_TESTONLY0123456789\n" >/token-file
    { printf "GH_TOKEN="; cat /token-file; } | install -o root -g cuenca -m 0640 /dev/stdin /etc/cuenca/gh.env
    systemctl restart cuenca; sleep 2'
on_host 'bash /deploy/check.sh'
echo "--- the authority reads landing's token from /etc/cuenca/gh.env (README step 5)"
on_host 'tr "\0" "\n" </proc/$(systemctl show -p MainPID --value cuenca)/environ | grep -qx GH_TOKEN=ghp_TESTONLY0123456789 && echo "GH_TOKEN is in the authority environment"'
echo "--- install.sh removed the policy-rc.d it wrote to hold the stock starts"
on_host '[ ! -e /usr/sbin/policy-rc.d ] && echo "no policy-rc.d left"'
echo "--- the volume, as the authority made it"
on_host 'sleep 3; stat -c "%a %U:%G %n" /srv/cuenca/volume /srv/cuenca/volume/main /srv/cuenca/volume/forks /srv/cuenca/volume/.cuenca'
echo "--- the authority's own host is refused by Samba over both addresses (hosts allow names only enrolled devices)"
on_host 'for a in 100.64.0.1 fd7a:115c:a1e0::1; do
        if smbclient -N -L "//$a" -m SMB3 >/tmp/smb 2>&1; then cat /tmp/smb; exit 1; fi
        grep -m1 "Denied connection from $a" /var/log/samba/log.smbd
    done'
echo "--- control: with the host's own address allowed, cuenca-owner logs in; main/ and the root are read-only, .cuenca/ cannot be entered"
on_host 'bash /deploy/install.sh --allow 100.64.0.1 --allow fd7a:115c:a1e0::1 >/dev/null
    smbclient //fd7a:115c:a1e0::1/cuenca -m SMB3 -U "cuenca-owner%$(cat /root/cuenca-owner.smbpass)" -c ls 2>&1 | grep -E "^  main "
    smb() { smbclient //100.64.0.1/cuenca -m SMB3 -U "cuenca-owner%$(cat /root/cuenca-owner.smbpass)" -c "$1" 2>&1 || true; }
    smb "ls" | grep -E "^  (main|forks) "
    smb "mkdir x" | grep NT_STATUS_ACCESS_DENIED
    smb "mkdir main/x" | grep NT_STATUS_ACCESS_DENIED
    smb "ls .cuenca/*" | grep NT_STATUS_ACCESS_DENIED
    if smbclient -N -L //100.64.0.1 -m SMB3 >/tmp/smb 2>&1; then echo "a guest listed shares"; exit 1; fi
    grep -E "NT_STATUS_(ACCESS_DENIED|LOGON_FAILURE)" /tmp/smb | head -n1
    bash /deploy/install.sh --allow 100.82.213.74 --allow fd7a:115c:a1e0::2 --allow 100.71.190.125 >/dev/null'
echo "--- smbd comes back after being killed (Restart=on-failure)"
on_host 'pid=$(systemctl show -p MainPID --value smbd); kill -9 "$pid"; sleep 14; systemctl is-active smbd
    [ "$(systemctl show -p MainPID --value smbd)" != "$pid" ]'
echo "--- the authority comes back after being killed (Restart=on-failure)"
on_host 'pid=$(systemctl show -p MainPID --value cuenca); kill -9 "$pid"; sleep 8; systemctl is-active cuenca
    [ "$(systemctl show -p MainPID --value cuenca)" != "$pid" ]'
echo "--- a second install keeps everything working"
on_host "bash /deploy/install.sh $allow >/dev/null && bash /deploy/check.sh | tail -n1"
echo "--- without tailscale0's IPv6 address, smbd does not start, and the journal says which address"
on_host 'ip -6 addr del fd7a:115c:a1e0::1/128 dev tailscale0; systemctl set-environment CUENCA_WAIT_SECS=3
    if systemctl restart smbd 2>/dev/null; then echo "smbd started without the IPv6 address"; exit 1; fi
    journalctl -u smbd --no-pager | grep -m1 "missing its IPv6 address fd7a:115c:a1e0::1"
    ip -6 addr add fd7a:115c:a1e0::1/128 dev tailscale0 nodad; systemctl unset-environment CUENCA_WAIT_SECS
    systemctl restart smbd && bash /deploy/check.sh | tail -n1'
echo "--- without tailscale0, smbd does not start (it waits, then fails)"
on_host 'ip link del tailscale0; systemctl set-environment CUENCA_WAIT_SECS=3
    if systemctl restart smbd 2>/dev/null; then echo "smbd started without tailscale0"; exit 1; fi
    journalctl -u smbd --no-pager | grep -m1 "wait-tailnet: tailscale0 is missing an IPv4 address"'
echo "--- with the tailnet down, install.sh stops at once with its own message"
on_host "t0=\$(date +%s); if bash /deploy/install.sh $allow >/tmp/err 2>&1; then echo 'install.sh passed with no tailnet'; exit 1; fi
    grep -m1 'the tailnet is not up' /tmp/err; [ \$(( \$(date +%s) - t0 )) -lt 60 ] && echo \"stopped in \$(( \$(date +%s) - t0 ))s\""
echo "test-systemd: all passed"
