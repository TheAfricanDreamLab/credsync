#!/usr/bin/env bash
#
# check-fuzz-coverage.sh - every wire-facing decoder has a fuzz target.
#
# CS-29's definition of done says "a fuzz target exists for every wire-facing decoder - none
# omitted". A hand-kept list satisfies that on the day it is written and quietly stops being true
# the first time somebody adds a type to the wire - which is exactly when fuzzing it matters most,
# because a brand new decoder is the least exercised code in the crate.
#
# So the list is derived, not maintained: every public type in wire.rs that derives Deserialize is
# reachable from the network by definition, and must have a target named after it.
set -euo pipefail

cd "$(dirname "$0")/.."

WIRE=credsync-protocol/src/wire.rs
TARGETS=credsync-protocol/fuzz/fuzz_targets

[ -f "$WIRE" ] || { echo "error: $WIRE not found" >&2; exit 1; }
[ -d "$TARGETS" ] || { echo "error: $TARGETS not found - has cargo fuzz init been run?" >&2; exit 1; }

# A public type that derives Deserialize is one bytes off the network can become.
#
# `Document` is excluded by name: it is generic over its limit and is never decoded as itself. Its
# two instantiations, Snapshot and Payload, have targets of their own and are what actually appear
# on the wire.
# The derive attribute is accumulated across lines, not read as one.
#
# rustfmt wraps a `#[derive(...)]` that exceeds max_width, which puts `Deserialize` on a
# continuation line. A single-line match would not see it, the type would look like it was never
# wire-facing, and this gate would quietly stop covering it -- the exact failure it exists to
# prevent, caused by reformatting. Caught in review on #73.
decoders=$(
  awk '
    # An attribute begins, and may continue over several lines until its brackets close.
    /^#\[/ { attr = attr " " $0; in_attr = ($0 !~ /\]$/); next }
    # A continuation line of a wrapped attribute. `)]` starts at column zero, so this cannot key
    # on indentation alone.
    in_attr { attr = attr " " $0; if ($0 ~ /\]$/) in_attr = 0; next }
    /^pub (struct|enum) / {
      if (attr ~ /Deserialize/) {
        name = $3
        sub(/[<({].*/, "", name)
        if (name != "Document") print name
      }
      attr = ""
      next
    }
    # A blank line separates items, so anything accumulated belonged to the previous one.
    /^[[:space:]]*$/ { attr = "" }
  ' "$WIRE" | sort -u
)

# Snapshot and Payload are type *aliases* for `Document<..>`, so they are declarations the scan
# above cannot see. Named here, and checked exactly like the rest: a missing target fails the same
# way. `Document` itself is excluded because it is generic over its limit and never decoded as
# itself -- only these two instantiations appear on the wire.
decoders=$(printf '%s\nSnapshot\nPayload\n' "$decoders" | sort -u)

missing=0
count=0
for ty in $decoders; do
  # `PullResponse` -> `pull_response`
  target=$(echo "$ty" | sed -E 's/([a-z0-9])([A-Z])/\1_\2/g' | tr '[:upper:]' '[:lower:]')
  count=$((count + 1))
  if [ ! -f "$TARGETS/${target}.rs" ]; then
    echo "  MISSING  $ty -> $TARGETS/${target}.rs"
    missing=1
  fi
done

# The check runs both ways.
#
# Checking only "every decoder has a target" passes when the parser finds *no* decoders, which is
# the one failure that matters: a gate that silently stops looking reports success for ever.
# Measured during review on #73 -- a wrapped derive made the scan miss a type, the count dropped
# from 18 to 17, and the script still said "ok".
#
# A target with no matching decoder therefore fails too. It means either the parser broke, or a
# wire type was removed and its target should go with it; both want a person to look.
for f in "$TARGETS"/*.rs; do
  target=$(basename "$f" .rs)
  [ "$target" = "_shared" ] && continue
  expected=$(echo "$target" | awk -F_ '{ for (i=1;i<=NF;i++) printf "%s%s", toupper(substr($i,1,1)), substr($i,2); print "" }')
  if ! echo "$decoders" | grep -qx "$expected"; then
    echo "  ORPHAN   $target.rs -> no '$expected' found in $WIRE"
    missing=1
  fi
done

echo "  $count wire-facing decoder(s) checked"
if [ "$missing" -ne 0 ]; then
  echo
  echo "FAIL: the fuzz targets and the wire types disagree." >&2
  echo "A MISSING line means a type reachable from the network has no target: add it, rather" >&2
  echo "than adding an exception here -- a new decoder is the least exercised code in the crate." >&2
  echo "An ORPHAN line means this script could not find a type one of its targets names, which" >&2
  echo "usually means the scan broke rather than that the type went away." >&2
  exit 1
fi
echo "  ok    every wire-facing decoder is fuzzed, and every target names a real one"
