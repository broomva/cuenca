#!/usr/bin/env bash
# Exercise install.sh and check.sh in Ubuntu's official 24.04 image (no systemd, so
# --no-start / --no-services): install twice and check nothing moved, then check that bad
# input is refused and that check.sh catches a broken host. Needs docker. Used by CI.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
exec docker run --rm -v "$here":/deploy:ro ubuntu:24.04 bash -euo pipefail -c '
fail() { echo "test-container: FAIL $*" >&2; exit 1; }
printf "#!/bin/sh\necho cuenca stub\n" >/stub && chmod 755 /stub
args="--no-start --bind 100.64.0.1:7419 --allow 100.82.213.74 --allow fd7a:115c:a1e0::2 --allow 100.71.190.125"

bash /deploy/install.sh --binary /stub $args | tee /tmp/i1
grep -q "landing has no token yet" /tmp/i1 || fail "install did not say landing has no token"
if bash /deploy/check.sh --no-services >/tmp/out 2>&1; then fail "check passed with no gh.env"; fi
grep -q "^FAIL /etc/cuenca/gh.env" /tmp/out || { cat /tmp/out; fail "check did not name the missing gh.env"; }
# README step 5, as written there.
printf "ghp_TESTONLY0123456789\n" >/token-file
{ printf "GH_TOKEN="; cat /token-file; } | install -o root -g cuenca -m 0640 /dev/stdin /etc/cuenca/gh.env
[ "$(cat /etc/cuenca/gh.env)" = GH_TOKEN=ghp_TESTONLY0123456789 ] || fail "README step 5 wrote $(cat /etc/cuenca/gh.env)"
bash /deploy/check.sh --no-services | tee /tmp/out
grep -q TESTONLY /tmp/out && fail "check.sh printed the token"
snap() { (stat -c "%n %a %U:%G" /etc/samba/smb.conf /etc/cuenca/cuenca.env /etc/systemd/system/cuenca.service \
    /usr/local/bin/cuenca /var/lib/cuenca/state /srv/cuenca/volume /root/cuenca-owner.smbpass /etc/cuenca/gh.env;
    sha256sum /etc/samba/smb.conf /etc/cuenca/cuenca.env /etc/systemd/system/cuenca.service /root/cuenca-owner.smbpass /etc/cuenca/gh.env;
    getent group cuenca; getent passwd cuenca cuenca-owner; pdbedit -L) ; }
first=$(snap)
bash /deploy/install.sh $args >/tmp/i2   # a re-run without --binary keeps the installed one
grep -q "no token" /tmp/i2 && fail "install said no token with gh.env in place"
[ "$first" = "$(snap)" ] || { diff <(echo "$first") <(snap); fail "a second install changed the host"; }
echo "test-container: a second install changed nothing"
[ -f /etc/samba/smb.conf.pre-cuenca ] || fail "Ubuntu smb.conf was not kept"
grep -q "hosts allow = 100.82.213.74 fd7a:115c:a1e0::2 100.71.190.125" /etc/samba/smb.conf || fail "hosts allow not rendered"
[ -x /usr/bin/gh ] || fail "gh was not installed"
grep -q "@" /etc/samba/smb.conf && fail "an unrendered placeholder is left in smb.conf"
grep -q "ReadWritePaths=/var/lib/cuenca /srv/cuenca/volume" /etc/systemd/system/cuenca.service || fail "unit not rendered"

# Bad input is refused.
for bad in "--allow 8.8.8.8" "--allow 100.128.0.1" "--allow 100.64.0.1/10" "--allow fd7a:115c:a1e0::/48" \
    "--allow fe80::1" "--allow FD7A:115C:A1E0:" "" "--allow 100.82.213.74 --volume rel"; do
    if bash /deploy/install.sh --no-start --bind 100.64.0.1:7419 $bad >/dev/null 2>&1; then fail "install accepted: $bad"; fi
done
# Each refused for its own reason: the message names the flag at fault.
refused() { if bash /deploy/install.sh --no-start --bind 100.64.0.1:7419 $2 >/tmp/err 2>&1; then fail "install accepted: $2"; fi
    grep -q -- "$1" /tmp/err || { cat /tmp/err; fail "refused for another reason: $2"; }; }
for bad in 100.64.0.256 100.064.0.1 100.64.0.01 fd7a:115c:a1e0:0:0:0:0:1 fd7a:115c:a1e0:::::1 fd7a:115c:a1e0::1::2 \
    fd7a:115c:a1e0::0001 FD7A:115C:A1E0::1 fd7a:115c:a1e0:0:0:1:0:0 fd7a:115c:a1e0:0:1:0:0:0:1 :fd7a:115c:a1e0::1; do
    refused "--allow " "--allow 100.82.213.74 --allow $bad"
done
for bad in /srv/../etc /. /srv/./x //srv/x /srv/x/ /var/lib/cuenca /var/lib/cuenca/v /var/lib /etc /; do
    refused "--volume " "--allow 100.82.213.74 --volume $bad"
done
echo "test-container: bad --allow / --volume refused"

# check.sh catches a broken host, one break at a time.
expect_fail() { if bash /deploy/check.sh --no-services >/tmp/out 2>&1; then fail "check passed with: $1"; fi
    grep -q "^FAIL" /tmp/out || fail "no FAIL line for: $1"; echo "test-container: check caught: $1"; }
useradd -M -s /bin/bash agentx && usermod -aG cuenca agentx
expect_fail "an agent account in group cuenca"
if bash /deploy/install.sh $args >/dev/null 2>&1; then fail "install accepted a foreign member of group cuenca"; fi
gpasswd -d agentx cuenca >/dev/null; bash /deploy/check.sh --no-services >/dev/null
chmod 0755 /var/lib/cuenca/state; expect_fail "state readable by others"; chmod 0700 /var/lib/cuenca/state
cp /etc/samba/smb.conf /smb.ok
mv /usr/lib/*/samba/vfs/streams_xattr.so /sx.so; expect_fail "no streams_xattr module"; mv /sx.so "$(ls -d /usr/lib/*/samba/vfs)/streams_xattr.so"
sed -i "s/wide links = no/wide links = yes/" /etc/samba/smb.conf; expect_fail "wide links = yes"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/hosts deny = ALL/hosts deny = 1.2.3.4/" /etc/samba/smb.conf; expect_fail "hosts deny not ALL"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   hosts allow = .*/   hosts allow = 100.0.0.0\/8/" /etc/samba/smb.conf; expect_fail "hosts allow a range"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   hosts allow = .*/   hosts allow = fd7a:115c:a1e0::\/48/" /etc/samba/smb.conf; expect_fail "hosts allow an IPv6 range"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   hosts allow = .*/   hosts allow = fd7a:115c:a1e0:0:0:0:0:2/" /etc/samba/smb.conf; expect_fail "hosts allow a non-canonical IPv6 address"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   hosts allow = .*/   hosts allow = 100.64.0.300/" /etc/samba/smb.conf; expect_fail "hosts allow an IPv4 octet over 255"; cp /smb.ok /etc/samba/smb.conf
mv /usr/bin/gh /gh; expect_fail "no gh"; mv /gh /usr/bin/gh
cp -p /etc/cuenca/gh.env /gh.ok
chmod 0644 /etc/cuenca/gh.env; expect_fail "gh.env readable by others"; cp -p /gh.ok /etc/cuenca/gh.env
chown cuenca /etc/cuenca/gh.env; expect_fail "gh.env writable by the authority"; cp -p /gh.ok /etc/cuenca/gh.env
echo "GH_TOKEN=" >/etc/cuenca/gh.env; expect_fail "an empty token"; cp -p /gh.ok /etc/cuenca/gh.env
echo "EXTRA=1" >>/etc/cuenca/gh.env; expect_fail "a second line in gh.env"; cp -p /gh.ok /etc/cuenca/gh.env
echo "GITHUB_TOKEN=ghp_x" >/etc/cuenca/gh.env; expect_fail "another variable name"; cp -p /gh.ok /etc/cuenca/gh.env
sed -i "/^EnvironmentFile=-\/etc\/cuenca\/gh.env$/d" /etc/systemd/system/cuenca.service; expect_fail "a unit that does not read gh.env"
bash /deploy/install.sh $args >/dev/null
sed -i "s/^   vfs objects = .*/   vfs objects = fruit/" /etc/samba/smb.conf; expect_fail "no streams_xattr"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   map to guest = Never/   map to guest = Bad User/" /etc/samba/smb.conf; expect_fail "guest mapping"; cp /smb.ok /etc/samba/smb.conf
bash /deploy/check.sh --no-services >/dev/null || fail "restored host does not pass"

# wait-tailnet waits for the IPv4 address and for the IPv6 address `tailscale ip -6` names.
# `ip` and `tailscale` are stubs that read /fake/{v4,v6,ts6}.
mkdir -p /fake
printf "#!/bin/sh\ncase \"\$1\" in -4) cat /fake/v4 2>/dev/null;; -6) cat /fake/v6 2>/dev/null;; esac; true\n" >/fake/ip
printf "#!/bin/sh\n[ \"\$1 \$2\" = \"ip -6\" ] && cat /fake/ts6 2>/dev/null; true\n" >/fake/tailscale
chmod 755 /fake/ip /fake/tailscale
v4="3: tailscale0    inet 100.64.0.1/32 scope global tailscale0"
v6() { echo "3: tailscale0    inet6 $1/128 scope global "; }
wt() { PATH=/fake:$PATH CUENCA_WAIT_SECS=${2:-2} sh /deploy/wait-tailnet 2>/tmp/wt; }
wt_fails() { if wt; then fail "wait-tailnet passed: $1"; fi; grep -q -- "$2" /tmp/wt || { cat /tmp/wt; fail "wrong reason: $1"; }; }
wt_fails "no address at all" "missing an IPv4 address"
echo "$v4" >/fake/v4; wt || fail "wait-tailnet failed on an IPv4-only node"
echo fd7a:115c:a1e0::1 >/fake/ts6
wt_fails "the IPv6 address not yet on tailscale0" "missing its IPv6 address fd7a:115c:a1e0::1"
v6 fd7a:115c:a1e0::12 >/fake/v6; wt_fails "another IPv6 address with the same prefix" "missing its IPv6 address fd7a:115c:a1e0::1"
rm /fake/v6; (sleep 2; v6 fd7a:115c:a1e0::1 >/fake/v6) & wt x 10 || fail "wait-tailnet gave up while the IPv6 address arrived"
wait; rm /fake/v4; wt_fails "IPv6 without IPv4" "missing an IPv4 address"
echo "test-container: wait-tailnet waits for both tailnet addresses"

# By address: CUENCA_TAILNET_ADDRS names the host's own addresses on whatever interface holds them
# (WSL2 mirrored networking: an ethN whose name changes between boots). /fake/all is `ip -o addr show`.
printf "#!/bin/sh\ncase \"\$1\" in -4) cat /fake/v4 2>/dev/null;; -6) cat /fake/v6 2>/dev/null;; -o) cat /fake/all 2>/dev/null;; esac; true\n" >/fake/ip
wa() { PATH=/fake:$PATH CUENCA_TAILNET_ADDRS="100.112.149.32 fd7a:115c:a1e0::9237:9520" CUENCA_WAIT_SECS=${2:-2} sh /deploy/wait-tailnet 2>/tmp/wt; }
wa_fails() { if wa; then fail "wait-tailnet (by address) passed: $1"; fi; grep -q -- "$2" /tmp/wt || { cat /tmp/wt; fail "wrong reason: $1"; }; }
wa_fails "no address at all" "no interface has 100.112.149.32 fd7a:115c:a1e0::9237:9520"
echo "5: eth5    inet 100.112.149.3/32 scope global eth5" >/fake/all
wa_fails "an IPv4 address the wanted one is a prefix of" "no interface has 100.112.149.32"
echo "5: eth5    inet 100x112x149x32/32 scope global eth5" >/fake/all
wa_fails "a spelling that only a regex would match" "no interface has 100.112.149.32"
echo "5: eth5    inet 100.112.149.32/32 scope global eth5" >/fake/all
wa_fails "IPv4 without the IPv6 address" "no interface has fd7a:115c:a1e0::9237:9520"
(sleep 2; echo "5: eth5    inet6 fd7a:115c:a1e0::9237:9520/128 scope global" >>/fake/all) & wa x 10 || fail "wait-tailnet (by address) gave up while the IPv6 address arrived"
wait
echo "4: eth4    inet 100.112.149.32/32 scope global eth4
4: eth4    inet6 fd7a:115c:a1e0::9237:9520/128 scope global" >/fake/all
wa || fail "wait-tailnet (by address) failed when the addresses moved to another interface"
echo "test-container: wait-tailnet by address waits for each address on any interface"

# install.sh --tailnet-addr: smbd binds the addresses, the agent API defaults to the IPv4 one.
targs="--no-start --allow 100.82.213.74 --tailnet-addr 100.112.149.32 --tailnet-addr fd7a:115c:a1e0::9237:9520"
bash /deploy/install.sh $targs >/dev/null
grep -qx "   interfaces = 100.112.149.32/32 fd7a:115c:a1e0::9237:9520/128" /etc/samba/smb.conf || fail "interfaces not rendered by address"
grep -qx "CUENCA_BIND=100.112.149.32:7419" /etc/cuenca/cuenca.env || fail "the agent API did not default to the IPv4 tailnet address"
grep -qx "CUENCA_TAILNET_ADDRS=\"100.112.149.32 fd7a:115c:a1e0::9237:9520\"" /etc/cuenca/cuenca.env || fail "CUENCA_TAILNET_ADDRS not written"
grep -qx "EnvironmentFile=-/etc/cuenca/cuenca.env" /etc/systemd/system/smbd.service.d/cuenca.conf || fail "the smbd drop-in does not read cuenca.env"
bash /deploy/check.sh --no-services | tee /tmp/out
grep -q "^ok   smb.conf interfaces = 100.112.149.32/32 fd7a:115c:a1e0::9237:9520/128" /tmp/out || fail "check.sh did not check interfaces by address"
cp /etc/samba/smb.conf /smb.addr
sed -i "s|^   interfaces = .*|   interfaces = tailscale0|" /etc/samba/smb.conf; expect_fail "interfaces = tailscale0 on a by-address host"; cp /smb.addr /etc/samba/smb.conf
sed -i "s|^   interfaces = .*|   interfaces = 100.112.149.32/32|" /etc/samba/smb.conf; expect_fail "the IPv6 address left out of interfaces"; cp /smb.addr /etc/samba/smb.conf
for bad in "--tailnet-addr 192.168.78.121" "--tailnet-addr 100.112.149.32/32" "--tailnet-addr fd7a:115c:a1e0:0:0:0:9237:9520" \
    "--tailnet-addr fd7a:115c:a1e0::9237:9520" "--tailnet-addr 100.112.149.32 --tailnet-addr 100.64.0.9"; do
    refused "--tailnet-addr" "--allow 100.82.213.74 $bad"
done
bash /deploy/install.sh $args >/dev/null
grep -qx "   interfaces = tailscale0" /etc/samba/smb.conf || fail "a re-install without --tailnet-addr did not return to tailscale0"
grep -q CUENCA_TAILNET_ADDRS /etc/cuenca/cuenca.env && fail "a re-install without --tailnet-addr kept CUENCA_TAILNET_ADDRS"
bash /deploy/check.sh --no-services >/dev/null || fail "the tailscale0 host does not pass after the round trip"
echo "test-container: --tailnet-addr binds by address, refuses bad input, and reverts"
echo "test-container: all passed"
'
