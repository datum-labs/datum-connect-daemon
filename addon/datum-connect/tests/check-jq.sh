#!/usr/bin/env bash
# Compile every jq program embedded in the add-on's shell scripts with whatever
# `jq` is on PATH, and fail on any that does not compile.
#
# Why this exists: the add-on runs on Debian bookworm, which ships jq 1.6.
# Developers have jq 1.7/1.8, where `$label` is a legal variable name. In 1.6
# `label` is a reserved word, so the same script crashes on the device while
# bash -n and shellcheck pass. Run this with the device's jq (CI does so inside
# a bookworm container; see .github/workflows/addon-script-check.yml).
#
# Usage: check-jq.sh [script ...]
#   Defaults to every *.sh under ../rootfs/usr/bin.
#   JQ=/path/to/jq overrides the binary.
#
# It only compiles; it never needs real input. Compilation is jq's own exit
# code 3, so runtime errors from running against null do not count.
#
# Extraction rules (kept deliberately simple, so a script that outgrows them
# fails loudly rather than being skipped):
#   * a program is the first single-quoted string after a `jq` command word,
#     on the same logical line (backslash-newline continues the line), and it
#     may itself span several physical lines
#   * programs passed unquoted (e.g. `jq -re .client_id`) are not checked
#   * quoted heredocs are skipped as shell, and if the opening line is
#     `jq -c .` their body is parsed as JSON by that jq instead
set -euo pipefail

JQ=${JQ:-jq}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ "$#" -eq 0 ]; then
  set -- "${here}"/../rootfs/usr/bin/*.sh
fi

command -v "${JQ}" >/dev/null || { echo "check-jq: ${JQ} not found" >&2; exit 2; }
echo "check-jq: using $("${JQ}" --version)"

work=$(mktemp -d)
trap 'rm -rf "${work}"' EXIT

# Emits files prog.N (the jq program) / heredoc.N (JSON body), each preceded in
# index.txt by "<kind> <N> <line>".
extract() {
  awk -v out="${work}" '
    function emit(kind, line, body,    f) {
      n++
      f = out "/" kind "." n
      printf "%s", body > f
      close(f)
      print kind, n, line >> (out "/index.txt")
    }
    # bs is a backslash spelled in octal so no shell/awk layer can eat it.
    BEGIN { RS = "\n"; bs = "\134" }
    { lines[NR] = $0 }
    END {
      i = 1
      while (i <= NR) {
        l = lines[i]
        # Quoted heredoc: skip the body, remember it if the command is `jq -c .`
        if (match(l, /<<-?[ ]*[\047"]?[A-Za-z_][A-Za-z0-9_]*[\047"]?/)) {
          tag = substr(l, RSTART, RLENGTH)
          gsub(/<<-?[ ]*|[\047"]/, "", tag)
          isjq = (l ~ /jq -c \.[ ]*<<-?/)
          start = i; body = ""
          i++
          while (i <= NR && lines[i] != tag) { body = body lines[i] "\n"; i++ }
          if (isjq) emit("heredoc", start + 1, body)
          i++
          continue
        }
        if (l ~ /^[ \t]*#/) { i++; continue }
        # Logical line: join backslash continuations, remember where it began.
        start = i; text = l
        while (substr(text, length(text)) == bs && i < NR) { text = substr(text, 1, length(text) - 1) " "; i++; text = text lines[i] }
        i++
        # Find `jq` as a command word, then the first quoted string after it.
        rest = text
        off = 0
        while (match(rest, /(^|[ \t(|;&`]|[$][(])jq[ \t]/)) {
          after = substr(rest, RSTART + RLENGTH)
          rest = after
          # args up to the program stop at a pipe, redirect, or end of command
          if (match(after, /^[^\047|;<>)]*\047/)) {
            prog = substr(after, RLENGTH + 1)
            # Close quote may be on a later physical line: keep consuming.
            while (index(prog, "\047") == 0 && i <= NR) { prog = prog "\n" lines[i]; i++ }
            q = index(prog, "\047")
            if (q == 0) { emit("prog", start, prog); break }
            emit("prog", start, substr(prog, 1, q - 1))
            rest = substr(prog, q + 1)
          }
        }
      }
    }
  ' "$1"
}

fail=0
checked=0
for script in "$@"; do
  rm -f "${work}/index.txt" "${work}"/prog.* "${work}"/heredoc.*
  : > "${work}/index.txt"
  extract "${script}"
  while read -r kind n line; do
    file="${work}/${kind}.${n}"
    args=()
    if [ "${kind}" = prog ]; then
      # Dummy value for every $var so only syntax problems surface.
      while read -r v; do
        case "${v}" in __loc__|ENV|__prog_args) continue ;; esac
        args+=(--arg "${v}" x)
      done < <(grep -o '\$[A-Za-z_][A-Za-z0-9_]*' "${file}" | tr -d '$' | sort -u)
      set +e
      err=$("${JQ}" -n "${args[@]}" -f "${file}" 2>&1 >/dev/null </dev/null)
      rc=$?
      set -e
      # 3 is jq's compile-error exit; 2 is usage (e.g. bad option).
      if [ "${rc}" -eq 3 ] || [ "${rc}" -eq 2 ]; then
        fail=1
        echo "${script}:${line}: jq program does not compile: $(echo "${err}" | head -n 3 | tr '\n' ' ')" >&2
      fi
    else
      set +e
      err=$("${JQ}" -c . "${file}" 2>&1 >/dev/null)
      rc=$?
      set -e
      if [ "${rc}" -ne 0 ]; then
        fail=1
        echo "${script}:${line}: heredoc is not valid JSON: ${err}" >&2
      fi
    fi
    checked=$((checked + 1))
  done < "${work}/index.txt"
done

echo "check-jq: ${checked} embedded jq programs/heredocs checked"
if [ "${checked}" -eq 0 ]; then
  echo "check-jq: found nothing to check; the extractor is probably broken" >&2
  exit 1
fi
exit "${fail}"
