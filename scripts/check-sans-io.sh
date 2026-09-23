#!/usr/bin/env bash
#
# check-sans-io.sh - refuse ambient access to the real world in the pure crates.
#
# The existing `core is sans-IO` CI job greps the *dependency graph*. That catches `tokio` and
# `reqwest`, and misses the thing most likely to actually happen: `SystemTime::now()` needs no
# dependency at all. It is one line, it looks harmless, it compiles, every test still passes --
# and deterministic replay is gone. The simulator keeps reporting green while having quietly lost
# the ability to find anything, which is the worst failure mode available to this project.
#
# So this checks the source itself. The two gates are complements, not duplicates: one watches
# what the crate depends on, the other watches what it writes.
#
# Usage:  ./scripts/check-sans-io.sh
#         CRATES="credsync-core" ./scripts/check-sans-io.sh
#
# See CLAUDE.md section 3 and .claude/skills/rust-sans-io/SKILL.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Only the pure crates. credsyncd and the bindings exist precisely to do this work, and
# credsync-sim implements the seeded fakes, so all three are legitimately full of it.
CRATES="${CRATES:-credsync-core credsync-protocol}"

# Pattern -> what to do instead. Kept in step with the ban list in the rust-sans-io skill; if you
# add a row in one place, add it in both.
PATTERNS=(
  'Instant::now'          'Clock::now(), injected'
  'SystemTime::now'       'Clock::now(), injected'
  'thread::sleep'         'Effect::ScheduleRetry(at)'
  'std::thread'           'the core is single-threaded; the caller owns threads'
  '\brand::'              'Entropy::fill()'
  '\bgetrandom\b'         'Entropy::fill()'
  '\btokio\b'             'the core is synchronous; async lives in credsyncd'
  '\breqwest\b'           'Effect::Send(req) / Transport::enqueue'
  'std::fs'               'Storage::transact(ops)'
  '\brusqlite\b'          'Storage::transact(ops)'
  'async fn'              'the core is synchronous'
)

fail=0
checked=0

for crate in $CRATES; do
  dir="$ROOT/$crate/src"
  [ -d "$dir" ] || { echo "  skip  $crate (no src/)"; continue; }

  while IFS= read -r file; do
    checked=$((checked + 1))

    # Strip comment-only lines before matching. Every one of these tokens appears legitimately in
    # the documentation that explains why it is banned -- lib.rs says "no `Instant::now`" in prose
    # -- and a gate that fired on its own rationale would be turned off within a week.
    #
    # A trailing comment on a line of code is NOT stripped, and that is deliberate: the check
    # stays strict rather than clever, and `// see Instant::now` after real code is rare enough to
    # move to its own line.
    body="$(sed 's|^[[:space:]]*//.*$||' "$file")"

    i=0
    while [ $i -lt ${#PATTERNS[@]} ]; do
      pattern="${PATTERNS[$i]}"
      instead="${PATTERNS[$((i + 1))]}"
      i=$((i + 2))

      if hits="$(printf '%s\n' "$body" | grep -nE "$pattern" || true)"; [ -n "$hits" ]; then
        rel="${file#"$ROOT"/}"
        while IFS= read -r hit; do
          echo "  FAIL  $rel:${hit%%:*}  '$pattern' -> use $instead" >&2
        done <<< "$hits"
        fail=1
      fi
    done
  done < <(find "$dir" -name '*.rs' -type f | sort)
done

echo
echo "  ${checked} source files checked in: $CRATES"

if [ "$fail" -ne 0 ]; then
  echo "sans-IO source check FAILED" >&2
  echo "A hidden clock read or a stray thread does not fail a test - it removes the simulator's" >&2
  echo "ability to find bugs, silently. That is why this is a build failure and not a warning." >&2
  exit 1
fi

echo "sans-IO source check clean"
