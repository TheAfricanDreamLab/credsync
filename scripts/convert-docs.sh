#!/usr/bin/env bash
#
# convert-docs.sh - regenerate docs/*.md from the Word originals in docs/source/.
#
# The planning documents are authored in Word. Claude Code cannot read .docx, so the
# markdown in docs/ is the working copy - but it is GENERATED, never hand-edited. Edit
# the .docx, re-run this, commit both.
#
# Fails the build on any ragged table row, which is how a silent conversion regression
# gets caught rather than shipped. See DECISIONS.md D-014.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/docs/source"
OUT="$ROOT/docs"
LIB="$ROOT/scripts/lib"

command -v node >/dev/null 2>&1 || { echo "error: node not found" >&2; exit 1; }

# Conversion deps live under scripts/lib so they never pollute the repo root and never
# end up in a published artifact.
if [ ! -d "$LIB/node_modules" ]; then
  echo "installing conversion dependencies..."
  (cd "$LIB" && npm install --silent --no-fund --no-audit)
fi

# docx basename | output markdown | document title
DOCS=(
  "DreamLabOS_Plan_v1.1.docx|platform-plan-v1.1.md|Dream Lab OS Platform Plan v1.1"
  "credSync_Design_v2.1.docx|credsync-design-v2.1.md|credSync Design Document v2.1"
  "Execution_Playbook_v1.0.docx|execution-playbook-v1.0.md|Execution Playbook v1.0"
)

rm -rf "$OUT/images"
failed=0

for entry in "${DOCS[@]}"; do
  IFS='|' read -r src dst title <<< "$entry"
  if [ ! -f "$SRC/$src" ]; then
    echo "error: missing source $SRC/$src" >&2
    failed=1
    continue
  fi
  node "$LIB/convert-doc.mjs" "$SRC/$src" "$OUT/$dst" "$title" "$src" || failed=1
done

# Cross-check: the markdown must contain exactly as many tables as the .docx does.
# Catches the failure mode where a table silently degrades into prose.
echo
echo "table fidelity check:"
for entry in "${DOCS[@]}"; do
  IFS='|' read -r src dst _ <<< "$entry"
  n_src="$(unzip -p "$SRC/$src" word/document.xml | grep -o '<w:tbl>' | wc -l | tr -d ' ')"
  n_md="$(grep -c '^| --- ' "$OUT/$dst" || true)"
  if [ "$n_src" = "$n_md" ]; then
    printf '  ok    %-32s %s tables\n' "$dst" "$n_md"
  else
    printf '  FAIL  %-32s docx=%s md=%s\n' "$dst" "$n_src" "$n_md"
    failed=1
  fi
done

if [ "$failed" -ne 0 ]; then
  echo
  echo "conversion FAILED - do not commit these files" >&2
  exit 1
fi

echo
echo "conversion clean"
