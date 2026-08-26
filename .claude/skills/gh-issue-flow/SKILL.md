---
name: gh-issue-flow
description: The credSync delivery loop - restate the issue, name skills, branch, implement, prove, PR with Closes #N and test evidence. Covers branch naming, commit conventions, when to open a new issue instead of expanding scope (always), and the slice-to-issue number offset. Use at the start of every build session and when opening any PR.
---

# The delivery loop

> pick the top open issue in the milestone → restate it → branch → implement → prove → PR with
> `Closes #N` → CI green → merge → issue closes itself → next issue

One issue per session by default. A second only if the first closed.

## Slice IDs vs issue numbers

**Slice CS-N is GitHub issue #N+1.** GitHub numbers start at 1, so `CS-0` is issue #1 and `CS-32`
is issue #33 (D-016).

Titles and branches carry the **slice ID**; only `Closes #N` uses the GitHub number. CS-1 is
issue #2 on branch `feat/cs-1-<slug>`, and its PR says `Closes #2`. Keeping the slice ID in the
branch means the branch name matches the plan, which is what you are actually working from.

## Open — before writing any code

1. **Read the issue.** All of it, including what it says is out of scope.
2. **Restate the slice and the definition of done in your own words.** If your restatement does
   not match the issue, the issue is unclear — **fix the issue first**. This is the cheapest bug
   you will ever fix; do not skip it because the issue "looks obvious".
3. **Name the skills you will use.** The issue's `## Skills to use` section lists them. Load them.
4. **Create the branch:**

```sh
git checkout main && git pull
git checkout -b feat/cs-<slice-id>-<slug>    # or fix/cs-<slice-id>-<slug>
```

## Build

- Implement **inside the named skills' rules**.
- **Tests are written with the change, not after.** For credsync specifically: the invariant or
  property test lands in the same commit as the behaviour it proves.
- **Scope discovered mid-slice becomes a new issue.** Never expand a slice in flight — that is the
  primary failure mode this whole process exists to prevent. Open the issue, note it, stay on
  yours.

Commits follow Conventional Commits — `feat:`, `fix:`, `test:`, `docs:`, `chore:` — with the body
referencing the issue.

**Commits are authored by Ukeme alone.** No `Co-Authored-By` trailer, no tool attribution
anywhere in the commit or PR (D-013).

## Prove

Run the definition-of-done commands **verbatim**, as written in the issue. Not something similar.
Not a subset. If a check cannot be run as written, the issue needs fixing.

## Close

PR title = issue title. Body opens with `Closes #N`.

```markdown
Closes #7

## What changed
<one paragraph>

## Test evidence
```
$ cargo test -p credsync-core
running 24 tests ... ok

$ cargo run -p credsync-sim -- --seeds 1000
1000/1000 seeds green
```

## Notes
<anything a reviewer or a future session needs>
```

**A PR without test evidence is not ready**, however green CI looks. CI proves the gates passed;
the evidence block proves *you ran the thing the issue asked for*. They are not the same claim.

## After the merge — sync the fork

`ukemeikot/credsync` is the owner's personal copy of this repo (D-014). Fast-forward it once the
merge lands:

```sh
./scripts/sync-fork.sh        # idempotent; a no-op when already in sync
```

A fork that silently rots is worse than no fork: it looks like a copy of the project while being
a copy of the project as it was weeks ago. The script verifies the result rather than assuming
it, because `gh repo sync` declines to fast-forward a diverged fork without being loud about it.

## Learn

Anything this session discovered that a future one needs — a build quirk, a pattern, a trap —
is committed as a skill edit or a `CLAUDE.md` line **in the same PR**. A lesson left in a session
transcript is a lesson lost.

## Filing a bug

Bugs get an issue like anything else. For simulator failures, **the seed goes in the title**:

```
sim: divergence at seed 0x4f21a9c3
```

The seed is the reproduction — the whole reproduction. Include the trace if it is short.

## Reconciling the backlog with the plan

`scripts/seed-backlog.sh` is idempotent: it matches on title and **updates** rather than
duplicating. After revising the Slice Plan, re-run it to bring GitHub back in line with the
document.

```sh
DRY_RUN=1 ./scripts/seed-backlog.sh     # preview
./scripts/seed-backlog.sh               # apply
```

## Never

- Never merge with a red gate. Weakening a gate to merge is the one forbidden move.
- Never open a PR that closes more than one issue, or none.
- Never mix planning and building in one session. Planning sessions produce issues; build sessions
  consume them, one at a time.
