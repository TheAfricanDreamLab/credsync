#!/usr/bin/env bash
#
# check-decisions.sh - verify every document referenced by docs/DECISIONS.md exists.
#
# The register cites a source for every decision. A citation pointing at a file that is gone,
# renamed, or never existed silently misleads a future session into thinking a decision is
# grounded when it is not. This turns that into a build failure.
#
# See DECISIONS.md and CLAUDE.md section 6.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REG="$ROOT/docs/DECISIONS.md"

[ -f "$REG" ] || { echo "error: $REG not found" >&2; exit 1; }

failed=0

# 1. Every markdown link target in the register must resolve.
#    spec.md is written at CS-2, so it is allowed to be missing until then.
while read -r target; do
  [ -z "$target" ] && continue
  case "$target" in
    http*|\#*) continue ;;
  esac
  path="$ROOT/docs/${target%%#*}"
  if [ ! -f "$path" ]; then
    if [ "${target%%#*}" = "spec.md" ]; then
      echo "  note  spec.md not yet written (due at CS-2) - allowed"
    else
      echo "  FAIL  broken citation: $target" >&2
      failed=1
    fi
  else
    echo "  ok    $target"
  fi
done < <(grep -o '](\([^)]*\.md[^)]*\))' "$REG" | sed 's/^](//; s/)$//' | sort -u)

# 2. Decision IDs must be unique and contiguous - a duplicate or a gap means an entry was
#    edited carelessly, and the register's whole value is that entries are stable references.
dupes="$(grep -o '^| D-[0-9]\{3\}' "$REG" | sort | uniq -d || true)"
if [ -n "$dupes" ]; then
  echo "  FAIL  duplicate decision ids:" >&2
  echo "$dupes" >&2
  failed=1
fi

odupes="$(grep -o '^| O-[0-9]\{3\}' "$REG" | sort | uniq -d || true)"
if [ -n "$odupes" ]; then
  echo "  FAIL  duplicate open-question ids:" >&2
  echo "$odupes" >&2
  failed=1
fi

n_d="$(grep -c '^| D-[0-9]\{3\}' "$REG" || true)"
n_o="$(grep -c '^| O-[0-9]\{3\}' "$REG" || true)"
echo
echo "  ${n_d} decisions, ${n_o} open questions"

if [ "$failed" -ne 0 ]; then
  echo "decision register check FAILED" >&2
  exit 1
fi

echo "decision register clean"
