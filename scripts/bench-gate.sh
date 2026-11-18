#!/usr/bin/env bash
# What a zone costs, three ways, and the feature `heap` held to its gates:
#
#   off        the feature off: the cost every user pays
#   on         the feature on, no counting allocator installed
#   installed  the feature on, `heap::Counting` installed
#
# Each case of `on` and `installed` may cost at most half a nanosecond more
# a zone than `off` (a case of two nested zones, a nanosecond), and never
# over 1.5 times it. Each bench also fails on its own ceiling.
set -euo pipefail

run() {
    cargo bench --locked "$@" | grep '^zones/'
}

off=$(run --bench overhead)
on=$(run --features heap --bench overhead)
installed=$(run --features heap --bench overhead_heap)

printf '%-32s %9s %9s %9s\n' case off on installed
fail=0
while read -r name ns _; do
    case "$name" in
        *overhead\(\)) continue ;;
    esac
    zones=1
    [[ "$name" == zones/nested_in_fn ]] && zones=2
    a=$(awk -v n="$name" '$1 == n { print $2 }' <<<"$on")
    b=$(awk -v n="$name" '$1 == n { print $2 }' <<<"$installed")
    printf '%-32s %9s %9s %9s\n' "$name" "$ns" "$a" "$b"
    for got in "$a" "$b"; do
        if ! awk -v off="$ns" -v got="$got" -v z="$zones" \
            'BEGIN { exit !(got <= off + 0.5 * z && got <= off * 1.5) }'; then
            echo "over its gate: $name $got ns against $ns ns off" >&2
            fail=1
        fi
    done
done <<<"$off"
grep -E 'alloc_pair|booking|heap::overhead' <<<"$installed"
exit "$fail"
