#!/usr/bin/env bash
# Exercise install.sh and check.sh in Ubuntu's official 24.04 image (no systemd, so
# --no-start / --no-services): install twice and check nothing moved, then check that bad
# input is refused and that check.sh catches a broken host. Needs docker. Used by CI.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
exec docker run --rm -v "$here":/deploy:ro ubuntu:24.04 bash -euo pipefail -c '
fail() { echo "test-container: FAIL $*" >&2; exit 1; }
printf "#!/bin/sh\necho cuenca stub\n" >/stub && chmod 755 /stub
args="--no-start --bind 100.64.0.1:7419 --allow 100.82.213.74 --allow 100.71.190.125"

bash /deploy/install.sh --binary /stub $args
bash /deploy/check.sh --no-services
snap() { (stat -c "%n %a %U:%G" /etc/samba/smb.conf /etc/cuenca/cuenca.env /etc/systemd/system/cuenca.service \
    /usr/local/bin/cuenca /var/lib/cuenca/state /srv/cuenca/volume /root/cuenca-owner.smbpass;
    sha256sum /etc/samba/smb.conf /etc/cuenca/cuenca.env /etc/systemd/system/cuenca.service /root/cuenca-owner.smbpass;
    getent group cuenca; getent passwd cuenca cuenca-owner; pdbedit -L) ; }
first=$(snap)
bash /deploy/install.sh $args            # a re-run without --binary keeps the installed one
[ "$first" = "$(snap)" ] || { diff <(echo "$first") <(snap); fail "a second install changed the host"; }
echo "test-container: a second install changed nothing"
[ -f /etc/samba/smb.conf.pre-cuenca ] || fail "Ubuntu smb.conf was not kept"
grep -q "^hosts allow\|hosts allow = 100.82.213.74 100.71.190.125" /etc/samba/smb.conf || fail "hosts allow not rendered"
grep -q "@" /etc/samba/smb.conf && fail "an unrendered placeholder is left in smb.conf"
grep -q "ReadWritePaths=/var/lib/cuenca /srv/cuenca/volume" /etc/systemd/system/cuenca.service || fail "unit not rendered"

# Bad input is refused.
for bad in "--allow 8.8.8.8" "--allow 100.128.0.1" "--allow 100.64.0.1/10" "" "--allow 100.82.213.74 --volume rel"; do
    if bash /deploy/install.sh --no-start --bind 100.64.0.1:7419 $bad >/dev/null 2>&1; then fail "install accepted: $bad"; fi
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
sed -i "s/^   vfs objects = .*/   vfs objects = fruit/" /etc/samba/smb.conf; expect_fail "no streams_xattr"; cp /smb.ok /etc/samba/smb.conf
sed -i "s/^   map to guest = Never/   map to guest = Bad User/" /etc/samba/smb.conf; expect_fail "guest mapping"; cp /smb.ok /etc/samba/smb.conf
bash /deploy/check.sh --no-services >/dev/null || fail "restored host does not pass"
echo "test-container: all passed"
'
