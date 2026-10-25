# shellcheck shell=bash
# Sourced by install.sh and check.sh. `canonical_tailnet ADDR` prints ADDR's canonical form
# (IPv4 dotted decimal without leading zeros; IPv6 per RFC 5952: lower case, no leading zeros,
# the longest run of two or more zero groups as `::`) and succeeds when ADDR is one address in
# 100.64.0.0/10 or fd7a:115c:a1e0::/48. Samba compares `hosts allow` entries as text, so a
# non-canonical spelling would never match and the device would silently be refused: callers
# accept only input equal to the printed form.
canonical_tailnet() {
    local a=$1 h i n best=0 bstart=-1 run=0 rstart=0 out=""
    local -a g=() t=()
    if [[ $a =~ ^[0-9.]+$ ]]; then
        IFS=. read -ra g <<<"$a"
        [[ $a =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
        for h in "${g[@]}"; do [[ $h =~ ^(0|[1-9][0-9]{0,2})$ ]] && [ "$h" -le 255 ] || return 1; done
        [ "${g[0]}" -eq 100 ] && [ "${g[1]}" -ge 64 ] && [ "${g[1]}" -le 127 ] || return 1
        echo "${g[0]}.${g[1]}.${g[2]}.${g[3]}"
        return 0
    fi
    a=${a,,}
    [[ $a =~ ^[0-9a-f:]+$ ]] || return 1
    # `read` drops a trailing empty field, so a lone leading or trailing ':' is refused here.
    if [[ $a == :* && $a != ::* ]] || [[ $a == *: && $a != *:: ]]; then return 1; fi
    if [[ $a == *::* ]]; then
        [[ ${a#*::} == *::* ]] && return 1
        IFS=: read -ra g <<<"${a%%::*}"
        IFS=: read -ra t <<<"${a#*::}"
        n=$((8 - ${#g[@]} - ${#t[@]}))
        [ "$n" -ge 1 ] || return 1
        for ((i = 0; i < n; i++)); do g+=(0); done
        g+=("${t[@]}")
    else
        IFS=: read -ra g <<<"$a"
    fi
    [ "${#g[@]}" -eq 8 ] || return 1
    for i in "${!g[@]}"; do
        [[ ${g[i]} =~ ^[0-9a-f]{1,4}$ ]] || return 1
        g[i]=$(printf '%x' "$((16#${g[i]}))")
        if [ "${g[i]}" = 0 ]; then
            [ "$run" -eq 0 ] && rstart=$i
            run=$((run + 1))
            if [ "$run" -gt "$best" ]; then best=$run; bstart=$rstart; fi
        else
            run=0
        fi
    done
    [ "${g[0]}:${g[1]}:${g[2]}" = fd7a:115c:a1e0 ] || return 1
    if [ "$best" -ge 2 ]; then
        for ((i = 0; i < bstart; i++)); do out+="${out:+:}${g[i]}"; done
        out+="::"
        for ((i = bstart + best; i < 8; i++)); do out+="${g[i]}"; [ "$i" -lt 7 ] && out+=":"; done
    else
        for ((i = 0; i < 8; i++)); do out+="${out:+:}${g[i]}"; done
    fi
    echo "$out"
}
