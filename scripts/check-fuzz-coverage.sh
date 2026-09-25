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
decoders=$(
  awk '
    /^#\[derive\(/ { d = $0 }
    /^pub (struct|enum) / {
      if (d ~ /Deserialize/) {
        name = $3
        sub(/[<({].*/, "", name)
        if (name != "Document") print name
      }
      d = ""
    }
  ' "$WIRE" | sort -u
)

# Snapshot and Payload are type aliases rather than declarations, so they are named here. They are
# still checked like the rest: a missing target fails the same way.
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

echo "  $count wire-facing decoder(s) checked"
if [ "$missing" -ne 0 ]; then
  echo
  echo "FAIL: a type reachable from the network has no fuzz target." >&2
  echo "A new decoder is the least exercised code in the crate, which is when fuzzing it" >&2
  echo "matters most. Add the target rather than adding an exception here." >&2
  exit 1
fi
echo "  ok    every wire-facing decoder is fuzzed"
