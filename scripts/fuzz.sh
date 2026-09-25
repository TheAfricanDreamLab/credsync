#!/usr/bin/env bash
#
# fuzz.sh - run every wire decoder's fuzz target for a fixed time each.
#
#   ./scripts/fuzz.sh              # 60s per target, the PR smoke run
#   ./scripts/fuzz.sh 900          # 15 minutes per target, the nightly run
#   ./scripts/fuzz.sh 60 batch     # one target, for chasing something down
#
# # Why a fixed time per target rather than a total
#
# A total would let the first target eat the budget and leave the last one unexercised, and it is
# the least-loved decoder that most needs the time. Equal shares is also what makes two runs
# comparable: a target that starts finding new coverage is a signal, and a signal only exists
# against a stable baseline.
#
# A crash writes its input to fuzz/artifacts/<target>/. That file IS the bug report: it reproduces
# with `cargo +nightly fuzz run <target> <file>`. CS-29's definition of done requires it to become
# an issue and to join the corpus, so the next run starts from the input that broke us.
set -euo pipefail

cd "$(dirname "$0")/../credsync-protocol"

SECONDS_PER_TARGET="${1:-60}"
ONLY="${2:-}"

command -v cargo-fuzz >/dev/null 2>&1 || {
  echo "error: cargo-fuzz not installed. cargo install cargo-fuzz --locked" >&2
  exit 1
}

targets=$(cargo +nightly fuzz list)
[ -n "$ONLY" ] && targets="$ONLY"

failed=0
for target in $targets; do
  echo "=== $target (${SECONDS_PER_TARGET}s) ==="
  mkdir -p "fuzz/corpus/$target"
  if ! cargo +nightly fuzz run "$target" -- \
        -max_total_time="$SECONDS_PER_TARGET" \
        -print_final_stats=1 2>&1 | tail -6; then
    echo "  CRASH in $target - see fuzz/artifacts/$target/" >&2
    failed=1
  fi
done

if [ "$failed" -ne 0 ]; then
  echo >&2
  echo "FAIL: a decoder crashed on network bytes." >&2
  echo "The artifact under fuzz/artifacts/ is the reproduction. Open an issue carrying it," >&2
  echo "and copy it into fuzz/corpus/<target>/ so the next run starts from it." >&2
  exit 1
fi
echo "all targets clean at ${SECONDS_PER_TARGET}s each"
