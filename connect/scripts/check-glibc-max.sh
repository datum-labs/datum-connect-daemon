#!/usr/bin/env bash
# Fail if a binary requires a newer glibc than the target system provides.
#
# Home Assistant OS is Debian bookworm (glibc 2.36). Building on a newer distro
# silently produces a binary that links against symbols the appliance does not
# have, and the failure only shows up at runtime on the device as a loader
# error — long after CI went green. This turns that into a build-time gate.
set -euo pipefail

BIN=${1:?usage: check-glibc-max.sh <binary> [max-glibc-version]}
MAX=${2:-2.36}

if ! command -v objdump >/dev/null 2>&1; then
    echo "check-glibc-max: objdump not found (install binutils)" >&2
    exit 2
fi

mapfile -t versions < <(
    objdump -T "$BIN" 2>/dev/null \
        | grep -o 'GLIBC_[0-9]\+\.[0-9]\+\(\.[0-9]\+\)\?' \
        | sed 's/^GLIBC_//' \
        | sort -uV
)

if [ ${#versions[@]} -eq 0 ]; then
    echo "check-glibc-max: no GLIBC_ version symbols found in $BIN"
    echo "  (statically linked, or not an ELF binary — nothing to check)"
    exit 0
fi

echo "glibc symbol versions required by $(basename "$BIN"):"
printf '  %s\n' "${versions[@]}"

# The highest required version sorts last under version-aware sort.
highest=${versions[-1]}

# `sort -V` puts the smaller version first; if that is MAX, then highest > MAX.
if [ "$highest" != "$MAX" ] && [ "$(printf '%s\n%s\n' "$MAX" "$highest" | sort -V | head -1)" = "$MAX" ]; then
    echo >&2
    echo "FAIL: requires glibc $highest, target provides $MAX" >&2
    echo "  This binary will not start on the appliance. Build against a" >&2
    echo "  bookworm-based toolchain rather than the host distribution's." >&2
    exit 1
fi

echo
echo "OK: highest requirement is $highest, within the $MAX target"
