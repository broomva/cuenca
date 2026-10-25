#!/usr/bin/env bash
# Check a host installed by deploy/install.sh (BRO-3257), as root on that host. Prints one
# line per check and exits 1 if any failed. `--no-services` skips what needs systemd and the
# network (a container, CI); everything else is checked the same way.
set -uo pipefail

services=1
[ "${1:-}" = "--no-services" ] && services=0
fails=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local what=$1; shift; if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi; }

[ "$(id -u)" -eq 0 ] || { echo "check.sh: run as root" >&2; exit 2; }
# shellcheck disable=SC1091
. /etc/cuenca/cuenca.env 2>/dev/null || { echo "check.sh: no /etc/cuenca/cuenca.env; run install.sh" >&2; exit 2; }

mode_of() { stat -c '%a %U:%G' "$1" 2>/dev/null; }
expect_mode() { local p=$1 want=$2; local got; got=$(mode_of "$p"); [ "$got" = "$want" ] && ok "$p is $want" || bad "$p is '$got', want $want"; }

# Accounts: group cuenca is exactly {cuenca, cuenca-owner}; neither has a shell.
gid=$(getent group cuenca | cut -d: -f3)
members=$( { getent group cuenca | cut -d: -f4 | tr ',' '\n'; getent passwd | awk -F: -v g="$gid" '$4 == g {print $1}'; } | grep -v '^$' | sort -u | tr '\n' ' ')
[ "$members" = "cuenca cuenca-owner " ] && ok "group cuenca is {cuenca, cuenca-owner}" || bad "group cuenca is {$members}"
for u in cuenca cuenca-owner; do
    [ "$(getent passwd "$u" | cut -d: -f7)" = /usr/sbin/nologin ] && ok "$u has no shell" || bad "$u has a shell"
done
check "cuenca-owner is Samba's account" pdbedit -L -u cuenca-owner
[ "$(pdbedit -L | wc -l)" -eq 1 ] && ok "Samba has one account" || bad "Samba has $(pdbedit -L | wc -l) accounts"

# Files and modes.
expect_mode "$CUENCA_STATE" "700 cuenca:cuenca"
expect_mode "$CUENCA_VOLUME" "2750 cuenca:cuenca"
expect_mode /etc/cuenca/cuenca.env "640 root:cuenca"
expect_mode /usr/local/bin/cuenca "755 root:root"
[ -x /usr/bin/gh ] && ok "gh is installed (landing)" || bad "/usr/bin/gh is missing (landing opens pull requests with it)"
# Landing's token: one GH_TOKEN line, read by the unit, never printed here.
expect_mode /etc/cuenca/gh.env "640 root:cuenca"
[ "$(grep -c '' /etc/cuenca/gh.env 2>/dev/null)" = 1 ] && grep -Eqx 'GH_TOKEN=[A-Za-z0-9_]+' /etc/cuenca/gh.env \
    && ok "/etc/cuenca/gh.env holds one GH_TOKEN line" || bad "/etc/cuenca/gh.env must be one line GH_TOKEN=<token> (README step 5)"
grep -qx 'EnvironmentFile=-/etc/cuenca/gh.env' /etc/systemd/system/cuenca.service \
    && ok "the unit reads /etc/cuenca/gh.env" || bad "the unit does not read /etc/cuenca/gh.env"
[ "$(cat /proc/sys/fs/protected_hardlinks)" = 1 ] && ok "fs.protected_hardlinks = 1" || bad "fs.protected_hardlinks != 1"
if [ -f /root/cuenca-owner.smbpass ]; then
    echo "note /root/cuenca-owner.smbpass still exists: hand the password over, then delete it"
fi

# Samba's effective settings (spec rev 2.2, section 2a).
param() { testparm -s --parameter-name="$1" ${2:+--section-name="$2"} 2>/dev/null; }
expect_param() {
    local name=$1 want=$2 section=${3:-}
    local got; got=$(param "$name" "$section")
    [ "$got" = "$want" ] && ok "smb.conf ${section:+[$section] }$name = $want" || bad "smb.conf ${section:+[$section] }$name = '$got', want '$want'"
}
for m in fruit streams_xattr; do
    compgen -G "/usr/lib/*/samba/vfs/$m.so" >/dev/null && ok "Samba's $m module is installed" || bad "Samba's $m module is missing (samba-vfs-modules)"
done
expect_param interfaces tailscale0
expect_param "bind interfaces only" Yes
expect_param "disable netbios" Yes
expect_param "hosts deny" ALL
expect_param "map to guest" Never
expect_param "vfs objects" "fruit streams_xattr" cuenca
expect_param "fruit:nfs_aces" no cuenca
expect_param "wide links" No cuenca
expect_param "guest ok" No cuenca
expect_param "valid users" cuenca-owner cuenca
expect_param "force group" cuenca cuenca
expect_param "create mask" 0660 cuenca
expect_param "force create mode" 0660 cuenca
expect_param "directory mask" 02770 cuenca
expect_param "force directory mode" 02770 cuenca
expect_param "nt acl support" No cuenca
expect_param path "$CUENCA_VOLUME" cuenca
allow=$(param "hosts allow")
[ -n "$allow" ] && ok "hosts allow names the enrolled devices: $allow" || bad "hosts allow is empty"
# shellcheck source=tailnet-addr.sh
. "$(dirname "$0")/tailnet-addr.sh"
for a in ${allow//,/ }; do
    [ "$(canonical_tailnet "$a")" = "$a" ] || bad "hosts allow entry $a is not a single, canonically written tailnet address"
done

if [ "$services" = 0 ]; then
    echo "--no-services: systemd, listeners and the spike are not checked"
    [ "$fails" -eq 0 ] && { echo "check.sh: all passed"; exit 0; } || { echo "check.sh: $fails failed"; exit 1; }
fi

# Services, enabled for boot and running.
for u in cuenca smbd; do
    check "$u is enabled" systemctl is-enabled --quiet "$u"
    check "$u is active" systemctl is-active --quiet "$u"
done
check "smbd restarts on failure" test "$(systemctl show -p Restart --value smbd)" = on-failure
for u in nmbd samba-ad-dc; do
    systemctl is-active --quiet "$u" && bad "$u is running" || ok "$u is not running"
done

# Listeners: the agent API and :445 on the tailnet address only, nothing on :139.
# smbd binds both of tailscale0's addresses; nothing else may be bound.
command -v ss >/dev/null || { bad "ss is missing; listeners not checked"; echo "check.sh: $fails failed"; exit 1; }
ts=$(tailscale ip -4 | head -n1)
ts6=$(tailscale ip -6 2>/dev/null | head -n1)
listen_on() { ss -Hltn "sport = :$1" | awk '{print $4}' | sort -u | tr '\n' ' '; }
want="$ts:445 "
[ -n "$ts6" ] && want=$(printf '%s\n' "$ts:445" "[$ts6]:445" | sort -u | tr '\n' ' ')
[ "$(listen_on 445)" = "$want" ] && ok ":445 listens on the tailnet addresses only: $want" || bad ":445 listens on '$(listen_on 445)', want $want"
[ -z "$(listen_on 139)" ] && ok "nothing listens on :139" || bad ":139 listens on $(listen_on 139)"
port=${CUENCA_BIND##*:}
[ "$(listen_on "$port")" = "$CUENCA_BIND " ] && ok "the agent API listens on $CUENCA_BIND only" || bad "the agent API listens on '$(listen_on "$port")', want $CUENCA_BIND"
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$CUENCA_BIND/v1/whoami")
[ "$code" = 401 ] && ok "the agent API refuses a request with no key (401)" || bad "GET /v1/whoami with no key gave $code, want 401"
check "the admin socket is the authority's alone" test "$(stat -c '%a %U' /run/cuenca)" = "700 cuenca"

# Spike C is retired.
if command -v docker >/dev/null; then
    docker ps -a --format '{{.Names}}' | grep -qx cuenca-spike && bad "the spike container cuenca-spike still exists" || ok "no cuenca-spike container"
fi

[ "$fails" -eq 0 ] && { echo "check.sh: all passed"; exit 0; } || { echo "check.sh: $fails failed"; exit 1; }
