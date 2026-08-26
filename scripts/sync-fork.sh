#!/usr/bin/env bash
#
# sync-fork.sh - fast-forward the owner's personal fork to match upstream main.
#
# `ukemeikot/credsync` is a personal copy of the canonical org repo (DECISIONS.md D-014). A fork
# that silently rots is worse than no fork: it looks like a copy of the project while actually
# being a copy of the project as it was weeks ago.
#
# Idempotent - running it when already in sync is a no-op that reports so.
#
# Usage:  ./scripts/sync-fork.sh
#         FORK=owner/name UPSTREAM=owner/name ./scripts/sync-fork.sh
#
set -euo pipefail

UPSTREAM="${UPSTREAM:-TheAfricanDreamLab/credsync}"
FORK="${FORK:-ukemeikot/credsync}"
BRANCH="${BRANCH:-main}"

command -v gh >/dev/null 2>&1 || { echo "error: gh not found" >&2; exit 1; }

# Returns a 40-char sha, or nothing. `gh api --jq` prints the API error body on failure, so the
# result is validated rather than trusted - otherwise an error payload flows on as if it were a
# commit id and the failure surfaces much later, wearing a confusing disguise.
sha_of() {
  local out
  out="$(gh api "repos/$1/commits/$BRANCH" --jq .sha 2>/dev/null || true)"
  if [[ "$out" =~ ^[0-9a-f]{40}$ ]]; then printf '%s' "$out"; fi
  # Always succeed. Returning non-zero here would trip `set -e` at the assignment site and kill
  # the script before it could report which repo it failed to read - a silent failure in the one
  # script whose job is to make a silent failure impossible.
  return 0
}

up="$(sha_of "$UPSTREAM")"
[ -n "$up" ] || { echo "error: cannot read $UPSTREAM@$BRANCH" >&2; exit 1; }

before="$(sha_of "$FORK")"
if [ -z "$before" ]; then
  echo "error: cannot read $FORK@$BRANCH - does the fork still exist?" >&2
  exit 1
fi

if [ "$before" = "$up" ]; then
  echo "already in sync: ${up:0:7}"
  exit 0
fi

echo "fork      ${before:0:7}"
echo "upstream  ${up:0:7}"
echo "syncing..."
gh repo sync "$FORK" --source "$UPSTREAM" --branch "$BRANCH" >/dev/null

# Verify rather than assume. `gh repo sync` can decline to fast-forward when the fork has
# diverged, and it is not loud about it.
after="$(sha_of "$FORK")"
if [ "$after" = "$up" ]; then
  echo "synced:   ${after:0:7}"
else
  echo "FAILED: fork is at ${after:0:7}, upstream at ${up:0:7}" >&2
  echo "The fork has probably diverged - resolve by hand rather than force-pushing blindly." >&2
  exit 1
fi
